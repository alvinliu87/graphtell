//! Java parser — one of the language adapters, verifying that the language layer is really pluggable.
//!
//! It only does what the syntax layer (P2) should: translate the Java syntax tree into the language-agnostic
//! [`SyntaxFacts`]. The semantic layer (Spring's `@GetMapping` / MyBatis Mapper / JPA entities) is declared by
//! FKB, through the same mechanism as ThinkPHP / Laravel.
//!
//! Once registered in `DefaultParserRegistry`, the pipeline handles Java projects with no changes at all.

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
                // The language was validated at construction; failing here is a programming error
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
        let mut loop_depth: u32 = 0;
        walk(
            tree.root_node(),
            src,
            &mut package,
            &mut out,
            &mut stack,
            &mut loop_depth,
        );
        Ok(out)
    }

    /// Java's namespace separator is `.` (PHP uses `\`).
    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// Java's member separator is `.` (PHP uses `::`).
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
    loop_depth: &mut u32,
) {
    if is_loop(node) {
        let body_id = node.child_by_field_name("body").map(|b| b.id());
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let in_body = Some(child.id()) == body_id;
            if in_body {
                *loop_depth += 1;
            }
            walk(child, src, package, out, stack, loop_depth);
            if in_body {
                *loop_depth -= 1;
            }
        }
        return;
    }
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
                collect_annotations(node, src, &fqn, &fqn, out);
                stack.push(fqn);
                recurse(node, src, package, out, stack, loop_depth);
                stack.pop();
                return;
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(fqn) = declare_member(node, src, out, stack) {
                stack.push(fqn);
                recurse(node, src, package, out, stack, loop_depth);
                stack.pop();
                return;
            }
        }
        "method_invocation" => {
            collect_call(node, src, out, stack, loop_depth);
        }
        "field_declaration" => {
            if let Some(class_fqn) = stack.last().cloned() {
                collect_annotations(node, src, &class_fqn, &class_fqn, out);
                collect_field_type(node, src, &class_fqn, out);
            }
        }
        _ => {}
    }
    recurse(node, src, package, out, stack, loop_depth);
}

fn recurse(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &mut Vec<String>,
    loop_depth: &mut u32,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, package, out, stack, loop_depth);
    }
}

/// Java loop statements (`for` / enhanced `for` / `while` / `do-while`).
fn is_loop(node: Node) -> bool {
    matches!(
        node.kind(),
        "for_statement" | "enhanced_for_statement" | "while_statement" | "do_statement"
    )
}

/// Register a class / interface / enum / record and return its FQN.
fn declare_type(
    node: Node,
    src: &[u8],
    package: &mut Option<String>,
    out: &mut SyntaxFacts,
    stack: &[String],
) -> Option<String> {
    let name = text(node.child_by_field_name("name")?, src)?;
    let fqn = match stack.last() {
        // Inner class: `Outer.Inner`
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

/// `extends` / `implements` -> inheritance facts.
///
/// Also: the **generic argument** of a JPA Repository / MyBatis Mapper is the entity it operates on
/// (`interface UserRepository extends JpaRepository<User, Long>` -> `User`).
/// This is the only static clue to "which DAO touches which table", and is recorded as a synthetic call site
/// for FKB to judge (which base classes count as DAOs, and how entities map to tables, are both framework knowledge).
fn collect_supertypes(node: Node, src: &[u8], fqn: &str, out: &mut SyntaxFacts) {
    let entity = first_generic_arg(node, src);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "superclass" => {
                if let Some(base) = type_name_of(child, src) {
                    out.inheritances.push(InheritanceFact {
                        child_fqn: fqn.to_string(),
                        base_name: base.clone(),
                        kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
                        span: span_of(child),
                    });
                    push_generic_entity(&base, &entity, fqn, child, src, out);
                }
            }
            "interfaces" | "extends_interfaces" => {
                let mut c2 = child.walk();
                for t in child.named_children(&mut c2) {
                    let bases: Vec<String> = if t.kind() == "type_list" {
                        let mut c3 = t.walk();
                        t.named_children(&mut c3)
                            .filter_map(|n| type_name_of(n, src))
                            .collect()
                    } else {
                        type_name_of(t, src).into_iter().collect()
                    };
                    for base in bases {
                        out.inheritances.push(InheritanceFact {
                            child_fqn: fqn.to_string(),
                            base_name: base.clone(),
                            kind: EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                            span: span_of(t),
                        });
                        push_generic_entity(&base, &entity, fqn, t, src, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Take the **bare name** of a type node: from `generic_type` (`BaseMapper<Order>`) take the inner
/// `type_identifier` (`BaseMapper`), so generic arguments do not leak into the base class name.
fn type_name_of(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "generic_type" {
        let mut c = node.walk();
        return node
            .named_children(&mut c)
            .find(|c| c.kind() == "type_identifier")
            .and_then(|c| text(c, src));
    }
    text(node, src)
}

/// Resolve a **bare type name** from the source back into an FQN.
///
/// Order: ① contains a dot -> as-is; ② an `import` of this file (including aliases) -> the imported FQN;
/// ③ same package (same-package references have no import) -> `{package of the enclosing class}.{name}`.
///
/// Why this must happen in the parser: P7's read / write verb classification looks up `MapsTo` by the receiver
/// type, and `MapsTo` hangs off an **FQN**; a short name finds nothing, so `mapper.insert()` produces no
/// `WritesDb`. Completing by same-package alone would build a wrong FQN when "the service and the mapper are in
/// different packages" (better missing than guessed — but when an import can restore it exactly, it should).
fn resolve_java_type(name: &str, out: &SyntaxFacts, class_fqn: &str) -> String {
    if name.contains('.') {
        return name.to_string();
    }
    for imp in out.imports.iter() {
        if let Some(alias) = &imp.alias {
            if alias == name {
                return imp.name.clone();
            }
        }
        let last = imp.name.rsplit('.').next().unwrap_or(&imp.name);
        if last == name {
            return imp.name.clone();
        }
    }
    match class_fqn.rsplit_once('.') {
        Some((pkg, _)) => format!("{}.{}", pkg, name),
        None => name.to_string(),
    }
}

/// Take the first type identifier of the first `type_arguments` group in the subtree (`<User, Long>` -> `User`).
fn first_generic_arg(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "type_arguments" {
        let mut c = node.walk();
        return node
            .named_children(&mut c)
            .find(|c| c.kind() == "type_identifier")
            .and_then(|c| text(c, src));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(v) = first_generic_arg(child, src) {
            return Some(v);
        }
    }
    None
}

/// Record "a DAO's generic argument = its entity" as a synthetic call site.
///
/// `callee_text` uses the `generic.` prefix (**no colon** — in FKB's callee a single colon is read as
/// `receiver:method`), and `entity` holds the entity's **short name**, resolved back into an FQN by FKB's
/// `resolve: class_const` through the import short-name index.
fn push_generic_entity(
    base: &str,
    entity: &Option<String>,
    fqn: &str,
    node: Node,
    src: &[u8],
    out: &mut SyntaxFacts,
) {
    let Some(entity) = entity else { return };
    let entity = resolve_java_type(entity, out, fqn);
    out.call_sites.push(CallSiteFact {
        owner_fqn: fqn.to_string(),
        owner_class: Some(fqn.to_string()),
        callee_text: format!("generic.{}", base),
        receiver: None,
        // Deliberately left empty: FKB's bare-callee patterns match **by method name**, so leaving it empty avoids misfires
        method: None,
        args: Vec::new(),
        span: span_of(node),
        snippet: None,
        db_table: None,
        in_loop: false,
        entity: Some(entity.clone()),
    });
    let _ = src;
}

/// Field declarations `Type name;` / `Type a, b;` -> record each field with its declared type.
///
/// Only the **bare type name** is taken (generics `<...>` and arrays `[]` stripped); generic parameters usually
/// cannot be resolved to an in-project class when restored by `import` in P2, so that is left for later.
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
    let type_name = resolve_java_type(&type_name, out, class_fqn);
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

/// Methods / constructors -> member declarations. Returns the method FQN (so the caller can push the method on a
/// stack, making the `owner_fqn` of call sites inside the body precise to `class.method` rather than just the class).
fn declare_member(node: Node, src: &[u8], out: &mut SyntaxFacts, stack: &[String]) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), src)?;
    let class_fqn = stack.last().cloned()?;
    let fqn = format!("{}.{}", class_fqn, name);
    out.declarations.push(Declaration {
        kind: NodeKind(NodeKind::METHOD.to_string()),
        name: name.clone(),
        // The member separator comes from `member_separator()` (Java `.` / PHP `::`)
        fqn: fqn.clone(),
        parent_fqn: Some(class_fqn.clone()),
        span: span_of(node),
        extra: serde_json::Value::Null,
    });
    collect_annotations(node, src, &fqn, &class_fqn, out);
    Some(fqn)
}

/// Annotations -> **call sites**.
///
/// A Java annotation, a Python decorator and a C# attribute are the same concept: a **declarative framework
/// call** (registering a route, injecting a dependency, mapping a table). Modelled as `CallSiteFact`, they can be
/// matched by the existing FKB DSL (`kind: call` + `callee`) with no extension.
///
/// `owner_class_fqn` is the FQN of the class the annotation sits on (class-level annotation = `class`,
/// method-level = `class`); it is recorded explicitly in `CallSiteFact.owner_class` so the kernel does not split
/// a class-level annotation into a package name when splitting on `.`.
fn collect_annotations(
    node: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    // In tree-sitter-java `modifiers` is **not a named field**, so children must be walked by kind.
    let mut cursor = node.walk();
    for mods in node.named_children(&mut cursor) {
        if mods.kind() != "modifiers" {
            continue;
        }
        let mut c2 = mods.walk();
        for child in mods.named_children(&mut c2) {
            collect_one_annotation(child, node, src, target_fqn, owner_class_fqn, out);
        }
    }
}

fn collect_one_annotation(
    child: Node,
    parent: Node,
    src: &[u8],
    target_fqn: &str,
    owner_class_fqn: &str,
    out: &mut SyntaxFacts,
) {
    if !matches!(child.kind(), "annotation" | "marker_annotation") {
        return;
    }
    // Annotation name: prefer the named field, fall back to the first child (`@GetMapping` / `@java.lang.X`)
    let name = opt_text(child.child_by_field_name("name"), src)
        .or_else(|| opt_text(child.named_child(0), src));
    let Some(name) = name else {
        return;
    };
    // Argument list: `@X("/a")` or `@X(value = "/a", method = GET)`
    let args_node = match child.child_by_field_name("arguments") {
        Some(a) => Some(a),
        None => (0..child.named_child_count())
            .filter_map(|i| child.named_child(i as u32))
            .find(|n| n.kind() == "annotation_argument_list"),
    };
    let args = args_node.map(|a| literal_args(a, src)).unwrap_or_default();
    let entity = if name == "EventListener" {
        first_param_type(parent, src)
    } else {
        None
    };
    out.call_sites.push(CallSiteFact {
        owner_fqn: target_fqn.to_string(),
        owner_class: Some(owner_class_fqn.to_string()),
        callee_text: name.clone(),
        receiver: None,
        method: Some(name),
        args,
        span: span_of(child),
        snippet: None,
        db_table: None,
        in_loop: false,
        entity,
    });
}

/// Literals in annotation arguments (quotes stripped), for `ValueSource.arg`.
///
/// Recurses downward: the literal of `@Table(name = "x")` is hidden inside `element_value_pair`.
fn literal_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    collect_literals(node, src, &mut out);
    out
}

/// Strip generics / array shells from a type name, keeping only the bare type name.
///
/// `List<Order>` -> `List`; `Order[]` -> `Order`. Used when normalising argument / parameter types.
fn bare_type_name(raw: String) -> String {
    raw.split(['<', '['])
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
}

/// The first parameter type (generics stripped) of a method / constructor, used by `@EventListener` to derive
/// the event type.
///
/// `onOrderPlaced(OrderPlacedEvent e)` -> `OrderPlacedEvent`. Returns `None` for non-method declarations
/// (class-level / field-level annotations).
fn first_param_type(node: Node, src: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "method_declaration" | "constructor_declaration") {
        return None;
    }
    let params = node.child_by_field_name("parameters")?;
    let first = (0..params.named_child_count())
        .filter_map(|i| params.named_child(i as u32))
        .find(|n| n.kind() == "formal_parameter")?;
    let type_node = first.child_by_field_name("type")?;
    text(type_node, src).map(bare_type_name)
}

/// The type name (generics stripped) of the first `new X(...)` expression in the argument list, so that
/// `publishEvent(new X())` can derive the event type X and merge publishers and subscribers onto one `Event` node.
fn constructed_entity_type(args_node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = args_node.walk();
    for child in args_node.named_children(&mut cursor) {
        if child.kind() == "object_creation_expression" {
            if let Some(t) = child.child_by_field_name("type") {
                return text(t, src).map(bare_type_name);
            }
        }
    }
    None
}

/// Capture literals from a method call's **direct arguments** by position.
///
/// Unlike `literal_args` for annotations (recursive, unordered), this preserves positional semantics: the i-th
/// argument yields its value when it is a string / integer literal, otherwise the placeholder `Unknown`. So
/// `arg:0` always means "the first argument" and never shifts because an earlier argument was a variable.
///
/// Example: `rabbitTemplate.convertAndSend("orders.queue", msg)` -> `["orders.queue", Unknown]`, which FKB uses
/// to materialise the producer side as a `PublishesTo` edge on the Queue node.
fn positional_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "string_literal" => {
                out.push(
                    text(child, src)
                        .map(|t| FactValue::String(t.trim_matches('"').to_string()))
                        .unwrap_or(FactValue::Unknown(None)),
                );
            }
            "decimal_integer_literal" => {
                out.push(text(child, src).map(FactValue::String).unwrap_or(FactValue::Unknown(None)));
            }
            _ => out.push(FactValue::Unknown(None)),
        }
    }
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

/// `obj.method(args)` -> a call site.
fn collect_call(
    node: Node,
    src: &[u8],
    out: &mut SyntaxFacts,
    stack: &[String],
    loop_depth: &mut u32,
) {
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
    let owner_fqn = stack.last().cloned().unwrap_or_default();
    let owner_class = if stack.len() >= 2 {
        stack.get(stack.len() - 2).cloned()
    } else {
        stack.last().cloned()
    };
    let args_node = match node.child_by_field_name("arguments") {
        Some(a) => Some(a),
        None => (0..node.named_child_count())
            .filter_map(|i| node.named_child(i as u32))
            .find(|n| n.kind() == "argument_list"),
    };
    let args = args_node.map(|a| positional_args(a, src)).unwrap_or_default();
    let entity = if method == "publishEvent" {
        args_node.and_then(|a| constructed_entity_type(a, src))
    } else {
        None
    };
    out.call_sites.push(CallSiteFact {
        owner_fqn,
        owner_class,
        callee_text,
        receiver,
        method: Some(method),
        args,
        span: span_of(node),
        snippet: None,
        db_table: None,
        // Calls inside a loop body (including nested ones): the N+1 rule uses this to judge "this runs N times".
        in_loop: *loop_depth > 0,
        entity,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Calls inside a loop body must be marked `in_loop` (the only place the N+1 rule can see "this code runs N times").
    /// Consistent with the PHP side: **only the body counts as inside the loop**, the condition / update expressions do not.
    #[test]
    fn marks_calls_in_loop_bodies() {
        let src = r#"package com.demo;

import java.util.List;

class Svc {
    private Repo repo;

    void run(List<Long> ids) {
        // ① classic for: inside the body counts, inside the condition does not
        for (int i = 0; i < ids.size(); i++) {
            repo.findById(ids.get(i));
        }
        // ② enhanced for
        for (Long id : ids) {
            repo.findById(id);
        }
        // ③ while
        while (repo.hasNext()) {
            repo.findById(1L);
        }
        // ④ do-while
        do {
            repo.findById(2L);
        } while (repo.hasNext());
        // ⑤ outside the loop
        repo.findAll();
        // ⑥ nested loop
        for (Long a : ids) {
            for (Long b : ids) {
                repo.findById(a);
            }
        }
    }
}
"#;
        let parser = JavaParser::new().unwrap();
        let facts = parser.parse("src/main/java/com/demo/Svc.java", src).unwrap();
        let in_loop: Vec<&str> = facts
            .call_sites
            .iter()
            .filter(|c| c.in_loop)
            .map(|c| c.callee_text.as_str())
            .collect();
        assert_eq!(in_loop.len(), 6, "expected 6 in-loop calls, got: {in_loop:?}");
        assert_eq!(
            in_loop.iter().filter(|c| c.contains("findById")).count(),
            5,
            "expected 5 findById calls, got: {in_loop:?}"
        );
        assert!(
            in_loop.iter().any(|c| c.contains("ids.get")),
            "calls nested inside an argument also count as in-loop, got: {in_loop:?}"
        );
        // findAll outside the loop is not marked
        assert!(!in_loop.iter().any(|c| c.contains("findAll")));
        // hasNext in the loop condition does not count as inside the loop
        assert!(!in_loop.iter().any(|c| c.contains("hasNext")));
    }

    /// A Repository / Mapper's generic argument is the entity it operates on (the JPA / MyBatis-Plus DAO convention).
    #[test]
    fn captures_dao_generic_entity() {
        let src = r#"package com.demo;

interface UserRepository extends JpaRepository<User, Long> {
}

interface OrderMapper extends BaseMapper<Order> {
}
"#;
        let parser = JavaParser::new().unwrap();
        let facts = parser.parse("src/main/java/com/demo/Repos.java", src).unwrap();
        let generic: Vec<(&str, Option<&str>)> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text.starts_with("generic."))
            .map(|c| (c.callee_text.as_str(), c.entity.as_deref()))
            .collect();
        // A same-package bare name is completed into an FQN using the DAO's package (a short name hits no node on the graph)
        assert!(
            generic.contains(&("generic.JpaRepository", Some("com.demo.User"))),
            "expected to capture JpaRepository<User> -> com.demo.User, got: {generic:?}"
        );
        assert!(
            generic.contains(&("generic.BaseMapper", Some("com.demo.Order"))),
            "expected to capture BaseMapper<Order> -> com.demo.Order, got: {generic:?}"
        );
    }
}
