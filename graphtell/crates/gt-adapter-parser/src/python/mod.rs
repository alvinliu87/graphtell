//! Python parser — the **third language**, further verifying that the language-layer abstraction is pluggable.
//!
//! It only does what the syntax layer (P2) should: translate the Python syntax tree into the language-agnostic
//! [`SyntaxFacts`]. The semantic layer (FastAPI's `@app.get` / SQLAlchemy's `__tablename__` / Celery's
//! `@app.task`) is declared by FKB, through the same mechanism as ThinkPHP / Spring Boot.
//!
//! ## Counterparts in Java (same concept, different expression)
//! | concept              | Java                     | Python                   |
//! |----------------------|--------------------------|--------------------------|
//! | declarative framework call | `@GetMapping("/x")` annotation | `@app.get("/x")` decorator |
//! | class                | `class X`                | `class X`                |
//! | member separator     | `.`                      | `.`                      |
//!
//! Both are modelled as [`CallSiteFact`], so FKB's `kind: call` selector matches decorators with no extension at
//! all — "an annotation and a decorator are the same concept" holds on the Python side too.
//!
//! ## Two Python-specific modelling decisions
//! 1. **A module is a namespace**: functions often sit directly in a module (not in a class). So the module's
//!    dotted name is derived from the file path (`app/api/users.py` -> `app.api.users`) and registered both as a
//!    `Namespace` declaration and as the call site's `owner_class`. That is what lets FKB's `HandledBy` link find
//!    the module node first and then drop onto **the function itself** via `{module}.{func}`
//!    (`find_target_node` requires locating a parent node first; otherwise Python route handlers would all miss).
//! 2. **A decorator belongs to what it decorates**: a decorator's `owner_fqn` is the function / class it decorates,
//!    consistent with Java's "a method-level annotation's owner is that method".

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use gt_domain::error::Result;
use gt_domain::model::{
    CallSiteFact, Declaration, EdgeKind, FactValue, ImportFact, InheritanceFact, Language, NodeKind,
    SyntaxFacts,
};
use gt_domain::port::LanguageParser;
use serde_json::json;
use tree_sitter::{Node, Parser};

use crate::ts_util::{bare_type_name, field_children, opt_text, span_of, text};

thread_local! {
    static PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

pub struct PythonParser {
    language: tree_sitter::Language,
}

impl PythonParser {
    pub fn new() -> Result<Self> {
        let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
        Ok(Self { language })
    }
}

impl LanguageParser for PythonParser {
    fn language(&self) -> Language {
        Language::new(Language::PYTHON)
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // The language was validated at construction; failing here is a programming error
                p.set_language(&self.language).expect("python language");
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
        facts.declarations.push(Declaration {
            kind: NodeKind(NodeKind::NAMESPACE.to_string()),
            name: module.clone(),
            fqn: module.clone(),
            parent_fqn: None,
            span: span_of(tree.root_node()),
            extra: json!({}),
        });

        let mut ctx = Ctx {
            src: source.as_bytes(),
            facts: &mut facts,
            module: module.clone(),
            owner_fqn: module,
            current_class: None,
            loop_depth: 0,
            imports: HashMap::new(),
        };
        walk(tree.root_node(), &mut ctx);
        Ok(facts)
    }

    /// Python's namespace separator is `.` (PHP uses `\`).
    fn namespace_separator(&self) -> &'static [char] {
        &['.']
    }

    /// Python's member separator is `.` (PHP uses `::`).
    fn member_separator(&self) -> &'static str {
        "."
    }

    /// Python field access (`self`, `Model.objects`) uses bare identifiers.
    fn bare_field_receivers(&self) -> bool {
        true
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["pyproject.toml", "requirements.txt", "setup.py", "Pipfile"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["__pycache__", "venv", ".venv", "site-packages", ".tox", ".mypy_cache", ".pytest_cache"]
    }
}

/// Parsing context.
struct Ctx<'a> {
    src: &'a [u8],
    facts: &'a mut SyntaxFacts,
    /// The dotted module name of this file (`app.api.users`).
    module: String,
    /// The FQN of the current scope: module -> class -> method, appended as nesting goes deeper.
    owner_fqn: String,
    /// The FQN of the current **class**; `None` at module level (outside a class body).
    current_class: Option<String>,
    /// How many levels of `for` / `while` loop body we are nested in (same as PHP's `loop_depth`):
    /// a depth rather than a boolean, so after an inner loop exits the outer one's remaining statements still count.
    loop_depth: u32,
    /// **Symbol short name -> fully qualified name** for this file (from `from x.y import z`).
    ///
    /// Lets call sites in parameter defaults resolve "the referenced thing" into an FQN (semantics such as
    /// dependency injection / default factories are written in the **signature**, not in a method body).
    /// The parser makes no framework assumption.
    imports: HashMap<String, String>,
}

impl Ctx<'_> {
    /// The owning class of a call site: the class FQN inside a class, the **module FQN** at module level.
    ///
    /// Python route handlers are mostly module-level, so writing `None` would make every link that depends on
    /// `owner_class` miss; using the module name as a fallback is what lets us keep locating the function by member
    /// name on the `Namespace` node.
    fn class_or_module(&self) -> String {
        self.current_class.clone().unwrap_or_else(|| self.module.clone())
    }
}

fn walk(node: Node, ctx: &mut Ctx) {
    match node.kind() {
        "import_statement" | "import_from_statement" => {
            collect_import(node, ctx);
            return;
        }
        "class_definition" => {
            collect_class(node, ctx);
            return;
        }
        "function_definition" => {
            collect_function(node, ctx);
            return;
        }
        "decorated_definition" => {
            collect_decorated(node, ctx);
            return;
        }
        "call" => {
            collect_call(node, ctx);
        }
        // Loop statements: only calls inside the **body** count as "inside the loop" (same as PHP).
        "for_statement" | "while_statement" => {
            collect_loop(node, ctx);
            return;
        }
        "expression_statement" if ctx.current_class.is_some() => {
            if collect_class_attribute(node, ctx) {
                return;
            }
        }
        _ => {}
    }
    recurse(node, ctx);
}

fn recurse(node: Node, ctx: &mut Ctx) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, ctx);
    }
}

// ------------------------------------------------------------------ declarations

/// A class definition -> declaration + inheritance facts + members of the class body. Returns the class FQN.
fn collect_class(node: Node, ctx: &mut Ctx) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), ctx.src)?;
    let fqn = format!("{}.{}", ctx.owner_fqn, name);
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(NodeKind::CLASS.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: Some(ctx.owner_fqn.clone()),
        span: span_of(node),
        extra: json!({}),
    });
    collect_supertypes(node, ctx, &fqn);

    let prev_fqn = ctx.owner_fqn.clone();
    let prev_class = ctx.current_class.clone();
    ctx.owner_fqn = fqn.clone();
    ctx.current_class = Some(fqn.clone());
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx);
    }
    ctx.owner_fqn = prev_fqn;
    ctx.current_class = prev_class;
    Some(fqn)
}

/// Base class list -> inheritance facts.
///
/// Keyword arguments such as `metaclass=Meta` are not base classes (common in PyORM), so they are skipped.
fn collect_supertypes(node: Node, ctx: &mut Ctx, child_fqn: &str) {
    let supers = node
        .child_by_field_name("superclasses")
        .or_else(|| {
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i as u32))
                .find(|n| n.kind() == "argument_list")
        });
    let Some(supers) = supers else { return };
    let mut cursor = supers.walk();
    for arg in supers.named_children(&mut cursor) {
        if arg.kind() == "keyword_argument" {
            continue;
        }
        // Parameterised base classes such as `Base(polymorphic_on=x)`: take the class-name part.
        let base_src = if arg.kind() == "call" {
            arg.child_by_field_name("function")
        } else {
            Some(arg)
        };
        let Some(base) = base_src.and_then(|b| text(b, ctx.src)) else {
            continue;
        };
        ctx.facts.inheritances.push(InheritanceFact {
            child_fqn: child_fqn.to_string(),
            base_name: bare_type_name(base),
            kind: EdgeKind(EdgeKind::EXTENDS.to_string()),
            span: span_of(arg),
        });
    }
}

/// A function / method definition -> a declaration. Returns its FQN.
///
/// Parameters with type annotations are recorded in `extra.parameters`, so the call chain
/// "dependency-injected parameter -> instance call" can later be resolved (same origin as the PHP side's
/// `__construct(LoginServices $services)` usage).
fn collect_function(node: Node, ctx: &mut Ctx) -> Option<String> {
    let name = opt_text(node.child_by_field_name("name"), ctx.src)?;
    let fqn = format!("{}.{}", ctx.owner_fqn, name);
    let kind = if ctx.current_class.is_some() {
        NodeKind::METHOD
    } else {
        NodeKind::FUNCTION
    };
    ctx.facts.declarations.push(Declaration {
        kind: NodeKind(kind.to_string()),
        name: name.clone(),
        fqn: fqn.clone(),
        parent_fqn: Some(ctx.owner_fqn.clone()),
        span: span_of(node),
        extra: json!({ "parameters": params_of(node, ctx.src) }),
    });

    collect_default_calls(node, ctx, &fqn);

    let prev_fqn = ctx.owner_fqn.clone();
    ctx.owner_fqn = fqn.clone();
    if let Some(body) = node.child_by_field_name("body") {
        walk(body, ctx);
    }
    ctx.owner_fqn = prev_fqn;
    Some(fqn)
}

/// An assignment inside a class body -> either a **literal property** or an **ORM field declaration**; both are registered.
///
/// Returns `true` when the statement has been handled completely and the caller need not `recurse` again (so the
/// call site inside a field declaration is not captured twice by recursive traversal); returns `false` when it is
/// "not a class property / field declaration", and `recurse` proceeds as usual (e.g. a local assignment inside a
/// method body, or `self.x = ...` whose left side is not a bare identifier).
///
/// # The two cases
/// * **Literal property** (`__tablename__ = "users"`, etc.): registered as a Property declaration so FKB can hit
///   it with "a node selector + `HasProperty`" (SQLAlchemy's table mapping takes exactly this path). The parser
///   knows no ORM framework, only "class attribute = literal".
/// * **Field declaration** (`name = models.CharField(...)` / `author = models.ForeignKey(User)`): **the same
///   mechanism** as a Java field annotation or a JS field decorator — the call site's owner is made precise to
///   "class.field", so FKB can take `owner_class.owner_member` as the column identity
///   (`app.models.user.User.name`), `HasColumn` attaches to the field itself, and field-level impact can be drilled
///   into. When the first positional argument of a relation field is a class name (an identifier), it is resolved
///   into an FQN via this file's imports and recorded in `entity`, so `References` can link straight to the target
///   model class (same mechanism as FastAPI's `Depends`). The parser likewise does not know Django — only the
///   neutral syntactic shape "a bare identifier in a class body = a call".
fn collect_class_attribute(node: Node, ctx: &mut Ctx) -> bool {
    if ctx.current_class.as_deref() != Some(ctx.owner_fqn.as_str()) {
        return false;
    }
    let Some(assign) = node.named_child(0).filter(|n| n.kind() == "assignment") else {
        return false;
    };
    let (Some(left), Some(right)) = (
        assign.child_by_field_name("left"),
        assign.child_by_field_name("right"),
    ) else {
        return false;
    };
    if left.kind() != "identifier" {
        return false;
    }
    let Some(name) = text(left, ctx.src) else { return false };
    let Some(class_fqn) = ctx.current_class.clone() else { return false };
    match right.kind() {
        // Literal property -> Property declaration (the language-neutral "class attribute = literal").
        "string" | "integer" | "true" | "false" | "none" => {
            let value = match right.kind() {
                "string" => string_value(right, ctx.src).map(FactValue::String),
                "integer" => text(right, ctx.src).map(FactValue::String),
                "true" => Some(FactValue::Bool(true)),
                "false" => Some(FactValue::Bool(false)),
                "none" => Some(FactValue::Null),
                _ => None,
            };
            let Some(value) = value else { return false };
            ctx.facts.declarations.push(Declaration {
                kind: NodeKind(NodeKind::PROPERTY.to_string()),
                name: name.clone(),
                fqn: format!("{}.{}", class_fqn, name),
                parent_fqn: Some(class_fqn),
                span: span_of(node),
                extra: json!({ "default": value }),
            });
            true
        }
        "call" => {
            let field_fqn = format!("{}.{}", class_fqn, name);
            // Compute the relation target first (immutable borrow of ctx), then build the call site (exclusive borrow), avoiding a borrow conflict.
            let entity = right
                .child_by_field_name("arguments")
                .and_then(|a| a.named_child(0))
                .filter(|first| first.kind() == "identifier")
                .and_then(|first| resolve_symbol(first, ctx));
            collect_call_like(right, ctx, &field_fqn, &class_fqn);
            if let (Some(e), Some(last)) = (entity, ctx.facts.call_sites.last_mut()) {
                last.entity = Some(e);
            }
            true
        }
        _ => false,
    }
}

/// A call inside a parameter **default** -> a call site, with the referenced thing resolved into an FQN in `entity`.
///
/// "Framework calls written in the signature" such as `def handler(db = Depends(get_db))` never appear inside a
/// method body, so ordinary traversal cannot catch them. The language layer does only two neutral things: ① this is
/// a call site; ② which name its first argument points at (resolved into an FQN via this file's imports / the same
/// module). What `Depends` means — "dependency injection" — is left entirely to FKB (see fkb/python/fastapi.yaml).
fn collect_default_calls(node: Node, ctx: &mut Ctx, owner_fqn: &str) {
    let Some(params) = node.child_by_field_name("parameters") else { return };
    let owner_class = ctx.class_or_module();
    let mut cursor = params.walk();
    for p in params.named_children(&mut cursor) {
        let Some(default) = p.child_by_field_name("value") else { continue };
        if default.kind() != "call" {
            continue;
        }
        // Compute entity first (immutable borrow of ctx), then build the call site (exclusive borrow), avoiding a borrow conflict.
        let entity = default
            .child_by_field_name("arguments")
            .and_then(|a| a.named_child(0))
            .and_then(|a| resolve_symbol(a, ctx));
        let before = ctx.facts.call_sites.len();
        collect_call_like(default, ctx, owner_fqn, &owner_class);
        if let (Some(e), true) = (entity, ctx.facts.call_sites.len() > before) {
            if let Some(last) = ctx.facts.call_sites.last_mut() {
                last.entity = Some(e);
            }
        }
    }
}

/// Resolve a "referenced name" into a fully qualified name.
///
/// * `from app.dependencies import get_db` -> `app.dependencies.get_db`
/// * a bare name that was not imported: it can only be this module or a builtin, so it is composed from the
///   **current module** (`{module}.{name}`)
fn resolve_symbol(node: Node, ctx: &Ctx) -> Option<String> {
    match node.kind() {
        "identifier" => {
            let name = text(node, ctx.src)?;
            Some(
                ctx.imports
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| format!("{}.{}", ctx.module, name)),
            )
        }
        "attribute" => {
            let obj = node.child_by_field_name("object")?;
            let attr = opt_text(node.child_by_field_name("attribute"), ctx.src)?;
            match resolve_symbol(obj, ctx) {
                Some(obj_fqn) => Some(format!("{}.{}", obj_fqn, attr)),
                None => text(node, ctx.src).map(|s| s.to_string()),
            }
        }
        _ => None,
    }
}

/// Parameter list -> `[{name, type}]` (`null` when the type is missing).
fn params_of(node: Node, src: &[u8]) -> Vec<serde_json::Value> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for p in params.named_children(&mut cursor) {
        let pname = opt_text(p.child_by_field_name("name"), src)
            .or_else(|| opt_text(p.named_child(0), src));
        let Some(pname) = pname else { continue };
        let ty = opt_text(p.child_by_field_name("type"), src);
        out.push(json!({ "name": pname, "type": ty }));
    }
    out
}

/// A decorated definition: decorators -> **call sites** (owner = the decorated thing).
///
/// Same route as Java annotations: a decorator's `owner_fqn` is the decorated function's FQN, and its
/// `owner_class` is the owning class (or the module name for a module-level function).
fn collect_decorated(node: Node, ctx: &mut Ctx) {
    let mut def: Option<Node> = None;
    let mut decorators: Vec<Node> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "decorator" => decorators.push(child),
            _ => def = Some(child),
        }
    }
    let Some(def) = def else { return };
    // The class / module a decorator attaches to: the class when the definition is inside a class body, otherwise the module.
    let owner_class = ctx.class_or_module();
    let target = match def.kind() {
        "function_definition" => collect_function(def, ctx),
        "class_definition" => collect_class(def, ctx),
        _ => {
            walk(def, ctx);
            None
        }
    };
    let Some(target_fqn) = target else { return };
    for d in decorators {
        collect_decorator(d, ctx, &target_fqn, &owner_class);
    }
}

fn collect_decorator(node: Node, ctx: &mut Ctx, target_fqn: &str, owner_class: &str) {
    // The first named child of `decorator` is the expression after `@`.
    let Some(expr) = node.named_child(0) else { return };
    collect_call_like(expr, ctx, target_fqn, owner_class);
    // Decorator arguments may contain further calls (`@app.get("/x", dependencies=[Depends(auth)])`)
    recurse(expr, ctx);
}

// ------------------------------------------------------------------ call sites

/// An ordinary call `f(x)` / `obj.m(x)` -> a call site.
fn collect_call(node: Node, ctx: &mut Ctx) {
    let owner_fqn = ctx.owner_fqn.clone();
    let owner_class = ctx.class_or_module();
    collect_call_like(node, ctx, &owner_fqn, &owner_class);
}

/// Record a "call-shaped expression" as a call site.
///
/// Serves both **decorators** (`@app.get("/x")`) and **ordinary calls** (`redis.get(k)`) — in FKB's eyes both
/// are `kind: call`, distinguished by the `callee` selector.
fn collect_call_like(node: Node, ctx: &mut Ctx, owner_fqn: &str, owner_class: &str) {
    let func = if node.kind() == "call" {
        // `@app.get("/x")`: it is itself a call, take the function field.
        node.child_by_field_name("function")
    } else {
        // A decorator that is not called (`@app.deprecated`): the node itself is the callee.
        Some(node)
    };
    let Some(func) = func else { return };
    let (callee_text, receiver, method) = resolve_callee(func, ctx.src);
    if callee_text.is_empty() {
        return;
    }
    let view_entity = if matches!(callee_text.as_str(), "path" | "re_path" | "url") {
        if let Some(args) = node.child_by_field_name("arguments") {
            match args.named_child(1) {
                Some(v) if matches!(v.kind(), "identifier" | "attribute") => {
                    resolve_symbol(v, ctx)
                }
                Some(v) if v.kind() == "call" => {
                    let func = v.child_by_field_name("function");
                    let is_as_view = func
                        .and_then(|f| opt_text(f.child_by_field_name("attribute"), ctx.src))
                        .as_deref()
                        == Some("as_view");
                    if is_as_view {
                        func.and_then(|f| f.child_by_field_name("object"))
                            .and_then(|o| resolve_symbol(o, ctx))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    let args = node
        .child_by_field_name("arguments")
        .map(|a| positional_args(a, ctx.src))
        .unwrap_or_default();
    ctx.facts.call_sites.push(CallSiteFact {
        owner_fqn: owner_fqn.to_string(),
        owner_class: Some(owner_class.to_string()),
        callee_text,
        receiver,
        method,
        args,
        span: span_of(node),
        snippet: snippet_of(node, ctx.src),
        db_table: None,
        in_loop: ctx.loop_depth > 0,
        entity: view_entity,
    });
}

/// Callee -> `(callee_text, receiver, method)`.
///
/// The receiver of `os.environ.get(...)` is `os.environ` (not `os`) — keeping the full attribute chain is what
/// lets FKB hit it precisely with a dotted scope such as `os.environ::get`.
fn resolve_callee(node: Node, src: &[u8]) -> (String, Option<String>, Option<String>) {
    if node.kind() == "attribute" {
        let obj = opt_text(node.child_by_field_name("object"), src);
        let attr = opt_text(node.child_by_field_name("attribute"), src);
        return match (obj, attr) {
            (Some(o), Some(a)) => (format!("{}.{}", o, a), Some(o), Some(a)),
            (Some(o), None) => (o.clone(), Some(o), None),
            (None, Some(a)) => (a.clone(), None, Some(a)),
            (None, None) => (String::new(), None, None),
        };
    }
    let t = text(node, src).unwrap_or_default();
    (t.clone(), None, Some(t).filter(|s| !s.is_empty()))
}

/// Capture literals from the argument list **by position**.
///
/// Same meaning as Java's `positional_args`: the i-th argument yields its value when it is a literal, otherwise the
/// placeholder `Unknown`. Non-literals never carry their raw text — so that if FKB forgets `require_literal`, a
/// Python expression cannot be mistaken for a semantic identity.
fn positional_args(node: Node, src: &[u8]) -> Vec<FactValue> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        out.push(match child.kind() {
            "keyword_argument" => {
                match (
                    opt_text(child.child_by_field_name("name"), src),
                    child.child_by_field_name("value").map(|v| literal_value(v, src)),
                ) {
                    (Some(k), Some(v)) => FactValue::Array(vec![(k, v)]),
                    _ => FactValue::Unknown(None),
                }
            }
            _ => literal_value(child, src),
        });
    }
    out
}

/// A single expression node -> a literal fact; anything non-literal becomes `Unknown(None)` (without the source
/// text — so that if FKB forgets `require_literal`, a Python expression cannot be taken for a semantic identity).
fn literal_value(node: Node, src: &[u8]) -> FactValue {
    match node.kind() {
        "string" => match string_value(node, src) {
            Some(s) => FactValue::String(s),
            None => FactValue::Unknown(None),
        },
        "integer" | "float" => match text(node, src) {
            Some(s) => FactValue::String(s),
            None => FactValue::Unknown(None),
        },
        "true" => FactValue::Bool(true),
        "false" => FactValue::Bool(false),
        "none" => FactValue::Null,
        "list" | "tuple" => {
            let mut items = Vec::new();
            let mut cursor = node.walk();
            for (i, child) in node.named_children(&mut cursor).enumerate() {
                items.push((i.to_string(), literal_value(child, src)));
            }
            FactValue::Array(items)
        }
        "identifier" | "attribute" => match text(node, src) {
            Some(s) => FactValue::Unknown(Some(s)),
            None => FactValue::Unknown(None),
        },
        _ => FactValue::Unknown(None),
    }
}

// ------------------------------------------------------------------ imports

fn collect_import(node: Node, ctx: &mut Ctx) {
    if node.kind() == "import_statement" {
        // `import os.path` / `import numpy as np`
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            push_import(child, None, ctx);
        }
        return;
    }
    // `from app.core.config import settings`
    let module_node = node.child_by_field_name("module_name");
    let module = module_node
        .and_then(|n| text(n, ctx.src))
        .map(|m| resolve_relative_module(&m, &ctx.module));
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if Some(child.id()) == module_node.map(|n| n.id()) {
            continue;
        }
        push_import(child, module.as_deref(), ctx);
    }
}

/// Record a `name` / `aliased_import` as an import fact.
///
/// `from x.y import z` is recorded as `x.y.z`: only then can the symbol short name `z` be restored into a fully
/// qualified name in P3 — same origin as PHP's `use think\facade\Db` restoring to `Db -> think\facade\Db`.
fn push_import(node: Node, from_module: Option<&str>, ctx: &mut Ctx) {
    let span = span_of(node);
    let (name, alias) = match node.kind() {
        "dotted_name" | "identifier" => (text(node, ctx.src), None),
        "aliased_import" => (
            opt_text(node.child_by_field_name("name"), ctx.src),
            opt_text(node.child_by_field_name("alias"), ctx.src),
        ),
        _ => (None, None),
    };
    let Some(name) = name else { return };
    let name = match from_module {
        Some(m) if !m.is_empty() => format!("{}.{}", m, name),
        _ => name,
    };
    let bound = alias
        .clone()
        .unwrap_or_else(|| name.rsplit('.').next().unwrap_or(&name).to_string());
    ctx.imports.entry(bound).or_insert(name.clone());
    ctx.facts.imports.push(ImportFact { alias, name, span });
}

// ------------------------------------------------------------------ loops

fn collect_loop(node: Node, ctx: &mut Ctx) {
    let bodies: HashSet<usize> = field_children(node, "body").iter().map(|n| n.id()).collect();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if bodies.contains(&child.id()) {
            ctx.loop_depth += 1;
            walk(child, ctx);
            ctx.loop_depth -= 1;
        } else {
            walk(child, ctx);
        }
    }
}

// ------------------------------------------------------------------ helpers

/// File path -> dotted module name.
///
/// `app/api/users.py` -> `app.api.users`; `app/__init__.py` -> `app` (the package directory itself).
fn module_fqn(path: &str) -> String {
    let p = path.replace('\\', "/");
    let p = p.strip_suffix(".pyi").or_else(|| p.strip_suffix(".py")).unwrap_or(&p);
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let mut segs = segs;
    if segs.last() == Some(&"__init__") {
        segs.pop();
    }
    segs.join(".")
}

/// Strip the quotes from a Python string literal (handles prefixes and triple quotes).
fn string_value(node: Node, src: &[u8]) -> Option<String> {
    let raw = text(node, src)?;
    Some(unquote(&raw))
}

/// Resolve the module name of a `from ... import` into an absolute dotted module name.
///
/// Python's relative imports use leading dots for the level: `from .views import x` is relative to the current
/// module's parent package. For the current module `myapp.urls` the parent package is `myapp`, so `.views` ->
/// `myapp.views`; `..sub` goes up one more level. `leading_dots == 0` means an absolute import and is returned
/// unchanged (never wrongly composed with a parent package).
fn resolve_relative_module(raw: &str, current_module: &str) -> String {
    let leading_dots = raw.chars().take_while(|c| *c == '.').count();
    if leading_dots == 0 {
        return raw.to_string();
    }
    let rest = &raw[leading_dots..];
    let cur_segs: Vec<&str> = current_module.split('.').collect();
    // The current module's "parent package" = drop the last segment (the file itself). `myapp.urls` -> parent ["myapp"].
    let pkg_len = cur_segs.len().saturating_sub(1);
    // Align the relative level with the parent-package level: `..` goes up one more.
    let base_len = pkg_len.saturating_sub(leading_dots - 1);
    let mut segs: Vec<&str> = cur_segs[..base_len].to_vec();
    if !rest.is_empty() {
        segs.extend(rest.split('.'));
    }
    segs.join(".")
}

fn unquote(raw: &str) -> String {
    let s = raw.trim();
    // Strip at most two prefix letters: `r"..."` / `rb"..."` / `f'...'`
    let body = s
        .trim_start_matches(|c| matches!(c, 'r' | 'R' | 'b' | 'B' | 'f' | 'F' | 'u' | 'U'));
    for q in ["\"\"\"", "'''", "\"", "'"] {
        if body.len() >= q.len() * 2 && body.starts_with(q) && body.ends_with(q) {
            return body[q.len()..body.len() - q.len()].to_string();
        }
    }
    body.trim_matches(|c| c == '"' || c == '\'').to_string()
}

/// The source text of the **line** a call site sits on, so the UI can show the call statement directly (same as the PHP side).
fn snippet_of(node: Node, src: &[u8]) -> Option<String> {
    const MAX: usize = 160;
    let start = node.start_byte().min(src.len());
    let line_start = src[..start].iter().rposition(|&b| b == b'\n').map(|i| i + 1).unwrap_or(0);
    let rest = std::str::from_utf8(src.get(line_start..).unwrap_or_default()).unwrap_or("");
    let line = rest.lines().next().unwrap_or("").trim();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_src(path: &str, src: &str) -> SyntaxFacts {
        PythonParser::new().unwrap().parse(path, src).unwrap()
    }

    /// Look up a call site by its raw callee (method names repeat within one source file, so only the callee can locate it precisely).
    fn call_of<'a>(facts: &'a SyntaxFacts, callee: &str) -> &'a CallSiteFact {
        facts
            .call_sites
            .iter()
            .find(|c| c.callee_text == callee)
            .unwrap_or_else(|| {
                panic!(
                    "no call site {callee} seen, actually: {:?}",
                    facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>()
                )
            })
    }

    /// Decorators land as "a call site on the decorated thing" — the Python version of "an annotation is a call site".
    ///
    /// This is the precondition for FastAPI routes being matched by FKB: the owner must be the **handler function**
    /// itself, and owner_class lands on the module (Python functions are often not in a class; see
    /// [`Ctx::class_or_module`]).
    #[test]
    fn decorators_become_call_sites_of_decorated_definition() {
        let facts = parse_src(
            "app/api/users.py",
            r#"@router.get("/users")
def list_users():
    pass

@app.delete("/orders/{order_id}", tags=["admin"])
def remove_order():
    pass
"#,
        );
        let get = call_of(&facts, "router.get");
        assert_eq!(get.owner_fqn, "app.api.users.list_users");
        assert_eq!(get.owner_class.as_deref(), Some("app.api.users"));
        assert_eq!(get.receiver.as_deref(), Some("router"));
        assert_eq!(get.method.as_deref(), Some("get"));
        // Positional semantics: the first argument (the path literal) must be in arg0
        assert_eq!(get.args.first().and_then(|a| a.as_str()), Some("/users"));

        let del = call_of(&facts, "app.delete");
        assert_eq!(del.owner_fqn, "app.api.users.remove_order");
        assert_eq!(del.args.first().and_then(|a| a.as_str()), Some("/orders/{order_id}"));
        assert_eq!(del.args.len(), 2);
        match del.args.get(1) {
            Some(FactValue::Array(items)) => {
                assert_eq!(items[0].0, "tags");
                // A list-literal value -> captured as a nested array keyed by index (see
                // `list_literal_is_captured_with_index_keys`）
                assert!(matches!(items[0].1, FactValue::Array(_)));
            }
            other => panic!("keyword arg should be captured as Array, got: {other:?}"),
        }
    }

    /// Class declaration and inheritance facts: only the **positional arguments** in `superclasses` are base
    /// classes; keyword forms such as `metaclass=...` must be skipped.
    #[test]
    fn class_declaration_records_inheritance_and_methods() {
        let facts = parse_src(
            "app/models/user.py",
            r#"class UserModel(Base, metaclass=Meta):
    def save(self):
        pass
"#,
        );
        let class = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::CLASS)
            .expect("expected a class declaration");
        assert_eq!(class.fqn, "app.models.user.UserModel");
        let method = facts
            .declarations
            .iter()
            .find(|d| d.kind.as_str() == NodeKind::METHOD)
            .expect("a function inside a class should be recorded as a method");
        assert_eq!(method.fqn, "app.models.user.UserModel.save");

        let bases: Vec<&str> = facts.inheritances.iter().map(|i| i.base_name.as_str()).collect();
        assert_eq!(bases, vec!["Base"]);
        assert_eq!(facts.inheritances[0].kind.as_str(), EdgeKind::EXTENDS);
    }

    /// Parameters with type annotations: `svc: OrderService` is the basis for later resolving the `svc.delete()` call chain.
    #[test]
    fn typed_parameters_are_recorded() {
        let facts = parse_src(
            "app/api/orders.py",
            "def remove_order(order_id: int, svc: OrderService):\n    svc.delete(order_id)\n",
        );
        let d = facts
            .declarations
            .iter()
            .find(|d| d.fqn == "app.api.orders.remove_order")
            .expect("expected the function to be registered");
        let params = d.extra.get("parameters").and_then(|v| v.as_array()).expect("expected a parameter table");
        let got: Vec<(&str, &str)> = params
            .iter()
            .map(|p| {
                (
                    p.get("name").and_then(|v| v.as_str()).unwrap(),
                    p.get("type").and_then(|v| v.as_str()).unwrap(),
                )
            })
            .collect();
        assert_eq!(got, vec![("order_id", "int"), ("svc", "OrderService")]);
    }

    /// Only calls inside the loop **body** count as in_loop: a call in the iterable does not run per record, and
    /// marking it would misreport "queried once before the loop" as N+1 (same trade-off as the PHP side).
    #[test]
    fn loop_body_marks_call_sites() {
        let facts = parse_src(
            "app/services/sync.py",
            r#"def run(items):
    total = count_all()
    for u in fetch_page():
        db.query(u)
    while True:
        db.flush()
"#,
        );
        assert!(!call_of(&facts, "fetch_page").in_loop, "a call inside an iterator is not in a loop body");
        assert!(call_of(&facts, "db.query").in_loop);
        assert!(call_of(&facts, "db.flush").in_loop);
        assert!(!call_of(&facts, "count_all").in_loop);
    }

    /// `from x.y import z` is recorded as `x.y.z` (for short-name restoration); `module_name` itself is not an import item.
    #[test]
    fn from_import_records_qualified_symbol() {
        let facts = parse_src("app/main.py", "from fastapi import FastAPI, APIRouter\nimport numpy as np\n");
        let names: Vec<&str> = facts.imports.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["fastapi.FastAPI", "fastapi.APIRouter", "numpy"]);
        assert_eq!(facts.imports.last().unwrap().alias.as_deref(), Some("np"));
    }

    /// A literal assignment in a class body is registered as a Property — the precondition for SQLAlchemy / Django
    /// table mapping being recognised.
    ///
    /// Note this contains **no** ORM knowledge: the parser only knows "class attribute = literal"; reading
    /// `__tablename__` as a table name is left entirely to FKB (see fkb/python/sqlalchemy.yaml).
    #[test]
    fn class_body_literal_assignment_becomes_property() {
        let facts = parse_src(
            "app/models/user.py",
            r#"class UserModel(Base):
    __tablename__ = "users"
    cache_ttl = 600

    def save(self):
        total = 5
        self.x = 1
"#,
        );
        let props: Vec<_> = facts
            .declarations
            .iter()
            .filter(|d| d.kind.as_str() == NodeKind::PROPERTY)
            .collect();
        let names: Vec<&str> = props.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["__tablename__", "cache_ttl"]);

        let table = props[0];
        assert_eq!(table.fqn, "app.models.user.UserModel.__tablename__");
        assert_eq!(table.parent_fqn.as_deref(), Some("app.models.user.UserModel"));
        let value: FactValue =
            serde_json::from_value(table.extra["default"].clone()).expect("should be able to deserialize the property value");
        assert_eq!(value, FactValue::String("users".to_string()));
    }

    /// A "field declaration" inside a class body (`name = models.CharField(...)`) is recorded as a call site, with
    /// `owner` pinned to `class.field` and `owner_class` falling back to the model class:
    /// FKB then takes `owner_class.owner_member` as the column identity — the same mechanism as the JS field decorator / Java field annotation.
    ///
    /// Also: when the first positional argument of a relation field (`author = models.ForeignKey(User, ...)`) is a class-name identifier,
    /// it is resolved via this file's imports into an FQN and recorded under `entity`, so `References` can link to the target model class.
    #[test]
    fn class_body_field_declaration_becomes_call_site_with_field_name() {
        let facts = parse_src(
            "app/models/blog.py",
            r#"from app.models.user import User

class Post(models.Model):
    title = models.CharField(max_length=200)
    author = models.ForeignKey(User, on_delete=models.CASCADE)
    tags = models.ManyToManyField("Tag")
"#,
        );
        let calls: Vec<&CallSiteFact> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text == "models.CharField" || c.callee_text == "models.ForeignKey")
            .collect();
        assert_eq!(calls.len(), 2, "there should be no duplicate call sites");

        let char = calls
            .iter()
            .find(|c| c.callee_text == "models.CharField")
            .expect("expected a CharField call site");
        assert_eq!(char.owner_fqn, "app.models.blog.Post.title");
        assert_eq!(char.owner_class.as_deref(), Some("app.models.blog.Post"));
        assert_eq!(char.entity, None);

        let fk = calls
            .iter()
            .find(|c| c.callee_text == "models.ForeignKey")
            .expect("expected a ForeignKey call site");
        assert_eq!(fk.owner_fqn, "app.models.blog.Post.author");
        assert_eq!(
            fk.entity.as_deref(),
            Some("app.models.user.User"),
            "the foreign-key target should resolve to an FQN via import"
        );
    }

    /// Keyword arguments are addressable by name: explicit queue / explicit task name — the semantics "written in `k=v`"
    /// would be lost without this step. Positional semantics is kept too — the argument index stays in source order.
    #[test]
    fn keyword_arguments_are_captured_by_name() {
        let facts = parse_src(
            "app/tasks.py",
            "def run():\n    send.apply_async(args=[1], queue=\"payments\", countdown=10)\n",
        );
        let call = call_of(&facts, "send.apply_async");
        assert_eq!(call.args.len(), 3, "the three keyword args each take one position");
        let (key, value) = match call.args.get(1) {
            Some(FactValue::Array(items)) => (items[0].0.clone(), items[0].1.clone()),
            other => panic!("index 1 should be an Array, got: {other:?}"),
        };
        assert_eq!(key, "queue");
        assert_eq!(value, FactValue::String("payments".to_string()));
        assert!(matches!(
            call.args.get(0),
            Some(FactValue::Array(items)) if matches!(items[0].1, FactValue::Array(_))
        ));
    }

    /// A list literal is captured keyed by **index** — Flask's `methods=["POST"]` is decoded through it.
    #[test]
    fn list_literal_is_captured_with_index_keys() {
        let facts = parse_src(
            "app.py",
            "@app.route(\"/orders\", methods=[\"POST\", \"PUT\"])\ndef create_order():\n    pass\n",
        );
        let call = call_of(&facts, "app.route");
        let methods = match call.args.get(1) {
            Some(FactValue::Array(items)) => {
                items.iter().find(|(k, _)| k == "methods").map(|(_, v)| v.clone())
            }
            _ => None,
        };
        let Some(FactValue::Array(inner)) = methods else {
            panic!("methods should be an array, got: {:?}", call.args)
        };
        assert_eq!(inner[0], ("0".to_string(), FactValue::String("POST".to_string())));
        assert_eq!(inner[1], ("1".to_string(), FactValue::String("PUT".to_string())));
    }

    /// Relative imports `from .x import y` must be restored to absolute module names, otherwise the FQN `y` resolves to is
    /// wrong, and Django routes / Flask blueprint cross-file references would fail to connect.
    #[test]
    fn relative_import_resolves_to_absolute_module() {
        let facts = parse_src(
            "app/urls.py",
            "from .views import user_list\nfrom ..core import helper\nfrom . import sibling\n",
        );
        let has = |want: &str| facts.imports.iter().any(|imp| imp.name == want);
        assert!(has("app.views.user_list"), "got: {:?}", facts.imports);
        assert!(has("core.helper"));
        assert!(has("app.sibling"));
    }

    /// A call in a parameter **default** (`db = Depends(get_db)`) -> call site + `entity`.
    ///
    /// The parser does not know what `Depends` means; it only does two neutral things: this is a call site, and what
    /// FQN the referenced name resolves to — semantics is handed to FKB (see fkb/python/fastapi.yaml).
    #[test]
    fn parameter_default_call_is_captured_with_entity() {
        let facts = parse_src(
            "app/api/users.py",
            r#"from app.dependencies import get_db

def get_user(user_id: int, db=Depends(get_db)):
    pass
"#,
        );
        let call = call_of(&facts, "Depends");
        assert_eq!(call.owner_fqn, "app.api.users.get_user");
        assert_eq!(call.owner_class.as_deref(), Some("app.api.users"));
        assert_eq!(call.entity.as_deref(), Some("app.dependencies.get_db"));

        let same = parse_src("app/api/orders.py", "def make(x=Depends(local_dep)):\n    pass\n");
        assert_eq!(
            call_of(&same, "Depends").entity.as_deref(),
            Some("app.api.orders.local_dep")
        );
    }

    #[test]
    fn module_fqn_derives_from_path() {
        assert_eq!(module_fqn("app/api/users.py"), "app.api.users");
        assert_eq!(module_fqn("app/api/__init__.py"), "app.api");
        assert_eq!(module_fqn("main.py"), "main");
    }

    /// `.pyi` stubs and Windows separators both occur in real trees; the `.pyi` suffix must win over `.py`
    /// (otherwise `models.pyi` would keep its extension in the FQN).
    #[test]
    fn module_fqn_handles_pyi_stubs_and_windows_separators() {
        assert_eq!(module_fqn("app\\api\\users.py"), "app.api.users");
        assert_eq!(module_fqn("app/models.pyi"), "app.models");
        assert_eq!(module_fqn("app//api/users.py"), "app.api.users", "repeated separators must not produce empty segments");
    }

    /// Strip the type-parameter shell so a generic base (`Base[Order]`) resolves to the same node as the bare
    /// `Base` — otherwise inheritance would never match.
    #[test]
    fn bare_typename_strips_generic_and_subscript_shells() {
        assert_eq!(bare_type_name("Base[Order]".to_string()), "Base");
        assert_eq!(bare_type_name("Base<Order>".to_string()), "Base");
        assert_eq!(bare_type_name("Base".to_string()), "Base");
        assert_eq!(bare_type_name("  List [int] ".to_string()), "List", "must strip generics and trim");
    }

    /// String prefixes (`r` / `rb` / `f` / `u`) and triple quotes are all common in decorators and defaults;
    /// the raw body is what becomes identity, so the quoting must come off cleanly.
    #[test]
    fn unquote_handles_prefixes_and_triple_quotes() {
        assert_eq!(unquote("'abc'"), "abc");
        assert_eq!(unquote("\"abc\""), "abc");
        assert_eq!(unquote("r\"a\\tb\""), "a\\tb", "the prefix letters must be stripped");
        assert_eq!(unquote("rb\"x\""), "x");
        assert_eq!(unquote("f\"{x}\""), "{x}");
        assert_eq!(unquote("\"\"\"doc\"\"\""), "doc", "triple quotes are stripped by length, not trimmed character by character");
        assert_eq!(unquote("'''doc'''"), "doc");
        assert_eq!(unquote("abc"), "abc", "with no quotes it is returned verbatim");
    }

    /// An absolute import must be returned **unchanged** — composing it with a parent package would produce a
    /// module that does not exist. Levels beyond the root are clamped rather than panicking.
    #[test]
    fn resolve_relative_module_keeps_absolute_and_clamps_level() {
        assert_eq!(
            resolve_relative_module("os.path", "app.urls"),
            "os.path",
            "an absolute import must not be joined with the parent package"
        );
        assert_eq!(resolve_relative_module(".views", "app.urls"), "app.views");
        assert_eq!(resolve_relative_module("..core.helper", "a.b.urls"), "a.core.helper");
        assert_eq!(resolve_relative_module(".", "app.urls"), "app", "empty rest → the parent package itself");
        assert_eq!(
            resolve_relative_module("...x", "a.b"),
            "x",
            "walking past the root must be clamped instead of panicking"
        );
    }
}
