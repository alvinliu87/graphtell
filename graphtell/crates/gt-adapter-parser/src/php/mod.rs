//! PHP 解析器：把 tree-sitter PHP 语法树翻译成语言无关的 [`SyntaxFacts`]。

pub mod value;

use std::cell::RefCell;
use std::collections::HashSet;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact,
    InheritanceFact, Language, NodeKind, SyntaxFacts,
};
use gt_domain::model::syntax::{HeaderAssignFact, SignCompareFact};
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
        let mut ctx = Ctx {
            src: source,
            facts: &mut facts,
            ns: None,
            class_stack: Vec::new(),
            loop_depth: 0,
        };

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
    /// 当前嵌套在几层 `for` / `foreach` / `while` / `do-while` 的**循环体内**。
    /// 收集调用点时写入 [`CallSiteFact::in_loop`] —— 循环是图里唯一没有建模的
    /// 控制流概念，而 N+1 检测全靠它。用深度而非布尔：嵌套循环体内层退出后，
    /// 外层剩余的语句仍要算「在循环内」。
    loop_depth: u32,
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

/// 沿对象链回溯，取出「目标表名」：`Db::name('goods')->where()->insert()` 里，
/// 对象的对象…是 `Db::name('goods')` —— 这是个 `name('goods')` 调用，其首个实参是
/// 字符串字面量 `goods`。命中即返回它，供 P7 把末端动词落成 `WritesDb` / `ReadsDb`。
///
/// 只认 `name` / `table` 两个表名动词（其它如 `Route::name` 是路由命名，不在此列，
/// 但因我们只在「方法本身是读 / 写动词」时才使用这个值，所以不会误用）。
fn db_table_of(node: Node, ctx: &Ctx) -> Option<String> {
    let object = node.child_by_field_name("object")?;
    match object.kind() {
        "scoped_call_expression" => {
            let m = object.child_by_field_name("name").map(|n| text(n, ctx.src))?;
            if m.eq_ignore_ascii_case("name") || m.eq_ignore_ascii_case("table") {
                let args = args_of(object, ctx);
                if let Some(gt_domain::model::FactValue::String(t)) = args.first() {
                    return Some(t.clone());
                }
            }
            None
        }
        // 继续往链上游回溯（`$q->name('x')->find()` 这类写法）
        "member_call_expression" | "nullsafe_member_call_expression" | "function_call_expression" => {
            db_table_of(object, ctx)
        }
        _ => None,
    }
}

fn find_child_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.named_children(&mut node.walk()).find(|c| c.kind() == kind)
}

/// 取带指定**字段名**的子节点（`for` 的 `body` 在语法里可以出现多次）。
///
/// `child_by_field_name` 只返回第一个，这里用游标遍历取全部。
fn field_children<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return out;
    }
    loop {
        let child = cursor.node();
        if child.is_named() && cursor.field_name() == Some(field) {
            out.push(child);
        }
        if !cursor.goto_next_sibling() {
            return out;
        }
    }
}

/// 当前收集位置是否在循环体内（供 [`CallSiteFact::in_loop`] 使用）。
fn in_loop_of(ctx: &Ctx) -> bool {
    ctx.loop_depth > 0
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
    // `@method` 注解：类用 phpdoc 声明了一批**由 `__call` 转发**的方法
    // （CRMEB `BaseServices` 的 `get` / `getList` / `delete` … 全靠它）。
    // 这些方法没有方法体、没有节点，但在调用点上是真实方法名 —— 不记下来，
    // `$this->services->getList()` 就只能退回"类级命中"，链到服务这一跳就断了。
    let magic_methods = docblock_method_names(node, ctx.src);

    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(kind.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: ctx.ns.clone(),
        span: span_of(node),
        extra: json!({
            "abstract": has_modifier(node, "abstract_modifier"),
            "final": has_modifier(node, "final_modifier"),
            "magic_methods": magic_methods,
        }),
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

/// 从类声明**前面**的 phpdoc 里抽出 `@method <name>(...)` 的方法名。
///
/// 只取名字：返回类型与形参对解析无用（真正要做的是把调用转发给 FKB 声明的委派属性），
/// 且返回类型里可能带 `|` `?` `\` 等符号，贪心匹配容易把 `array|Model|null` 当方法名。
fn docblock_method_names(node: Node, src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    // 只向前看紧邻的几个注释 / 空白节点（`use_trait` 之类不会夹在中间）。
    for _ in 0..4 {
        let Some(n) = cur else { break };
        if n.kind() == "comment" {
            for line in text(n, src).lines() {
                let line = line.trim().trim_start_matches('*').trim_start_matches('/').trim();
                let Some(rest) = line.strip_prefix("@method ") else {
                    continue;
                };
                // `array|Model|null get($id, ...)` → 取 `(` 之前的最后一个标识符
                let before_paren = rest.split('(').next().unwrap_or(rest);
                let name = before_paren
                    .split([' ', '\t'])
                    .last()
                    .unwrap_or("")
                    .trim();
                if !name.is_empty() && !out.iter().any(|x| x == name) {
                    out.push(name.to_string());
                }
            }
        }
        cur = n.prev_sibling();
    }
    out
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
    // 右侧自带类型的赋值：`$this->x = new Y` / `Y::make()` / `app(Y::class)`。
    let mut this_assign_types: Vec<serde_json::Value> = Vec::new();
    // 方法内局部变量：`$x = new Y(...)` → `$x->m()` 可解析。
    let mut local_assign_types: Vec<serde_json::Value> = Vec::new();
    // `return X::class;` → 供给 P7 建「所属类 → X」的声明式联系（如 Dao::setModel() → Model）。
    let mut returns_class: Vec<serde_json::Value> = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        collect_assigns(
            body,
            ctx,
            &mut this_assigns,
            &mut this_assign_types,
            &mut local_assign_types,
        );
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
            "this_assign_types": this_assign_types,
            "local_assign_types": local_assign_types,
            "returns_class": returns_class,
        }),
    });

    if let Some(body) = node.child_by_field_name("body") {
        collect_call_sites(body, ctx, &fqn);
    }
}

/// 收集方法体内的赋值，一次遍历服务三条类型推断来源（避免对每个方法体重复递归）：
///
/// 1. `$this->prop = $var;` —— 构造器注入，类型来自**参数类型提示**（`this_out` 的
///    `{prop, var}`）。ThinkPHP 的控制器/服务常写成
///    `__construct(LoginServices $services) { $this->services = $services; }`。
/// 2. `$this->prop = <可静态确定类型的表达式>` —— `new Y` / `Y::make()` / `app(Y::class)`（
///    `type_out` 的 `{prop, class}`）。
/// 3. `$x = <同上>` —— 方法内局部变量（`local_out` 的 `{var, class}`），供 `$x->m()` 解析。
fn collect_assigns(
    node: Node,
    ctx: &Ctx,
    this_out: &mut Vec<serde_json::Value>,
    type_out: &mut Vec<serde_json::Value>,
    local_out: &mut Vec<serde_json::Value>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "assignment_expression" {
            if let (Some(l), Some(r)) = (
                child.child_by_field_name("left"),
                child.child_by_field_name("right"),
            ) {
                let lt = text(l, ctx.src).trim().to_string();
                if lt.starts_with("$this->") {
                    // ① 参数类型提示（更权威，先记；P2 侧遇到已推断的类型会跳过 ②）
                    let rt = text(r, ctx.src).trim().to_string();
                    if rt.starts_with('$') && !rt.contains("->") && !rt.contains('[') {
                        let prop = lt.trim_start_matches("$this->").to_string();
                        let var = rt.trim_start_matches('$').to_string();
                        if !prop.is_empty() && !var.is_empty() {
                            this_out.push(json!({ "prop": prop, "var": var }));
                        }
                    } else if let Some(cls) = rhs_class_of(r, ctx.src) {
                        let prop = lt.trim_start_matches("$this->").to_string();
                        if !prop.is_empty() {
                            type_out.push(json!({ "prop": prop, "class": cls }));
                        }
                    }
                } else if lt.starts_with('$') && !lt.contains("->") {
                    if let Some(cls) = rhs_class_of(r, ctx.src) {
                        let var = lt.trim_start_matches('$').to_string();
                        if !var.is_empty() {
                            local_out.push(json!({ "var": var, "class": cls }));
                        }
                    }
                }
            }
        }
        collect_assigns(child, ctx, this_out, type_out, local_out);
    }
}

/// 从赋值右侧文本里读出**可静态确定的类名**（`new` / 静态工厂 / 容器取实例）。
///
/// 只接受一眼可见的形状，其它一律 `None` —— 宁可缺边，不可错边。
fn rhs_class_of(node: Node, src: &str) -> Option<String> {
    let raw = text(node, src).trim().to_string();
    match node.kind() {
        // `new Foo(...)` / `new Foo`（`object_creation_expression`）
        "object_creation_expression" => node
            .named_children(&mut node.walk())
            .find(|c| matches!(c.kind(), "name" | "qualified_name"))
            .map(|c| trim_leading(text(c, src))),
        // `Foo::getDetail(...)` / `Foo::getInstance()` —— 返回同类的静态工厂
        "scoped_call_expression" => node
            .child_by_field_name("scope")
            .map(|s| trim_leading(text(s, src)))
            .filter(|s| !s.is_empty() && !s.contains('$') && !s.contains("->")),
        // `app(Foo::class)` / `app()->make(Foo::class, [...])`
        _ => {
            let cls = raw
                .split([',', '(']) // 取 `Foo::class` 所在的片段
                .find(|seg| seg.contains("::class"))?
                .trim();
            let cls = cls
                .split_whitespace()
                .last()
                .unwrap_or(cls)
                .trim_end_matches("::class")
                .trim()
                .trim_start_matches('\\');
            if cls.is_empty() || cls.contains('$') || cls.contains("->") {
                None
            } else {
                Some(cls.to_string())
            }
        }
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
    // 类型化属性（PHP 7.4+）：`private OrderModel $orderInfo;`
    //
    // 这是**静态可确定**的属性类型来源，必须记进 `field_types` → `prop_types`：
    // yoshop 这类工程几乎全靠它注入依赖（`protected UserModel $user;` 而**不写**
    // 构造器参数类型提示），漏掉它，`$this->user->xxx()` 就永远推不出类型，
    // 「路由 → 服务 → 表」的整条调用链在服务这一跳断掉。
    let declared_type = node
        .child_by_field_name("type")
        .map(|t| text(t, ctx.src).trim().trim_start_matches('?').to_string())
        .filter(|t| !t.is_empty());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "property_element" {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .map(|n| text(n, ctx.src).trim_start_matches('$').to_string())
            .unwrap_or_default();
        if let Some(t) = declared_type.as_deref() {
            ctx.facts.field_types.push(FieldTypeFact {
                class_fqn: class_fqn.to_string(),
                field: name.clone(),
                type_name: t.to_string(),
                span: span_of(child),
            });
        }
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
                    let db_table = db_table_of(child, ctx);
                    ctx.facts.call_sites.push(CallSiteFact {
                        owner_fqn: owner_fqn.to_string(),
                        owner_class: None,
                        callee_text: format!("{}->{}", object, method),
                        snippet,
                        receiver: Some(object),
                        method: Some(method),
                        args,
                        span: span_of(child),
                        db_table,
                        in_loop: in_loop_of(ctx),
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
                        db_table: None,
                        span: span_of(child),
                        in_loop: in_loop_of(ctx),
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
                        db_table: None,
                        span: span_of(child),
                        in_loop: in_loop_of(ctx),
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
                        db_table: None,
                        span: span_of(child),
                        in_loop: in_loop_of(ctx),
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            // 循环语句：**只有 body 内的调用算「循环体内」**。
            // 条件 / 初始化表达式（`while ($this->hasNext())`、`for ($i=0; $i<count($x); $i++)`）
            // 不是每条记录都执行一次，标成 `in_loop` 会把「循环前查一次」误报成 N+1。
            // 用深度而不是布尔：嵌套循环退出内层后，外层的后续语句仍在循环内。
            "for_statement" | "foreach_statement" | "while_statement" | "do_statement" => {
                let bodies: HashSet<usize> =
                    field_children(child, "body").iter().map(|n| n.id()).collect();
                let mut cursor = child.walk();
                for inner in child.named_children(&mut cursor) {
                    if bodies.contains(&inner.id()) {
                        ctx.loop_depth += 1;
                        collect_call_sites(inner, ctx, owner_fqn);
                        ctx.loop_depth -= 1;
                    } else {
                        collect_call_sites(inner, ctx, owner_fqn);
                    }
                }
            }
            // CORS 反射源站：`$header['Access-Control-Allow-Origin'] = <请求 Origin>`。
            // 仅捕获左侧是 Allow-Origin 下标键的赋值，反射判定留给 `phase::cors`。
            // 签名值的相等性比较：`$sign == $ipay_signature` /
            // `$this->CreatedSign($params) != $params['sign']`。
            // 只收**松散**比较（== / !=）且至少一侧像签名值 —— 见 SignCompareFact 的文档。
            "binary_expression" => {
                if let Some(fact) = sign_compare_of(child, ctx, owner_fqn) {
                    ctx.facts.sign_compares.push(fact);
                }
                recurse_calls(child, ctx, owner_fqn);
            }
            // CORS 反射源站：`$header['Access-Control-Allow-Origin'] = <请求 Origin>`。
            // 仅捕获左侧是 Allow-Origin 下标键的赋值，反射判定留给 `phase::cors`。
            "assignment_expression" => {
                if let (Some(l), Some(r)) = (
                    child.child_by_field_name("left"),
                    child.child_by_field_name("right"),
                ) {
                    if let Some(key) = cors_header_key(l, ctx.src) {
                        ctx.facts.header_assignments.push(HeaderAssignFact {
                            key,
                            rhs_snippet: text(r, ctx.src),
                            file: String::new(),
                            span: span_of(child),
                        });
                    }
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

/// 若赋值左侧是 `['Access-Control-Allow-Origin']` 这类下标访问，返回归一化头名；否则 `None`。
///
/// 只关心会触发反射型 CORS 的 `Access-Control-Allow-Origin` 头（大小写不敏感、忽略引号）。
fn cors_header_key(node: Node, src: &str) -> Option<String> {
    if node.kind() != "subscript_expression" {
        return None;
    }
    // 取下标键：优先按字段名 `index`，回退到第二个命名子节点（兼容不同 tree-sitter-php 版本）。
    let idx = node
        .child_by_field_name("index")
        .or_else(|| node.named_children(&mut node.walk()).nth(1))?;
    let raw = text(idx, src).trim().to_string();
    let key = raw.trim_matches('\'').trim_matches('"').to_string();
    let lower = key.to_ascii_lowercase();
    if lower == "access-control-allow-origin" {
        Some(lower)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_cors_header_assignment() {
        let src = "<?php
Route::miss(function () {
    $header['Access-Control-Allow-Origin'] = app()->request->header('origin');
    $header['Access-Control-Allow-Credentials'] = 'true';
});";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("route/route.php", src).unwrap();
        // 只捕获 Allow-Origin，不捕获 Allow-Credentials（非反射型头）。
        assert_eq!(facts.header_assignments.len(), 1);
        let ha = &facts.header_assignments[0];
        assert_eq!(ha.key, "access-control-allow-origin");
        let lower = ha.rhs_snippet.to_ascii_lowercase();
        assert!(lower.contains("header(") && lower.contains("origin"));
        // 同行应存在一个读取请求 origin 的调用点（CORS 阶段据此定位标注落点）。
        let origin_call = facts.call_sites.iter().any(|c| {
            c.method.as_deref() == Some("header")
                && c.args
                    .first()
                    .and_then(|a| a.as_str())
                    .map(|s| s.eq_ignore_ascii_case("origin"))
                    .unwrap_or(false)
        });
        assert!(origin_call, "应捕获同行的 ->header('origin') 调用点");
    }

    /// 按调用点原文片段取它的 `in_loop` 标记（同一段源码里动词会重名，只能按行区分）。
    fn in_loop_of<'a>(facts: &'a SyntaxFacts, needle: &str) -> Option<&'a CallSiteFact> {
        facts
            .call_sites
            .iter()
            .find(|c| c.snippet.as_deref().is_some_and(|s| s.contains(needle)))
    }

    #[test]
    fn marks_call_sites_inside_loop_body() {
        let src = "<?php
class S {
    public function run($list) {
        $conf = Db::name('config')->find();
        foreach ($list as $item) {
            $user = Db::name('user')->where('id', $item['id'])->find();
            foreach ($item['tags'] as $tag) {
                Tag::get($tag);
            }
        }
        while ($row = $q->fetch()) {
            $this->dao->save($row);
        }
        return $conf;
    }
}";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("app/S.php", src).unwrap();
        // 循环外：只执行一次
        assert_eq!(in_loop_of(&facts, "'config'").unwrap().in_loop, false);
        // foreach 体内：每条记录一次（这才是 N+1）
        assert_eq!(in_loop_of(&facts, "'user'").unwrap().in_loop, true);
        // 嵌套循环体内
        assert_eq!(in_loop_of(&facts, "Tag::get").unwrap().in_loop, true);
        // 循环**条件**里的调用每次进入前求值，不算循环体内
        assert_eq!(in_loop_of(&facts, "fetch()").unwrap().in_loop, false);
        // while 体内
        assert_eq!(in_loop_of(&facts, "save(").unwrap().in_loop, true);
    }

    #[test]
    fn captures_signature_comparison() {
        let src = "<?php
class P {
    public function respond($params) {
        if ($this->CreatedSign($params) != $params['sign']) {
            return -1;
        }
        if ($sign === $signature) {
            return -1;
        }
        return 0;
    }
}";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("extend/payment/P.php", src).unwrap();
        // 只收松散比较：`===` 不算问题，不该进来。
        assert_eq!(facts.sign_compares.len(), 1);
        let c = &facts.sign_compares[0];
        assert_eq!(c.operator, "!=");
        assert!(c.left.contains("CreatedSign"));
        assert!(c.right.contains("['sign']"));
        assert_eq!(c.owner_fqn, "P::respond");
    }

    /// 电商代码里 `sign` 绝大多数是**签到** —— 这是整条规则最大的噪声源，必须挡在解析期。
    #[test]
    fn ignores_checkin_comparison() {
        let src = "<?php
class S {
    public function run($user) {
        if ($sign_mode == 1) { return 1; }
        if ($signMode == 2) { return 1; }
        if ($sign_last_date != date('Y-m-d')) { return 1; }
        if ($sign_total_days == 3) { return 1; }
        if ($points_sign_enabled != 1) { return 1; }
        if ($assignedCouponMoney == 0) { return 1; }
        return 0;
    }
}";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("app/service/Sign.php", src).unwrap();
        assert!(
            facts.sign_compares.is_empty(),
            "签到类比较不应被收进签名比较事实，实际 {:?}",
            facts.sign_compares.iter().map(|c| &c.left).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ignores_non_cors_assignment() {
        let src = "<?php
$header['Content-Type'] = 'application/json';
$list['X-Foo'] = $request->header('origin');";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("x.php", src).unwrap();
        assert!(facts.header_assignments.is_empty());
    }
}

/// 抽取「签名值的相等性比较」：`$sign == $ipay_signature` /
/// `$this->CreatedSign($params) != $params['sign']`。
///
/// 只收 `==` / `!=`：`===` / `!==` 是严格比较，不是这里要找的问题。
/// 且至少一侧要像"一个签名值" —— 一个工程里有几万条 `==`，全收会撑爆事实集
/// （与 CORS 只收 Allow-Origin 一个头同理）。
fn sign_compare_of(node: Node, ctx: &Ctx, owner_fqn: &str) -> Option<SignCompareFact> {
    let op = node.child_by_field_name("operator").map(|o| text(o, ctx.src))?;
    if op != "==" && op != "!=" {
        return None;
    }
    let left = node.child_by_field_name("left").map(|n| text(n, ctx.src))?;
    let right = node.child_by_field_name("right").map(|n| text(n, ctx.src))?;
    if !looks_like_signature(&left) && !looks_like_signature(&right) {
        return None;
    }
    // 一侧是**字符串字面量**时不是验签：那是在比对算法名之类的常量
    // （实测误报：`$ssl[$i]['signatureTypeLN'] == "sha1WithRSAEncryption"`）。
    // 验签比较的两侧都应当是变量 / 计算表达式。
    if is_string_literal(&left) || is_string_literal(&right) {
        return None;
    }
    Some(SignCompareFact {
        left,
        right,
        operator: op,
        owner_fqn: owner_fqn.to_string(),
        file: String::new(),
        span: span_of(node),
    })
}

/// 文本是否像**一个签名值**（而不是签到 / 赋值之类的同形词）。
///
/// 这是整条规则唯一的噪声闸口：电商代码里 `sign` 绝大多数是**签到**
/// （`$sign_mode` / `$sign_last_date` / `$sign_total_days` / `$points_sign_enabled`），
/// 实测 32 处"含 sign 的 == 比较"里 24 处是签到。不排掉它们，规则就刷屏。
fn looks_like_signature(s: &str) -> bool {
    let t = s.trim();
    // 必须引用变量：`$sign`、`$params['sign']`、`$ipay_signature`，
    // 也包括被函数包起来的 `strtolower($sign)`（CRMEB 的 allinpay 验签就是这么写的）。
    if !t.contains('$') {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if !lower.contains("sign") {
        return false;
    }
    // `assign` / `design` / `resign` 里同样含 `sign` —— 先排除同形词。
    if lower.contains("assign") || lower.contains("design") || lower.contains("resign") {
        return false;
    }
    // 去下划线后比对，一次覆盖 `$sign_mode` 与 `$signMode` 两种写法。
    let flat: String = lower.chars().filter(|c| !matches!(c, '_' | '-')).collect();
    const NOISE: &[&str] = &[
        "signtype", "signmode", "signlast", "signtotal", "signdays", "signnum", "signcount",
        "signdate", "signenabled", "signstatus", "signrule", "signconfig", "signset", "signin",
        "signup", "pointsign", "usersign", "signrecord", "signlog", "signremind", "signpoints",
        // 证书里也有 `signatureTypeLN` / 签名算法名，那是元数据不是待验的签名值
        "signaturetype", "signalg", "signmethod",
    ];
    !NOISE.iter().any(|n| flat.contains(n))
}

/// 是否是裸字符串字面量（`'x'` / `"x"`）。
fn is_string_literal(s: &str) -> bool {
    let t = s.trim();
    (t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"'))
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
