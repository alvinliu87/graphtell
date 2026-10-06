//! PHP parser: translate a tree-sitter PHP syntax tree into the language-agnostic [`SyntaxFacts`].

pub mod value;

use std::cell::RefCell;
use std::collections::HashSet;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    CallSiteFact, ConfigEntryFact, Declaration, EdgeKind, FactValue, FieldTypeFact, ImportFact,
    InheritanceFact, Language, NodeKind, SyntaxFacts,
};
use gt_domain::model::syntax::{HeaderAssignFact, SignCompareFact, VariableAssignFact};
use serde_json::json;
use tree_sitter::{Language as TsLanguage, Node, Parser};
use crate::ts_util::{field_children, span_of, text_owned as text};
use value::eval_expr;

use gt_domain::port::LanguageParser;

/// The PHP parser adapter.
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

/// PHP's primitive / builtin type names — PHP vocabulary, so it lives in the PHP adapter, not in the
/// language-neutral core (`gt-domain`). `int` / `string` / `array` … must not be treated as class
/// references during type inference.
const PHP_BUILTIN_TYPES: &[&str] = &[
    "int", "integer", "string", "bool", "boolean", "float", "double", "array", "void", "mixed",
    "object", "callable", "iterable", "null", "false", "true", "self", "static", "parent", "never",
];

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

    fn variable_prefixes(&self) -> &'static [&'static str] {
        // `$var` / `{$var}` / `"${var}"` — all three contain `$`.
        &["$"]
    }

    fn manifest_files(&self) -> &'static [&'static str] {
        &["composer.json"]
    }

    fn exclude_dirs(&self) -> &'static [&'static str] {
        &["vendor"]
    }

    fn builtin_types(&self) -> &'static [&'static str] {
        PHP_BUILTIN_TYPES
    }

    fn parse(&self, path: &str, source: &str) -> Result<SyntaxFacts> {
        let tree = PARSER.with(|cell| {
            let mut borrow = cell.borrow_mut();
            let parser = borrow.get_or_insert_with(|| {
                let mut p = Parser::new();
                // The language was validated at construction; failing here is a programming error
                p.set_language(&self.language).expect("php language");
                p
            });
            parser.parse(source, None)
        });
        let tree = tree.ok_or_else(|| {
            DomainError::Parse { file: path.into(), reason: "tree-sitter returned None".into() }
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

        // Namespace (take the first namespace_definition)
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
    /// The FQN stack of the class / interface / trait currently being entered.
    class_stack: Vec<String>,
    /// How many levels of `for` / `foreach` / `while` / `do-while` **loop body** we are currently nested in.
    /// Written into [`CallSiteFact::in_loop`] when collecting call sites — a loop is the only control-flow concept
    /// the graph does not model, and N+1 detection depends entirely on this. A depth rather than a boolean: after an
    /// inner loop body exits, the remaining statements of the outer one still count as "inside a loop".
    loop_depth: u32,
}

/// The root object of a chained call: `a()->b()->c()` returns `a`.
///
/// Only **call** expressions are pierced, not ordinary property access
/// (the root of `$this->dao->getList` is `$this->dao`, not `$this`).
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

/// Walk back along the object chain to find the "target table name": in `Db::name('goods')->where()->insert()`,
/// the object's object … is `Db::name('goods')` — a `name('goods')` call whose first argument is the string
/// literal `goods`. On a hit it is returned so P7 can turn the terminal verb into `WritesDb` / `ReadsDb`.
///
/// Only the two table-name verbs `name` / `table` are recognised (things like `Route::name` are route naming and
/// not in this list, but since we only use the value when "the method itself is a read / write verb", it cannot be
/// misapplied).
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
        // Keep walking upstream along the chain (spellings like `$q->name('x')->find()`)
        "member_call_expression" | "nullsafe_member_call_expression" | "function_call_expression" => {
            db_table_of(object, ctx)
        }
        _ => None,
    }
}

fn find_child_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.named_children(&mut node.walk()).find(|c| c.kind() == kind)
}

/// Whether the current collection position is inside a loop body (for [`CallSiteFact::in_loop`]).
fn in_loop_of(ctx: &Ctx) -> bool {
    ctx.loop_depth > 0
}

fn walk_program(root: Node, ctx: &mut Ctx) {
    // When a namespace has a body, the real declarations live inside that body
    let entry = if let Some(ns) = find_child_kind(root, "namespace_definition") {
        ns.child_by_field_name("body").unwrap_or(root)
    } else {
        root
    };
    walk_scope(entry, ctx, None);
}

fn walk_scope(node: Node, ctx: &mut Ctx, owner_fqn: Option<&str>) {
    // Ownership of top-level / namespace-level statements: with no method they attach to "<file>" (landing on the File node)
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
            // Config file: `return [...];`
            "return_statement" => {
                collect_config_return(child, ctx);
                collect_call_sites(child, ctx, &top);
            }
            "namespace_definition" => walk_scope(child, ctx, owner_fqn),
            // Every other statement (`Route::get(...)` in a route file, etc.) collects call sites uniformly
            _ => collect_call_sites(child, ctx, &top),
        }
    }
}

fn collect_imports(node: Node, ctx: &mut Ctx) {
    // The body may be a namespace_use_group (use A\{B, C};)
    //
    // The group's **prefix is a sibling of the group**, a direct child of the `use` declaration
    // (`use App\{Repo\User, ...}` -> `namespace_name "App"` next to `namespace_use_group`), not nested
    // inside it. Reading it from the group's children found nothing, so every group import lost its
    // prefix and gained a stray leading separator (`\Repo\UserRepo` instead of `App\Repo\UserRepo`) —
    // an FQN that matches no node, silently breaking short-name resolution for those classes.
    let mut pc = node.walk();
    let prefix = node
        .named_children(&mut pc)
        .find(|c| c.kind() == "namespace_name")
        .map(|c| trim_leading(text(c, ctx.src)))
        .unwrap_or_default();

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                if let Some(imp) = import_from_clause(child, ctx) {
                    ctx.facts.imports.push(imp);
                }
            }
            "namespace_use_group" => {
                let mut g = child.walk();
                for c in child.named_children(&mut g) {
                    if c.kind() == "namespace_use_clause" {
                        if let Some(mut imp) = import_from_clause(c, ctx) {
                            if !prefix.is_empty() {
                                imp.name = format!("{}\\{}", prefix, imp.name);
                            }
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

    // Inheritance / implementation
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

/// Extract method names from `@method <name>(...)` in the phpdoc **preceding** a class declaration.
///
/// Only the name is taken: the return type and parameters are useless for parsing (what really matters is
/// forwarding the call to the delegation property declared by FKB), and a return type may contain `|` `?` `\`,
/// so greedy matching would easily mistake `array|Model|null` for a method name.
fn docblock_method_names(node: Node, src: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = node.prev_sibling();
    // Only look back at the immediately adjacent comment / whitespace nodes (things like `use_trait` are never in between).
    for _ in 0..4 {
        let Some(n) = cur else { break };
        if n.kind() == "comment" {
            for line in text(n, src).lines() {
                let line = line.trim().trim_start_matches('*').trim_start_matches('/').trim();
                let Some(rest) = line.strip_prefix("@method ") else {
                    continue;
                };
                // `array|Model|null get($id, ...)` -> take the last identifier before `(`
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

/// Symfony route attribute (`#[Route]` / `#[Get]` …) extraction.
///
/// Modern Symfony declares routes with PHP 8 attributes:
/// ```php
/// #[Route('/api/users', methods: ['GET'])]
/// #[Get('/api/users')]
/// public function list() {}
/// ```
/// Returns `(attribute name, path, method list)` for each route; when `Route` declares no `methods` the list is
/// empty (treated as unrestricted -> the caller degrades it to the wildcard `ANY`). Non-route attributes
/// (e.g. `#[ORM\Entity]`) are ignored outright.
fn route_attributes_of(method: Node, src: &str) -> Vec<(String, String, Vec<String>)> {
    let mut out = Vec::new();
    // attribute_list / attribute_group / attribute on the method node (possibly nested one level)
    let mut attr_nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<Node> = method.named_children(&mut method.walk()).collect();
    while let Some(node) = stack.pop() {
        match node.kind() {
            "attribute" => attr_nodes.push(node),
            "attribute_list" | "attribute_group" => {
                let mut c2 = node.walk();
                stack.extend(node.named_children(&mut c2));
            }
            _ => {}
        }
    }
    for attr in attr_nodes {
        // Find the name by **kind** (an `attribute`'s `name` is not a named field, so `child_by_field_name` cannot get it)
        let name = attr
            .named_children(&mut attr.walk())
            .find(|c| c.kind() == "name")
            .map(|n| text(n, src))
            .unwrap_or_default();
        let is_route = matches!(
            name.as_str(),
            "Route" | "Get" | "Post" | "Put" | "Delete" | "Patch" | "Options" | "Head"
        );
        if !is_route {
            continue;
        }
        let mut path = String::new();
        let mut methods: Vec<String> = Vec::new();
        if let Some(args) = attr
            .named_children(&mut attr.walk())
            .find(|c| c.kind() == "arguments")
        {
            let mut c = args.walk();
            for arg in args.named_children(&mut c) {
                if arg.kind() != "argument" {
                    continue;
                }
                let named = arg
                    .named_children(&mut arg.walk())
                    .find(|c| c.kind() == "name")
                    .map(|n| text(n, src));
                if named.is_none() {
                    // Positional argument = the path
                    if path.is_empty() {
                        path = find_string_content(arg, src).unwrap_or_default();
                    }
                } else if named.as_deref() == Some("methods") {
                    methods = collect_array_strings(arg, src);
                }
            }
        }
        // Shortcut attributes (Get/Post…) imply the method; `Route` without methods is left to the caller as `ANY`
        let methods = if name != "Route" && methods.is_empty() {
            vec![name.to_uppercase()]
        } else {
            methods
        };
        out.push((name, path, methods));
    }
    out
}

/// Take the text of the first `string_content` in the node subtree (used for path strings).
fn find_string_content(node: Node, src: &str) -> Option<String> {
    if node.kind() == "string_content" {
        return Some(text(node, src));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(s) = find_string_content(child, src) {
            return Some(s);
        }
    }
    None
}

/// Take every string element of an array literal (`['GET', 'POST']`) (recursively finding `array_element_initializer`).
fn collect_array_strings(node: Node, src: &str) -> Vec<String> {
    let mut out = Vec::new();
    if node.kind() == "array_element_initializer" {
        if let Some(s) = find_string_content(node, src) {
            out.push(s);
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        out.extend(collect_array_strings(child, src));
    }
    out
}

fn collect_method(node: Node, ctx: &mut Ctx, class_fqn: &str) {
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, ctx.src))
        .unwrap_or_default();
    let fqn = format!("{}::{}", class_fqn, name);
    // Parameters: record "name + type" so P7 can resolve instance calls by "variable type" (`$services->appAuth()`).
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

    // Constructor injection: `$this->services = $services;` -> the type of property services = the type of parameter services.
    let mut this_assigns: Vec<serde_json::Value> = Vec::new();
    // Assignment whose right side carries a type: `$this->x = new Y` / `Y::make()` / `app(Y::class)`.
    let mut this_assign_types: Vec<serde_json::Value> = Vec::new();
    // Local variables inside a method: `$x = new Y(...)` -> `$x->m()` can be resolved.
    let mut local_assign_types: Vec<serde_json::Value> = Vec::new();
    // `return X::class;` -> lets P7 build a declarative link "owning class -> X" (e.g. Dao::setModel() -> Model).
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

    for (attr_name, path, methods) in route_attributes_of(node, ctx.src) {
        let methods = if methods.is_empty() {
            vec!["ANY".to_string()]
        } else {
            methods
        };
        for m in methods {
            let snippet = snippet_of(node, ctx.src);
            ctx.facts.call_sites.push(CallSiteFact {
                owner_fqn: fqn.clone(),
                owner_class: Some(class_fqn.to_string()),
                callee_text: format!("attr.{}", attr_name),
                receiver: None,
                method: None,
                args: vec![FactValue::String(path.clone()), FactValue::String(m.clone())],
                span: span_of(node),
                snippet,
                db_table: None,
                in_loop: false,
                entity: Some(fqn.clone()),
            });
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        collect_call_sites(body, ctx, &fqn);
    }
}

/// Collect assignments inside a method body; one traversal serves three type-inference sources (avoiding repeated
/// recursion over each method body):
///
/// 1. `$this->prop = $var;` — constructor injection, the type comes from the **parameter type hint**
///    (`this_out`'s `{prop, var}`). ThinkPHP controllers / services are often written as
///    `__construct(LoginServices $services) { $this->services = $services; }`.
/// 2. `$this->prop = <an expression whose type is statically determinable>` — `new Y` / `Y::make()` /
///    `app(Y::class)` (`type_out`'s `{prop, class}`).
/// 3. `$x = <same as above>` — a local variable inside the method (`local_out`'s `{var, class}`), so `$x->m()`
///    can be resolved.
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
                    // ① Parameter type hint (more authoritative, recorded first; P2 skips ② when a type was already inferred)
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

/// Read a **statically determinable class name** out of the text on the right side of an assignment (`new` /
/// static factory / container lookup).
///
/// Only shapes obvious at a glance are accepted; everything else is `None` — better a missing edge than a wrong one.
fn rhs_class_of(node: Node, src: &str) -> Option<String> {
    let raw = text(node, src).trim().to_string();
    match node.kind() {
        // `new Foo(...)` / `new Foo`（`object_creation_expression`）
        "object_creation_expression" => node
            .named_children(&mut node.walk())
            .find(|c| matches!(c.kind(), "name" | "qualified_name"))
            .map(|c| trim_leading(text(c, src))),
        // `Foo::getDetail(...)` / `Foo::getInstance()` — static factories returning the same class
        "scoped_call_expression" => node
            .child_by_field_name("scope")
            .map(|s| trim_leading(text(s, src)))
            .filter(|s| !s.is_empty() && !s.contains('$') && !s.contains("->")),
        // `app(Foo::class)` / `app()->make(Foo::class, [...])`
        _ => {
            let cls = raw
                .split([',', '(']) // take the fragment containing `Foo::class`
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

/// Collect the class names referenced by `return X::class;` (raw text, possibly with a namespace prefix).
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

/// Config files: flatten `return [...]` into `config_entries`.
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
                    // The array itself is also an entry (value = the whole array); then recurse to expand the sub-keys
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

/// Collect the call sites inside a method body.
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
                        entity: None,
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
                        entity: None,
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
                        entity: None,
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
                        entity: None,
                    });
                }
                recurse_calls(child, ctx, owner_fqn);
            }
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
            "binary_expression" => {
                if let Some(fact) = sign_compare_of(child, ctx, owner_fqn) {
                    ctx.facts.sign_compares.push(fact);
                }
                recurse_calls(child, ctx, owner_fqn);
            }
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
                    if let Some(var) = plain_var_name(l, ctx.src) {
                        ctx.facts.variable_assignments.push(VariableAssignFact {
                            var,
                            rhs: text(r, ctx.src).trim().to_string(),
                            owner_fqn: owner_fqn.to_string(),
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
    // Calls inside a closure / anonymous function belong to the enclosing method
    collect_call_sites(node, ctx, owner_fqn);
}

/// If the left side of an assignment is a subscript access such as `['Access-Control-Allow-Origin']`, return the
/// normalised header name; otherwise `None`.
///
/// Only the `Access-Control-Allow-Origin` header that triggers reflected CORS is of interest (case-insensitive,
/// quotes ignored). When the left side is a **bare variable** the variable name is returned (without `$`);
/// `$this->x` / `$a['k']` return `None`.
///
/// Only the `$sql` shape is recognised: `$this->` is a property and `$a['k']` is a subscript, neither is a local
/// variable, and collecting them would make P9 Taint's backward tracking mistake property / array reads and writes
/// for a link in a local-assignment chain.
fn plain_var_name(node: Node, src: &str) -> Option<String> {
    let raw = text(node, src).trim().to_string();
    let body = raw.strip_prefix('$')?;
    if body.is_empty() || !body.contains(|c: char| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // Keep only identifiers: `->`, `[`, spaces and so on all mean it is not a bare variable.
    if !body.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // The first character must be a letter or `_` (`$1` is not a legal PHP variable).
    let first = body.chars().next()?;
    if !first.is_ascii_alphabetic() && first != '_' {
        return None;
    }
    Some(body.to_string())
}

fn cors_header_key(node: Node, src: &str) -> Option<String> {
    if node.kind() != "subscript_expression" {
        return None;
    }
    // Take the subscript key: prefer the `index` field name, fall back to the second named child (compatibility across tree-sitter-php versions).
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
        // Only Allow-Origin is captured, not Allow-Credentials (a non-reflected header).
        assert_eq!(facts.header_assignments.len(), 1);
        let ha = &facts.header_assignments[0];
        assert_eq!(ha.key, "access-control-allow-origin");
        let lower = ha.rhs_snippet.to_ascii_lowercase();
        assert!(lower.contains("header(") && lower.contains("origin"));
        // On the same line there should be a call site reading the request origin (the Cors phase locates the annotation from it).
        let origin_call = facts.call_sites.iter().any(|c| {
            c.method.as_deref() == Some("header")
                && c.args
                    .first()
                    .and_then(|a| a.as_str())
                    .map(|s| s.eq_ignore_ascii_case("origin"))
                    .unwrap_or(false)
        });
        assert!(origin_call, "expected to capture the ->header('origin') call site on the same line");
    }

    #[test]
    fn captures_local_variable_assignments() {
        let src = "<?php
class M {
    public function run() {
        $sql = 'select 1';
        $exec = str_replace('@table', $t, $sql);
        $this->conf = $sql;
        $list['k'] = $sql;
    }
}";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("m.php", src).unwrap();
        let vars: Vec<&str> = facts
            .variable_assignments
            .iter()
            .map(|a| a.var.as_str())
            .collect();
        // Only bare-variable left sides are collected: `$this->conf` / `$list['k']` are not local variables.
        assert_eq!(vars, vec!["sql", "exec"]);
        let exec = &facts.variable_assignments[1];
        assert!(exec.rhs.contains("$sql"), "the verbatim right-hand side should keep the variable reference: {}", exec.rhs);
        assert!(exec.owner_fqn.ends_with("run"), "owner_fqn={}", exec.owner_fqn);
    }

    /// Look up a call site's `in_loop` mark by its raw source fragment (verbs repeat in the same source, so only the line can tell them apart).
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
        // Outside the loop: executes once
        assert_eq!(in_loop_of(&facts, "'config'").unwrap().in_loop, false);
        // Inside the foreach body: once per record (this is the real N+1)
        assert_eq!(in_loop_of(&facts, "'user'").unwrap().in_loop, true);
        // Inside a nested loop body
        assert_eq!(in_loop_of(&facts, "Tag::get").unwrap().in_loop, true);
        // A call in the loop **condition** is evaluated before each entry, so it is not inside the loop body
        assert_eq!(in_loop_of(&facts, "fetch()").unwrap().in_loop, false);
        // Inside the while body
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
        // Only loose comparisons are collected: `===` is not the problem we are looking for and must not come in.
        assert_eq!(facts.sign_compares.len(), 1);
        let c = &facts.sign_compares[0];
        assert_eq!(c.operator, "!=");
        assert!(c.left.contains("CreatedSign"));
        assert!(c.right.contains("['sign']"));
        assert_eq!(c.owner_fqn, "P::respond");
    }

    /// In e-commerce code `sign` almost always means **check-in** — the biggest noise source of this whole rule, and it must be blocked at parse time.
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
            "a check-in style comparison should not be collected as a signature-comparison fact, got {:?}",
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

    #[test]
    fn captures_symfony_route_attributes_as_call_sites() {
        let src = "<?php
namespace App\\Controller;

use Symfony\\Component\\HttpFoundation\\Response;
use Symfony\\Component\\Routing\\Annotation\\Route;

class UserController
{
    #[Route('/api/users', methods: ['GET'])]
    public function listUsers(): Response { return new Response(); }

    #[Route('/api/users/{id}', methods: ['GET', 'POST'])]
    public function show(int $id): Response { return new Response(); }

    #[Get('/api/ping')]
    public function ping(): Response { return new Response(); }
}
";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("src/Controller/UserController.php", src).unwrap();
        let route_calls: Vec<&CallSiteFact> = facts
            .call_sites
            .iter()
            .filter(|c| c.callee_text.starts_with("attr."))
            .collect();
        // Route without methods -> 1 (ANY); Route with methods[GET,POST] -> 2; Get -> 1
        assert_eq!(route_calls.len(), 4, "expected 4 synthetic route call sites, got: {:?}",
            facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>());
        // entity points at the controller method
        assert!(route_calls.iter().all(|c| c.entity.as_deref() == Some("App\\Controller\\UserController::listUsers") || c.entity.as_deref() == Some("App\\Controller\\UserController::show") || c.entity.as_deref() == Some("App\\Controller\\UserController::ping")));
    }

    // ---- the signature noise gate (the rule's only filter) ----

    /// In e-commerce code `sign` almost always means **check-in**: of 32 measured "`==` comparisons containing
    /// sign", 24 were check-in. This gate is the only thing standing between the rule and a flooded report, so
    /// the curated noise list must be pinned.
    #[test]
    fn looks_like_signature_filters_checkin_noise() {
        // Real signature values.
        for s in ["$sign", "$signature", "$signValue", "$data['sign']", "$this->sign"] {
            assert!(looks_like_signature(s), "`{s}` must be judged a signature value");
        }
        // Check-in / points noise (the measured majority).
        for s in [
            "$sign_mode", "$signMode", "$sign_last_date", "$sign_total_days", "$sign_num",
            "$sign_count", "$sign_date", "$sign_enabled", "$sign_status", "$sign_rule",
            "$points_sign_enabled", "$userSign", "$signRecord", "$signLog",
        ] {
            assert!(!looks_like_signature(s), "`{s}` is check-in / points noise and must not be judged a signature value");
        }
        // Sign-in / sign-up are the same shape but not a signature value.
        assert!(!looks_like_signature("$signIn"));
        assert!(!looks_like_signature("$signUp"));
        // Same-shaped words that merely contain `sign`.
        assert!(!looks_like_signature("$assign_sign"));
        assert!(!looks_like_signature("$design"));
        assert!(!looks_like_signature("$resign"));
        // Certificate metadata is not a signature value to verify.
        assert!(!looks_like_signature("$signatureType"));
        assert!(!looks_like_signature("$signAlg"));
        // Hard requirements: a `$` variable and the `sign` stem.
        assert!(!looks_like_signature("sign"), "no `$` → not a variable");
        assert!(!looks_like_signature("$foo"), "no `sign` stem");
    }

    #[test]
    fn is_string_literal_detects_bare_quoted_strings() {
        assert!(is_string_literal("'x'"));
        assert!(is_string_literal("\"x\""));
        assert!(is_string_literal("  'x'  "), "it must be trimmed before judging");
        assert!(is_string_literal("''"), "an empty string literal is still a literal");
        assert!(!is_string_literal("$sign"));
        assert!(!is_string_literal("'a' . $b"), "a concatenation expression is not a pure literal");
    }

    // ---- naming helpers ----

    #[test]
    fn qualify_prefixes_the_namespace() {
        assert_eq!(qualify(Some("App\\Services"), "Foo"), "App\\Services\\Foo");
        assert_eq!(qualify(Some(""), "Foo"), "Foo", "an empty namespace must not produce a leading backslash");
        assert_eq!(qualify(None, "Foo"), "Foo");
    }

    #[test]
    fn trim_leading_strips_one_or_more_leading_backslashes() {
        assert_eq!(trim_leading("\\Foo".to_string()), "Foo");
        assert_eq!(trim_leading("\\\\Foo".to_string()), "Foo");
        assert_eq!(trim_leading("Foo".to_string()), "Foo");
    }

    /// A chained facade verb (`Db::name('goods')->where(...)->insert()`) carries the table name picked up
    /// upstream, so P7 can turn the terminal verb into `WritesDb` / `ReadsDb`.
    #[test]
    fn chained_facade_verb_carries_the_upstream_table_name() {
        let src = "<?php
namespace app\\services;

class Svc {
    public function run($data) {
        Db::name('goods')->where('id', 1)->insert($data);
    }
}
";
        let parser = PhpParser::new().unwrap();
        let facts = parser.parse("app/services/Svc.php", src).unwrap();
        let insert = facts
            .call_sites
            .iter()
            .find(|c| c.method.as_deref() == Some("insert"))
            .unwrap_or_else(|| {
                panic!(
                    "the insert call site must be captured: {:?}",
                    facts.call_sites.iter().map(|c| &c.callee_text).collect::<Vec<_>>()
                )
            });
        assert_eq!(
            insert.db_table.as_deref(),
            Some("goods"),
            "a chained verb must carry the table name from the upstream name('goods'), otherwise P7 cannot land a WritesDb"
        );
    }

    // ===== Below: facts the existing tests never assert (metadata, imports, declarations,
    // inheritance, field types, config entries) =====

    /// The pluggable-language surface: the kernel must learn PHP's notation from the adapter (`\` / `::` /
    /// `$var`) instead of hard-coding it.
    #[test]
    fn parser_declares_the_php_notation() {
        let p = PhpParser::new().unwrap();
        assert_eq!(p.language(), Language::new(Language::PHP));
        assert_eq!(p.namespace_separator(), &['\\']);
        assert_eq!(p.member_separator(), "::");
        assert_eq!(p.variable_prefixes(), &["$"]);
        assert_eq!(p.manifest_files(), &["composer.json"]);
        assert_eq!(p.exclude_dirs(), &["vendor"]);
        assert!(p.extensions().contains(&"php"));
        assert!(
            p.builtin_types().contains(&"string"),
            "the builtin-type table distinguishes builtin from project types: {:?}",
            p.builtin_types()
        );
    }

    /// `use` statements feed the short-name index, so a bare `UserService` in the source can be restored to
    /// its FQN. All three spellings must land in `facts.imports`.
    #[test]
    fn use_statements_become_imports_in_every_form() {
        let src = "<?php
namespace App;

use App\\Service\\UserService;
use App\\Service\\OrderService as Order;
use App\\{Repo\\UserRepo, Repo\\OrderRepo};
";
        let facts = PhpParser::new().unwrap().parse("app/Ctl.php", src).unwrap();
        let got: Vec<(Option<&str>, &str)> = facts
            .imports
            .iter()
            .map(|i| (i.alias.as_deref(), i.name.as_str()))
            .collect();
        assert!(got.contains(&(None, "App\\Service\\UserService")), "fully qualified import: {got:?}");
        assert!(
            got.contains(&(Some("Order"), "App\\Service\\OrderService")),
            "import with alias: {got:?}"
        );
        assert!(got.contains(&(None, "App\\Repo\\UserRepo")), "a group import must be joined with the prefix: {got:?}");
        assert!(got.contains(&(None, "App\\Repo\\OrderRepo")), "a group import must be joined with the prefix: {got:?}");
    }

    /// Every type declaration must reach the graph with its kind and a namespace-qualified FQN — the FQNs
    /// later phases look nodes up by.
    #[test]
    fn type_declarations_carry_kinds_and_qualified_fqns() {
        let src = "<?php
namespace App\\Model;

interface Iface {}
trait Tr {}
enum Suit { case Hearts; }
class User extends Base implements Iface, Other {
    public function run() {}
}
";
        let facts = PhpParser::new().unwrap().parse("app/Model/User.php", src).unwrap();
        let kinds: Vec<(&str, &str)> = facts
            .declarations
            .iter()
            .map(|d| (d.fqn.as_str(), d.kind.as_str()))
            .collect();
        assert!(kinds.contains(&("App\\Model", "Namespace")), "the namespace itself must be declared as well: {kinds:?}");
        assert!(kinds.contains(&("App\\Model\\User", "Class")), "{kinds:?}");
        assert!(kinds.contains(&("App\\Model\\Iface", "Interface")), "{kinds:?}");
        assert!(kinds.contains(&("App\\Model\\Tr", "Trait")), "{kinds:?}");
        assert!(kinds.contains(&("App\\Model\\Suit", "Enum")), "{kinds:?}");
        assert!(kinds.contains(&("App\\Model\\Suit::Hearts", "EnumCase")), "{kinds:?}");
        assert!(kinds.contains(&("App\\Model\\User::run", "Method")), "a method FQN is Class::method: {kinds:?}");
    }

    /// `extends` / `implements` / `use Trait` are three different relations and must land under their own
    /// kinds — collapsing them makes "is a subtype of" indistinguishable from "shares an implementation".
    #[test]
    fn inheritance_is_split_by_kind() {
        let src = "<?php
namespace App\\Model;

class User extends Base implements Iface, Other {
    use Tr;
}
";
        let facts = PhpParser::new().unwrap().parse("app/Model/User.php", src).unwrap();
        let inh: Vec<(&str, &str)> = facts
            .inheritances
            .iter()
            .map(|i| (i.base_name.as_str(), i.kind.as_str()))
            .collect();
        assert!(inh.contains(&("Base", "Extends")), "extends -> Extends: {inh:?}");
        assert!(inh.contains(&("Iface", "Implements")), "implements -> Implements: {inh:?}");
        assert!(inh.contains(&("Other", "Implements")), "every entry of the implements list must be recorded: {inh:?}");
        assert!(inh.contains(&("Tr", "UsesTrait")), "trait use -> UsesTrait: {inh:?}");
        assert!(
            facts.inheritances.iter().all(|i| i.child_fqn == "App\\Model\\User"),
            "an inheritance fact must hang on the subtype FQN: {inh:?}"
        );
    }

    /// A typed property feeds P7's `MapsTo` lookup by receiver type; the nullable marker must be stripped so
    /// `?string` resolves as `string`.
    #[test]
    fn typed_properties_record_field_types_and_declarations() {
        let src = "<?php
namespace App;

class Ctl {
    private UserRepository $repo;
    protected ?string $name = null;
}
";
        let facts = PhpParser::new().unwrap().parse("app/Ctl.php", src).unwrap();
        let ft: Vec<(&str, &str)> = facts
            .field_types
            .iter()
            .map(|f| (f.field.as_str(), f.type_name.as_str()))
            .collect();
        assert!(ft.contains(&("repo", "UserRepository")), "{ft:?}");
        assert!(ft.contains(&("name", "string")), "the `?` of a nullable type must be stripped: {ft:?}");
        assert!(
            facts.field_types.iter().all(|f| f.class_fqn == "App\\Ctl"),
            "a field must hang on the class declaring it: {ft:?}"
        );
        let fqns: Vec<&str> = facts.declarations.iter().map(|d| d.fqn.as_str()).collect();
        assert!(fqns.contains(&"App\\Ctl::$repo"), "a property declaration is precise down to Class::$field: {fqns:?}");
    }

    /// Config files (`config/*.php`) flatten `return [...]` into `config_entries`: nested arrays are recorded
    /// both as a whole **and** expanded into dotted key paths.
    #[test]
    fn config_return_is_flattened_into_config_entries() {
        let src = "<?php
return [
    'db' => [
        'host' => '127.0.0.1',
    ],
    'debug' => true,
];
";
        let facts = PhpParser::new()
            .unwrap()
            .parse("config/database.php", src)
            .unwrap();
        let paths: Vec<&str> = facts.config_entries.iter().map(|e| e.key_path.as_str()).collect();
        assert!(paths.contains(&"db"), "the array itself must be an entry too: {paths:?}");
        assert!(paths.contains(&"db.host"), "nested keys must be expanded into dotted paths: {paths:?}");
        assert!(paths.contains(&"debug"), "{paths:?}");
    }
}

/// Extract "equality comparisons of a signature value": `$sign == $ipay_signature` /
/// `$this->CreatedSign($params) != $params['sign']`.
///
/// Only `==` / `!=` are collected: `===` / `!==` are strict comparisons and are not the problem being looked for.
/// At least one side has to look like "a signature value" — a project has tens of thousands of `==`, and collecting
/// them all would blow up the fact set (same reasoning as Cors collecting only the Allow-Origin header).
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

/// Whether the text looks like **a signature value** (rather than a same-shaped word such as check-in).
///
/// This is the only noise gate of the whole rule: in e-commerce code `sign` almost always means **check-in**
/// (`$sign_mode` / `$sign_last_date` / `$sign_total_days` / `$points_sign_enabled`), and of 32 measured
/// "`==` comparisons containing sign", 24 were check-in. Without filtering them out the rule floods the report.
fn looks_like_signature(s: &str) -> bool {
    let t = s.trim();
    if !t.contains('$') {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if !lower.contains("sign") {
        return false;
    }
    // `assign` / `design` / `resign` also contain `sign` — exclude same-shaped words first.
    if lower.contains("assign") || lower.contains("design") || lower.contains("resign") {
        return false;
    }
    // Compare after stripping underscores, covering both `$sign_mode` and `$signMode`.
    let flat: String = lower.chars().filter(|c| !matches!(c, '_' | '-')).collect();
    const NOISE: &[&str] = &[
        "signtype", "signmode", "signlast", "signtotal", "signdays", "signnum", "signcount",
        "signdate", "signenabled", "signstatus", "signrule", "signconfig", "signset", "signin",
        "signup", "pointsign", "usersign", "signrecord", "signlog", "signremind", "signpoints",
        // Certificates also contain `signatureTypeLN` / signature algorithm names — those are metadata, not a signature value to verify
        "signaturetype", "signalg", "signmethod",
    ];
    !NOISE.iter().any(|n| flat.contains(n))
}

/// Whether this is a bare string literal (`'x'` / `"x"`).
fn is_string_literal(s: &str) -> bool {
    let t = s.trim();
    (t.starts_with('\'') && t.ends_with('\'')) || (t.starts_with('"') && t.ends_with('"'))
}

/// Take the source text of the **line** a call site sits on, so the UI can show the "call statement" for human
/// verification.
///
/// A single line only: `span` may cover a multi-line expression, and a multi-line fragment does not help "judge at
/// a glance" — it only inflates storage and view responses. Over-long lines are truncated (avoiding UTF-8
/// boundaries) so an abnormal line cannot pollute the view.
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
