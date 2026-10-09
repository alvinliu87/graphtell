//! JS/TS front-end parser (a full tree-sitter call graph).
//!
//! Same **tree-sitter -> [`SyntaxFacts`]** pipeline as PHP / Java: turn function / method / class
//! declarations into `Declaration`s and every call site into a `CallSiteFact`, so the upper pipeline
//! (P2 CfAst / P7 call-chain resolution) builds **first-class semantic nodes** (functions / methods)
//! plus the call chain (`Calls` edges + `CallSite` nodes) for the front end exactly as it does for the back end.
//!
//! The front end is then isomorphic to the back end on the graph:
//! * `Component` (method) ->`Calls`-> `api/agent.js:agentGet` (function) ->`CallsHttp`-> `HttpContract`
//! * the folded view shows semantic nodes (`HttpContract`) by default; syntactic hops such as `agentGet` are
//!   folded into the edge's `via` chain, and clicking the edge shows each call site hop by hop in the drawer —
//!   consistent with the back end's `Method -> CallSite -> HttpContract`.
//!
//! Why the single `tree-sitter-typescript` crate: TS grammar is a superset of JS, so `LANGUAGE_TYPESCRIPT`
//! parses `.js/.ts` directly and `LANGUAGE_TSX` parses `.jsx/.tsx`; `.vue` is handled by extracting the
//! `<script>` block and parsing it as TS (with line numbers aligned).
//!
//! HTTP calls (`uni.request` / `request` / `http.request` / `axios*` / `fetch`) are still recognised, and their
//! `args[0]` is normalised into a `{ url, method }` object, fed to FKB's `frontend-js-common-http-contract` rule to
//! synthesize an `HttpContract` and attach `CallsHttp` (converging on the same `ContractId` node as the back
//! end's `HandledBy`).
//!
//! The object form is not the only shape: the **member form** (`request.get('/v2/index', data, opts)`, where the
//! member name is the method and the URL is the first argument) is also normalised into `{ url, method }` —
//! sample_project's uni-app sub-project wraps its HTTP client, so the real `uni.request` exists in exactly one place with
//! a dynamically concatenated URL; the layer where the location can be determined statically is this outer
//! member-form call. See [`HttpStyle::Member`] and [`is_http_client_recv`].

use std::cell::RefCell;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact,
    InheritanceFact, Language, NodeKind, Span, SyntaxFacts, TemplateBindingFact,
};
use gt_domain::port::LanguageParser;
use serde_json::json;
use tree_sitter::{Language as TsLanguage, Node, Parser};
use tree_sitter_typescript::{LANGUAGE_TSX, LANGUAGE_TYPESCRIPT};

use crate::ts_util::{span_of_str as span_of, text_str as text};

// Pure string / URL / HTTP-expression / Vue-script helpers live in `syntax`, kept out of this file
// so the (mutually-recursive) `walk`/`collect_*` traversal stays the focus here.
mod syntax;
use syntax::*;

/// The JS/TS front-end parser adapter (one implementation serves javascript / typescript / vue / jsx / tsx).
pub struct JsFrontendParser;

impl JsFrontendParser {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

impl LanguageParser for JsFrontendParser {
    fn language(&self) -> Language {
        Language::new(Language::JAVASCRIPT)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["js", "ts", "jsx", "tsx", "mjs", "cjs", "vue"]
    }

    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    fn member_separator(&self) -> &'static str {
        "."
    }

    /// JS/TS field injection (`@InjectRepository(User) repo`) uses bare identifiers.
    fn bare_field_receivers(&self) -> bool {
        true
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["package.json"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &[
            "node_modules",
            "dist",
            "build",
            ".output",
            "unpackage",
            "static",
            ".nuxt",
            ".next",
            ".svelte-kit",
        ]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let lower = path.to_ascii_lowercase();
        let owned_vue: String;
        // `.vue`: extract the `<script>` block and align line numbers to the original file, then parse as TS.
        let (ts_lang, src): (TsLanguage, &str) = if lower.ends_with(".vue") {
            match extract_vue_script(source) {
                Some(s) => {
                    owned_vue = s;
                    (LANGUAGE_TYPESCRIPT.into(), &owned_vue)
                }
                None => return Ok(SyntaxFacts::default()),
            }
        } else if lower.ends_with(".tsx") || lower.ends_with(".jsx") {
            (LANGUAGE_TSX.into(), source)
        } else {
            (LANGUAGE_TYPESCRIPT.into(), source)
        };

        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                p.set_language(&ts_lang).expect("tree-sitter-typescript language");
                p
            });
            parser.parse(src, None)
        });
        let tree = tree.ok_or_else(|| DomainError::Parse {
            file: path.into(),
            reason: "tree-sitter returned None".into(),
        })?;

        let mut facts = SyntaxFacts::default();
        let mut ctx = Ctx {
            src,
            facts: &mut facts,
            path,
            loop_depth: 0,
        };
        let root = tree.root_node();
        let file_owner = ctx.path;
        walk(root, &mut ctx, file_owner, None);
        // Vue SFC: the `<template>` block lives outside the `<script>` syntax tree, so it is parsed
        // separately into template bindings (component refs / event / prop).
        if lower.ends_with(".vue") {
            if let Some(tpl) = extract_vue_template(source) {
                let owner = ctx.path;
                collect_vue_template_bindings(&tpl, &mut ctx, owner);
            }
        }
        // Mirror template bindings into the classical fact kinds so CfAst / P7 link them into the graph
        // without special-casing the front-end:
        // * `event`        -> a `CallSite` (the template invokes the handler; P7 resolves it to the component
        //   method, which may itself call an `HttpContract` — that is the front-end → back-end chain).
        // * `component_ref` -> an `Import` (the by_name index resolves it to the component declaration, modelling
        //   a `RendersComponent` relationship without a new kernel edge kind).
        let mut mirrored_calls: Vec<CallSiteFact> = Vec::new();
        let mut mirrored_imports: Vec<ImportFact> = Vec::new();
        for tb in &facts.template_bindings {
            match tb.kind.as_str() {
                "event" => {
                    if let Some(h) = &tb.handler {
                        mirrored_calls.push(CallSiteFact {
                            owner_fqn: tb.owner_fqn.clone(),
                            owner_class: None,
                            callee_text: h.clone(),
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
                "component_ref" => {
                    mirrored_imports.push(ImportFact {
                        name: tb.name.clone(),
                        alias: None,
                        span: tb.span.clone(),
                    });
                }
                _ => {}
            }
        }
        facts.call_sites.extend(mirrored_calls);
        facts.imports.extend(mirrored_imports);
        Ok(facts)
    }
}

/// Parsing context.
struct Ctx<'a> {
    src: &'a str,
    facts: &'a mut SyntaxFacts,
    path: &'a str,
    /// How many levels of `for` / `while` loop body we are nested in (same semantics as the PHP / Python
    /// parsers): a depth rather than a boolean, so after an inner loop exits the outer one's remaining
    /// statements still count as inside a loop. Used to populate `CallSiteFact.in_loop` for the
    /// "external call inside a loop" judgement (P12), which is language-agnostic.
    loop_depth: u32,
}

/// The shape of an HTTP call (decides how url / method are read out of the handle).
enum HttpStyle {
    /// `uni.request({ url, method })` / `request({...})` / `http.request({...})`。
    Obj,
    /// `axios.get(...)` (method from the member name) / `axios({ url, method })` (object form).
    Axios(Option<String>),
    /// `request.get('/v2/index', data, options)`: **the member name is the HTTP method and the URL is the first argument**.
    ///
    /// uni-app / Vue projects commonly wrap the HTTP client once more (`request.get` -> a single internal
    /// `uni.request({ url: MASTER + '/api/' + path })`), where the real `uni.request` URL is a dynamic
    /// concatenation and cannot be captured; the layer whose location is statically determinable is this outer
    /// member-form call.
    Member(String),
    /// `fetch(url, { method })`。
    Fetch,
    /// A non-HTTP call (an ordinary function / component method call).
    Other,
}

impl HttpStyle {
    fn is_http(&self) -> bool {
        !matches!(self, HttpStyle::Other)
    }
}

/// Walk the syntax tree recursively, producing declarations / imports / call sites by node kind.
///
/// * `owner`: the FQN of the node that currently "owns" calls. Top-level statements belong to the file
///   (`ctx.path`), and after entering a function / method it becomes that function's / method's FQN — this is
///   exactly `CallSiteFact.owner_fqn`, and it lets P2 hang the `CallSite` node under the **function** (rather
///   than the `File`), so `CallsHttp` also originates from a front-end function.
/// * `class`: the FQN of the class currently being entered (used for method ownership).
fn walk(node: Node, ctx: &mut Ctx, owner: &str, class: Option<&str>) {
    // Collect a call / construction wherever it syntactically appears — as a statement, an argument, a
    // variable-initialiser value (`const x = f()`), a `for`-of iterable (`for (… of g())`), or a `while`
    // condition. Previously only calls that were a *named child* of the walked node were picked up, so those
    // boundary positions were silently missed (P12 could not flag them). Handling the node itself here and
    // letting the generic `_ =>` arm recurse into children collects each call exactly once.
    match node.kind() {
        "call_expression" => collect_invocation(node, ctx, owner, false),
        "new_expression" => collect_invocation(node, ctx, owner, true),
        _ => {}
    }
    let mut c = node.walk();
    let mut pending: Vec<Node> = Vec::new();
    for child in node.named_children(&mut c) {
        match child.kind() {
            "decorator" => pending.push(child),
            // React / Preact JSX: `<MyButton onClick={handleClick} />` carries a component reference and event
            // bindings. Collected as `TemplateBindingFact`s (isomorphic to Vue / Angular template bindings).
            "jsx_self_closing_element" | "jsx_opening_element" => {
                collect_jsx_element(child, ctx, owner, class);
            }
            "import_statement" => collect_imports(child, ctx),
            "export_statement" => {
                // `export default { ... }`: only an **object literal** counts as config (a function / identifier does not).
                collect_export_default_object(child, ctx);
                walk(child, ctx, owner, class);
            }
            "expression_statement" => {
                if let Some(ae) = child.named_child(0) {
                    if ae.kind() == "assignment_expression" {
                        collect_export_default_object(ae, ctx);
                    }
                }
                walk(child, ctx, owner, class);
            }
            "class_declaration" => {
                let fqn = collect_class(child, ctx);
                for d in pending.drain(..) {
                    collect_decorator(d, ctx, &fqn);
                }
            }
            "function_declaration" | "generator_function_declaration" => {
                if let Some(fqn) = collect_named_function(child, ctx) {
                    for d in pending.drain(..) {
                        collect_decorator(d, ctx, &fqn);
                    }
                } else {
                    pending.clear();
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                let mut d = child.walk();
                for dec in child.named_children(&mut d) {
                    if dec.kind() == "variable_declarator" {
                        collect_declarator(dec, ctx, owner, class);
                    } else {
                        walk(dec, ctx, owner, class);
                    }
                }
            }
            // `call_expression` / `new_expression` are handled at the top of `walk` (so a call reached as a
            // variable-initialiser value or `for`-of iterable is collected too, not just as a statement child),
            // and the generic `_ =>` arm below recurses into their arguments for nested calls.
            // Loop statements: only calls inside the **body** count as "inside the loop" (same as PHP /
            // Python) — a call in the iterable / condition runs once, not per iteration. Bump `loop_depth`
            // for the body child only, then recurse into every child so init / condition calls are still
            // collected (just not flagged as in-loop).
            "for_statement" | "for_in_statement" | "for_of_statement" | "while_statement"
            | "do_statement" => {
                let body_id = child.child_by_field_name("body").map(|b| b.id());
                let mut cc = child.walk();
                for sub in child.named_children(&mut cc) {
                    if Some(sub.id()) == body_id {
                        ctx.loop_depth += 1;
                        walk(sub, ctx, owner, class);
                        ctx.loop_depth -= 1;
                    } else {
                        walk(sub, ctx, owner, class);
                    }
                }
            }
            _ => walk(child, ctx, owner, class),
        }
    }
}

/// `export default { ... }` / `module.exports = { ... }`: collected as "config entries".
///
/// Front-end config-shaped files (route tables, site config, uni-app's declarative config) all look like this;
/// once collected as `ConfigEntryFact` the `kind: config_entry` selector also works for the front end — fully
/// isomorphic to PHP's `return [...]`. Only **scalar leaves** are collected; functions and nested objects are
/// not expanded into leaves (otherwise every Vue component's `methods` would become config — pure noise).
fn collect_export_default_object(node: Node, ctx: &mut Ctx) {
    let mut obj: Option<Node> = None;
    if node.kind() == "assignment_expression" {
        if let Some(rhs) = node.child_by_field_name("right") {
            if rhs.kind() == "object" {
                obj = Some(rhs);
            }
        }
    } else {
        let mut c = node.walk();
        for child in node.named_children(&mut c) {
            if child.kind() == "object" {
                obj = Some(child);
            }
        }
    }
    let Some(obj) = obj else { return };
    collect_object_entries(obj, ctx, String::new());
}

/// Expand an object literal recursively: only scalar leaves become config entries, with key paths joined by `.`.
fn collect_object_entries(node: Node, ctx: &mut Ctx, prefix: String) {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        let key_text = text(key, ctx.src)
            .trim_matches(|c| c == '\'' || c == '"')
            .to_string();
        if key_text.is_empty() {
            continue;
        }
        let path = if prefix.is_empty() {
            key_text
        } else {
            format!("{prefix}.{key_text}")
        };
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        match value.kind() {
            "object" => collect_object_entries(value, ctx, path),
            "array" => collect_object_entries(value, ctx, path),
            "string" | "number" | "true" | "false" | "template_string" => {
                let fv = js_value(text(value, ctx.src));
                if matches!(fv, FactValue::Unknown(_)) {
                    continue;
                }
                ctx.facts.config_entries.push(ConfigEntryFact {
                    key_path: path,
                    value: fv,
                    span: span_of(child, ctx.src),
                });
            }
            _ => {}
        }
    }
}

/// Take the base class name of `class X extends Y` (the TS grammar puts `extends` inside `class_heritage`).
/// Every supertype of a class / interface as `(base name, edge kind)`.
///
/// `extends` yields `Extends` and `implements` yields `Implements`. Taking only the **first** clause and
/// always tagging it `Extends` lost the rest: `class A extends B implements C` dropped `C`, and a class that
/// only implements (`implements OnModuleInit`, the NestJS norm) was recorded as *extending* it.
fn supertypes_of(class_node: Node, src: &str) -> Vec<(String, EdgeKind)> {
    let mut out = Vec::new();
    let mut c = class_node.walk();
    for child in class_node.named_children(&mut c) {
        if child.kind() != "class_heritage" {
            continue;
        }
        let mut h = child.walk();
        for clause in child.named_children(&mut h) {
            let kind = match clause.kind() {
                "extends_clause" => EdgeKind(EdgeKind::EXTENDS.to_string()),
                "implements_clause" => EdgeKind(EdgeKind::IMPLEMENTS.to_string()),
                _ => continue,
            };
            let mut i = clause.walk();
            for t in clause.named_children(&mut i) {
                push_type_node(t, src, &mut out, &kind);
            }
        }
    }
    out
}

/// Record one type node of an `extends` / `implements` clause.
///
/// The types may sit behind a `type_list` (`implements A, B`), and a generic base (`Base<T>`) keeps only its
/// bare name so generic arguments never leak into the base name.
fn push_type_node(t: Node, src: &str, out: &mut Vec<(String, EdgeKind)>, kind: &EdgeKind) {
    match t.kind() {
        "identifier" | "type_identifier" | "nested_identifier" | "member_expression" => {
            out.push((text(t, src).to_string(), kind.clone()));
        }
        "type_list" => {
            let mut c = t.walk();
            for inner in t.named_children(&mut c) {
                push_type_node(inner, src, out, kind);
            }
        }
        "generic_type" => {
            let mut c = t.walk();
            let found = t
                .named_children(&mut c)
                .find(|n| matches!(n.kind(), "identifier" | "type_identifier" | "nested_identifier"));
            if let Some(inner) = found {
                out.push((text(inner, src).to_string(), kind.clone()));
            }
        }
        _ => {}
    }
}

/// Take the text of a field / parameter type annotation (`svc: UserService` -> `UserService`).
///
/// TypeORM repository injection (`@InjectRepository(UserEntity) private repo: Repository<UserEntity>`) is
/// unwrapped to the **entity** (`UserEntity`), not the repository type, so P7 can resolve `repo.save(x)` to
/// the entity's table via its `MapsTo` edge — exactly like Java's `JpaRepository<User>` -> `User`. Only
/// `Repository` / `MongoRepository` generics are unwrapped; unrelated generics (`Promise<User>`, `Array<User>`)
/// keep their declared text.
fn type_annotation_of(node: Node, src: &str) -> Option<String> {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() == "type_annotation" {
            let mut t = child.walk();
            for inner in child.named_children(&mut t) {
                match inner.kind() {
                    "type_identifier" | "predefined_type" | "nested_type_identifier" => {
                        return Some(text(inner, src).to_string());
                    }
                    "generic_type" => {
                        if let Some(entity) = unwrap_repository_generic(inner, src) {
                            return Some(entity);
                        }
                        return Some(text(inner, src).to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

/// Extract the entity type parameter of a TypeORM repository generic:
/// `Repository<UserEntity>` / `MongoRepository<OrderEntity>` -> `UserEntity`.
fn unwrap_repository_generic(node: Node, src: &str) -> Option<String> {
    let mut c = node.walk();
    let mut base: Option<String> = None;
    for child in node.named_children(&mut c) {
        if child.kind() == "type_identifier" {
            base = Some(text(child, src).to_string());
            break;
        }
    }
    if !matches!(base.as_deref(), Some("Repository") | Some("MongoRepository")) {
        return None;
    }
    let mut c2 = node.walk();
    for child in node.named_children(&mut c2) {
        if child.kind() == "type_arguments" {
            let mut a = child.walk();
            for arg in child.named_children(&mut a) {
                return Some(text(arg, src).to_string());
            }
        }
    }
    None
}

/// Take a field's type annotation, **unwrapping arrays to the element type** (`Comment[]` -> `Comment`).
///
/// Only serves relation decorators (`@OneToMany(…) comments: Comment[]`): one side of the relation is an entity,
/// and the array is merely the carrier for "many rows".
///
/// **Deliberately not merged into [`type_annotation_of`]**: that type is written into `FieldTypeFact` for P7 to
/// resolve `field.method()` instance calls; the real type of an array field is `Array`, and recording the element
/// type would make `comments.push(x)` resolve as `Comment.push(x)`, fabricating a wrong call edge.
fn relation_type_of(node: Node, src: &str) -> Option<String> {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() != "type_annotation" {
            continue;
        }
        let mut t = child.walk();
        for inner in child.named_children(&mut t) {
            match inner.kind() {
                "type_identifier" | "predefined_type" | "nested_type_identifier" | "generic_type" => {
                    return Some(text(inner, src).to_string());
                }
                // `Comment[]` -> take the element type
                "array_type" => {
                    let mut a = inner.walk();
                    for e in inner.named_children(&mut a) {
                        if matches!(
                            e.kind(),
                            "type_identifier" | "nested_type_identifier" | "generic_type"
                        ) {
                            return Some(text(e, src).to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

/// Constructor parameter properties: `constructor(private readonly svc: UserService)` ->
/// `(svc, UserService)`, so P7 can resolve `this.svc.method()`.
fn collect_param_types(ctor: Node, ctx: &mut Ctx, class_fqn: &str) {
    let Some(params) = ctor.child_by_field_name("parameters") else {
        return;
    };
    let mut c = params.walk();
    for p in params.named_children(&mut c) {
        if p.kind() != "required_parameter" && p.kind() != "optional_parameter" {
            continue;
        }
        // Only parameters with an access modifier are "properties injected into this" (ordinary parameters are not).
        let mut pc = p.walk();
        let has_modifier = p
            .named_children(&mut pc)
            .any(|ch| ch.kind() == "accessibility_modifier");
        if !has_modifier {
            continue;
        }
        let name = p
            .child_by_field_name("pattern")
            .map(|n| text(n, ctx.src).to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        if let Some(t) = type_annotation_of(p, ctx.src) {
            ctx.facts.field_types.push(FieldTypeFact {
                class_fqn: class_fqn.to_string(),
                field: name,
                type_name: t.clone(),
                span: span_of(p, ctx.src),
            });
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: class_fqn.to_string(),
                owner_class: Some(class_fqn.to_string()),
                callee_text: "@Inject".to_string(),
                snippet: line_snippet(ctx.src, p.start_byte()),
                receiver: None,
                method: Some("Inject".to_string()),
                args: Vec::new(),
                span: span_of(p, ctx.src),
                db_table: None,
                in_loop: false,
                entity: Some(t),
            });
        }
    }
}

/// Decorators -> **call sites** (same mechanism as Java annotations / Python decorators).
///
/// A decorator like `@Get(':id')` contains a `call_expression`, so the ordinary call-collection logic is reused;
/// a bare decorator (`@UseGuards`) is just an identifier. Parameter decorators (`@Param('id')`) sit inside
/// `parameters` and are not direct children here, so they are **not** collected by mistake.
fn collect_decorator(node: Node, ctx: &mut Ctx, owner: &str) {
    let Some(inner) = node.named_child(0) else { return };
    if inner.kind() == "call_expression" {
        collect_invocation(inner, ctx, owner, false);
        if let Some(last) = ctx.facts.call_sites.last_mut() {
            last.callee_text = format!("@{}", last.callee_text);
        }
        // Angular inline template: `@Component({ template: \`...\` })` carries the HTML template in the
        // decorator source. External `templateUrl` cannot be read (single-file parse), so only inline
        // templates are parsed into bindings.
        let dec_src = text(node, ctx.src);
        if dec_src.starts_with("@Component") {
            if let Some(tpl) = extract_angular_inline_template(dec_src) {
                collect_angular_template_bindings(&tpl, ctx, owner);
            }
        }
        return;
    }
    let t = text(inner, ctx.src).to_string();
    if t.is_empty() {
        return;
    }
    ctx.facts.call_sites.push(CallSiteFact {
        owner_fqn: owner.to_string(),
        owner_class: None,
        callee_text: format!("@{t}"),
        snippet: line_snippet(ctx.src, node.start_byte()),
        receiver: None,
        method: Some(t),
        args: Vec::new(),
        span: span_of(node, ctx.src),
        db_table: None,
        in_loop: false,
        entity: None,
    });
}

/// First identifier in an expression that is **not** `this`, used to recover a JSX / template event-handler
/// name from `{ handleClick }` / `this.handleClick` / `onSubmit(user)` / `() => doX()`. A bare `this` is skipped
/// so `this.handleClick` yields `handleClick`, never `this`.
fn first_handler_identifier(s: &str) -> Option<String> {
    let mut cur_start: Option<usize> = None;
    for (i, b) in s.bytes().enumerate() {
        let is_id = b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
        if is_id {
            if cur_start.is_none() {
                cur_start = Some(i);
            }
        } else if let Some(st) = cur_start {
            let id = &s[st..i];
            if id != "this" {
                return Some(id.to_string());
            }
            cur_start = None;
        }
    }
    if let Some(st) = cur_start {
        let id = &s[st..];
        if id != "this" {
            return Some(id.to_string());
        }
    }
    None
}

/// React / Preact JSX element: `<MyButton onClick={handleClick} />` carries a **component reference**
/// (`MyButton`, PascalCase) and **event bindings** (`onClick={handleClick}` -> handler `handleClick`).
fn collect_jsx_element(node: Node, ctx: &mut Ctx, owner: &str, _class: Option<&str>) {
    if let Some(name_node) = node.child_by_field_name("name") {
        let name = text(name_node, ctx.src);
        // The last dotted segment (`app.foo` -> `foo`); a leading-uppercase name is a component, lowercase is
        // a native HTML tag (skip).
        let last = name.rsplit('.').next().unwrap_or(name);
        if !last.is_empty() && last.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            ctx.facts.template_bindings.push(TemplateBindingFact {
                kind: "component_ref".to_string(),
                name: last.to_string(),
                handler: None,
                owner_fqn: owner.to_string(),
                span: span_of(name_node, ctx.src),
            });
        }
    }
    let mut c = node.walk();
    for attr in node.named_children(&mut c) {
        if attr.kind() != "jsx_attribute" {
            continue;
        }
        // tree-sitter stores the attribute name / value as positional named children (no `name` field).
        // The name node is a `property_identifier` (e.g. `onClick`); we only need its text.
        let Some(name_node) = attr.named_child(0) else { continue };
        let aname = text(name_node, ctx.src);
        let Some(rest) = aname.strip_prefix("on") else { continue };
        // `on` alone, or a lowercase 2nd char (`online`), is not an event handler.
        if rest.chars().next().map(|ch| !ch.is_uppercase()).unwrap_or(true) {
            continue;
        }
        let Some(value) = attr.named_child(1) else { continue };
        // `value` is either a bare `jsx_expression` (`{ handleClick }`) or a `jsx_attribute_value`
        // wrapper whose child is the `jsx_expression` / `string`. A plain string attribute
        // (`onClick="noop"`) is not a handler binding.
        let expr = if value.kind() == "jsx_attribute_value" {
            match value.named_child(0) {
                Some(n) => n,
                None => continue,
            }
        } else if value.kind() == "jsx_expression" {
            value
        } else {
            continue;
        };
        if expr.kind() != "jsx_expression" {
            continue;
        }
        let handler = first_handler_identifier(
            text(expr, ctx.src).trim_matches(|c| c == '{' || c == '}'),
        );
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "event".to_string(),
            // Normalise to the lower-case DOM event name (`onClick` -> `click`) so React / Vue (`@click`) /
            // Angular (`(click)`) bindings share one key.
            name: rest.to_ascii_lowercase(),
            handler,
            owner_fqn: owner.to_string(),
            span: span_of(attr, ctx.src),
        });
    }
}

/// Vue `<template>` (parsed as text, outside the `<script>` syntax tree): component references (`<HelloWorld>`),
/// event bindings (`@click="onX"` / `v-on:click="onX"`), and prop bindings (`:user="u"` / `v-bind:user="u"`).
fn collect_vue_template_bindings(tpl: &str, ctx: &mut Ctx, owner: &str) {
    // Vue built-ins (PascalCase + kebab) are excluded so they are not mistaken for project components.
    const VUE_BUILTINS: &[&str] = &[
        "Transition", "TransitionGroup", "KeepAlive", "Teleport", "Suspense", "RouterView",
        "RouterLink", "Component", "Slot",
    ];
    const VUE_KEBAB_BUILTINS: &[&str] = &[
        "router-view", "router-link", "transition-group", "keep-alive", "teleport", "suspense", "slot",
    ];
    for m in regex_vue_component().captures_iter(tpl) {
        let name = &m[1];
        if VUE_BUILTINS.contains(&name) || VUE_KEBAB_BUILTINS.contains(&name) {
            continue;
        }
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "component_ref".to_string(),
            name: name.to_string(),
            handler: None,
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_vue_event().captures_iter(tpl) {
        let event = m[1].split('.').next().unwrap_or(&m[1]);
        let handler = first_handler_identifier(&m[2]);
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "event".to_string(),
            name: event.to_string(),
            handler,
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_vue_prop().captures_iter(tpl) {
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "prop".to_string(),
            name: m[1].to_string(),
            handler: Some(m[2].to_string()),
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
}

/// Angular inline template (parsed as text): component references (`<app-hello>`), event bindings
/// (`(click)="onX()"`), and prop bindings (`[user]="u"`).
fn collect_angular_template_bindings(tpl: &str, ctx: &mut Ctx, owner: &str) {
    for m in regex_angular_component().captures_iter(tpl) {
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "component_ref".to_string(),
            name: m[1].to_string(),
            handler: None,
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_angular_event().captures_iter(tpl) {
        let handler = first_handler_identifier(&m[2]);
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "event".to_string(),
            name: m[1].to_string(),
            handler,
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
    for m in regex_angular_prop().captures_iter(tpl) {
        ctx.facts.template_bindings.push(TemplateBindingFact {
            kind: "prop".to_string(),
            name: m[1].to_string(),
            handler: Some(m[2].to_string()),
            owner_fqn: owner.to_string(),
            span: Span::default(),
        });
    }
}

/// Attach every decorator among a node's **direct children** to `owner`.
fn collect_decorators_of(node: Node, ctx: &mut Ctx, owner: &str) {
    let mut c = node.walk();
    for child in node.named_children(&mut c) {
        if child.kind() == "decorator" {
            collect_decorator(child, ctx, owner);
        }
    }
}

/// Top-level / in-class named function declarations: `function foo() {}` / `export function foo() {}`.
fn collect_named_function(node: Node, ctx: &mut Ctx) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return None; // Anonymous (e.g. `export default () => {}`): build no node; calls in its body belong to the outer scope.
    }
    let fqn = name.clone();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::FUNCTION.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: None,
        span: span_of(node, ctx.src),
        extra: json!({}),
    });
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx, &fqn, None);
    }
    Some(fqn)
}

/// Assigned functions such as `const foo = () => {}` / `const foo = function() {}`.
fn collect_declarator(dec: Node, ctx: &mut Ctx, owner: &str, class: Option<&str>) {
    let name = dec
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string());
    let value = dec.child_by_field_name("value");
    match (name, value) {
        (Some(name), Some(v))
            if v.kind() == "arrow_function"
                || v.kind() == "function_expression"
                || v.kind() == "generator_function" =>
        {
            let fqn = name.clone();
            ctx.facts.declarations.push(Declaration {
                kind: NodeKind(NodeKind::FUNCTION.to_string()),
                name: name.clone(),
                fqn: fqn.clone(),
                parent_fqn: None,
                span: span_of(dec, ctx.src),
                extra: json!({}),
            });
            if let Some(body) = v.child_by_field_name("body") {
                walk(body, ctx, &fqn, class);
            }
        }
        (_, Some(v)) => walk(v, ctx, owner, class),
        _ => {}
    }
}

/// Class declaration: build a `Class` node and handle each member method / field method. Returns the class FQN.
fn collect_class(node: Node, ctx: &mut Ctx) -> String {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src).to_string())
        .unwrap_or_else(|| "<anonymous>".to_string());
    let cfqn = name.clone();
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::CLASS.to_string()),
        name: name.clone(),
        fqn: cfqn.clone(),
        parent_fqn: None,
        span: span_of(node, ctx.src),
        extra: json!({}),
    });

    for (base, kind) in supertypes_of(node, ctx.src) {
        ctx.facts.inheritances.push(InheritanceFact {
            child_fqn: cfqn.clone(),
            base_name: base,
            kind,
            span: span_of(node, ctx.src),
        });
    }

    collect_decorators_of(node, ctx, &cfqn);

    let Some(body) = node.child_by_field_name("body") else {
        return cfqn;
    };
    let mut c = body.walk();
    let mut pending: Vec<Node> = Vec::new();
    for member in body.named_children(&mut c) {
        match member.kind() {
            "decorator" => pending.push(member),
            "method_definition" | "constructor" => {
                let mname = member
                    .child_by_field_name("name")
                    .map(|n| text(n, ctx.src).to_string())
                    .unwrap_or_else(|| member.kind().to_string());
                if mname == "constructor" {
                    collect_param_types(member, ctx, &cfqn);
                }
                let mfqn = format!("{cfqn}.{mname}");
                ctx.facts.declarations.push(Declaration {
                    kind: NodeKind(NodeKind::METHOD.to_string()),
                    name: mname.clone(),
                    fqn: mfqn.clone(),
                    parent_fqn: Some(cfqn.clone()),
                    span: span_of(member, ctx.src),
                    extra: json!({}),
                });
                for d in pending.drain(..) {
                    collect_decorator(d, ctx, &mfqn);
                }
                if let Some(b) = member.child_by_field_name("body") {
                    walk(b, ctx, &mfqn, Some(&cfqn));
                }
            }
            "public_field_definition" => {
                let fname = member
                    .child_by_field_name("name")
                    .map(|n| text(n, ctx.src).to_string())
                    .unwrap_or_default();
                if !fname.is_empty() {
                    let ffqn = format!("{cfqn}.{fname}");
                    let rel_ty = relation_type_of(member, ctx.src);
                    let start = ctx.facts.call_sites.len();
                    for d in pending.drain(..) {
                        collect_decorator(d, ctx, &ffqn);
                    }
                    collect_decorators_of(member, ctx, &ffqn);
                    if let Some(ty) = rel_ty {
                        for c in ctx.facts.call_sites[start..].iter_mut() {
                            if c.entity.is_none() {
                                c.entity = Some(ty.clone());
                            }
                        }
                    }
                } else {
                    pending.clear();
                }
                // Field type annotation: `private svc: UserService;` (TS / Vue class components)
                if !fname.is_empty() {
                    if let Some(t) = type_annotation_of(member, ctx.src) {
                        ctx.facts.field_types.push(FieldTypeFact {
                            class_fqn: cfqn.clone(),
                            field: fname.clone(),
                            type_name: t,
                            span: span_of(member, ctx.src),
                        });
                    }
                }
                let value = member.child_by_field_name("value");
                if let Some(v) = value {
                    if v.kind() == "arrow_function"
                        || v.kind() == "function_expression"
                        || v.kind() == "generator_function"
                    {
                        let mfqn = format!("{cfqn}.{fname}");
                        ctx.facts.declarations.push(Declaration {
                            kind: NodeKind(NodeKind::METHOD.to_string()),
                            name: fname.clone(),
                            fqn: mfqn.clone(),
                            parent_fqn: Some(cfqn.clone()),
                            span: span_of(member, ctx.src),
                            extra: json!({}),
                        });
                        if let Some(b) = v.child_by_field_name("body") {
                            walk(b, ctx, &mfqn, Some(&cfqn));
                        }
                        continue;
                    }
                }
                walk(member, ctx, ctx.path, Some(&cfqn));
            }
            _ => {}
        }
    }
    cfqn
}

/// Import statements: `import axios from 'axios'` / `import { agentGet } from './api'`.
fn collect_imports(node: Node, ctx: &mut Ctx) {
    // tree-sitter-typescript exposes the clause as a plain **child** (`import_clause`), not under a field
    // name: `child_by_field_name("clause")` is always `None`, which silently dropped **every** import — and
    // with it the short-name index that resolves a bare `UserService` / `new UserService()` back to an FQN.
    let mut outer = node.walk();
    let found = node.named_children(&mut outer).find(|n| n.kind() == "import_clause");
    let Some(clause) = found else {
        return;
    };
    let mut c = clause.walk();
    for imp in clause.named_children(&mut c) {
        match imp.kind() {
            // Default import: `import axios from 'axios'`
            "identifier" => {
                ctx.facts.imports.push(ImportFact {
                    alias: None,
                    name: text(imp, ctx.src).to_string(),
                    span: span_of(imp, ctx.src),
                });
            }
            // Named imports: `import { a, b as c } from '...'`
            "named_imports" => {
                let mut g = imp.walk();
                for spec in imp.named_children(&mut g) {
                    if spec.kind() == "import_specifier" {
                        let name = spec
                            .child_by_field_name("name")
                            .map(|n| text(n, ctx.src).to_string())
                            .unwrap_or_default();
                        let alias = spec
                            .child_by_field_name("alias")
                            .map(|a| text(a, ctx.src).to_string());
                        ctx.facts.imports.push(ImportFact {
                            alias,
                            name,
                            span: span_of(spec, ctx.src),
                        });
                    }
                }
            }
            // Namespace import: `import * as ns from '...'`
            "namespace_import" => {
                if let Some(n) = imp.named_child(0) {
                    ctx.facts.imports.push(ImportFact {
                        alias: None,
                        name: text(n, ctx.src).to_string(),
                        span: span_of(imp, ctx.src),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Collect one call (or `new`) call site.
fn collect_invocation(call: Node, ctx: &mut Ctx, owner: &str, is_new: bool) {
    let func = call.child_by_field_name("function");
    let ctor = if is_new {
        call.child_by_field_name("constructor")
    } else {
        func
    };

    let (callee_text, receiver, method_field, style) = match ctor {
        Some(f) if f.kind() == "member_expression" => {
            let obj = f
                .child_by_field_name("object")
                .map(|o| text(o, ctx.src).to_string());
            let prop = f
                .child_by_field_name("property")
                .map(|p| text(p, ctx.src).to_string())
                .unwrap_or_default();
            match obj.as_deref() {
                Some("axios") => {
                    let verb = if is_http_verb(&prop) {
                        Some(prop.to_ascii_uppercase())
                    } else {
                        None
                    };
                    (
                        "axios".to_string(),
                        Some("axios".to_string()),
                        verb.clone(),
                        HttpStyle::Axios(verb.clone()),
                    )
                }
                Some(o) if format!("{o}.{prop}") == "uni.request"
                    || format!("{o}.{prop}") == "http.request" =>
                {
                    (
                        format!("{o}.{prop}"),
                        Some(o.to_string()),
                        Some(prop.clone()),
                        HttpStyle::Obj,
                    )
                }
                Some(o) if is_http_verb(&prop) && is_http_client_recv(o) => (
                    format!("{o}.{prop}"),
                    Some(o.to_string()),
                    Some(prop.clone()),
                    HttpStyle::Member(prop.to_ascii_uppercase()),
                ),
                _ => {
                    let raw = obj.as_deref().unwrap_or("");
                    // NestJS / TypeORM DI: `this.repo.save(x)` — strip the `this.` so the kernel's
                    // variable-type resolver sees the bare field name (`repo`) it recorded via `prop_type`.
                    let recv = raw.strip_prefix("this.").unwrap_or(raw);
                    (
                        format!("{raw}.{prop}"),
                        Some(recv.to_string()),
                        Some(prop),
                        HttpStyle::Other,
                    )
                }
            }
        }
        Some(f) => {
            let t = text(f, ctx.src).to_string();
            if t == "fetch" {
                ("fetch".to_string(), None, Some("fetch".to_string()), HttpStyle::Fetch)
            } else if t == "request" {
                ("request".to_string(), None, Some("request".to_string()), HttpStyle::Obj)
            } else if t == "axios" {
                // `axios({ url, method })` object form: the method comes from an argument, there is no member name.
                ("axios".to_string(), Some("axios".to_string()), None, HttpStyle::Axios(None))
            } else if is_new {
                (format!("new {t}"), None, Some(t), HttpStyle::Other)
            } else {
                (t.clone(), None, Some(t), HttpStyle::Other)
            }
        }
        None => return,
    };

    let args_node = call.child_by_field_name("arguments");
    let args = if style.is_http() {
        let args_text = args_node.map(|a| text(a, ctx.src)).unwrap_or("");
        let (url, method) = extract_http_args(&style, args_text);
        let url_fv = match url {
            Some(u) => FactValue::String(u),
            None => FactValue::Unknown(Some("<dynamic-url>".to_string())),
        };
        vec![FactValue::Array(vec![
            ("url".to_string(), url_fv),
            ("method".to_string(), FactValue::String(method)),
        ])]
    } else {
        args_node
            .map(|a| literal_args(text(a, ctx.src)))
            .unwrap_or_default()
    };

    ctx.facts.call_sites.push(CallSiteFact {
        owner_fqn: owner.to_string(),
        owner_class: None,
        callee_text,
        snippet: line_snippet(ctx.src, call.start_byte()),
        receiver,
        method: method_field,
        args,
        span: span_of(call, ctx.src),
        db_table: None,
        in_loop: ctx.loop_depth > 0,
        entity: None,
    });
}

/// Derive (url, method) from the argument substring according to the HTTP shape.
///
/// The URL is not limited to a pure literal: the **concatenated** form (`'v2/invoice/detail/' + id`) and the
/// **template string** form (`` `v2/order/invoice_detail/${id}` ``) also have a statically determinable shape —
/// non-literal segments fold into `:param` placeholders, and together with FKB's `param_wildcard` normalisation
/// they carry the same shape as a backend route's `/:id` — which is what lets the frontend's contract find its
/// `ResolvesToContract` counterpart instead of fabricating a separate endpoint.
/// Only when the **first segment** is a variable (`BASE + '/api' + url`) is the whole thing abandoned (returns
/// `None`): a URL whose prefix cannot be anchored has a shape that would be pure guesswork.

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a piece of source and return every syntax fact.
    fn parse_src(src: &str) -> SyntaxFacts {
        JsFrontendParser::new()
            .unwrap()
            .parse("sample.js", src)
            .expect("parsing should succeed")
    }

    /// Take the first HTTP call site's (callee_text, url, method) for easy assertions.
    fn first_http(src: &str) -> (String, Option<String>, String) {
        let facts = parse_src(src);
        let c = facts
            .call_sites
            .iter()
            .find(|c| c.args.iter().any(|a| matches!(a, FactValue::Array(_))))
            .expect("expected one HTTP call to be extracted");
        let url = c
            .args
            .first()
            .and_then(|a| match a {
                FactValue::Array(items) => items
                    .iter()
                    .find(|(k, _)| k == "url")
                    .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
                _ => None,
            });
        let method = c
            .args
            .first()
            .and_then(|a| match a {
                FactValue::Array(items) => items
                    .iter()
                    .find(|(k, _)| k == "method")
                    .and_then(|(_, v)| v.as_str().map(|s| s.to_string())),
                _ => None,
            })
            .unwrap_or_default();
        (c.callee_text.clone(), url, method)
    }

    /// Only calls inside the loop **body** count as in_loop: a call in the iterable / condition runs once, not
    /// per iteration (same trade-off as the PHP / Python parsers).
    /// Calls reached via field positions (variable-initialiser value, `for`-of iterable) are collected too,
    /// and only calls inside the loop **body** count as in_loop: `countAll()` (declarator init) and `fetchPage()`
    /// (for-of iterable) run once, so must be collected but `in_loop = false`; `db.query` / `db.flush` are in bodies.
    #[test]
    fn loop_body_marks_call_sites() {
        let facts = parse_src(
            r#"function run(items) {
    const total = countAll();
    const page = fetchPage();
    for (const u of items) {
        db.query(u);
    }
    for (const v of getMore()) {
        db.save(v);
    }
    while (more()) {
        db.flush();
    }
}"#,
        );
        let find = |callee: &str| facts.call_sites.iter().find(|c| c.callee_text == callee);
        // Calls reached via field positions (declarator initialiser / for-of iterable / while condition) are
        // collected...
        assert!(find("countAll").is_some(), "declarator-initialiser call is collected");
        assert!(find("fetchPage").is_some(), "declarator-initialiser call is collected");
        assert!(find("getMore").is_some(), "for-of iterable call is collected");
        assert!(find("more").is_some(), "while-condition call is collected");
        // ...but only calls inside the loop **body** count as in_loop; an initialiser / iterable / condition runs
        // once per loop, so must be collected as NOT in a loop body.
        assert!(!find("countAll").unwrap().in_loop);
        assert!(!find("fetchPage").unwrap().in_loop);
        assert!(!find("getMore").unwrap().in_loop, "for-of iterable call is not in a loop body");
        assert!(!find("more").unwrap().in_loop, "while-condition call is not in a loop body");
        assert!(find("db.query").unwrap().in_loop);
        assert!(find("db.save").unwrap().in_loop);
        assert!(find("db.flush").unwrap().in_loop);
    }

    /// Decorators -> call sites (the basis of NestJS routes): same mechanism as Java annotations / Python decorators.
    #[test]
    fn nestjs_decorators_become_call_sites() {
        let facts = parse_src(
            "@Controller('cats')\nexport class CatsController {\n  @Get(':id')\n  findOne(@Param('id') id: string) { return 1; }\n}\n",
        );
        let ctrl = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("Controller"))
            .expect("expected @Controller to be collected");
        assert_eq!(ctrl.owner_fqn, "CatsController");
        assert_eq!(ctrl.args.first().and_then(|a| a.as_str()), Some("cats"));
        let get = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("Get"))
            .expect("expected @Get to be collected");
        assert_eq!(get.owner_fqn, "CatsController.findOne");
        assert_eq!(get.args.first().and_then(|a| a.as_str()), Some(":id"));
        assert_eq!(ctrl.callee_text, "@Controller");
        assert_eq!(get.callee_text, "@Get");
        assert!(
            facts.call_sites.iter().all(|c| c.method.as_deref() != Some("Param")),
            "parameter decorators must not be collected as call sites, got: {:?}",
            facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>()
        );
    }

    /// Constructor parameter injection -> an `@Inject` call site (the basis of NestJS dependency injection).
    ///
    /// `constructor(private readonly userService: UserService)` should produce a call site with
    /// `callee_text = "@Inject"` and `entity = "UserService"` (the injected type), owned by the class itself —
    /// so FKB can build a `DependsOn` edge.
    #[test]
    fn constructor_param_injection_becomes_inject_call_site() {
        let facts = parse_src(
            "export class UserController {\n  constructor(private readonly userService: UserService) {}\n}\n",
        );
        let inject = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "@Inject")
            .expect("expected the constructor injection points to be collected");
        assert_eq!(inject.owner_fqn, "UserController");
        assert_eq!(inject.owner_class.as_deref(), Some("UserController"));
        assert_eq!(inject.entity.as_deref(), Some("UserService"));
        let facts2 = parse_src(
            "export class X {\n  constructor(plain: Foo, private svc: Bar) {}\n}\n",
        );
        let injects: Vec<&str> = facts2
            .call_sites
            .iter()
            .filter(|c| c.callee_text == "@Inject")
            .map(|c| c.entity.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(injects, vec!["Bar"], "only the decorated svc: Bar counts as an injection; plain: Foo does not");
    }

    /// Field decorators -> call sites (the basis of TypeORM's `@Column`): the owner is precise to the **field FQN**
    /// (`Class.field`), so FKB can take `owner_class.owner_member` as the column identity
    /// (`UserEntity.username`) — otherwise same-named columns in different entities would merge into one.
    #[test]
    fn field_decorators_become_call_sites_on_field_fqn() {
        let facts = parse_src(
            "export class UserEntity {\n  @PrimaryGeneratedColumn()\n  id: number;\n\n  @Column()\n  username: string;\n}\n",
        );
        let cols: Vec<&str> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text == "@Column" || c.callee_text == "@PrimaryGeneratedColumn")
            .map(|c| c.owner_fqn.as_str())
            .collect();
        assert_eq!(
            cols,
            vec!["UserEntity.id", "UserEntity.username"],
            "a field decorator should attach to the field FQN (Class.field), got: {cols:?}"
        );
    }

    /// Relation decorators -> the target entity is written into the call site's `entity` (the basis of TypeORM relations).
    #[test]
    fn relation_decorators_carry_target_entity() {
        let facts = parse_src(
            "export class ArticleEntity {\n  @ManyToOne(type => UserEntity, user => user.articles)\n  author: UserEntity;\n\n  @OneToMany(type => Comment, comment => comment.article)\n  comments: Comment[];\n}\n",
        );
        let rel: Vec<(&str, &str)> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text == "@ManyToOne" || c.callee_text == "@OneToMany")
            .map(|c| (c.owner_fqn.as_str(), c.entity.as_deref().unwrap_or("<none>")))
            .collect();
        assert_eq!(
            rel,
            vec![
                ("ArticleEntity.author", "UserEntity"),
                ("ArticleEntity.comments", "Comment"),
            ],
            "a relation decorator should carry the target entity (element type for arrays), got: {rel:?}"
        );
    }

    #[test]
    fn uni_request_object() {
        let (callee, url, method) = first_http("uni.request({ url: '/api/delete', method: 'POST' })");
        assert_eq!(callee, "uni.request");
        assert_eq!(url.as_deref(), Some("/api/delete"));
        assert_eq!(method, "POST");
    }

    #[test]
    fn axios_member_verb() {
        let (callee, url, method) = first_http("axios.post('/api/list')");
        assert_eq!(callee, "axios");
        assert_eq!(url.as_deref(), Some("/api/list"));
        assert_eq!(method, "POST");
    }

    #[test]
    fn fetch_with_options() {
        let (callee, url, method) = first_http("fetch('/api/x', { method: 'PUT' })");
        assert_eq!(callee, "fetch");
        assert_eq!(url.as_deref(), Some("/api/x"));
        assert_eq!(method, "PUT");
    }

    #[test]
    fn bare_request_defaults_get() {
        let (callee, url, method) = first_http("request({ url: '/y' })");
        assert_eq!(callee, "request");
        assert_eq!(url.as_deref(), Some("/y"));
        assert_eq!(method, "GET");
    }

    #[test]
    fn axios_object_form() {
        let (callee, url, method) =
            first_http("axios({ url: '/obj', method: 'delete' })");
        assert_eq!(callee, "axios");
        assert_eq!(url.as_deref(), Some("/obj"));
        assert_eq!(method, "DELETE");
    }

    #[test]
    fn non_http_calls_ignored() {
        let facts = parse_src("doWork('a', b);\nif (x) { foo() }");
        assert!(!facts.call_sites.is_empty(), "ordinary call sites should still be collected");
        assert!(
            facts.call_sites.iter().all(|c| !c.args.iter().any(|a| {
                matches!(a, FactValue::Array(items)
                    if items.iter().any(|(k, _)| k == "url" || k == "method"))
            })),
            "a non-HTTP call must not carry normalised url/method arguments"
        );
        let work = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "doWork")
            .expect("expected the doWork call to be collected");
        assert!(
            matches!(work.args.first(), Some(FactValue::String(s)) if s == "a"),
            "literal arguments of an ordinary call must be usable (frontend semantic nodes take their identity from arg:0), got: {:?}",
            work.args
        );
    }

    #[test]
    fn variable_url_is_unknown() {
        let facts = parse_src("const BASE='https://x';\naxios.get(BASE + '/api/' + path)");
        let c = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "axios")
            .expect("expected the axios call to be collected");
        let url_unknown = matches!(
            c.args.first(),
            Some(FactValue::Array(items)) if items
                .iter()
                .any(|(k, v)| k == "url" && matches!(v, FactValue::Unknown(_)))
        );
        assert!(url_unknown, "a url whose first segment is a variable must be Unknown");
    }

    #[test]
    fn templated_url_keeps_param_shape() {
        let (_, url, method) = first_http("axios.get(`/api/order/${id}/detail`)");
        assert_eq!(url.as_deref(), Some("/api/order/:param/detail"));
        assert_eq!(method, "GET");
        let (_, url2, _) = first_http("request.get('/api/invoice/' + id)");
        assert_eq!(url2.as_deref(), Some("/api/invoice/:param"));
    }

    #[test]
    fn variable_url_const_ref() {
        let (_, url, method) = first_http(
            "const IndexUrl = '/home/index';\nrequest({ url: IndexUrl, method: 'get' })",
        );
        assert!(url.is_none(), "a url that references a constant must not resolve to a literal");
        assert_eq!(method, "GET");
    }

    #[test]
    fn url_key_not_confused_with_base_url() {
        let (_, url, _) = first_http("request({ base_url: '/x', url: '/y' })");
        assert_eq!(url.as_deref(), Some("/y"));
    }

    #[test]
    fn multiline_object_url_still_resolved() {
        let (_, url, method) = first_http(
            "request({\n    url: 'agent/statistics',\n    method: 'get',\n    params,\n  });",
        );
        assert_eq!(url.as_deref(), Some("agent/statistics"));
        assert_eq!(method, "GET");
    }

    #[test]
    fn function_declaration_is_semantic_node() {
        let src = "export function agentGet() {\n  return request({ url: '/api/get', method: 'get' });\n}";
        let facts = parse_src(src);
        let fn_decl = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "agentGet");
        assert!(fn_decl.is_some(), "expected an agentGet function node to be built");
        let http = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "request")
            .expect("expected the request call to be collected");
        assert_eq!(
            http.owner_fqn, "agentGet",
            "the owner of an HTTP call should be the function agentGet, not the file"
        );
    }

    #[test]
    fn component_calls_api_function() {
        let src = "export function loadList() {\n  return agentGet();\n}";
        let facts = parse_src(src);
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "loadList"),
            "expected a loadList function node to be built"
        );
        let call = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "agentGet")
            .expect("expected the call to agentGet to be collected");
        assert_eq!(call.owner_fqn, "loadList");
    }

    #[test]
    fn class_method_owner_is_method_fqn() {
        let src = "class Api { getX() { return request({ url: '/x' }); } }";
        let facts = parse_src(src);
        assert!(
            facts
                .declarations
                .iter()
                .any(|d| d.kind.as_str() == NodeKind::CLASS && d.name == "Api"),
            "expected an Api class node to be built"
        );
        let m = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::METHOD && d.name == "getX");
        assert!(m.is_some(), "expected a getX method node to be built");
        let call = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "request")
            .expect("expected the request call to be collected");
        assert_eq!(call.owner_fqn, "Api.getX", "the owner of a call inside a method should be Api.getX");
    }

    #[test]
    fn vue_script_is_parsed_with_aligned_lines() {
        let src = "<template>\n  <div></div>\n</template>\n<script>\nfunction foo() {}\n</script>\n";
        let facts = JsFrontendParser::new()
            .unwrap()
            .parse("Comp.vue", src)
            .expect("parsing should succeed");
        let foo = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::FUNCTION && d.name == "foo")
            .expect("expected the foo function inside the vue file to be parsed");
        assert_eq!(foo.span.start_line, 5, "line numbers of functions inside a vue file should align with the original file (<script> is on line 4, the function on line 5)");
    }

    // ---- the string helpers behind HTTP extraction (only ever covered indirectly above) ----

    #[test]
    fn split_top_commas_respects_strings_and_nesting() {
        assert_eq!(split_top_commas("a, b"), vec!["a", "b"]);
        assert_eq!(split_top_commas("'x,y', z"), vec!["'x,y'", "z"], "a comma inside a string must not split");
        assert_eq!(split_top_commas("{a: 1, b: 2}, c"), vec!["{a: 1, b: 2}", "c"], "a comma inside braces must not split");
        assert_eq!(split_top_commas("f(1,2), e"), vec!["f(1,2)", "e"], "a comma inside parentheses must not split");
        assert_eq!(split_top_commas("only"), vec!["only"]);
        assert_eq!(
            split_top_commas(r#""a\",b", c"#),
            vec![r#""a\",b""#, "c"],
            "an escaped quote must not end the string early"
        );
    }

    #[test]
    fn split_top_plus_skips_strings() {
        let parts = |s: &str| -> Vec<String> {
            split_top_plus(s).iter().map(|p| p.trim().to_string()).collect()
        };
        assert_eq!(parts("'a' + id"), vec!["'a'", "id"]);
        assert_eq!(parts("a + b + c"), vec!["a", "b", "c"]);
        assert_eq!(parts("'a+b' + c"), vec!["'a+b'", "c"], "a `+` inside a string must not split");
    }

    /// Degrading instead of panicking matters here: argument text may lack the closing symbol or end with a
    /// multi-byte character, and a raw byte slice would panic.
    #[test]
    fn strip_ends_degrades_when_the_close_is_missing() {
        assert_eq!(strip_ends("[1,2]", '[', ']'), "1,2");
        assert_eq!(strip_ends("`x`", '`', '`'), "x");
        assert_eq!(strip_ends("[1,2", '[', ']'), "1,2", "with the right half missing it degrades to stripping the left only");
        assert_eq!(strip_ends("abc", '[', ']'), "abc", "when it does not match it is returned verbatim");
    }

    /// `$var` / `fn()` must fall through to `Unknown` — FKB's `require_literal` rejects them, so a variable can
    /// never become identity (same handling as the back end's `Cache::get($name)`).
    #[test]
    fn js_value_maps_literals_and_unknown() {
        assert!(matches!(js_value("'abc'"), FactValue::String(ref s) if s == "abc"));
        assert!(matches!(js_value("123"), FactValue::Int(123)));
        assert!(matches!(js_value("-5"), FactValue::Int(-5)));
        assert!(matches!(js_value("true"), FactValue::Bool(true)));
        assert!(matches!(js_value("false"), FactValue::Bool(false)));
        assert!(
            matches!(js_value("foo"), FactValue::Unknown(Some(ref s)) if s == "foo"),
            "a variable must land on Unknown (the FKB's require_literal rejects it)"
        );
        match js_value("['a','b']") {
            FactValue::Array(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0].1, FactValue::String(ref s) if s == "a"));
            }
            other => panic!("an array literal must fold into Array: {other:?}"),
        }
        match js_value("{a: 1}") {
            FactValue::Array(items) => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].0, "a");
                assert!(matches!(items[0].1, FactValue::Int(1)));
            }
            other => panic!("an object literal must fold into a key-value Array: {other:?}"),
        }
    }

    /// The documented shapes of URL normalisation: only the *shape* matters, so every interpolation folds into
    /// `:param`, and a variable first segment abandons the whole URL (no anchoring, no guessing).
    #[test]
    fn literal_url_expr_normalises_the_documented_shapes() {
        assert_eq!(
            literal_url_expr("'v2/invoice/detail/' + id").as_deref(),
            Some("v2/invoice/detail/:param")
        );
        assert_eq!(
            literal_url_expr("`v2/order/invoice_detail/${id}`").as_deref(),
            Some("v2/order/invoice_detail/:param")
        );
        assert_eq!(literal_url_expr("'v2/index'").as_deref(), Some("v2/index"));
        assert_eq!(literal_url_expr("'v2/index/'").as_deref(), Some("v2/index"), "a trailing slash must be removed");
        assert_eq!(
            literal_url_expr("BASE + '/api' + url"),
            None,
            "the first segment is a variable → even the prefix cannot be anchored, so it is abandoned entirely"
        );
        assert_eq!(literal_url_expr(""), None);
        assert_eq!(literal_url_expr("'/api' + x + '/y'").as_deref(), Some("/api:param/y"));
    }

    #[test]
    fn top_level_segment_stops_at_the_first_top_level_comma() {
        assert_eq!(top_level_segment("'a/' + id, data, {x: 1}").as_deref(), Some("'a/' + id"));
        assert_eq!(top_level_segment("f(1,2), x").as_deref(), Some("f(1,2)"), "a comma inside parentheses is not top-level");
        assert_eq!(top_level_segment(""), None);
    }

    /// `url: url` must not overrun its key, and `base_url:` must not be mistaken for `url:` — the prefix rule
    /// (the preceding byte must not be an identifier byte) is what keeps them apart.
    #[test]
    fn field_expr_does_not_confuse_base_url_with_url() {
        assert_eq!(field_expr("({url: '/a', method: 'POST'})", "url").as_deref(), Some("'/a'"));
        assert_eq!(
            field_expr("({base_url: 'x', url: '/a'})", "url").as_deref(),
            Some("'/a'"),
            "`base_url:` must not be taken for `url:`"
        );
        assert_eq!(field_expr("({method: 'POST'})", "url"), None);
    }

    /// Two closed sets that guard against fake contracts: an HTTP verb alone is not enough (`$store.get()` /
    /// `cache.get()` are everywhere), and the verb table is lowercase because that is the source form.
    /// The pluggable-language surface: the kernel must learn JS/TS's notation from the adapter instead of
    /// assuming PHP's (`\` / `::` / `$this`).
    #[test]
    fn parser_declares_the_js_notation_and_layout() {
        let p = JsFrontendParser::new().unwrap();
        assert_eq!(p.language(), Language::new(Language::JAVASCRIPT));
        assert_eq!(p.namespace_separator(), &['.']);
        assert_eq!(p.member_separator(), ".", "JS's member separator is `.` (PHP's is `::`)");
        assert!(p.bare_field_receivers(), "`@InjectRepository(User) repo` uses a bare identifier");
        assert_eq!(p.manifest_files(), &["package.json"]);
        assert!(p.extensions().contains(&"vue"), "`.vue` single-file components must count as an extension too");
        assert!(p.exclude_dirs().contains(&"node_modules"));
    }

    /// Imports feed the short-name index (`resolve_name_in_file`), so `new UserService()` / a bare type
    /// reference can be restored to an FQN. All three spellings must land in `facts.imports`.
    #[test]
    fn imports_are_collected_in_every_spelling() {
        let facts = parse_src(
            "import axios from 'axios';\nimport { UserService as Svc, Other } from './svc';\nimport * as ns from './ns';\n",
        );
        let got: Vec<(Option<&str>, &str)> = facts
            .imports
            .iter()
            .map(|i| (i.alias.as_deref(), i.name.as_str()))
            .collect();
        assert!(got.contains(&(None, "axios")), "default import: {got:?}");
        assert!(got.contains(&(Some("Svc"), "UserService")), "named import with alias: {got:?}");
        assert!(got.contains(&(None, "Other")), "named import: {got:?}");
        assert!(got.contains(&(None, "ns")), "namespace import: {got:?}");
    }

    /// Supertypes become inheritance facts — the only clue to "which base class carries the shared `MapsTo` /
    /// behaviour", and what `parents_of` walks later.
    ///
    /// Regressions pinned here: `extends` and `implements` must **both** be recorded, each under its own
    /// kind (only the first clause used to be taken, always as `Extends`), and an `implements` list with
    /// several entries must yield one fact per entry.
    #[test]
    fn supertypes_are_recorded_with_their_own_kind() {
        let inh = |src: &str| -> Vec<(String, String, String)> {
            let facts = parse_src(src);
            facts
                .inheritances
                .iter()
                .map(|i| {
                    (
                        i.child_fqn.clone(),
                        i.base_name.clone(),
                        i.kind.as_str().to_string(),
                    )
                })
                .collect()
        };
        let want = |v: &[(&str, &str, &str)]| -> Vec<(String, String, String)> {
            v.iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect()
        };

        // extends + implements: both, each under its own kind.
        assert_eq!(
            inh("export class CatService extends BaseService implements OnModuleInit {\n  run() {}\n}\n"),
            want(&[
                ("CatService", "BaseService", "Extends"),
                ("CatService", "OnModuleInit", "Implements"),
            ])
        );

        // Only implements (the NestJS norm) must not be recorded as "extends".
        assert_eq!(
            inh("export class CatService implements OnModuleInit, OnDestroy {\n  run() {}\n}\n"),
            want(&[
                ("CatService", "OnModuleInit", "Implements"),
                ("CatService", "OnDestroy", "Implements"),
            ])
        );

        // A generic base keeps only its bare name.
        assert_eq!(
            inh("export class Repo extends BaseRepo<UserEntity> {\n}\n"),
            want(&[("Repo", "BaseRepo", "Extends")]),
        );
    }

    /// The declared type of an injected constructor property: a TypeORM `Repository<T>` is unwrapped to the
    /// **entity** `T`, so P7 can resolve `repo.save(x)` to the entity's table through its `MapsTo` edge —
    /// while an unrelated generic keeps its declared text.
    #[test]
    fn injected_field_types_unwrap_the_repository_entity() {
        let facts = parse_src(
            "export class UserController {\n  constructor(private readonly repo: Repository<UserEntity>, private readonly svc: UserService, private readonly p: Promise<User>) {}\n}\n",
        );
        let types: Vec<(&str, &str)> = facts
            .field_types
            .iter()
            .map(|f| (f.field.as_str(), f.type_name.as_str()))
            .collect();
        assert!(
            types.contains(&("repo", "UserEntity")),
            "`Repository<T>` must unwrap to the entity T: {types:?}"
        );
        assert!(types.contains(&("svc", "UserService")), "a plain type is kept verbatim: {types:?}");
        assert!(
            types.contains(&("p", "Promise<User>")),
            "a non-Repository generic keeps its declared text: {types:?}"
        );
        assert!(
            facts.field_types.iter().all(|f| f.class_fqn == "UserController"),
            "the field type must be attached to the class injecting it: {:?}",
            facts.field_types
        );
        // The same entity rides on the `@Inject` call site.
        assert!(
            facts
                .call_sites
                .iter()
                .any(|c| c.callee_text == "@Inject" && c.entity.as_deref() == Some("UserEntity")),
            "an @Inject call site must carry the entity as well: {:?}",
            facts.call_sites
        );
    }

    /// A call site carries the source line it sits on (`snippet`) and a **1-based** span: the view shows the
    /// snippet as the call site and jumps by the span, so an off-by-one line would point at the wrong code.
    #[test]
    fn call_sites_carry_snippet_and_one_based_span() {
        let facts =
            parse_src("export class Svc {\n  constructor(private readonly svc: UserService) {}\n}\n");
        let inject = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "@Inject")
            .expect("expected an @Inject call site");
        assert_eq!(inject.span.start_line, 2, "the span line number must be 1-based: {:?}", inject.span);
        assert!(
            inject.snippet.as_deref().unwrap_or("").contains("UserService"),
            "the snippet must be the source of that line: {:?}",
            inject.snippet
        );
    }

    #[test]
    fn http_verb_and_client_receiver_are_closed_sets() {
        for v in ["get", "post", "put", "delete", "patch", "head", "options"] {
            assert!(is_http_verb(v), "`{v}` must be recognised as an HTTP verb");
        }
        assert!(!is_http_verb("GET"), "the verb table only holds lower-case forms (the shape of member names in source)");
        assert!(!is_http_verb("fetch"));

        assert!(is_http_client_recv("request"));
        assert!(is_http_client_recv("this.request"), "judged by the last segment");
        assert!(is_http_client_recv("$http"));
        assert!(is_http_client_recv("apiClient"));
        assert!(!is_http_client_recv("cache"), "`cache.get()` must not be taken for a contract");
        assert!(!is_http_client_recv("storage"));
        assert!(!is_http_client_recv("$store"));
    }

    /// `export default { ... }` / `module.exports = { ... }` are how front-end config is written (route tables,
    /// site config): collecting them as `ConfigEntryFact` makes the `kind: config_entry` selector work for the
    /// front end exactly as it does for PHP's `return [...]`. This whole feature had **no coverage** — only the
    /// producing push existed, never an assertion.
    ///
    /// The rule pinned here is "**scalar leaves only**, joined by `.`": a method / arrow function is deliberately
    /// not expanded (otherwise every Vue component's `methods` would become config), and a value that cannot be
    /// evaluated statically is not invented.
    #[test]
    fn exported_object_literals_become_config_entries_only_at_scalar_leaves() {
        let facts = parse_src(
            r#"export default {
  name: 'myapp',
  port: 3000,
  debug: true,
  nested: { a: 'x', deep: { b: 'y' } },
  handler() { return 1; },
  arrow: () => 1,
  interp: `hi ${x}`,
  ref: someVar,
  "quoted-key": 'v',
  empty: {},
};
"#,
        );
        let get = |k: &str| facts.config_entries.iter().find(|e| e.key_path == k).map(|e| e.value.clone());

        // Scalar leaves keep their own kind, nested objects are flattened into dotted key paths.
        assert_eq!(get("name"), Some(FactValue::String("myapp".into())), "{:?}", facts.config_entries);
        assert_eq!(get("port"), Some(FactValue::Int(3000)));
        assert_eq!(get("debug"), Some(FactValue::Bool(true)));
        assert_eq!(get("nested.a"), Some(FactValue::String("x".into())), "nested objects join their keys with `.`");
        assert_eq!(get("nested.deep.b"), Some(FactValue::String("y".into())));
        assert_eq!(get("quoted-key"), Some(FactValue::String("v".into())), "quotes around a key are trimmed");

        // What must NOT appear: non-scalar values and anything not statically known.
        assert_eq!(get("handler"), None, "a method is not a scalar leaf");
        assert_eq!(get("arrow"), None, "an arrow function is not a scalar leaf");
        assert_eq!(get("interp"), None, "an interpolated template is not statically evaluable");
        assert_eq!(get("ref"), None, "a variable reference must never be guessed into config");
        assert!(facts.config_entries.iter().all(|e| !e.key_path.starts_with("empty")), "an empty nested object has no leaf");
        assert!(
            facts.config_entries.iter().all(|e| !e.key_path.starts_with("list")),
            "array values are not flattened into entries: {:?}",
            facts.config_entries
        );

        // `module.exports = { ... }` (the CommonJS spelling) is collected by the same rule.
        let cjs = parse_src("module.exports = { timeout: 5, nested2: { k: 'v' } };");
        let cjs_get = |k: &str| cjs.config_entries.iter().find(|e| e.key_path == k).map(|e| e.value.clone());
        assert_eq!(cjs_get("timeout"), Some(FactValue::Int(5)), "{:?}", cjs.config_entries);
        assert_eq!(cjs_get("nested2.k"), Some(FactValue::String("v".into())));
    }

    /// Only an **object literal** counts as config, and an anonymous default export must not invent a node for
    /// itself — its body's calls belong to the enclosing scope.
    #[test]
    fn non_object_exports_produce_no_config_and_no_anonymous_node() {
        let f = parse_src("export default function () { return 1; }");
        assert!(f.config_entries.is_empty(), "a function export is not a config table: {:?}", f.config_entries);
        assert!(
            f.declarations.iter().all(|d| d.kind.as_str() != NodeKind::FUNCTION),
            "an anonymous default export must not build a FUNCTION node: {:?}",
            f.declarations
        );

        assert!(
            parse_src("export default someIdentifier;").config_entries.is_empty(),
            "exporting an identifier is not config"
        );

        // No invented node, but the body is still walked so the call is collected with the outer owner.
        let arrow = parse_src("export default () => { helper(); };");
        assert!(arrow.declarations.is_empty(), "no node for an anonymous export: {:?}", arrow.declarations);
        assert!(
            arrow.call_sites.iter().any(|c| c.callee_text == "helper"),
            "calls inside an anonymous body still belong to the outer scope: {:?}",
            arrow.call_sites
        );
    }

    /// The decorator branch existing tests never exercise: a **bare** decorator (`@Injectable`, no call form).
    /// It still becomes a call site so FKB can match it, and it records no `owner_class` (unlike the call form,
    /// which inherits one from `collect_invocation`).
    #[test]
    fn bare_decorator_without_arguments_becomes_a_call_site() {
        let facts = parse_src("@Injectable\nclass Foo { run() {} }");
        let d = facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == "@Injectable")
            .unwrap_or_else(|| panic!("a bare decorator must become a call site: {:?}", facts.call_sites));
        assert_eq!(d.method.as_deref(), Some("Injectable"), "the decorator name is also the method field");
        assert!(d.args.is_empty(), "no argument list ⇒ no arguments, got: {:?}", d.args);
        assert_eq!(d.owner_class, None, "the bare form records no owner_class (the call form does)");
    }

    /// React / Preact JSX: `<MyButton onClick={handleClick} />` yields a `component_ref` binding (PascalCase
    /// tag `MyButton`) and an `event` binding (`onClick` -> handler `handleClick`). A native HTML tag (`div` /
    /// `Plain`) and a string-literal `onClick="noop"` must NOT become bindings.
    #[test]
    fn react_jsx_template_yields_component_ref_and_event_binding() {
        let src = r#"
          export function App() {
            return (
              <div>
                <MyButton onClick={handleClick} />
                <Other onSend={this.onSend} />
                <span onClick="noop" />
              </div>
            );
          }
        "#;
        let facts = JsFrontendParser::new().unwrap().parse("App.tsx", src).unwrap();
        let comps: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "component_ref")
            .map(|b| b.name.as_str())
            .collect();
        assert!(comps.contains(&"MyButton"), "a PascalCase tag is a component ref: {comps:?}");
        assert!(comps.contains(&"Other"), "a second PascalCase tag is a component ref: {comps:?}");
        assert!(
            !comps.contains(&"span"),
            "a lowercase tag is a native element, not a component: {comps:?}"
        );

        let events: Vec<(&str, &str)> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "event")
            .map(|b| (b.name.as_str(), b.handler.as_deref().unwrap_or("")))
            .collect();
        assert!(
            events.iter().any(|(e, h)| *e == "click" && *h == "handleClick"),
            "onClick={{handleClick}} -> event click / handler handleClick: {events:?}"
        );
        assert!(
            events.iter().any(|(e, h)| *e == "send" && *h == "onSend"),
            "onClick={{this.onSend}} -> handler onSend (this stripped): {events:?}"
        );
        assert!(
            !events.iter().any(|(e, _)| *e == "noop"),
            "a string-literal onClick is not a handler binding: {events:?}"
        );
    }

    /// Vue SFC: the `<template>` block (outside the `<script>` tree) yields component refs, event bindings
    /// (`@click="onX"` / `v-on:click="onX"`) and prop bindings (`:user="u"`), with built-ins (`RouterLink`)
    /// excluded.
    #[test]
    fn vue_template_yields_component_ref_event_and_prop_bindings() {
        let src = r#"
          <template>
            <div>
              <HelloWorld :msg="hello" @click="onClick" />
              <RouterLink :to="path" />
              <Child v-on:submit="onSubmit" />
            </div>
          </template>
          <script>
            export default { methods: { onClick() {}, onSubmit() {} } };
          </script>
        "#;
        let facts = JsFrontendParser::new().unwrap().parse("Hello.vue", src).unwrap();
        let comps: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "component_ref")
            .map(|b| b.name.as_str())
            .collect();
        assert!(comps.contains(&"HelloWorld"), "PascalCase SFC tag is a component ref: {comps:?}");
        assert!(comps.contains(&"Child"), "a second PascalCase tag is a component ref: {comps:?}");
        assert!(
            !comps.contains(&"RouterLink"),
            "a Vue built-in must be excluded: {comps:?}"
        );

        let events: Vec<(&str, &str)> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "event")
            .map(|b| (b.name.as_str(), b.handler.as_deref().unwrap_or("")))
            .collect();
        assert!(
            events.iter().any(|(e, h)| *e == "click" && *h == "onClick"),
            "@click=\"onClick\" -> event click / handler onClick: {events:?}"
        );
        assert!(
            events.iter().any(|(e, h)| *e == "submit" && *h == "onSubmit"),
            "v-on:submit=\"onSubmit\" -> event submit / handler onSubmit: {events:?}"
        );

        let props: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "prop")
            .map(|b| b.name.as_str())
            .collect();
        assert!(props.contains(&"msg"), ":msg=\"hello\" -> prop msg: {props:?}");
    }

    /// React JSX event names are case-sensitive: `onClick` (on + uppercase) binds, but `onclick` (all
    /// lowercase) and `online` (on + lowercase) must NOT. A string-literal event value is also ignored.
    #[test]
    fn react_jsx_event_is_case_sensitive() {
        let src = r#"
          export function App() {
            return (
              <div>
                <MyButton onClick={go} />
                <Other onclick="x" />
                <Widget online="y" />
              </div>
            );
          }
        "#;
        let facts = JsFrontendParser::new().unwrap().parse("App.tsx", src).unwrap();
        let events: Vec<(&str, &str)> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "event")
            .map(|b| (b.name.as_str(), b.handler.as_deref().unwrap_or("")))
            .collect();
        assert!(
            events.iter().any(|(e, h)| *e == "click" && *h == "go"),
            "onClick={{go}} binds to click/go: {events:?}"
        );
        // `onclick` is lowercase after the `on` prefix -> the binding is skipped entirely (no handler `x`).
        assert!(
            !events.iter().any(|(_, h)| *h == "x"),
            "onclick (lowercase) must NOT bind: {events:?}"
        );
        assert!(
            events.iter().all(|(e, _)| *e != "line"),
            "online must NOT bind: {events:?}"
        );
    }

    /// Vue SFC: kebab-case components (`<user-card>`) and event/prop modifiers (`@click.prevent`) are captured;
    /// the modifier is stripped so the event name stays the canonical `click`.
    #[test]
    fn vue_template_kebab_component_and_modifiers() {
        let src = r#"
          <template>
            <user-card :name="n" @click.prevent="onCardClick" />
          </template>
          <script>
            export default { methods: { onCardClick() {} } };
          </script>
        "#;
        let facts = JsFrontendParser::new().unwrap().parse("Card.vue", src).unwrap();
        let comps: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "component_ref")
            .map(|b| b.name.as_str())
            .collect();
        assert!(comps.contains(&"user-card"), "kebab component is a component ref: {comps:?}");

        let events: Vec<(&str, &str)> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "event")
            .map(|b| (b.name.as_str(), b.handler.as_deref().unwrap_or("")))
            .collect();
        assert!(
            events.iter().any(|(e, h)| *e == "click" && *h == "onCardClick"),
            "@click.prevent -> event click / handler onCardClick: {events:?}"
        );

        let props: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "prop")
            .map(|b| b.name.as_str())
            .collect();
        assert!(props.contains(&"name"), ":name -> prop name: {props:?}");
    }

    /// Angular inline template: `@Component({ template: \`...\` })` yields component refs (`<app-hello>`),
    /// event bindings (`(click)="onSave()"`), and prop bindings (`[user]="user"`). External `templateUrl` is
    /// intentionally NOT parsed (parse is single-file, so the sibling `.html` cannot be read).
    #[test]
    fn angular_inline_template_yields_component_ref_event_and_prop_bindings() {
        let src = r#"
          @Component({
            selector: 'app-root',
            template: `<div>
              <app-hello [user]="user" (click)="onHello()"></app-hello>
              <app-footer (submit)="onSubmit()"></app-footer>
            </div>`,
          })
          export class AppComponent {
            user = 'x';
            onHello() {}
            onSubmit() {}
          }
        "#;
        let facts = JsFrontendParser::new().unwrap().parse("app.component.ts", src).unwrap();
        let comps: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "component_ref")
            .map(|b| b.name.as_str())
            .collect();
        assert!(comps.contains(&"app-hello"), "an app- prefixed tag is a component ref: {comps:?}");
        assert!(
            comps.contains(&"app-footer"),
            "a second app- prefixed tag is a component ref: {comps:?}"
        );

        let events: Vec<(&str, &str)> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "event")
            .map(|b| (b.name.as_str(), b.handler.as_deref().unwrap_or("")))
            .collect();
        assert!(
            events.iter().any(|(e, h)| *e == "click" && *h == "onHello"),
            "(click)=\"onHello()\" -> event click / handler onHello: {events:?}"
        );
        assert!(
            events.iter().any(|(e, h)| *e == "submit" && *h == "onSubmit"),
            "(submit)=\"onSubmit()\" -> event submit / handler onSubmit: {events:?}"
        );

        let props: Vec<&str> = facts
            .template_bindings
            .iter()
            .filter(|b| b.kind == "prop")
            .map(|b| b.name.as_str())
            .collect();
        assert!(props.contains(&"user"), "[user]=\"user\" -> prop user: {props:?}");

        // Negative: a `templateUrl` (external file) must NOT yield bindings.
        let ext = JsFrontendParser::new()
            .unwrap()
            .parse(
                "x.component.ts",
                "@Component({ selector: 'app-x', templateUrl: './x.html' })\nexport class X {}",
            )
            .unwrap();
        assert!(
            ext.template_bindings.is_empty(),
            "templateUrl (external) must not be parsed: {:?}",
            ext.template_bindings
        );
    }
}
