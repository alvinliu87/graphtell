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
    CallSiteFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact, InheritanceFact,
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
            let args = call_positional_args(node, ctx.src);
            // Axum-style routing: `Router::route("/x", get(handler))` — the HTTP verb lives in the second
            // argument (a bare function like `get`). Rewrite the call into `route.<verb>` so the verb is
            // first-class and the FKB can synthesise an `HttpContract` exactly like actix / rocket attributes.
            // Neutral: only triggers on a method literally named `route` whose 2nd argument is a bare call.
            let (callee_text, method) = if method.as_deref() == Some("route") {
                if let Some(FactValue::String(verb)) = args.get(1) {
                    let v = verb.to_ascii_lowercase();
                    (format!("route.{}", v), Some(v))
                } else {
                    (callee_text, method)
                }
            } else {
                (callee_text, method)
            };
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: ctx.current(),
                owner_class: ctx.owner_class(),
                callee_text,
                receiver,
                method,
                args,
                span: span_of(node),
                snippet: None,
                db_table: None,
                in_loop: false,
                entity: None,
            });
            walk_children(node, ctx);
        }
        "macro_invocation" => {
            // Keep only the final segment of the macro path (`sqlx::query!` -> `query!`) so FKB rules can
            // match the bare macro name regardless of its crate prefix.
            let mac = node
                .child_by_field_name("macro")
                .map(|n| t(n, ctx.src))
                .unwrap_or_default();
            let mac = mac.rsplit("::").next().unwrap_or(&mac).to_string();
            let name = format!("{}!", mac);
            let args = collect_string_literals(node, ctx.src);
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: ctx.current(),
                owner_class: ctx.owner_class(),
                callee_text: name.clone(),
                receiver: None,
                method: Some(name),
                args,
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
            // annotation / decorator in Java / Python. String-literal arguments (the route path, the
            // `#[route(.., method = "POST")]` verb, …) are captured as `args` so an FKB `Synthesize` rule
            // can read `path: { arg: 0 }` / `method: { arg: 1 }` exactly like Java / Spring annotations.
            //
            // The callee is decorated with a leading `@` (mirroring the TS decorator convention): a Rust
            // framework attribute `#[get]` would otherwise collide with an ordinary method call `HashMap::get`,
            // whose name also resolves to `get`. The `@`-prefixed callee is matched verbatim by FKB route rules.
            let raw = t(node, ctx.src);
            let bare = raw
                .trim_start_matches('#')
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(['(', ' '])
                .next()
                .unwrap_or("")
                .to_string();
            if !bare.is_empty() {
                let callee = format!("@{}", bare);
                let args = collect_string_literals(node, ctx.src);
                ctx.facts.call_sites.push(CallSiteFact {
                    owner_fqn: ctx.current(),
                    owner_class: ctx.owner_class(),
                    callee_text: callee.clone(),
                    receiver: None,
                    method: Some(callee),
                    args,
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

/// Recursively collect string-literal arguments (quotes stripped) from an attribute / macro body, in
/// source order. Used so `#[get("/health")]` / `#[route("/x", method = "POST")]` / `info!("hi")` expose their
/// literals as `CallSiteFact.args` for FKB `Synthesize` rules.
fn collect_string_literals(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    collect_string_literals_inner(node, src, &mut out);
    out
}

fn collect_string_literals_inner(node: Node, src: &[u8], out: &mut Vec<FactValue>) {
    if node.kind() == "string_literal" {
        let s = t(node, src);
        out.push(FactValue::String(s.trim_matches('"').to_string()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_string_literals_inner(child, src, out);
    }
}

/// Positional arguments of a call: the i-th argument yields `String(literal)` when it is a string
/// literal, `String(fn_name)` when it is a bare function call (`get(handler)` — the Axum verb), otherwise
/// `Unknown`. Mirrors the Java parser's `positional_args` so FKB can read `arg: 0` / `arg: 1`.
fn call_positional_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    if let Some(args) = node.child_by_field_name("arguments") {
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            match child.kind() {
                "string_literal" => {
                    let s = t(child, src);
                    out.push(FactValue::String(s.trim_matches('"').to_string()));
                }
                "call_expression" => {
                    let fn_name = split_callee(child.child_by_field_name("function"), src).2;
                    if let Some(name) = fn_name {
                        out.push(FactValue::String(name));
                    } else {
                        out.push(FactValue::Unknown(None));
                    }
                }
                _ => out.push(FactValue::Unknown(None)),
            }
        }
    }
    out
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
            f.call_sites.iter().any(|c| c.callee_text == "@get"),
            "the #[get(...)] attribute must be a call site (decorated with @)"
        );
    }

    #[test]
    fn route_attribute_captures_path_and_verb_args() {
        let src = r#"
#[get("/health")]
pub fn health() -> String { "ok".into() }

#[route("/orders", method = "POST")]
pub fn create_order() {}

pub fn build_routes() {
    let _app = Router::route("/users", get(list_users));
}
"#;
        let f = RustParser::new()
            .unwrap()
            .parse("crates/app/src/routes.rs", src)
            .unwrap();
        // `#[get("/health")]` -> callee `@get`, path is arg0.
        let get = f
            .call_sites
            .iter()
            .find(|c| c.callee_text == "@get")
            .expect("get attribute must be a call site");
        assert_eq!(
            get.args.first(),
            Some(&FactValue::String("/health".into())),
            "route path must land in arg0"
        );
        // `#[route("/orders", method = "POST")]` -> callee `@route`, path arg0, verb arg1.
        let route = f
            .call_sites
            .iter()
            .find(|c| c.callee_text == "@route")
            .expect("route attribute must be a call site");
        assert_eq!(route.args.first(), Some(&FactValue::String("/orders".into())));
        assert_eq!(route.args.get(1), Some(&FactValue::String("POST".into())));
        // Axum `Router::route("/users", get(list_users))` is rewritten to `route.get`.
        let axum = f
            .call_sites
            .iter()
            .find(|c| c.callee_text == "route.get")
            .expect("axum route must be rewritten to route.get");
        assert_eq!(
            axum.args.first(),
            Some(&FactValue::String("/users".into())),
            "axum route path must land in arg0"
        );

        // An ordinary method call named `get` must NOT be decorated, so it is never mistaken for a route.
        let src2 = r#"pub fn f(m: std::collections::HashMap<String, i32>) { m.get("k"); }"#;
        let f2 = RustParser::new()
            .unwrap()
            .parse("crates/app/src/x.rs", src2)
            .unwrap();
        assert!(
            !f2.call_sites.iter().any(|c| c.callee_text == "@get"),
            "HashMap::get must not be mistaken for a route attribute"
        );
        assert!(
            f2.call_sites
                .iter()
                .any(|c| c.method.as_deref() == Some("get")),
            "HashMap::get stays an ordinary call with method `get`"
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

    /// Dog-feeding smoke test: parse real GraphTell Rust source (this module + the app container) through
    /// the tree-sitter front end. Production Rust uses `async fn`, generics, `impl` blocks, `#[derive]`
    /// attributes and macros — all of which must survive the walk without panicking and must yield a
    /// non-empty, well-formed graph. This is the "Rust code gets built into a graph" half of dog feeding;
    /// recall itself is language-agnostic once the graph exists.
    /// The pluggable-language surface — the one thing every other adapter pins and this one never did: the
    /// kernel must learn Rust's notation from the adapter instead of assuming PHP's (`\` / `::` / `$this`).
    ///
    /// Two of these are deliberately **undeclared** here, so the inherited trait defaults are what applies:
    /// `builtin_types` is empty (unlike PHP, Rust supplies no vocabulary to separate `i32` / `usize` / `String`
    /// from project types) and `bare_field_receivers` is `false` (unlike Java / Python, a receiver is always an
    /// explicit `self.field`, never a bare identifier). Pinned so neither changes silently.
    #[test]
    fn parser_declares_the_rust_notation() {
        let p = RustParser::new().unwrap();
        assert_eq!(p.language(), Language::new(Language::RUST));
        assert_eq!(p.extensions(), &["rs"]);
        assert_eq!(
            p.namespace_separator(),
            &[':'],
            "a single `:`, not `::` — the kernel splits on one char, so `a::b` reads as two separators"
        );
        assert_eq!(p.member_separator(), ".", "a method call is `obj.method()` (path calls use `::`)");
        assert!(
            p.manifest_files().contains(&"Cargo.toml") && p.manifest_files().contains(&"Cargo.lock"),
            "both Cargo files count as manifests: {:?}",
            p.manifest_files()
        );
        assert!(p.exclude_dirs().contains(&"target"));
        assert!(
            p.builtin_types().is_empty(),
            "Rust declares no builtin-type vocabulary, so `i32` / `String` are not separated from project types: {:?}",
            p.builtin_types()
        );
        assert!(!p.bare_field_receivers(), "Rust receivers are never bare identifiers");
    }

    /// The `module_fqn` branch matrix: `src/` is the boundary, and Rust's three special file names collapse to
    /// their **directory** instead of becoming a segment (`mod.rs` → the dir itself, `lib.rs` / `main.rs` → the
    /// crate root). Without this, `api/mod.rs` and `api.rs` would produce two different module FQNs for one
    /// module.
    #[test]
    fn module_fqn_collapses_rust_module_boundaries() {
        assert_eq!(module_fqn("crates/app/src/api/order.rs"), "api.order", "everything after `src/`");
        assert_eq!(module_fqn("crates/app/src/api/mod.rs"), "api", "`mod.rs` is its own directory");
        assert_eq!(module_fqn("crates/app/src/deep/nested/mod.rs"), "deep.nested");
        assert_eq!(module_fqn("crates/app/src/lib.rs"), "crate", "`lib.rs` is the crate root");
        assert_eq!(module_fqn("crates/app/src/main.rs"), "crate", "`main.rs` is the crate root");
        assert_eq!(module_fqn("src/lib.rs"), "crate");
        assert_eq!(module_fqn("foo.rs"), "foo", "with no `src/` boundary the whole path is dotted");
        assert_eq!(module_fqn(""), "crate", "an empty path degrades to the crate root");
    }

    #[test]
    fn dogfood_parses_real_graphtell_rust_source() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let targets = [
            format!("{}/src/rust.rs", manifest),
            format!("{}/../gt-app/src/container.rs", manifest),
        ];
        for path in targets {
            let src = std::fs::read_to_string(&path).expect("read a real rust file");
            let f = RustParser::new().unwrap().parse(&path, &src).unwrap();
            assert!(!f.declarations.is_empty(), "must yield declarations for {path}");
            assert!(!f.imports.is_empty(), "must capture `use` for {path}");
            let kinds: Vec<&str> = f.declarations.iter().map(|d| d.kind.as_str()).collect();
            assert!(
                kinds.iter().any(|k| *k == "Function" || *k == "Method"),
                "must extract fns for {path}: {kinds:?}"
            );
            eprintln!(
                "dogfood {path}: {} declarations, {} call_sites, {} imports, {} field_types",
                f.declarations.len(),
                f.call_sites.len(),
                f.imports.len(),
                f.field_types.len()
            );
        }
    }
}
