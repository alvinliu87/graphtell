//! PHP 解析器：把 tree-sitter PHP 语法树翻译成语言无关的 [`SyntaxFacts`]。

pub mod value;

use std::cell::RefCell;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, ImportFact, InheritanceFact,
    Language, NodeKind, SyntaxFacts,
};
use serde_json::json;
use tree_sitter::{Language as TsLanguage, Node, Parser};
use value::{eval_expr, span_of, text};

use gt_domain::port::LanguageParser;

/// PHP 解析器适配器。
pub struct PhpParser {
    language: TsLanguage,
}

impl PhpParser {
    pub fn new() -> Result<Self> {
        let language: TsLanguage = tree_sitter_php::LANGUAGE_PHP.into();
        Ok(Self { language })
    }
}

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

impl LanguageParser for PhpParser {
    fn language(&self) -> Language {
        Language::new(Language::PHP)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["php", "phtml", "php5", "php7", "php8", "inc"]
    }

    fn namespace_separator(&self) -> &'static [char] {
        &['\\']
    }

    fn member_separator(&self) -> &'static str {
        "::"
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["composer.json"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["vendor"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // 语言在构造时已校验，这里失败属于编程错误
                p.set_language(&self.language).expect("php language");
                p
            });
            parser.parse(source, None)
        });
        let tree = tree.ok_or_else(|| {
            DomainError::Parse { file: path.into(), reason: "tree-sitter 返回 None".into() }
        })?;

        let mut facts = SyntaxFacts::default();
        let root = tree.root_node();
        let mut ctx = Ctx { src: source, facts: &mut facts, ns: None, class_stack: Vec::new() };

        // 命名空间（取第一个 namespace_definition）
        if let Some(ns) = find_child_kind(root, "namespace_definition") {
            let name_node = ns
                .child_by_field_name("name")
                .or_else(|| find_child_kind(ns, "namespace_name"));
            if let Some(name_node) = name_node {
                ctx.ns = Some(trim_leading(text(name_node, source)));
            }
        }
        let ns = ctx.ns.clone();
        ctx.facts.namespace = ns.clone();
        if let Some(ns) = &ns {
            ctx.facts.declarations.push(Declaration {
                kind: NodeKind(NodeKind::NAMESPACE.to_string()),
                name: ns.clone(),
                fqn: ns.clone(),
                parent_fqn: None,
                span: span_of(root),
                extra: json!({}),
            });
        }

        walk_program(root, &mut ctx);
        Ok(facts)
    }
}

struct Ctx<'a> {
    src: &'a str,
    facts: &'a mut SyntaxFacts,
    ns: Option<String>,
    /// 当前所处的类/接口/trait 的 FQN 栈。
    class_stack: Vec<String>,
}

/// 链式调用的根对象：`a()->b()->c()` 返回 `a`。
///
/// 只穿透**调用**表达式，不穿透普通属性访问
/// （`$this->dao->getList` 的根是 `$this->dao` 而不是 `$this`）。
fn chain_root<'a>(mut node: Node<'a>, src: &str) -> String {
    loop {
        let next = match node.kind() {
            "member_call_expression" | "nullsafe_member_call_expression" => {
                node.child_by_field_name("object")
            }
            "scoped_call_expression" => node.child_by_field_name("scope"),
            "function_call_expression" => node.child_by_field_name("function"),
            _ => None,
        };
        match next {
            Some(n) => node = n,
            None => return trim_leading(text(node, src)),
        }
    }
}

fn trim_leading(s: String) -> String {
    s.trim_start_matches('\\').to_string()
}

fn find_child_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.named_children(&mut node.walk()).find(|c| c.kind() == kind)
}

fn walk_program(root: Node, ctx: &mut Ctx) {
    // 命名空间有 body 时，真正的声明在 body 内部
    let entry = if let Some(ns) = find_child_kind(root, "namespace_definition") {
        ns.child_by_field_name("body").unwrap_or(root)
    } else {
        root
    };
    walk_scope(entry, ctx, None);
}

fn walk_scope(node: Node, ctx: &mut Ctx, owner_fqn: Option<&str>) {
    // 顶层/命名空间层语句的归属：没有方法时挂到 "<file>"（最终落到 File 节点）
    let top = owner_fqn.unwrap_or("<file>").to_string();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_use_declaration" => collect_imports(child, ctx),
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => collect_type(child, ctx),
            "function_definition" => collect_function(child, ctx),
            "const_declaration" => collect_const(child, ctx, owner_fqn),
            // 配置文件：`return [...];`
            "return_statement" => {
                collect_config_return(child, ctx);
                collect_call_sites(child, ctx, &top);
            }
            "namespace_definition" => walk_scope(child, ctx, owner_fqn),
            // 其余语句（路由文件里的 `Route::get(...)` 等）统一收集调用点
            _ => collect_call_sites(child, ctx, &top),
        }
    }
}

fn collect_imports(node: Node, ctx: &mut Ctx) {
    // body 可能是 namespace_use_group（use A\{B, C};）
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                if let Some(imp) = import_from_clause(child, ctx) {
                    ctx.facts.imports.push(imp);
                }
            }
            "namespace_use_group" => {
                let prefix = child
                    .named_children(&mut child.walk())
                    .find(|c| matches!(c.kind(), "namespace_name" | "qualified_name" | "name"))
                    .map(|c| trim_leading(text(c, ctx.src)))
                    .unwrap_or_default();
                let mut g = child.walk();
                for c in child.named_children(&mut g) {
                    if c.kind() == "namespace_use_clause" {
                        if let Some(mut imp) = import_from_clause(c, ctx) {
                            imp.name = format!("{}\\{}", prefix, imp.name);
                            ctx.facts.imports.push(imp);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn import_from_clause(node: Node, ctx: &Ctx) -> Option<ImportFact> {
    let name_node = node
        .named_children(&mut node.walk())
        .find(|c| matches!(c.kind(), "name" | "qualified_name" | "namespace_name"))?;
    let alias = node
        .child_by_field_name("alias")
        .map(|a| text(a, ctx.src));
    Some(ImportFact {
        alias,
        name: trim_leading(text(name_node, ctx.src)),
        span: span_of(node),
    })
}

fn collect_type(node: Node, ctx: &mut Ctx) {
    let kind = match node.kind() {
        "class_declaration" => NodeKind::CLASS,
        "interface_declaration" => NodeKind::INTERFACE,
        "trait_declaration" => NodeKind::TRAIT,
        _ => NodeKind::ENUM,
    };
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src))
        .unwrap_or_else(|| "<anonymous>".into());
    let fqn = qualify(ctx.ns.as_deref(), &name);

    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(kind.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: ctx.ns.clone(),
        span: span_of(node),
        extra: json!({ "abstract": has_modifier(node, "abstract_modifier"), "final": has_modifier(node, "final_modifier") }),
    });

    // 继承 / 实现
    if let Some(base) = find_child_kind(node, "base_clause") {
        let mut c = base.walk();
        for b in base.named_children(&mut c) {
            ctx.facts.inheritances.push(InheritanceFact {
                child_fqn: fqn.clone(),
                base_name: trim_leading(text(b, ctx.src)),
                kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
                span: span_of(b),
            });
        }
    }
    if let Some(iface) = find_child_kind(node, "class_interface_clause") {
        let mut c = iface.walk();
        for b in iface.named_children(&mut c) {
            ctx.facts.inheritances.push(InheritanceFact {
                child_fqn: fqn.clone(),
                base_name: trim_leading(text(b, ctx.src)),
                kind: EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                span: span_of(b),
            });
        }
    }

    ctx.class_stack.push(fqn.clone());
    if let Some(body) = find_type_body(node) {
        let mut c = body.walk();
        for member in body.named_children(&mut c) {
            match member.kind() {
                "method_declaration" => collect_method(member, ctx, &fqn),
                "property_declaration" => collect_property(member, ctx, &fqn),
                "const_declaration" => collect_const(member, ctx, Some(&fqn)),
                "enum_case" => collect_enum_case(member, ctx, &fqn),
                "use_declaration" => collect_trait_use(member, ctx, &fqn),
                _ => {}
            }
        }
    }
    ctx.class_stack.pop();
}

fn find_type_body<'a>(node: Node<'a>) -> Option<Node<'a>> {
    find_child_kind(node, "declaration_list").or_else(|| find_child_kind(node, "enum_declaration_list"))
}

fn collect_trait_use(node: Node, ctx: &mut Ctx, owner: &str) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "use_list" => {
                let mut g = child.walk();
                for t in child.named_children(&mut g) {
                    if let Some(n) = t
                        .named_children(&mut t.walk())
                        .find(|c| matches!(c.kind(), "name" | "qualified_name"))
                    {
                        push_trait(ctx, owner, &trim_leading(text(n, ctx.src)), span_of(t));
                    }
                }
            }
            "name" | "qualified_name" => {
                push_trait(ctx, owner, &trim_leading(text(child, ctx.src)), span_of(child));
            }
            _ => {}
        }
    }
}

fn push_trait(ctx: &mut Ctx, owner: &str, base: &str, span: gt_domain::model::Span) {
    ctx.facts.inheritances.push(InheritanceFact {
        child_fqn: owner.to_string(),
        base_name: base.to_string(),
        kind: EdgeKind(EdgeKind::USES_TRAIT.to_string()),
        span,
    });
}

fn collect_method(node: Node, ctx: &mut Ctx, class_fqn: &str) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src))
        .unwrap_or_default();
    let fqn = format!("{}::{}", class_fqn, name);
    // 参数：记录「名称 + 类型」，供 P7 按"变量类型"解析实例调用（`$services->appAuth()`）。
    let params = node
        .child_by_field_name("parameters")
        .map(|p| {
            p.named_children(&mut p.walk())
                .filter(|c| c.kind() == "simple_parameter")
                .filter_map(|c| {
                    let name = c.child_by_field_name("name").map(|n| text(n, ctx.src))?;
                    let ty = c
                        .child_by_field_name("type")
                        .map(|t| text(t, ctx.src))
                        .filter(|t| !t.is_empty());
                    Some(json!({ "name": name, "type": ty }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // 构造器注入：`$this->services = $services;` → 属性 services 的类型 = 参数 services 的类型。
    let mut this_assigns: Vec<serde_json::Value> = Vec::new();
    // `return X::class;` → 供给 P7 建「所属类 → X」的声明式联系（如 Dao::setModel() → Model）。
    let mut returns_class: Vec<serde_json::Value> = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        collect_this_assigns(body, ctx, &mut this_assigns);
        collect_return_classes(body, ctx, &mut returns_class);
    }

    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::METHOD.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: Some(class_fqn.to_string()),
        span: span_of(node),
        extra: json!({
            "visibility": visibility(node, ctx.src),
            "static": has_modifier(node, "static_modifier"),
            "abstract": has_modifier(node, "abstract_modifier"),
            "parameters": params,
            "this_assigns": this_assigns,
            "returns_class": returns_class,
        }),
    });

    if let Some(body) = node.child_by_field_name("body") {
        collect_call_sites(body, ctx, &fqn);
    }
}

/// 收集 `$this->prop = $var;` 形式的属性赋值（构造器注入常见写法）。
///
/// ThinkPHP 的控制器/服务常写成 `__construct(LoginServices $services) { $this->services = $services; }`，
/// 这里把「属性 → 局部变量」记下来；类型由 P7 结合参数类型推断。
fn collect_this_assigns(node: Node, ctx: &Ctx, out: &mut Vec<serde_json::Value>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "assignment_expression" {
            if let (Some(l), Some(r)) = (
                child.child_by_field_name("left"),
                child.child_by_field_name("right"),
            ) {
                let lt = text(l, ctx.src).trim().to_string();
                let rt = text(r, ctx.src).trim().to_string();
                if lt.starts_with("$this->")
                    && rt.starts_with('$')
                    && !rt.contains("->")
                    && !rt.contains('[')
                {
                    let prop = lt.trim_start_matches("$this->").to_string();
                    let var = rt.trim_start_matches('$').to_string();
                    if !prop.is_empty() && !var.is_empty() {
                        out.push(json!({ "prop": prop, "var": var }));
                    }
                }
            }
        }
        collect_this_assigns(child, ctx, out);
    }
}

/// 收集 `return X::class;` 中引用的类名（原文，可能带命名空间前缀）。
fn collect_return_classes(node: Node, ctx: &Ctx, out: &mut Vec<serde_json::Value>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "return_statement" {
            if let Some(expr) = child.named_child(0) {
                let t = text(expr, ctx.src).trim().to_string();
                if let Some(cls) = t.strip_suffix("::class") {
                    let cls = cls.trim().trim_start_matches('\\');
                    if !cls.is_empty() && !cls.contains('$') && !cls.contains("->") {
                        out.push(json!(cls));
                    }
                }
            }
        }
        collect_return_classes(child, ctx, out);
    }
}

fn collect_function(node: Node, ctx: &mut Ctx) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src))
        .unwrap_or_default();
    let fqn = qualify(ctx.ns.as_deref(), &name);
    let is_closure = name.is_empty();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::FUNCTION.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: ctx.ns.clone(),
        span: span_of(node),
        extra: json!({ "closure": is_closure }),
    });
    if let Some(body) = node.child_by_field_name("body") {
        collect_call_sites(body, ctx, &fqn);
    }
}

fn collect_property(node: Node, ctx: &mut Ctx, class_fqn: &str) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "property_element" {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .map(|n| text(n, ctx.src).trim_start_matches('$').to_string())
            .unwrap_or_default();
        let default_value = child
            .child_by_field_name("default_value")
            .map(|v| eval_expr(v, ctx.src))
            .unwrap_or(FactValue::Null);
        ctx.facts.declarations.push(Declaration {
            kind: NodeKind(NodeKind::PROPERTY.to_string()),
            name: name.clone(),
            fqn: format!("{}::${}", class_fqn, name),
            parent_fqn: Some(class_fqn.to_string()),
            span: span_of(child),
            extra: json!({
                "visibility": visibility(node, ctx.src),
                "static": has_modifier(node, "static_modifier"),
                "default": default_value,
            }),
        });
    }
}

fn collect_const(node: Node, ctx: &mut Ctx, owner: Option<&str>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        let name = child
            .named_children(&mut child.walk())
            .next()
            .map(|n| text(n, ctx.src))
            .unwrap_or_default();
        let value = child
            .named_children(&mut child.walk())
            .nth(1)
            .map(|v| eval_expr(v, ctx.src))
            .unwrap_or(FactValue::Null);
        let fqn = match owner {
            Some(o) => format!("{}::{}", o, name),
            None => qualify(ctx.ns.as_deref(), &name),
        };
        ctx.facts.declarations.push(Declaration {
            kind: NodeKind(NodeKind::CONST.to_string()),
            name,
            fqn,
            parent_fqn: owner.map(|s| s.to_string()).or_else(|| ctx.ns.clone()),
            span: span_of(child),
            extra: json!({ "value": value }),
        });
    }
}

fn collect_enum_case(node: Node, ctx: &mut Ctx, owner: &str) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src))
        .unwrap_or_default();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::ENUM_CASE.to_string()),
        name: name.clone(),
        fqn: format!("{}::{}", owner, name),
        parent_fqn: Some(owner.to_string()),
        span: span_of(node),
        extra: json!({}),
    });
}

/// 配置文件：把 `return [...]` 展平成 `config_entries`。
fn collect_config_return(node: Node, ctx: &mut Ctx) {
    let Some(expr) = node.named_child(0) else { return };
    let value = eval_expr(expr, ctx.src);
    flatten_config(&value, String::new(), span_of(node), ctx);
}

fn flatten_config(value: &FactValue, prefix: String, span: gt_domain::model::Span, ctx: &mut Ctx) {
    match value {
        FactValue::Array(items) => {
            for (k, v) in items {
                let key_path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                if matches!(v, FactValue::Array(_)) {
                    // 数组本身也是一条条目（value 为整个数组），再递归展开子键
                    ctx.facts.config_entries.push(ConfigEntryFact {
                        key_path: key_path.clone(),
                        value: v.clone(),
                        span,
                    });
                    flatten_config(v, key_path, span, ctx);
                } else {
                    ctx.facts.config_entries.push(ConfigEntryFact {
                        key_path,
                        value: v.clone(),
                        span,
                    });
                }
            }
        }
        other => {
            ctx.facts.config_entries.push(ConfigEntryFact {
                key_path: prefix,
                value: other.clone(),
                span,
            });
        }
    }
}

/// 收集方法体内的调用点。
fn collect_call_sites(node: Node, ctx: &mut Ctx, owner_fqn: &str) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "member_call_expression" | "nullsafe_member_call_expression" => {
                let raw_object = child.child_by_field_name("object");
                let object = raw_object.map(|o| chain_root(o, ctx.src));
                let method = child.child_by_field_name("name").map(|n| text(n, ctx.src));
                let args = args_of(child, ctx);
                if let (Some(object), Some(method)) = (object, method) {
                    let snippet = snippet_of(child, ctx.src);
                    ctx.facts.call_sites.push(CallSiteFact {
                        owner_fqn: owner_fqn.to_string(),
                        owner_class: None,
                        callee_text: format!("{}->{}", object, method),
                        snippet,
                        receiver: Some(object),
                        method: Some(method),
                        args,
                        span: span_of(child),
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            "scoped_call_expression" => {
                let scope = child.child_by_field_name("scope").map(|o| text(o, ctx.src));
                let method = child.child_by_field_name("name").map(|n| text(n, ctx.src));
                let args = args_of(child, ctx);
                if let (Some(scope), Some(method)) = (scope, method) {
                    let scope = trim_leading(scope);
                    let snippet = snippet_of(child, ctx.src);
                    ctx.facts.call_sites.push(CallSiteFact {
                        owner_fqn: owner_fqn.to_string(),
                        owner_class: None,
                        callee_text: format!("{}::{}", scope, method),
                        snippet,
                        receiver: Some(scope),
                        method: Some(method),
                        args,
                        span: span_of(child),
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            "function_call_expression" => {
                let func = child.child_by_field_name("function").map(|f| text(f, ctx.src));
                let args = args_of(child, ctx);
                if let Some(func) = func {
                    let f = trim_leading(func);
                    let snippet = snippet_of(child, ctx.src);
                    ctx.facts.call_sites.push(CallSiteFact {
                        owner_fqn: owner_fqn.to_string(),
                        owner_class: None,
                        callee_text: f.clone(),
                        snippet,
                        receiver: None,
                        method: Some(f),
                        args,
                        span: span_of(child),
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            "object_creation_expression" => {
                let cls = child
                    .named_children(&mut child.walk())
                    .find(|c| matches!(c.kind(), "name" | "qualified_name"))
                    .map(|c| trim_leading(text(c, ctx.src)));
                let args = args_of(child, ctx);
                if let Some(cls) = cls {
                    let snippet = snippet_of(child, ctx.src);
                    ctx.facts.call_sites.push(CallSiteFact {
                        owner_fqn: owner_fqn.to_string(),
                        owner_class: None,
                        callee_text: format!("new {}", cls),
                        snippet,
                        receiver: None,
                        method: Some(cls),
                        args,
                        span: span_of(child),
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            _ => recurse_calls(child, ctx, owner_fqn),
        }
    }
}

fn recurse_calls(node: Node, ctx: &mut Ctx, owner_fqn: &str) {
    // 闭包/匿名函数内部的调用归属外层方法
    collect_call_sites(node, ctx, owner_fqn);
}

/// 取调用点所在**行**的源码文本，供 UI 直接显示"调用语句"，便于人工核验。
///
/// 只取单行：`span` 可能覆盖跨行的长表达式，多行片段对"一眼判断"并无帮助，
/// 只会撑大存储与视图响应。超长则截断（避开 UTF-8 边界），避免异常长的行污染视图。
fn snippet_of(node: Node, src: &str) -> Option<String> {
    const MAX: usize = 160;
    let start = node.start_byte().min(src.len());
    let line_start = src[..start].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = src[line_start..].lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return None;
    }
    if line.len() <= MAX {
        return Some(line.to_string());
    }
    let mut end = MAX;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &line[..end]))
}

fn args_of(node: Node, ctx: &Ctx) -> Vec<FactValue> {
    match node.child_by_field_name("arguments") {
        Some(args) => args
            .named_children(&mut args.walk())
            .filter(|a| a.kind() == "argument")
            .map(|a| a.named_child(0).map(|v| eval_expr(v, ctx.src)).unwrap_or(FactValue::Null))
            .collect(),
        None => Vec::new(),
    }
}

fn qualify(ns: Option<&str>, name: &str) -> String {
    match ns {
        Some(ns) if !ns.is_empty() => format!("{}\\{}", ns, name),
        _ => name.to_string(),
    }
}

fn visibility(node: Node, src: &str) -> String {
    node.named_children(&mut node.walk())
        .find(|c| c.kind() == "visibility_modifier")
        .map(|c| text(c, src))
        .unwrap_or_else(|| "public".to_string())
}

fn has_modifier(node: Node, kind: &str) -> bool {
    node.named_children(&mut node.walk()).any(|c| c.kind() == kind)
}
