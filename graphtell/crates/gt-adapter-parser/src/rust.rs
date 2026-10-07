//! Rust parser — one of the language adapters, verifying that the language layer is really pluggable.
//!
//! It only does what the syntax layer (P2) should: translate the Rust syntax tree into the
//! language-agnostic [`SyntaxFacts`]. The semantic layer (actix-web's `#[get("/x")]`, Diesel / SQLx
//! schema, tokio tasks, …) is left to FKB, through the same mechanism as ThinkPHP / Spring Boot.
//!
//! Unlike Java / Python, Rust has no class-oriented framework conventions baked into the language: the
//! unit of structure is the **module** (a file / `mod` item), with `struct` / `enum` / `trait` types and
//! `impl` blocks that attach methods. Those map onto the kernel's open `NodeKind`s (`Namespace` /
//! `Class` / `Enum` / `Trait` / `Method` / `Function`) without inventing new kinds.

use std::cell::RefCell;

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, Declaration, EdgeKind, FieldTypeFact, ImportFact, InheritanceFact,
    Language, NodeKind, SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use tree_sitter::{Node, Parser};

use crate::ts_util::{opt_text, span_of};

/// `ts_util::text` returns `Option<String>`; most call sites here want an owned `String` (`format!`,
/// `rsplit`, `unwrap_or` on a callee). This shim keeps the call sites readable.
fn t(node: Node, src: &[u8]) -> String {
    opt_text(Some(node), src).unwrap_or_default()
}

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub struct RustParser {
    language: tree_sitter::Language,
}

impl RustParser {
    pub fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        Ok(Self { language })
    }
}

impl LanguageParser for RustParser {
    fn language(&self) -> Language {
        Language::new(Language::RUST)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                p.set_language(&self.language).expect("rust language");
                p
            });
            parser.parse(source, None)
        });
        let Some(tree) = tree else {
            return Ok(SyntaxFacts::default());
        };

        let mut facts = SyntaxFacts::default();
        let module = module_fqn(path);
        facts.namespace = Some(module.clone());
        facts.declarations.push(decl(
            NodeKind::NAMESPACE,
            &module,
            &module,
            None,
            tree.root_node(),
        ));

        let mut ctx = Ctx {
            src: source.as_bytes(),
            facts: &mut facts,
            // stack of (fqn, is_type_scope) — the top type-scope is the `owner_class`.
            stack: vec![(module.clone(), false)],
        };
        walk(tree.root_node(), &mut ctx);
        Ok(facts)
    }

    /// Rust's namespace separator is `::` (PHP uses `\`, Java / Python use `.`). The kernel splits on a single
    /// char, so we treat `:` as the separator (a `::` then reads as two separators, which is harmless for the
    /// module paths we store with `.`).
    fn namespace_separator(&self) -> &'static [char] {
        &[':']
    }

    /// Rust's member separator is `.` for method calls (`obj.method()`); path-style calls use `::`.
    fn member_separator(&self) -> &'static str {
        "."
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["Cargo.toml", "Cargo.lock"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["target"]
    }
}

/// A walk context: the source bytes, the accumulating facts, and an owner stack.
struct Ctx<'a> {
    src: &'a [u8],
    facts: &'a mut SyntaxFacts,
    stack: Vec<(String, bool)>,
}

impl<'a> Ctx<'a> {
    fn current(&self) -> String {
        self.stack.last().map(|(f, _)| f.clone()).unwrap_or_default()
    }

    /// The nearest enclosing type scope (struct / enum / trait / impl) — used as `owner_class`.
    fn owner_class(&self) -> Option<String> {
        self.stack
            .iter()
            .rev()
            .find(|(_, is_type)| *is_type)
            .map(|(f, _)| f.clone())
    }

    /// Whether the current scope is a type (so a `fn` is a method, not a free function).
    fn in_type(&self) -> bool {
        self.stack.last().map(|(_, is_type)| *is_type).unwrap_or(false)
    }

    fn push(&mut self, fqn: String, is_type: bool) {
        self.stack.push((fqn, is_type));
    }

    fn pop(&mut self) {
        self.stack.pop();
    }
}

fn decl(kind: &'static str, name: &str, fqn: &str, parent: Option<String>, node: Node) -> Declaration {
    Declaration {
        kind: NodeKind(kind.to_string()),
        name: name.to_string(),
        fqn: fqn.to_string(),
        parent_fqn: parent,
        span: span_of(node),
        extra: serde_json::json!({}),
    }
}

/// Declare a type (struct / enum / trait) as a namespace-scoped `Class` / `Enum` / `Trait`, entering it as a
/// type scope so inner `fn`s become methods and the type becomes `owner_class` for nested calls.
fn item_decl(node: Node, ctx: &mut Ctx, kind: &'static str, is_type: bool) {
    if let Some(name) = node.child_by_field_name("name") {
        let name = t(name, ctx.src);
        let fqn = format!("{}.{}", ctx.current(), name);
        ctx.facts
            .declarations
            .push(decl(kind, &name, &fqn, Some(ctx.current()), node));
        ctx.push(fqn, is_type);
        walk_children(node, ctx);
        ctx.pop();
    } else {
        walk_children(node, ctx);
    }
}

fn walk(node: Node, ctx: &mut Ctx) {
    match node.kind() {
        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = t(name, ctx.src);
                let fqn = format!("{}.{}", ctx.current(), name);
                ctx.facts.declarations.push(decl(
                    NodeKind::NAMESPACE,
                    &name,
                    &fqn,
                    Some(ctx.current()),
                    node,
                ));
                ctx.push(fqn, false);
                walk_children(node, ctx);
                ctx.pop();
            } else {
                walk_children(node, ctx);
            }
        }
        "struct_item" => item_decl(node, ctx, NodeKind::CLASS, true),
        "enum_item" => item_decl(node, ctx, NodeKind::ENUM, true),
        "trait_item" => item_decl(node, ctx, NodeKind::TRAIT, true),
        "impl_item" => {
            let type_name = node.child_by_field_name("type").map(|n| t(n, ctx.src));
            let trait_name = node.child_by_field_name("trait").map(|n| t(n, ctx.src));
            if let Some(type_name) = type_name {
                let fqn = format!("{}.{}", ctx.current(), type_name);
                if let Some(tr) = trait_name {
                    ctx.facts.inheritances.push(InheritanceFact {
                        child_fqn: fqn.clone(),
                        base_name: tr,
                        kind: EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                        span: span_of(node),
                    });
                }
                ctx.push(fqn, true);
                walk_children(node, ctx);
                ctx.pop();
            } else {
                walk_children(node, ctx);
            }
        }
        "function_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = t(name, ctx.src);
                let owner = ctx.current();
                let fqn = format!("{}.{}", owner, name);
                let kind = if ctx.in_type() {
                    NodeKind::METHOD
                } else {
                    NodeKind::FUNCTION
                };
                let parent = Some(owner.clone());
                ctx.facts
                    .declarations
                    .push(decl(kind, &name, &fqn, parent, node));
                ctx.push(fqn, false);
                walk_children(node, ctx);
                ctx.pop();
            } else {
                walk_children(node, ctx);
            }
        }
        "enum_variant" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = t(name, ctx.src);
                let fqn = format!("{}.{}", ctx.current(), name);
                ctx.facts.declarations.push(decl(
                    NodeKind::ENUM_CASE,
                    &name,
                    &fqn,
                    Some(ctx.current()),
                    node,
                ));
            }
            walk_children(node, ctx);
        }
        "field_declaration" => {
            // A struct field becomes a field-type fact so P7 can resolve `self.field.method()` chains.
            if let (Some(name), Some(ty)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("type"),
            ) {
                ctx.facts.field_types.push(FieldTypeFact {
                    class_fqn: ctx.current(),
                    field: t(name, ctx.src),
                    type_name: t(ty, ctx.src),
                    span: span_of(node),
                });
            }
            walk_children(node, ctx);
        }
        "use_declaration" => {
            let raw = t(node, ctx.src);
            let cleaned = raw
                .strip_prefix("use")
                .unwrap_or(&raw)
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_string();
            if !cleaned.is_empty() {
                ctx.facts.imports.push(ImportFact {
                    alias: None,
                    name: cleaned,
                    span: span_of(node),
                });
            }
            walk_children(node, ctx);
        }
        "call_expression" => {
            let func = node.child_by_field_name("function");
            let (callee_text, receiver, method) = split_callee(func, ctx.src);
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: ctx.current(),
                owner_class: ctx.owner_class(),
                callee_text,
                receiver,
                method,
                args: Vec::new(),
                span: span_of(node),
                snippet: None,
                db_table: None,
                in_loop: false,
                entity: None,
            });
            walk_children(node, ctx);
        }
        "macro_invocation" => {
            let mac = node
                .child_by_field_name("macro")
                .map(|n| t(n, ctx.src))
                .unwrap_or_default();
            let name = format!("{}!", mac);
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: ctx.current(),
                owner_class: ctx.owner_class(),
                callee_text: name.clone(),
                receiver: None,
                method: Some(name),
                args: Vec::new(),
                span: span_of(node),
                snippet: None,
                db_table: None,
                in_loop: false,
                entity: None,
            });
            walk_children(node, ctx);
        }
        "attribute_item" => {
            // A `#[derive(Debug)]` / `#[get("/x")]` is modelled as a call site on the decorated item, so FKB
            // can match framework attributes (`#[get]`, `#[route]`, `#[tokio::test]`, …) exactly like an
            // annotation / decorator in Java / Python.
            let raw = t(node, ctx.src);
            let callee = raw
                .trim_start_matches('#')
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(['(', ' '])
                .next()
                .unwrap_or("")
                .to_string();
            if !callee.is_empty() {
                ctx.facts.call_sites.push(CallSiteFact {
                    owner_fqn: ctx.current(),
                    owner_class: ctx.owner_class(),
                    callee_text: callee.clone(),
                    receiver: None,
                    method: Some(callee),
                    args: Vec::new(),
                    span: span_of(node),
                    snippet: None,
                    db_table: None,
                    in_loop: false,
                    entity: None,
                });
            }
            walk_children(node, ctx);
        }
        _ => walk_children(node, ctx),
    }
}

/// Recurse into named children only (skips tokens).
fn walk_children(node: Node, ctx: &mut Ctx) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, ctx);
    }
}

/// Extract `(callee_text, receiver, method)` from a call's `function` node.
///
/// * `a.b()`     → `("a.b", Some("a"), Some("b"))`
/// * `a::b::c()` → `("a::b::c", Some("a::b"), Some("c"))`
/// * `foo()`     → `("foo", None, Some("foo"))`
fn split_callee(func: Option<Node>, src: &[u8]) -> (String, Option<String>, Option<String>) {
    let Some(func) = func else {
        return (String::new(), None, None);
    };
    let full = t(func, src);
    match func.kind() {
        "field_expression" => {
            // `receiver.field`
            let receiver = func.child_by_field_name("value").map(|n| t(n, src));
            let method = func.child_by_field_name("field").map(|n| t(n, src));
            (full, receiver, method)
        }
        "scoped_identifier" | "identifier" => {
            // `a::b::c` or bare `foo`
            let method = full.rsplit("::").next().map(|s| s.to_string());
            let receiver = if full.contains("::") {
                Some(
                    full.rsplit_once("::")
                        .map(|(r, _)| r.to_string())
                        .unwrap_or_default(),
                )
            } else {
                None
            };
            (full, receiver, method)
        }
        _ => (full.clone(), None, Some(full)),
    }
}

/// Derive a dotted module FQN from a file path: everything after `src/`, `/` and `-` collapsed to `.`,
/// the `.rs` suffix dropped. `lib.rs` / `main.rs` collapse to the enclosing directory; `mod.rs` to its
/// parent directory.
fn module_fqn(path: &str) -> String {
    let cleaned: String = path.replace(['/', '-'], ".");
    let cleaned = cleaned.strip_suffix(".rs").unwrap_or(&cleaned);
    let segs: Vec<&str> = cleaned.split('.').collect();
    // Find the `src` boundary if present.
    let after_src: Vec<&str> = match segs.iter().position(|&s| s == "src") {
        Some(i) => segs[i + 1..].to_vec(),
        None => segs,
    };
    if after_src.is_empty() {
        return "crate".to_string();
    }
    let last = *after_src.last().unwrap();
    let mut body = after_src[..after_src.len() - 1].to_vec();
    let module = if last == "mod" {
        // `…/foo/mod` → module is `foo`
        body.pop().unwrap_or("crate").to_string()
    } else if last == "lib" || last == "main" {
        // crate root module = the enclosing directory (or `crate` if at the top)
        body.pop().unwrap_or("crate").to_string()
    } else if last.is_empty() {
        "crate".to_string()
    } else {
        last.to_string()
    };
    if module == "crate" && body.is_empty() {
        return "crate".to_string();
    }
    let mut fqn = body.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if !module.is_empty() && module != "crate" {
        fqn.push(module);
    }
    if fqn.is_empty() {
        "crate".to_string()
    } else {
        fqn.join(".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DefaultParserRegistry;
    use gt_domain::port::ParserRegistry;

    const SAMPLE: &str = r#"
mod api {
    use std::collections::HashMap;

    pub struct OrderService {
        repo: OrderRepo,
    }

    impl OrderService {
        pub fn create(&self, o: Order) -> Result<()> {
            self.repo.insert(o);
            info!("created");
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
pub enum Status { Active, Inactive }

pub trait Handler {
    fn handle(&self);
}

#[get("/health")]
pub fn health() -> String { "ok".to_string() }
"#;

    fn parse() -> SyntaxFacts {
        RustParser::new()
            .unwrap()
            .parse("crates/app/src/api/order.rs", SAMPLE)
            .unwrap()
    }

    #[test]
    fn registered_in_default_registry() {
        let reg = DefaultParserRegistry::new();
        assert!(reg.parser_for(&Language::new(Language::RUST)).is_some());
    }

    #[test]
    fn module_and_struct_and_impl_are_declared() {
        let f = parse();
        let kinds: Vec<&str> = f.declarations.iter().map(|d| d.kind.as_str()).collect();
        assert!(kinds.contains(&"Namespace"), "module must be a Namespace: {kinds:?}");
        assert!(kinds.contains(&"Class"), "struct must be a Class: {kinds:?}");
        assert!(kinds.contains(&"Enum"), "enum must be an Enum: {kinds:?}");
        assert!(kinds.contains(&"Trait"), "trait must be a Trait: {kinds:?}");
        assert!(kinds.contains(&"Method"), "impl fn must be a Method: {kinds:?}");
        assert!(kinds.contains(&"Function"), "free fn must be a Function: {kinds:?}");
        assert!(kinds.contains(&"EnumCase"), "enum variant must be an EnumCase: {kinds:?}");
    }

    #[test]
    fn enum_variants_have_the_enum_as_parent() {
        let f = parse();
        let active = f
            .declarations
            .iter()
            .find(|d| d.name == "Active")
            .expect("Active variant must be declared");
        assert_eq!(active.kind.as_str(), "EnumCase");
        assert!(
            active.parent_fqn.as_deref().unwrap_or("").ends_with("Status"),
            "variant's parent must be its enum"
        );
    }

    #[test]
    fn impl_method_carries_owner_class() {
        let f = parse();
        let create = f
            .declarations
            .iter()
            .find(|d| d.name == "create")
            .expect("create method must be declared");
        assert_eq!(create.kind.as_str(), "Method");
        assert!(
            create.parent_fqn.as_deref().unwrap_or("").ends_with("OrderService"),
            "method's parent must be the impl type"
        );
        // the call site inside `create` must resolve its owner_class to OrderService
        let call = f
            .call_sites
            .iter()
            .find(|c| c.callee_text.contains("insert"))
            .expect("repo.insert must be a call site");
        assert_eq!(
            call.owner_class.as_deref(),
            Some(create.parent_fqn.as_deref().unwrap_or("")),
            "the call site's owner_class must be the enclosing impl type"
        );
    }

    #[test]
    fn struct_field_becomes_field_type() {
        let f = parse();
        let ft = f
            .field_types
            .iter()
            .find(|x| x.field == "repo")
            .expect("repo field must be a FieldTypeFact");
        assert_eq!(ft.type_name, "OrderRepo");
    }

    #[test]
    fn use_and_macro_and_attribute_are_captured() {
        let f = parse();
        assert!(
            f.imports.iter().any(|i| i.name.contains("HashMap")),
            "use must be captured"
        );
        assert!(
            f.call_sites.iter().any(|c| c.callee_text == "info!"),
            "macro must be a call site"
        );
        assert!(
            f.call_sites.iter().any(|c| c.callee_text == "get"),
            "the #[get(...)] attribute must be a call site"
        );
    }

    #[test]
    fn dogfood_module_fqn_is_dotted() {
        // A real GraphTell path collapses to a dotted module FQN.
        let fqn = module_fqn("crates/gt-adapter-parser/src/rust.rs");
        assert!(
            fqn.ends_with("rust"),
            "module fqn should end with the file stem: {fqn}"
        );
        assert!(
            !fqn.contains("src"),
            "the `src` boundary must be stripped: {fqn}"
        );
    }
}
