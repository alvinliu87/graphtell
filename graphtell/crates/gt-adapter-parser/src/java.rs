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
    Language, NodeKind, Span, SyntaxFacts, TemplateBindingFact,
};
use gt_domain::port::LanguageParser;
use regex::Regex;
use tree_sitter::{Node, Parser};

use crate::ts_util::{bare_type_name, opt_text, span_of, text};

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
        &["java", "jsp", "jspx", "ftl", "vm"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        // JSP / Freemarker templates (`.jsp` / `.jspx` / `.ftl` / `.vm`) are not valid Java; the template layer
        // (tag-library includes, custom-tag / macro components, form-action route refs) is extracted from text.
        if is_java_template(path) {
            return Ok(extract_java_template(source, path));
        }
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

    /// Java field injection (`@Autowired Repository<User> repo`) uses bare identifiers.
    fn bare_field_receivers(&self) -> bool {
        true
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
                // The `superclass` node's text is the whole clause (`extends Base`), so the base type is
                // its first type child — taking `text(node)` yielded `base_name: "extends Base"`, which
                // never matches an FQN downstream (`resolve_impl` compares `base == type_fqn` exactly).
                if let Some(base) = superclass_type_name(child, src) {
                    out.inheritances.push(InheritanceFact {
                        child_fqn: fqn.to_string(),
                        base_name: base.clone(),
                        kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
                        span: span_of(child),
                    });
                    push_generic_entity(&base, &entity, fqn, child, src, out);
                }
            }
            // `super_interfaces` is what tree-sitter-java emits for a **class**'s `implements` clause
            // (`extends_interfaces` is the interface one); without it a class's implements list was
            // silently dropped and Java implementation relations never reached the graph.
            "interfaces" | "extends_interfaces" | "super_interfaces" => {
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

/// The base type of a `superclass` node (`extends Base` / `extends Repo<User, Long>`).
///
/// The clause keyword belongs to the node itself, so the type is the **first type child**, not the node's
/// text: `type_name_of` then reduces `generic_type` to its bare name, so generic arguments stay out of the
/// base name just like they do on the `extends_interfaces` path.
fn superclass_type_name(node: Node, src: &[u8]) -> Option<String> {
    let mut c = node.walk();
    let found = node.named_children(&mut c).find_map(|n| type_name_of(n, src));
    found
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
    let raw_receiver = node.child_by_field_name("object").and_then(|n| text(n, src));
    // `this.repo.save(x)`: the qualified-self spelling of the same call. P7 resolves the receiver's `MapsTo`
    // through the **field type** recorded under the bare field name (`repo`), so `this.` must be stripped —
    // otherwise a Spring service written this way (very common) loses every read / write classification.
    // Same handling as the JS parser (`this.request.get(...)`); the callee keeps the source spelling.
    let receiver = raw_receiver
        .as_deref()
        .map(|r| r.strip_prefix("this.").unwrap_or(r).to_string());
    let callee_text = match &raw_receiver {
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

/// JSP / Freemarker templates (`.jsp` / `.jspx` / `.ftl` / `.vm`) are not valid Java, so tree-sitter-java cannot
/// parse them; the template layer (tag-library includes, custom-tag / macro components, form-action route refs)
/// is extracted from the raw text and surfaced as bindings.
fn is_java_template(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".jsp") || p.ends_with(".jspx") || p.ends_with(".ftl") || p.ends_with(".vm")
}

fn regex_java_include_file() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<%@\s*include\s+file\s*=\s*["']([^"']+)["']"#).unwrap())
}
fn regex_java_include_page() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<jsp:include\b[^>]*\bpage\s*=\s*["']([^"']+)["']"#).unwrap())
}
fn regex_java_ftl_include() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<#include\s+["']([^"']+)["']"#).unwrap())
}
fn regex_java_ftl_include2() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\[#include\s+["']([^"']+)["']"#).unwrap())
}
fn regex_java_tag() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<([a-zA-Z]+:[a-zA-Z][\w]*)\b"#).unwrap())
}
fn regex_java_macro() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<@([a-zA-Z][\w.]*)"#).unwrap())
}
fn regex_form_action() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<form\b[^>]*\baction\s*=\s*["']([^"']+)["']"#).unwrap())
}

/// Extract JSP / Freemarker template bindings from raw text: view includes, component refs (custom tags / macros),
/// and form-action route references.
fn extract_java_template(source: &str, path: &str) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    for re in [
        regex_java_include_file(),
        regex_java_include_page(),
        regex_java_ftl_include(),
        regex_java_ftl_include2(),
    ] {
        for m in re.captures_iter(source) {
            facts.template_bindings.push(TemplateBindingFact {
                kind: "include".to_string(),
                name: m[1].to_string(),
                handler: None,
                owner_fqn: path.to_string(),
                span: Span::default(),
            });
        }
    }
    for re in [regex_java_tag(), regex_java_macro()] {
        for m in re.captures_iter(source) {
            let name = m[1].to_string();
            if name == "jsp:include" {
                continue;
            }
            facts.template_bindings.push(TemplateBindingFact {
                kind: "component_ref".to_string(),
                name,
                handler: None,
                owner_fqn: path.to_string(),
                span: Span::default(),
            });
        }
    }
    for m in regex_form_action().captures_iter(source) {
        facts.template_bindings.push(TemplateBindingFact {
            kind: "route".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: path.to_string(),
            span: Span::default(),
        });
    }
    // Mirror into classical facts so CfAst links them without special-casing the back end (same pattern as the
    // JS front-end parser): `component_ref` / `include` -> an `Import` (a view depends on a component / included
    // partial); `route` -> a `CallSite` (the rendered view reaches a backend endpoint).
    for tb in &facts.template_bindings {
        match tb.kind.as_str() {
            "component_ref" | "include" => {
                facts.imports.push(ImportFact {
                    name: tb.name.clone(),
                    alias: None,
                    span: tb.span.clone(),
                });
            }
            "route" => {
                // A route reference from a template. URL-form refs (`<form action="/api/orders">`,
                // `<a href="/path">`, a template fetch) become an `HttpContract` + `CallsHttp` edge exactly like
                // the front-end `http-contract` rule (the synthetic `template-route` callee is matched by
                // `fkb/universal/common.yaml::template-route-http-contract`); the view then links to the backend
                // endpoint and bridges to the route-declared contract. Named routes carry no URL, so they stay a
                // queryable CallSite instead of fabricating an orphan contract.
                if tb.name.starts_with('/') {
                    facts.call_sites.push(CallSiteFact {
                        owner_fqn: tb.owner_fqn.clone(),
                        owner_class: None,
                        callee_text: "template-route".to_string(),
                        snippet: None,
                        receiver: None,
                        method: None,
                        args: vec![FactValue::Array(vec![
                            ("url".to_string(), FactValue::String(tb.name.clone())),
                            ("method".to_string(), FactValue::String("ANY".to_string())),
                        ])],
                        span: tb.span.clone(),
                        db_table: None,
                        in_loop: false,
                        entity: None,
                    });
                } else {
                    facts.call_sites.push(CallSiteFact {
                        owner_fqn: tb.owner_fqn.clone(),
                        owner_class: None,
                        callee_text: tb.name.clone(),
                        snippet: None,
                        receiver: None,
                        method: None,
                        args: Vec::new(),
                        span: tb.span.clone(),
                        db_table: None,
                        in_loop: false,
                        entity: None,
                    });
                }
            }
            _ => {}
        }
    }
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::Span;

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

    // ---- pure helpers ----

    /// Normalising argument / parameter types: `List<Order>` -> `List`, `Order[]` -> `Order`.
    #[test]
    fn bare_type_name_strips_generics_and_arrays() {
        assert_eq!(bare_type_name("List<Order>".to_string()), "List");
        assert_eq!(bare_type_name("Order[]".to_string()), "Order");
        assert_eq!(bare_type_name("Order".to_string()), "Order");
        assert_eq!(bare_type_name("List<Order>[]".to_string()), "List");
        assert_eq!(bare_type_name("  Map <String, Object> ".to_string()), "Map", "must strip generics and trim");
        assert_eq!(bare_type_name("int".to_string()), "int");
    }

    fn import(name: &str, alias: Option<&str>) -> ImportFact {
        ImportFact { name: name.to_string(), alias: alias.map(|a| a.to_string()), span: Span::default() }
    }

    fn facts_with(imports: Vec<ImportFact>) -> SyntaxFacts {
        SyntaxFacts { imports, ..Default::default() }
    }

    /// P7 classifies read / write verbs by looking up `MapsTo` through the receiver type, and `MapsTo` hangs off
    /// an **FQN** — a short name finds nothing, so `mapper.insert()` would produce no `WritesDb`. The order
    /// (qualified -> import -> same package) is the contract that makes that lookup work.
    #[test]
    fn resolve_java_type_prefers_qualified_then_import_then_package() {
        // ① already qualified: returned as-is, never rewritten.
        assert_eq!(
            resolve_java_type("com.x.User", &facts_with(vec![]), "com.demo.Svc"),
            "com.x.User"
        );
        // ② an import whose last segment matches restores the exact FQN (even across packages).
        assert_eq!(
            resolve_java_type("User", &facts_with(vec![import("com.other.User", None)]), "com.demo.Svc"),
            "com.other.User",
            "no same-package guessing — when an import can restore it exactly, that must be used"
        );
        // ② an alias matches on the alias itself, not on the imported name's last segment.
        assert_eq!(
            resolve_java_type("U", &facts_with(vec![import("com.other.User", Some("U"))]), "com.demo.Svc"),
            "com.other.User"
        );
        // ③ no import: a same-package reference needs none, so complete with the enclosing class's package.
        assert_eq!(
            resolve_java_type("Repo", &facts_with(vec![]), "com.demo.Svc"),
            "com.demo.Repo"
        );
        // No package at all: there is nothing to complete with, keep the bare name.
        assert_eq!(resolve_java_type("Repo", &facts_with(vec![]), "Svc"), "Repo");
    }

    // ---- call-site capture ----

    fn arg_strings(args: &[FactValue]) -> Vec<Option<String>> {
        args.iter()
            .map(|a| match a {
                FactValue::String(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    /// Positional semantics: the i-th argument keeps its slot, a non-literal becomes the `Unknown` placeholder —
    /// so `arg:0` always means "the first argument" and never shifts because an earlier one was a variable.
    /// FKB uses this to materialise `rabbitTemplate.convertAndSend("orders.queue", msg)` as a Queue producer.
    #[test]
    fn positional_args_keeps_positions_and_placeholders() {
        let src = r#"package com.demo;

class Svc {
    private RabbitTemplate rabbitTemplate;

    void publish(Object msg) {
        rabbitTemplate.convertAndSend("orders.queue", msg);
        rabbitTemplate.convertAndSend("a", "b");
    }
}
"#;
        let facts = JavaParser::new()
            .unwrap()
            .parse("src/main/java/com/demo/Svc.java", src)
            .unwrap();
        let calls: Vec<&CallSiteFact> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text.contains("convertAndSend"))
            .collect();
        assert_eq!(calls.len(), 2, "both calls must be captured: {calls:?}");
        assert_eq!(
            arg_strings(&calls[0].args),
            vec![Some("orders.queue".to_string()), None],
            "position 0 is a string literal, position 1 is a non-literal Unknown placeholder"
        );
        assert_eq!(
            arg_strings(&calls[1].args),
            vec![Some("a".to_string()), Some("b".to_string())]
        );
    }

    /// `publishEvent(new X(...))` derives the event type X, so publishers and `@EventListener` subscribers
    /// merge onto one `Event` node.
    #[test]
    fn publish_event_derives_the_event_type_from_the_constructed_object() {
        let src = r#"package com.demo;

class Svc {
    private Publisher publisher;

    void place() {
        publisher.publishEvent(new OrderPlacedEvent(this));
    }
}
"#;
        let facts = JavaParser::new()
            .unwrap()
            .parse("src/main/java/com/demo/Svc.java", src)
            .unwrap();
        let ev = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text.contains("publishEvent"))
            .unwrap_or_else(|| panic!("publishEvent must be captured: {:?}", facts.call_sites));
        assert_eq!(
            ev.entity.as_deref(),
            Some("OrderPlacedEvent"),
            "`publishEvent(new X())` must infer the event type X"
        );
    }

    // ===== Below: the rest of the adapter, which had no coverage at all =====

    /// The pluggable-language surface: the kernel must learn Java's notation from the adapter instead of
    /// assuming PHP's (`\` / `::` / `$this`).
    #[test]
    fn parser_declares_the_java_notation_and_layout() {
        let p = JavaParser::new().unwrap();
        assert_eq!(p.language(), Language::new(Language::JAVA));
        assert_eq!(p.extensions(), &["java", "jsp", "jspx", "ftl", "vm"]);
        assert_eq!(p.namespace_separator(), &['.'], "Java's namespace separator is `.`");
        assert_eq!(p.member_separator(), ".", "Java's member separator is `.` (PHP's is `::`)");
        assert!(p.bare_field_receivers(), "`@Autowired Repo repo` uses a bare identifier");
        assert!(p.manifest_files().contains(&"pom.xml"));
        assert!(p.exclude_dirs().contains(&"target"));
    }

    /// Declarations: every type kind maps to its node kind, an inner type is `Outer.Inner`, and members are
    /// `Class.member` — the FQNs that every later phase looks nodes up by.
    #[test]
    fn declarations_carry_kinds_and_qualified_fqns() {
        let src = r#"package com.demo;

class Outer {
    static class Inner {}
    void run() {}
    Outer() {}
}

interface Iface {}
enum Color { RED }
record Point(int x) {}
"#;
        let facts = JavaParser::new().unwrap().parse("Demo.java", src).unwrap();
        let kinds: Vec<(&str, &str)> = facts
            .declarations
            .iter()
            .map(|d| (d.fqn.as_str(), d.kind.as_str()))
            .collect();

        assert!(kinds.contains(&("com.demo.Outer", "Class")), "{kinds:?}");
        assert!(kinds.contains(&("com.demo.Outer.Inner", "Class")), "inner class FQN: {kinds:?}");
        assert!(kinds.contains(&("com.demo.Outer.run", "Method")), "a method FQN must be Class.method: {kinds:?}");
        assert!(kinds.contains(&("com.demo.Outer.Outer", "Method")), "a constructor is registered as a member too: {kinds:?}");
        assert!(kinds.contains(&("com.demo.Iface", "Interface")), "{kinds:?}");
        assert!(kinds.contains(&("com.demo.Color", "Enum")), "{kinds:?}");
        assert!(kinds.contains(&("com.demo.Point", "Class")), "a record is classified as Class: {kinds:?}");
    }

    /// A Java annotation is a **declarative framework call**: it must become a call site with its literal
    /// arguments, or FKB could never match `@GetMapping("/users")` the same way it matches a PHP route call.
    #[test]
    fn annotations_become_call_sites_with_literal_args() {
        let src = r#"package com.demo;

@RequestMapping("/api")
class Ctrl {
    @GetMapping("/users")
    void list() {}

    @PostMapping(value = "/orders", produces = "json")
    void create() {}
}
"#;
        let facts = JavaParser::new().unwrap().parse("Ctrl.java", src).unwrap();
        let get = |m: &str| facts.call_sites.iter().find(|c| c.method.as_deref() == Some(m));

        let rm = get("RequestMapping").expect("a class-level annotation must become a call site");
        assert_eq!(rm.callee_text, "RequestMapping");
        assert_eq!(arg_strings(&rm.args), vec![Some("/api".to_string())]);
        assert_eq!(rm.owner_fqn, "com.demo.Ctrl", "a class-level annotation lands on the class");
        assert_eq!(rm.owner_class.as_deref(), Some("com.demo.Ctrl"));

        let gm = get("GetMapping").expect("a method-level annotation must become a call site");
        assert_eq!(arg_strings(&gm.args), vec![Some("/users".to_string())]);
        assert_eq!(gm.owner_fqn, "com.demo.Ctrl.list", "a method-level annotation lands on the method FQN");
        assert_eq!(
            gm.owner_class.as_deref(),
            Some("com.demo.Ctrl"),
            "owner_class still records the owning class explicitly (the kernel cannot cut wrong when splitting on `.`)"
        );

        // Literals hidden inside `element_value_pair` must still be found (recursive descent).
        let pm = get("PostMapping").expect("a PostMapping annotation");
        assert_eq!(
            arg_strings(&pm.args),
            vec![Some("/orders".to_string()), Some("json".to_string())]
        );
    }

    /// `@EventListener` derives the event type from the method's **first parameter**, so a subscriber merges
    /// onto the same `Event` node as `publishEvent(new X())`. A method with no parameter yields no type.
    #[test]
    fn event_listener_derives_the_event_type_from_the_first_parameter() {
        let src = r#"package com.demo;

class Listener {
    @EventListener
    void on(OrderPlacedEvent e) {}

    @EventListener
    void onNothing() {}
}
"#;
        let facts = JavaParser::new().unwrap().parse("Listener.java", src).unwrap();
        let seen: Vec<(&str, Option<&str>)> = facts
            .call_sites
            .iter()
            .filter(|c| c.method.as_deref() == Some("EventListener"))
            .map(|c| (c.owner_fqn.as_str(), c.entity.as_deref()))
            .collect();

        assert!(
            seen.contains(&("com.demo.Listener.on", Some("OrderPlacedEvent"))),
            "the first parameter type must be taken: {seen:?}"
        );
        assert!(
            seen.contains(&("com.demo.Listener.onNothing", None)),
            "a method with no parameter must not invent an event type: {seen:?}"
        );
    }

    /// Field declarations feed Spring's `@Autowired` injection: the declared type must be resolved back into
    /// an FQN, because P7 looks `MapsTo` up by the receiver's FQN and a short name finds nothing.
    #[test]
    fn field_declarations_record_resolved_field_types() {
        let src = r#"package com.demo;

import com.other.UserRepository;

class Svc {
    private UserRepository repo;
    private OrderMapper a, b;
    private List<Order> orders;
}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let f = |field: &str| facts.field_types.iter().find(|t| t.field == field);

        assert_eq!(
            f("repo").map(|t| t.type_name.as_str()),
            Some("com.other.UserRepository"),
            "when an import can restore the FQN exactly it wins over same-package guessing: {:?}",
            facts.field_types
        );
        assert_eq!(f("a").map(|t| t.type_name.as_str()), Some("com.demo.OrderMapper"), "same-package completion");
        assert_eq!(f("b").map(|t| t.type_name.as_str()), Some("com.demo.OrderMapper"), "every variable of the same declaration must be registered");
        assert_eq!(
            f("orders").map(|t| t.type_name.as_str()),
            Some("com.demo.List"),
            "the generic shell `<...>` must be stripped"
        );
        assert_eq!(
            facts.field_types.iter().filter(|t| t.class_fqn == "com.demo.Svc").count(),
            4,
            "a field belongs to the class declaring it: {:?}",
            facts.field_types
        );
    }

    /// `extends` / `implements` become inheritance facts, and a generic base (`JpaRepository<User, Long>`)
    /// keeps only its bare name so generic arguments never leak into the base name.
    ///
    /// Regressions pinned here: the base name of a class `extends` must be the bare type (`Base`, not the
    /// whole `extends Base` clause), and a **class**'s `implements` list must be captured at all
    /// (tree-sitter-java emits it as `super_interfaces`, which used to be unmatched).
    #[test]
    fn supertypes_record_extends_and_implements() {
        let src = r#"package com.demo;

class A extends Base implements Iface, Other {
}

class R extends JpaRepository<User, Long> {
}

interface I extends Marker {
}
"#;
        let facts = JavaParser::new().unwrap().parse("A.java", src).unwrap();
        let ext: Vec<&str> = facts
            .inheritances
            .iter()
            .filter(|i| i.kind.as_str() == EdgeKind::EXTENDS)
            .map(|i| i.base_name.as_str())
            .collect();
        let imp: Vec<&str> = facts
            .inheritances
            .iter()
            .filter(|i| i.kind.as_str() == EdgeKind::IMPLEMENTS)
            .map(|i| i.base_name.as_str())
            .collect();

        assert_eq!(ext, vec!["Base", "JpaRepository"], "a generic base class keeps only the bare name: {ext:?}");
        assert_eq!(imp, vec!["Iface", "Other", "Marker"], "every entry of the implements list must be recorded: {imp:?}");
        assert!(
            facts.inheritances.iter().all(|i| matches!(
                i.child_fqn.as_str(),
                "com.demo.A" | "com.demo.R" | "com.demo.I"
            )),
            "an inheritance fact must hang on the subtype FQN: {:?}",
            facts.inheritances
        );

        // The DAO generic entity is captured through the class `extends` path too.
        let generic: Vec<(&str, Option<&str>)> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text.starts_with("generic."))
            .map(|c| (c.callee_text.as_str(), c.entity.as_deref()))
            .collect();
        assert!(
            generic.contains(&("generic.JpaRepository", Some("com.demo.User"))),
            "a class extending a generic base must infer the entity too: {generic:?}"
        );
    }

    /// A call site records the receiver / method split and the enclosing **member** as `owner_fqn`, so a DB
    /// write is attributed to `Class.method` rather than merely to the class.
    #[test]
    fn call_sites_carry_receiver_method_and_owner() {
        let src = r#"package com.demo;

class Svc {
    void run() {
        mapper.insert(user);
        helper();
    }
}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();

        let ins = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("insert"))
            .expect("the insert call must be captured");
        assert_eq!(ins.receiver.as_deref(), Some("mapper"));
        assert_eq!(ins.callee_text, "mapper.insert", "receiver + method form the callee text");
        assert_eq!(ins.owner_fqn, "com.demo.Svc.run", "the call site is attributed to the method FQN");
        assert_eq!(ins.owner_class.as_deref(), Some("com.demo.Svc"));

        // A bare call has no receiver: the callee is just the method name.
        let bare = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("helper"))
            .expect("the helper call must be captured");
        assert_eq!(bare.receiver, None);
        assert_eq!(bare.callee_text, "helper");
    }

    /// Spans are **1-based**: a 0-based line would misreport every evidence location downstream.
    #[test]
    fn spans_are_one_based_and_cover_the_node() {
        let src = "package com.demo;\n\nclass Svc {\n    void run() {\n        mapper.insert();\n    }\n}\n";
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let ins = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("insert"))
            .expect("the insert call must be captured");
        assert_eq!(ins.span.start_line, 5, "it must be 1-based line 5: {:?}", ins.span);
        assert!(ins.span.end_byte > ins.span.start_byte, "the span must cover the node range");
    }

    /// Imports are what turn a short name back into an FQN (`resolve_java_type`), and the whole short-name
    /// index P2 restores decls by — yet the collection itself had no test. Pinned: the **full dotted name**,
    /// never an alias (Java has none), `static` stripped, and a wildcard keeps no `.*` marker.
    #[test]
    fn imports_are_collected_with_their_full_name() {
        let src = r#"package com.demo;

import java.util.List;
import java.util.*;
import static org.junit.Assert.assertEquals;

class Svc {}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let got: Vec<(&str, Option<&str>)> =
            facts.imports.iter().map(|i| (i.name.as_str(), i.alias.as_deref())).collect();

        assert_eq!(got.len(), 3, "every import statement must be collected: {got:?}");
        assert!(got.contains(&("java.util.List", None)), "a plain import keeps its full dotted name: {got:?}");
        assert!(
            got.contains(&("org.junit.Assert.assertEquals", None)),
            "the `static` modifier is not part of the imported name: {got:?}"
        );
        assert!(
            got.iter().all(|(_, alias)| alias.is_none()),
            "Java imports have no alias, and none must be invented: {got:?}"
        );
        assert!(
            got.iter().any(|(n, _)| *n == "java.util"),
            "a wildcard import drops the trailing `.*` rather than carrying it into the index: {got:?}"
        );
    }

    /// An annotation may be written fully qualified (`@java.lang.SuppressWarnings`): the callee must keep the
    /// dotted name so the FKB rule matches on what the source says. A marker annotation has no argument list,
    /// and an empty list must stay empty rather than becoming one bogus `Unknown` argument.
    #[test]
    fn annotation_names_and_argument_lists_keep_their_source_shape() {
        let src = r#"package com.demo;

class Svc {
    @java.lang.SuppressWarnings("unchecked")
    @Transactional
    void run() {}
}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let get = |m: &str| facts.call_sites.iter().find(|c| c.method.as_deref() == Some(m));

        let qualified = get("java.lang.SuppressWarnings")
            .unwrap_or_else(|| panic!("a fully-qualified annotation must be captured: {:?}", facts.call_sites));
        assert_eq!(qualified.callee_text, "java.lang.SuppressWarnings", "the dotted name is kept verbatim");
        assert_eq!(arg_strings(&qualified.args), vec![Some("unchecked".to_string())]);

        let marker = get("Transactional").expect("a marker annotation must become a call site");
        assert!(marker.args.is_empty(), "no argument list ⇒ no arguments at all, got: {:?}", marker.args);
    }

    /// The `positional_args` branches the existing test does not reach: a **numeric** literal is captured by its
    /// source text (`String("1")`, not an `Int`) because FKB matches arguments textually, an empty arg list stays
    /// empty, and a nested call takes exactly one slot — so `arg:0` never shifts.
    #[test]
    fn positional_args_cover_numeric_empty_and_nested_expressions() {
        let src = r#"package com.demo;

class Svc {
    void run() {
        helper(1);
        helper(1, "a");
        helper();
        helper(this.build(2));
    }
}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let calls: Vec<&CallSiteFact> =
            facts.call_sites.iter().filter(|c| c.method.as_deref() == Some("helper")).collect();
        assert_eq!(calls.len(), 4, "all four helper calls must be captured: {calls:?}");

        assert_eq!(
            arg_strings(&calls[0].args),
            vec![Some("1".to_string())],
            "a numeric literal is captured as its source text"
        );
        assert_eq!(arg_strings(&calls[1].args), vec![Some("1".to_string()), Some("a".to_string())]);
        assert!(calls[2].args.is_empty(), "no arguments ⇒ an empty list, got: {:?}", calls[2].args);
        assert_eq!(
            arg_strings(&calls[3].args),
            vec![None],
            "a nested call is one non-literal argument ⇒ one placeholder, positions must not shift"
        );
    }

    /// The `@EventListener` side already pins "no parameter ⇒ no event type"; the publisher side needs the same
    /// honesty: `publishEvent(evt)` (an existing variable, no `new`) must infer **nothing** rather than guess the
    /// variable name as the event type, which would create a phantom `Event` node that merges with nothing.
    #[test]
    fn publish_event_without_a_new_expression_infers_no_entity() {
        let src = r#"package com.demo;

class Svc {
    private Publisher publisher;

    void run(Object evt) {
        publisher.publishEvent(evt);
        publisher.publishEvent(new OrderPlacedEvent("a"));
    }
}
"#;
        let facts = JavaParser::new().unwrap().parse("Svc.java", src).unwrap();
        let calls: Vec<&CallSiteFact> =
            facts.call_sites.iter().filter(|c| c.method.as_deref() == Some("publishEvent")).collect();
        assert_eq!(calls.len(), 2, "both publishEvent calls must be captured: {calls:?}");

        assert_eq!(
            calls[0].entity, None,
            "an already-built variable gives no event type; guessing it would fabricate a phantom Event"
        );
        assert_eq!(
            calls[1].entity.as_deref(),
            Some("OrderPlacedEvent"),
            "`new X(...)` still derives X"
        );
    }

    /// JSP: `<%@ include file %>` / `<jsp:include page>` are view includes, a custom tag (`<c:if>`) is a component
    /// ref, and `<form action>` is a backend route reference. Surfaced as `TemplateBindingFact`s.
    #[test]
    fn jsp_template_yields_include_component_and_route_bindings() {
        let src = r#"
          <%@ include file="header.jsp" %>
          <jsp:include page="footer.jsp" />
          <c:if test="${x}">...</c:if>
          <form action="/api/orders" method="post"></form>
        "#;
        let facts = JavaParser::new()
            .unwrap()
            .parse("WEB-INF/views/list.jsp", src)
            .unwrap();

        let includes: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "include")
            .map(|b| b.name.as_str())
            .collect();
        assert!(
            includes.contains(&"header.jsp"),
            "@include file -> include: {includes:?}"
        );
        assert!(
            includes.contains(&"footer.jsp"),
            "jsp:include page -> include: {includes:?}"
        );

        let comps: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "component_ref")
            .map(|b| b.name.as_str())
            .collect();
        assert!(
            comps.contains(&"c:if"),
            "a JSP custom tag is a component ref: {comps:?}"
        );

        let routes: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "route")
            .map(|b| b.name.as_str())
            .collect();
        assert!(
            routes.contains(&"/api/orders"),
            "form action -> route: {routes:?}"
        );

        // component_ref + include must be mirrored into `imports`, and route into `call_sites`, so CfAst links
        // them into the graph.
        let import_names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert!(
            import_names.contains(&"header.jsp"),
            "@include file -> import header.jsp: {import_names:?}"
        );
        assert!(
            import_names.contains(&"c:if"),
            "custom tag -> import c:if: {import_names:?}"
        );
        assert!(
            facts.call_sites.iter().any(|c| c.callee_text == "template-route"
                && matches!(
                    c.args.first(),
                    Some(FactValue::Array(items))
                        if items.iter().any(|(k, v)| k == "url" && v == &FactValue::String("/api/orders".to_string()))
                           && items.iter().any(|(k, v)| k == "method" && v == &FactValue::String("ANY".to_string()))
                )),
            "form action -> CallsHttp contract /api/orders (url + method:ANY packed into args[0])"
        );
    }

    /// A plain `.java` file must NOT be diverted to the template extractor — tree-sitter still produces declarations.
    #[test]
    fn plain_java_is_not_treated_as_template() {
        let src = "package com.x; public class Foo { void bar() {} }";
        let facts = JavaParser::new().unwrap().parse("Foo.java", src).unwrap();
        assert!(
            facts.template_bindings.is_empty(),
            "plain .java must not yield template bindings: {:?}",
            facts.template_bindings
        );
        assert!(
            facts.declarations.iter().any(|d| d.name == "Foo"),
            "plain .java must still be parsed for declarations"
        );
    }
}
