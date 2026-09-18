//! Java 解析器 —— **第二语言**，用于验证语言层抽象是否真的可插拔。
//!
//! 只做语法层（P2）该做的事：把 Java 语法树翻译成语言无关的 [`SyntaxFacts`]。
//! 语义层（Spring 的 `@GetMapping` / MyBatis Mapper / JPA 实体）应由 FKB 声明，
//! 与 ThinkPHP / Laravel 走同一套机制。
//!
//! 注册进 `DefaultParserRegistry` 后，流水线无需任何改动即可处理 Java 工程。

use std::cell::RefCell;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact, InheritanceFact,
    Language, NodeKind, Span, SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use tree_sitter::{Node, Parser};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub struct JavaParser {
    language: tree_sitter::Language,
}

impl JavaParser {
    pub fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
        Ok(Self { language })
    }
}

impl LanguageParser for JavaParser {
    fn language(&self) -> Language {
        Language::new(Language::JAVA)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["java"]
    }

    fn parse(&self, _path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // 语言在构造时已校验，这里失败属于编程错误
                p.set_language(&self.language).expect("java language");
                p
            });
            parser.parse(source, None)
        });
        let Some(tree) = tree else {
            return Ok(SyntaxFacts::default());
        };

        let mut out = SyntaxFacts::default();
        let src = source.as_bytes();
        let mut package: Option<String> = None;
        let mut stack: Vec<String> = Vec::new();
        walk(
            tree.root_node(),
            src,
            &mut package,
            &mut out,
            &mut stack,
        );
        Ok(out)
    }

    /// Java 的命名空间分隔符是 `.`（PHP 是 `\`）。
    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// Java 的成员分隔符是 `.`（PHP 是 `::`）。
    fn member_separator(&self) -> &'static str {
        "."
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["pom.xml", "build.gradle", "build.gradle.kts"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["target", "build", ".gradle", ".m2"]
    }
}

fn walk(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &mut Vec<String>,
) {
    match node.kind() {
        "package_declaration" => {
            if let Some(name) = node.named_child(0).and_then(|n| text(n, src)) {
                *package = Some(name);
            }
        }
        "import_declaration" => {
            if let Some(name) = node.named_child(0).and_then(|n| text(n, src)) {
                out.imports.push(ImportFact {
                    alias: None,
                    name,
                    span: span_of(node),
                });
            }
        }
        "class_declaration" | "interface_declaration" | "enum_declaration"
        | "record_declaration" => {
            if let Some(fqn) = declare_type(node, src, package, out, stack) {
                collect_supertypes(node, src, &fqn, out);
                // 类级注解（如 `@RequestMapping("/api")` 作为路径前缀）
                // owner_class 即类本身（类级注解的 owner_fqn 已是类，无需再切）。
                collect_annotations(node, src, &fqn, &fqn, out);
                stack.push(fqn);
                recurse(node, src, package, out, stack);
                stack.pop();
                return;
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(fqn) = declare_member(node, src, out, stack) {
                stack.push(fqn);
                recurse(node, src, package, out, stack);
                stack.pop();
                return;
            }
        }
        "method_invocation" => {
            collect_call(node, src, out, stack);
        }
        "field_declaration" => {
            // 字段级注解（`@Value` / `@Autowired` / `@TableField` …）归属所属类：
            // owner_fqn 用类 FQN（字段属于类而非方法），owner_class 同为该类的 FQN。
            if let Some(class_fqn) = stack.last().cloned() {
                collect_annotations(node, src, &class_fqn, &class_fqn, out);
                collect_field_type(node, src, &class_fqn, out);
            }
        }
        _ => {}
    }
    recurse(node, src, package, out, stack);
}

fn recurse(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &mut Vec<String>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, package, out, stack);
    }
}

/// 登记类 / 接口 / 枚举 / record，返回其 FQN。
fn declare_type(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &[String],
) -> Option<String> {
    let name = text(node.child_by_field_name("name")?, src)?;
    let fqn = match stack.last() {
        // 内部类：`Outer.Inner`
        Some(outer) => format!("{}.{}", outer, name),
        None => match package {
            Some(p) => format!("{}.{}", p, name),
            None => name.clone(),
        },
    };
    let kind = match node.kind() {
        "interface_declaration" => NodeKind(NodeKind::INTERFACE.to_string()),
        "enum_declaration" => NodeKind(NodeKind::ENUM.to_string()),
        _ => NodeKind(NodeKind::CLASS.to_string()),
    };
    out.declarations.push(Declaration {
        kind,
        name,
        fqn: fqn.clone(),
        parent_fqn: stack.last().cloned(),
        span: span_of(node),
        extra: serde_json::Value::Null,
    });
    Some(fqn)
}

/// `extends` / `implements` → 继承事实。
fn collect_supertypes(node: Node, src: &[u8], fqn: &str, out: &mut SyntaxFacts) {
    if let Some(sup) = node.child_by_field_name("superclass") {
        if let Some(base) = sup.named_child(0).and_then(|n| text(n, src)) {
            out.inheritances.push(InheritanceFact {
                child_fqn: fqn.to_string(),
                base_name: base,
                kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
                span: span_of(sup),
            });
        }
    }
    if let Some(ifs) = node.child_by_field_name("interfaces") {
        let mut cursor = ifs.walk();
        for child in ifs.named_children(&mut cursor) {
            let base = match child.kind() {
                "type_list" => {
                    let mut c2 = child.walk();
                    child
                        .named_children(&mut c2)
                        .filter_map(|n| text(n, src))
                        .collect::<Vec<_>>()
                }
                _ => text(child, src).into_iter().collect(),
            };
            for base in base {
                out.inheritances.push(InheritanceFact {
                    child_fqn: fqn.to_string(),
                    base_name: base,
                    kind: EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                    span: span_of(child),
                });
            }
        }
    }
}

/// 字段声明 `Type name;` / `Type a, b;` → 记录每条字段与其声明类型。
///
/// 仅取**裸类型名**（剔除泛型 `<...>` 与数组 `[]`），泛型参数在 P2 按
/// `import` 还原时通常无法定位到工程内类，留待需要时再扩展。
fn collect_field_type(node: Node, src: &[u8], class_fqn: &str, out: &mut SyntaxFacts) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let Some(raw) = text(type_node, src) else {
        return;
    };
    let type_name = raw
        .split(['<', '['])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string();
    if type_name.is_empty() {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        if let Some(name) = opt_text(child.child_by_field_name("name"), src) {
            out.field_types.push(FieldTypeFact {
                class_fqn: class_fqn.to_string(),
                field: name,
                type_name: type_name.clone(),
                span: span_of(child),
            });
        }
    }
}

/// 方法 / 构造器 → 成员声明。返回方法 FQN（供调用者把方法压栈，使方法体内的
/// 调用点 `owner_fqn` 精确到 `类.方法` 而非仅类）。
fn declare_member(node: Node, src: &[u8], out: &mut SyntaxFacts, stack: &[String]) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), src)?;
    let class_fqn = stack.last().cloned()?;
    let fqn = format!("{}.{}", class_fqn, name);
    out.declarations.push(Declaration {
        kind: NodeKind(NodeKind::METHOD.to_string()),
        name: name.clone(),
        // 成员分隔符由 `member_separator()` 决定（Java `.` / PHP `::`）
        fqn: fqn.clone(),
        parent_fqn: Some(class_fqn.clone()),
        span: span_of(node),
        extra: serde_json::Value::Null,
    });
    // 方法级注解（`@GetMapping("/list")` / `@Value("${x}")` …）
    // owner_class 是所属类（target_fqn 即 `类.方法`，故单独传入 class_fqn）。
    collect_annotations(node, src, &fqn, &class_fqn, out);
    Some(fqn)
}

/// 注解 → **调用点**。
///
/// Java 的注解、Python 的装饰器、C# 的 Attribute 是同一个概念：**声明式的框架调用**
/// （注册路由、注入依赖、映射表）。建模成 `CallSiteFact` 后，现有 FKB DSL
/// （`kind: call` + `callee`）无需扩展即可匹配它们。
///
/// `owner_class_fqn` 是注解所在类的 FQN（类级注解 = `类`，方法级注解 = `类`），
/// 显式记录到 `CallSiteFact.owner_class`，避免内核按 `.` 切分时把类级注解误切成包名。
fn collect_annotations(
    node: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    // `modifiers` 在 tree-sitter-java 里**不是具名字段**，需按 kind 遍历子节点。
    let mut cursor = node.walk();
    for mods in node.named_children(&mut cursor) {
        if mods.kind() != "modifiers" {
            continue;
        }
        let mut c2 = mods.walk();
        for child in mods.named_children(&mut c2) {
            collect_one_annotation(child, src, target_fqn, owner_class_fqn, out);
        }
    }
}

fn collect_one_annotation(
    child: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    if !matches!(child.kind(), "annotation" | "marker_annotation") {
        return;
    }
    // 注解名：优先具名字段，取不到退回第一个子节点（`@GetMapping` / `@java.lang.X`）
    let name = opt_text(child.child_by_field_name("name"), src)
        .or_else(|| opt_text(child.named_child(0), src));
    let Some(name) = name else {
        return;
    };
    // 参数列表：`@X("/a")` 或 `@X(value = "/a", method = GET)`
    let args_node = match child.child_by_field_name("arguments") {
        Some(a) => Some(a),
        None => (0..child.named_child_count())
            .filter_map(|i| child.named_child(i as u32))
            .find(|n| n.kind() == "annotation_argument_list"),
    };
    let args = args_node.map(|a| literal_args(a, src)).unwrap_or_default();
    out.call_sites.push(CallSiteFact {
        owner_fqn: target_fqn.to_string(),
        owner_class: Some(owner_class_fqn.to_string()),
        callee_text: name.clone(),
        receiver: None,
        method: Some(name),
        args,
        span: span_of(child),
        snippet: None,
    });
}

/// 注解参数里的字面量（去引号），供 `ValueSource.arg` 取用。
///
/// 递归下钻：`@Table(name = "x")` 的字面量藏在 `element_value_pair` 里。
fn literal_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    collect_literals(node, src, &mut out);
    out
}

fn collect_literals(node: Node, src: &[u8], out: &mut Vec<FactValue>) {
    match node.kind() {
        "string_literal" => {
            if let Some(t) = text(node, src) {
                out.push(FactValue::String(t.trim_matches('"').to_string()));
                return;
            }
        }
        "decimal_integer_literal" => {
            if let Some(t) = text(node, src) {
                out.push(FactValue::String(t));
                return;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_literals(child, src, out);
    }
}

/// `obj.method(args)` → 调用点。
fn collect_call(node: Node, src: &[u8], out: &mut SyntaxFacts, stack: &[String]) {
    let Some(method) = opt_text(node.child_by_field_name("name"), src) else {
        return;
    };
    let receiver = node
        .child_by_field_name("object")
        .and_then(|n| text(n, src));
    let callee_text = match &receiver {
        Some(r) => format!("{}.{}", r, method),
        None => method.clone(),
    };
    // 方法压栈后：`stack.last()` 是方法 FQN，`stack[len-2]` 是所属类；
    // 类级调用（不在方法体内）时两者相同。
    let owner_fqn = stack.last().cloned().unwrap_or_default();
    let owner_class = if stack.len() >= 2 {
        stack.get(stack.len() - 2).cloned()
    } else {
        stack.last().cloned()
    };
    out.call_sites.push(CallSiteFact {
        owner_fqn,
        owner_class,
        callee_text,
        receiver,
        method: Some(method),
        args: vec![FactValue::Unknown(None)],
        span: span_of(node),
        snippet: None,
    });
}

fn text(node: Node, src: &[u8]) -> Option<String> {
    node.utf8_text(src)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn opt_text(node: Option<Node>, src: &[u8]) -> Option<String> {
    node.and_then(|n| text(n, src))
}

fn span_of(node: Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_line: start.row as u32 + 1,
        end_line: end.row as u32 + 1,
        start_byte: node.start_byte() as u32,
        end_byte: node.end_byte() as u32,
    }
}
