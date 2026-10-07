use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Resolution rules for route handlers (**framework knowledge, not hard-coded in the kernel**).
///
/// "Which class / method handles this request" looks completely different in every framework:
///
/// | Framework | handler form |
/// |------|-------------|
/// | ThinkPHP | `'admin.Login/login'` (dots for hierarchy, slash before the method) |
/// | Laravel | `[LoginController::class, 'login']` / `'Ctrl@login'` |
/// | Symfony | `App\Controller\LoginController::login` |
/// | Rails | `'login#index'` |
///
/// So "which symbol separates the method" and "how the class name is assembled" are declared by FKB; the kernel
/// only expands the candidates per the declaration and looks them up. Where a framework would otherwise hard-code a
/// controller *directory name* (e.g. `controller` / `Http/Controllers`), the kernel **never assumes that name**:
/// instead it takes the stack's root namespaces (`root_namespaces`, from the tech-stack adapter's manifest) and
/// resolves the handler against the real class FQNs on the graph — see the resolver for the name-agnostic matching
/// rule. Namespace *separators* come from [`crate::model::NamespacePolicy`], so nothing here is PHP-shaped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MethodRefSpec {
    /// Separators between controller and method inside the handler string (tried in order, the first one that splits wins).
    pub method_separators: Vec<String>,
    /// The character that represents a namespace level inside the controller (it is replaced by that language's
    /// namespace separator).
    ///
    /// ThinkPHP's `v1.agent.AgentManage` -> `v1\agent\AgentManage`.
    pub hierarchy_separators: Vec<String>,
    /// Candidate values for `{app}` (template x segment, expanded one by one).
    ///
    /// Deprecated in favour of deriving the app modules from the project layout; kept only as an optional
    /// extra allow-list of module names to try (besides the one inferred from the route file path and the
    /// single-app shape). An empty list means "rely on path inference + single-app only".
    pub app_segments: Vec<String>,
    /// Root namespaces for the sub-project, derived at prepare time from the tech-stack manifest
    /// (PSR-4 autoload for PHP, and whatever the equivalent is for other stacks).
    ///
    /// These drive the resolver: a handler string is resolved against the real class FQNs under these namespaces,
    /// so **no controller directory name is ever assumed**. Filled by [`crate::port::TechStackAdapter::enrich_method_ref`],
    /// never written by FKB.
    pub root_namespaces: Vec<String>,
    /// How many namespace segments sit between the (inferred) app module and the controller class itself.
    ///
    /// This is a *structural* fact (e.g. ThinkPHP puts controllers one directory below the module; Laravel's
    /// `App\Http\Controllers` is two), **not** a name — so `controller` / `Http/Controllers` are never hard-coded.
    /// Default `1`.
    pub controller_layer_depth: usize,
    /// Infer `{app}` from the route file path: take the directory name **one level above** the anchor directory.
    /// Example: `app/api/route/pc.php` + anchor `route` -> `api`.
    pub app_anchor_dir: Option<String>,
    /// Fallback value for `{app}` when it cannot be inferred.
    pub app_fallback: String,
}

/// One "middleware class -> capability" declaration: what capability a middleware definitively provides.
///
/// Example (sample_project): `AuthTokenMiddleware` provides `Authentication`.
/// Matching is done on the **short name** only (the last segment after stripping the namespace) and is
/// case-insensitive — the same kind of middleware lives under different namespaces in different app directories
/// (`app\api\middleware\AuthTokenMiddleware` vs `app\kefuapi\middleware\KefuAuthTokenMiddleware`), yet the
/// semantics are expressed by the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiddlewareCapability {
    /// A fragment that must be **contained** in the middleware class short name, e.g. `AuthToken` / `Throttle`.
    pub matches: String,
    /// The capability name produced (goes into the `Capability` channel, e.g. `Authentication` / `RateLimiting`).
    pub capability: String,
}

/// **Route-guard recognition rules** (declared at framework level; the kernel collects guards from the call graph
/// accordingly).
///
/// Three attachment models cover the mainstream forms:
/// * `chain`: `Route::get(path)->middleware(X)` (ThinkPHP / Laravel style);
/// * `positional`: `app.get(path, mw1, mw2, handler)` (Express / Koa);
/// * `decorator`: `@UseGuards(X)` / `@login_required` / `@PreAuthorize` sits on **the same method** as the route
///   it decorates, associated by `owner_fqn` (NestJS / Python / Spring).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteGuardSpec {
    /// Route-definition call patterns (several allowed: e.g. ThinkPHP's `Route::`, Express's `app` / `router`,
    /// NestJS's `@Get` decorator).
    pub route_calls: Vec<RouteCallSpec>,
    /// How a guard is attached to a route. **Several may be declared**: one framework often has multiple
    /// attachment forms (NestJS has both `@UseGuards` and `consumer.apply(...).forRoutes(...)`), and the kernel
    /// takes the **union** of the guards produced by each model.
    #[serde(default)]
    pub guard_attach: GuardAttach,
    /// Optional: the symbol-table name of a middleware alias table (Laravel's `Kernel::$routeMiddleware` ->
    /// `middleware_aliases`).
    /// When a guard is written as an alias (`auth` / `web`), it is resolved back to the real class via this table.
    #[serde(default)]
    pub alias_table: Option<String>,
    /// Whether to also create a `Middleware` node for a **guard whose node cannot be found in the graph**. Default
    /// **false**.
    ///
    /// Why this is framework knowledge:
    /// * In PHP a guard is always a class, so not finding it means the class lives in `vendor` or the namespace was
    ///   not restored — creating a node would be fabricating one, so it stays false ("better missing than
    ///   guessed"; that behaviour is pinned by tests).
    /// * In JS / Python a guard is a **function value** (`const loginLimiter = rateLimit({...})`) for which the
    ///   parser builds no syntax node — yet "this route has a middleware called `loginLimiter`" is a confirmed
    ///   fact in the source. Creating a same-named `Middleware` semantic node is then **faithful recording**, not
    ///   guessing.
    #[serde(default)]
    pub synthesize_unresolved: bool,
}

/// One route-definition call recognition pattern.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteCallSpec {
    /// Match target:
    /// * `by = receiver` (default): `call.receiver` (case-insensitive) **containing** this string matches
    ///   (ThinkPHP `Route`, Express `app`); with `receiver_ends_with = true` it becomes "ends with this string"
    ///   instead (Laravel's `\Route`).
    /// * `by = callee`: match on `call.callee` (case-insensitive) — the decorator / annotation form
    ///   (NestJS `@Get` has callee `Get`, Spring `@GetMapping` has callee `GetMapping`, Python `@app.route` has
    ///   callee `app.route`). In that case the `receiver` field holds the decorator name.
    pub receiver: String,
    /// Match dimension: `receiver` (default) or `callee`.
    #[serde(default)]
    pub by: RouteMatchBy,
    /// Method name -> HTTP verb (`GET`/`POST`/`PUT`/`DELETE`/`PATCH`/`ANY`). Keys are case-insensitive; values are
    /// upper-cased. ThinkPHP's `rule` -> `ANY` and Laravel's `any` -> `ANY` are both normalised here.
    #[serde(default)]
    pub verb_methods: HashMap<String, String>,
    /// Index of the path argument (default 0).
    #[serde(default = "default_zero")]
    pub path_arg: usize,
    /// Index of the handler argument (default 1). `None` means the route definition carries no handler argument.
    #[serde(default)]
    pub handler_arg: Option<usize>,
    /// Group-prefix method name (e.g. ThinkPHP's `group`) — used to prepend the group prefix to every route path inside the group.
    #[serde(default)]
    pub group_method: Option<String>,
    /// When `by = receiver`, whether it "ends with `receiver`" instead of "contains". Default false.
    #[serde(default)]
    pub receiver_ends_with: bool,
    /// Whether to also treat an **identifier argument** (`loginLimiter` in `app.post('/x', loginLimiter, handler)`)
    /// as middleware. Default **false**.
    ///
    /// Why this is framework knowledge: in PHP middleware is always an `X::class` literal, while in JS / Python it
    /// is a **function reference** (`loginLimiter` / `isAuthenticated`) that lands in the call graph as an
    /// `Unknown` with the variable name. The PHP side must stay false — pulling in a dynamic argument like
    /// `->middleware($v)` would attach a variable name as middleware, which is fabrication (that behaviour is
    /// pinned by tests).
    #[serde(default)]
    pub accept_identifier: bool,
}

/// The dimension a route matches on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatchBy {
    /// Match on `call.receiver` (the chained / positional-argument form).
    #[default]
    Receiver,
    /// Match on `call.callee` (the decorator / annotation form).
    Callee,
}

/// Guard attachment model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuardAttachSpec {
    /// Chained: `Route::get(path)->middleware(X)`.
    Chain(ChainGuardSpec),
    /// Positional: `app.get(path, mw1, mw2, handler)` — every argument after `path_arg` up to (excluding)
    /// `handler_arg` is middleware.
    #[default]
    Positional,
    /// Decorator / annotation: the guard and the route it decorates share an **owner** (method / function), associated by `owner_fqn`.
    Decorator(DecoratorGuardSpec),
    /// Consumer-style attachment: NestJS's `consumer.apply(X).forRoutes(...)`.
    Consumer(ConsumerGuardSpec),
}

/// One / many guard attachment models (the value of `guard_attach`).
///
/// A single value or a list is accepted so that one framework can declare **several** attachment forms without changing the kernel or breaking existing FKBs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuardAttach {
    One(GuardAttachSpec),
    Many(Vec<GuardAttachSpec>),
}

impl GuardAttach {
    /// Normalise into a list of models.
    pub fn specs(&self) -> Vec<&GuardAttachSpec> {
        match self {
            GuardAttach::One(s) => vec![s],
            GuardAttach::Many(v) => v.iter().collect(),
        }
    }
}

impl Default for GuardAttach {
    fn default() -> Self {
        GuardAttach::One(GuardAttachSpec::Positional)
    }
}

/// Attachment of the `consumer.apply(X).forRoutes(...)` kind: "declared in a module, applied to routes elsewhere".
///
/// NestJS middleware is attached inside `configure()` in `*.module.ts`:
/// ```ts
/// consumer.apply(AuthMiddleware).forRoutes({ path: '*', method: RequestMethod.ALL });
/// ```
/// Which routes it applies to is decided by the **arguments** of `forRoutes`, and the module -> controller -> route
/// mapping is unknowable to the kernel, so the "scope" must be declared by FKB (see [`ConsumerScope`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConsumerGuardSpec {
    /// The consumer variable name (default `consumer`).
    pub receiver: String,
    /// The method name that attaches middleware (default `apply`).
    pub apply_method: String,
    /// The method name that declares the scope (default `forRoutes`).
    pub for_routes_method: String,
    /// Wildcard arguments that mean "every route in this module" (default `["*"]`).
    pub wildcards: Vec<String>,
    /// Which scope to expand to when the wildcard matches (default [`ConsumerScope::Directory`]).
    pub scope: ConsumerScope,
}

/// Which routes the middleware applies to when `forRoutes` matches the wildcard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerScope {
    /// Only honour **explicit paths** (`forRoutes('users')`); a wildcard never expands — the most conservative.
    ExplicitOnly,
    /// On a wildcard, apply to routes in controllers **in the same directory as the module** (the NestJS
    /// convention: `user.module.ts` and `user.controller.ts` both live in `src/user/`). Default.
    #[default]
    Directory,
    /// On a wildcard, apply to **all** routes (equivalent for a single-module project; over-claims for multi-module ones).
    All,
}

impl Default for ConsumerGuardSpec {
    fn default() -> Self {
        Self {
            receiver: "consumer".into(),
            apply_method: "apply".into(),
            for_routes_method: "forRoutes".into(),
            wildcards: vec!["*".into()],
            scope: ConsumerScope::Directory,
        }
    }
}

/// Chained guard: `Route::get(path)->middleware(X)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChainGuardSpec {
    /// The member method name that attaches the guard (e.g. `middleware`).
    pub method: String,
    /// Index of the guard-class argument (default 0).
    #[serde(default = "default_zero")]
    pub arg_index: usize,
    /// Index of the second argument (distinguishing mandatory from optional, e.g. `AuthTokenMiddleware::class, false`). `None` means there is none.
    #[serde(default)]
    pub arg2_index: Option<usize>,
}

/// Decorator / annotation guard: associated by `owner_fqn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DecoratorGuardSpec {
    /// The callee list of route decorators / annotations (e.g. `app.route` / `@Get` / `@GetMapping`).
    /// Compared verbatim against the parser's callee output (the TS parser prefixes decorators with `@`); case-insensitive.
    pub route_decorators: Vec<String>,
    /// The callee list of guard decorators / annotations (e.g. `login_required` / `@UseGuards` / `@PreAuthorize`).
    pub guard_decorators: Vec<String>,
    /// **Regex patterns** for guard names (case-insensitive). Matching any one of them counts as a guard.
    ///
    /// Why this has to exist: in JS / Python guards are often **decorators the project wrote itself**
    /// (`@requires_admin` / `@jwt_or_403` / `@staff_only`…), so enumerating names is whack-a-mole.
    /// And "which names count as a guard" is a framework / project convention, declared by FKB:
    ///   `["(login|auth|jwt|token)", "(permission|role|admin|staff|owner)", "(guard|secure|required|only)"]`
    /// Java / Spring annotations are fixed by the framework (`@PreAuthorize` etc.), so listing them precisely in
    /// `guard_decorators` is enough.
    #[serde(default)]
    pub guard_name_patterns: Vec<String>,
    /// **Exclusion regexes** for guard names (case-insensitive), taking **priority over** both `guard_decorators`
    /// and `guard_name_patterns` — a match means "not a guard".
    ///
    /// Why this has to exist: broad containment patterns cause collateral damage —
    /// * The Swagger / OpenAPI **documentation** decorator `@ApiBearerAuth()` has `auth` in its name but performs
    ///   no authentication at all (measured: 17 routes in a real Node sample were mislabelled as
    ///   "guard passed" because of it);
    /// * NestJS **parameter** decorators `@User('email')` / `@Body()` / `@Param()` only read values, they are not
    ///   guards; and local variable names (`_user`) should not become middleware either.
    /// "Which names are not guards" is likewise framework knowledge, declared by FKB.
    #[serde(default)]
    pub guard_exclude_patterns: Vec<String>,
    /// Require the guard / route call's callee to **start with `@`** (i.e. a call site the parser marked as a
    /// decorator). Default false.
    ///
    /// The TS parser prefixes a decorator's callee with `@` (`@Get` / `@UseGuards`), which distinguishes decorators
    /// from **ordinary method calls** — otherwise a business method like `this.userService.generateJWT(...)`, whose
    /// name contains `jwt`, would be taken for a guard (measured false positive).
    #[serde(default)]
    pub require_at_prefix: bool,
    /// Associate guards by the **route call's handler argument** instead of by owner. Default `None`.
    ///
    /// Frameworks like Django write routes and views **apart**:
    ///   urls.py     `path('profile', views.profile)`        <- the route lives here
    ///   views.py    `@login_required\ndef profile(request):` <- the guard lives here
    /// The two have different owners (a urls module vs a view function), so associating by owner always misses.
    /// With this field declared (the index of the argument holding the handler), guards match on "`owner_fqn`
    /// **ends with the handler name**" — the handler reads `views.profile` and the view function's owner is
    /// `myapp.views.profile`, so the suffix matches.
    #[serde(default)]
    pub link_via_handler_arg: Option<usize>,
    /// Require the guard / route call to have **no receiver** (a bare-name call). Default false.
    ///
    /// Python's `@login_required` and Java's `@PreAuthorize` are both bare names, whereas member calls like
    /// `self.generate_jwt()` / `this.checkAuth()` have a receiver and are not decorators.
    #[serde(default)]
    pub require_no_receiver: bool,
    /// Whether the guard name comes from the **argument** or from **the decorator name itself**. Default **true**
    /// (from the argument).
    ///
    /// * `true`: NestJS's `@UseGuards(JwtAuthGuard)` — the guard is the class in the argument; a no-argument
    ///   `@login_required` still falls back to the decorator name.
    /// * `false`: Spring's `@PreAuthorize("hasRole('ADMIN')")` — the argument is a SpEL expression, and the real
    ///   "guard" is the annotation itself (`PreAuthorize` / `Secured` / `RolesAllowed`).
    #[serde(default = "default_true")]
    pub name_from_args: bool,
    /// Whether to also honour **class-level** guards (a guard annotation / decorator on the class, applying to every
    /// route method of that class). Default **true**: NestJS commonly puts `@UseGuards` on the `@Controller`
    /// class, Spring commonly puts `@PreAuthorize` on the class, and Python class-based views often use
    /// class-level decorators too.
    #[serde(default = "default_true")]
    pub include_class_level: bool,
}

/// The two "on by default" switches make `Default` disagree with what `false` would give, so it is written
/// out instead of derived — otherwise `DecoratorGuardSpec::default()` (the Rust path) and `{}` in YAML (the
/// serde path) would silently mean different things.
impl Default for DecoratorGuardSpec {
    fn default() -> Self {
        Self {
            route_decorators: Vec::new(),
            guard_decorators: Vec::new(),
            guard_name_patterns: Vec::new(),
            guard_exclude_patterns: Vec::new(),
            require_at_prefix: false,
            link_via_handler_arg: None,
            require_no_receiver: false,
            name_from_args: default_true(),
            include_class_level: default_true(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_zero() -> usize {
    0
}

/// The read / write verb list of a data model (method names, case-insensitive).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbVerbsSpec {
    /// Write verbs: `save` / `insert` / `update` / `delete` …
    #[serde(default)]
    pub write: Vec<String>,
    /// Read verbs: `find` / `select` / `value` / `count` …
    #[serde(default)]
    pub read: Vec<String>,
}

/// One raw-SQL sink: a method name, optionally bound to a receiver (`Db::query` — `query` alone would
/// match any `->query()`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaintSink {
    pub method: String,
    /// `None` = any receiver (`whereRaw` / `whereExp` / `raw` are SQL however they are reached).
    #[serde(default)]
    pub receiver: Option<String>,
}

/// The vocabulary of **SQL injection** (used by P9 Taint).
///
/// Which ORM methods execute raw SQL, and which expressions read user input, is framework / library
/// knowledge — ThinkPHP's `Db::query` and `whereRaw`, PHP's superglobals, a framework's `input()`. The
/// kernel knows none of them. A stack that declares nothing gets **no** taint judgement, which is honest:
/// "does this argument contain a variable" is not answerable without knowing how the language writes one
/// (that part comes from the parser, via `NamespacePolicy::variable_prefixes`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TaintSpec {
    /// Methods that execute raw SQL (argument 0 is the SQL text).
    pub raw_sql_sinks: Vec<TaintSink>,
    /// Methods whose **condition string** interpolation is the injection form (`where` / `whereOr`).
    pub where_interp_sinks: Vec<String>,
    /// Text features meaning "this expression reads user input" (case-insensitive substring of an
    /// assignment's right-hand side).
    pub request_sources: Vec<String>,
}

/// The vocabulary of **signature verification** (used by P11 Sign).
///
/// Which calls compute or verify a signature, and which algorithms count as weak, is language and library
/// knowledge: PHP's `hash_hmac` / `hash_equals` / `openssl_verify`, Java's `MessageDigest.getInstance`,
/// Node's `crypto.createHash`. The kernel knows none of them — a stack that declares nothing is simply not
/// judged ("no knowledge" → "no annotation"), which is honest, instead of silently applying PHP's vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SignCheckSpec {
    /// Calls that compute / verify a signature: `md5`, `hash_hmac`, `hash_equals`, `openssl_verify` …
    pub hash_calls: Vec<String>,
    /// Algorithms considered weak **when used for a signature** (`md5` / `sha1`).
    pub weak_algos: Vec<String>,
    /// A method name containing this substring also counts as signature computation (`sign` — `CreatedSign`,
    /// `GetSign`, `makeSign`, `verifySign`). Case-insensitive.
    pub name_contains: Option<String>,
    /// …unless it also contains one of these. Needed because in e-commerce code `sign` is overwhelmingly
    /// about **check-ins** (`signin` / `signup` / `signmode` / `signtype`).
    pub name_excludes: Vec<String>,
    /// Argument text hints that mark a call as signature-related **on their own** (`sign`).
    pub value_hints: Vec<String>,
    /// Argument text hints that only count when a signature comparison exists in the same function (`key=`).
    pub value_hints_require_compare: Vec<String>,
}

/// The forwarding target of magic methods (`@method` annotations).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MagicDelegationSpec {
    /// Name of the property to forward to (e.g. sample_project's `dao`). Its type is inferred from how the property is injected, per the existing rules.
    pub property: String,
    /// Confidence of the forwarding resolution (lower than an "exact method hit", since this is an annotation declaration rather than source code).
    pub confidence: f32,
}

impl Default for MagicDelegationSpec {
    fn default() -> Self {
        Self { property: String::new(), confidence: 0.7 }
    }
}

impl Default for MethodRefSpec {
    fn default() -> Self {
        Self {
            method_separators: vec!["/".into()],
            hierarchy_separators: Vec::new(),
            app_segments: Vec::new(),
            root_namespaces: Vec::new(),
            controller_layer_depth: 1,
            app_anchor_dir: None,
            app_fallback: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ------------------------------------------------------- GuardAttach::specs (the only logic-bearing method)

    #[test]
    fn guard_attach_specs_one_and_many() {
        // A single spec is normalised to a one-element list.
        let one: GuardAttach =
            serde_json::from_value(json!({ "kind": "chain", "method": "middleware", "arg_index": 0 }))
                .unwrap();
        assert_eq!(one.specs().len(), 1);
        match one {
            GuardAttach::One(GuardAttachSpec::Chain(c)) => assert_eq!(c.method, "middleware"),
            _ => panic!("expected One(Chain)"),
        }

        // A list is kept as-is.
        let many: GuardAttach = serde_json::from_value(json!([
            { "kind": "chain", "method": "m", "arg_index": 0 },
            { "kind": "positional" }
        ]))
        .unwrap();
        assert_eq!(many.specs().len(), 2);
        match many {
            GuardAttach::Many(ref v) => assert_eq!(v.len(), 2),
            _ => panic!("expected Many"),
        }

        // An empty list yields zero specs (no panic).
        let empty: GuardAttach = serde_json::from_value(json!([])).unwrap();
        assert!(empty.specs().is_empty());
    }

    // ------------------------------------------------------- Default contracts (incl. the non-trivial ones)

    /// `MethodRefSpec::default()` fixes `controller_layer_depth = 1` and `method_separators = ["/"]`.
    #[test]
    fn method_ref_spec_default() {
        let d = MethodRefSpec::default();
        assert_eq!(d.controller_layer_depth, 1);
        assert_eq!(d.method_separators, vec!["/".to_string()]);
        assert!(d.hierarchy_separators.is_empty());
        assert!(d.root_namespaces.is_empty());
        assert_eq!(d.app_fallback, "");
        assert!(d.app_anchor_dir.is_none());
    }

    /// `#[serde(default)]` on `RouteGuardSpec` means `{}` is a valid, fully-defaulted spec — and
    /// `guard_attach` falls back to `GuardAttach::default()` (`One(Positional)`), `synthesize_unresolved` to false.
    #[test]
    fn route_guard_spec_empty_default() {
        let rg: RouteGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert!(!rg.synthesize_unresolved, "false by default (PHP: never fabricate a missing node)");
        assert!(rg.alias_table.is_none());
        assert!(rg.route_calls.is_empty());
        match rg.guard_attach {
            GuardAttach::One(GuardAttachSpec::Positional) => {}
            _ => panic!("guard_attach defaults to One(Positional)"),
        }
    }

    /// `RouteCallSpec::default()` must apply `default_zero` (path_arg 0) and the enum default (by = Receiver).
    #[test]
    fn route_call_spec_defaults() {
        let rc: RouteCallSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(rc.path_arg, 0, "default_zero() gives 0");
        assert_eq!(rc.by, RouteMatchBy::Receiver, "enum default");
        assert!(rc.verb_methods.is_empty());
        assert!(rc.handler_arg.is_none());
        assert_eq!(rc.receiver, "");
        assert!(!rc.receiver_ends_with);
        assert!(!rc.accept_identifier, "PHP: never pull a dynamic identifier arg as middleware");
    }

    /// The two "on by default" switches must be `true` via serde `{}` — NOT the `false` a bare
    /// `#[serde(default)]` would give. This pins the contract the struct's manual `Default` exists to guard.
    #[test]
    fn decorator_guard_spec_defaults_true() {
        let d: DecoratorGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert!(d.name_from_args, "defaults to true (argument carries the guard name)");
        assert!(d.include_class_level, "defaults to true (class-level guards apply)");
        assert!(!d.require_at_prefix);
        assert!(!d.require_no_receiver);
        // The Rust `Default` path must agree with the serde `{}` path.
        let r = DecoratorGuardSpec::default();
        assert!(r.name_from_args && r.include_class_level);
    }

    #[test]
    fn consumer_guard_spec_default() {
        let c: ConsumerGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.receiver, "consumer");
        assert_eq!(c.apply_method, "apply");
        assert_eq!(c.for_routes_method, "forRoutes");
        assert_eq!(c.wildcards, vec!["*".to_string()]);
        assert_eq!(c.scope, ConsumerScope::Directory);
    }

    #[test]
    fn chain_guard_spec_default_arg_index_zero() {
        let c: ChainGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(c.arg_index, 0, "default_zero()");
        assert!(c.arg2_index.is_none());
        assert_eq!(c.method, "");
    }

    #[test]
    fn magic_delegation_default_confidence() {
        let m = MagicDelegationSpec::default();
        assert_eq!(m.property, "");
        assert_eq!(m.confidence, 0.7);
    }

    #[test]
    fn trivial_container_defaults() {
        let db: DbVerbsSpec = serde_json::from_value(json!({})).unwrap();
        assert!(db.write.is_empty() && db.read.is_empty());
        let t: TaintSpec = serde_json::from_value(json!({})).unwrap();
        assert!(t.raw_sql_sinks.is_empty() && t.where_interp_sinks.is_empty() && t.request_sources.is_empty());
        let s: SignCheckSpec = serde_json::from_value(json!({})).unwrap();
        assert!(s.hash_calls.is_empty());
        assert!(s.name_contains.is_none());
        // TaintSink's `receiver` defaults to None.
        let sink: TaintSink = serde_json::from_value(json!({ "method": "query" })).unwrap();
        assert!(sink.receiver.is_none());
    }

    // ------------------------------------------------------- snake_case enum tags (positive round-trips)

    #[test]
    fn route_match_by_snake_case() {
        assert_eq!(serde_json::to_value(RouteMatchBy::Receiver).unwrap(), json!("receiver"));
        assert_eq!(serde_json::to_value(RouteMatchBy::Callee).unwrap(), json!("callee"));
        assert_eq!(serde_json::from_value::<RouteMatchBy>(json!("callee")).unwrap(), RouteMatchBy::Callee);
        assert_eq!(serde_json::from_value::<RouteMatchBy>(json!("receiver")).unwrap(), RouteMatchBy::Receiver);
    }

    #[test]
    fn consumer_scope_snake_case_and_default() {
        assert_eq!(serde_json::to_value(ConsumerScope::ExplicitOnly).unwrap(), json!("explicit_only"));
        assert_eq!(serde_json::to_value(ConsumerScope::Directory).unwrap(), json!("directory"));
        assert_eq!(serde_json::to_value(ConsumerScope::All).unwrap(), json!("all"));
        assert_eq!(ConsumerScope::default(), ConsumerScope::Directory);
        assert_eq!(serde_json::from_value::<ConsumerScope>(json!("all")).unwrap(), ConsumerScope::All);
    }

    #[test]
    fn guard_attach_spec_variant_tags() {
        assert_eq!(
            serde_json::to_value(GuardAttachSpec::Chain(ChainGuardSpec::default())).unwrap()["kind"],
            json!("chain")
        );
        assert_eq!(
            serde_json::to_value(GuardAttachSpec::Positional).unwrap()["kind"],
            json!("positional")
        );
        assert_eq!(
            serde_json::to_value(GuardAttachSpec::Decorator(DecoratorGuardSpec::default())).unwrap()["kind"],
            json!("decorator")
        );
        assert_eq!(
            serde_json::to_value(GuardAttachSpec::Consumer(ConsumerGuardSpec::default())).unwrap()["kind"],
            json!("consumer")
        );
        assert!(matches!(GuardAttachSpec::default(), GuardAttachSpec::Positional));
    }

    // ------------------------------------------------------- deny_unknown_fields (unknown key = load error)

    /// Every `#[serde(deny_unknown_fields)]` struct here must reject a field the model does not have —
    /// a key the kernel cannot read must error, never be silently dropped.
    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        let structs: Vec<(&str, serde_json::Value)> = vec![
            ("MethodRefSpec", json!({ "bogus": 1 })),
            ("MiddlewareCapability", json!({ "matches": "x", "capability": "y", "bogus": 1 })),
            ("RouteGuardSpec", json!({ "bogus": 1 })),
            ("RouteCallSpec", json!({ "bogus": 1 })),
            ("ConsumerGuardSpec", json!({ "bogus": 1 })),
            ("ChainGuardSpec", json!({ "bogus": 1 })),
            ("DecoratorGuardSpec", json!({ "bogus": 1 })),
            ("DbVerbsSpec", json!({ "bogus": 1 })),
            ("TaintSpec", json!({ "bogus": 1 })),
            ("SignCheckSpec", json!({ "bogus": 1 })),
            ("MagicDelegationSpec", json!({ "property": "p", "bogus": 1 })),
            ("TaintSink", json!({ "method": "m", "bogus": 1 })),
        ];
        for (name, value) in structs {
            let ok = match name {
                "MethodRefSpec" => serde_json::from_value::<MethodRefSpec>(value).is_ok(),
                "MiddlewareCapability" => serde_json::from_value::<MiddlewareCapability>(value).is_ok(),
                "RouteGuardSpec" => serde_json::from_value::<RouteGuardSpec>(value).is_ok(),
                "RouteCallSpec" => serde_json::from_value::<RouteCallSpec>(value).is_ok(),
                "ConsumerGuardSpec" => serde_json::from_value::<ConsumerGuardSpec>(value).is_ok(),
                "ChainGuardSpec" => serde_json::from_value::<ChainGuardSpec>(value).is_ok(),
                "DecoratorGuardSpec" => serde_json::from_value::<DecoratorGuardSpec>(value).is_ok(),
                "DbVerbsSpec" => serde_json::from_value::<DbVerbsSpec>(value).is_ok(),
                "TaintSpec" => serde_json::from_value::<TaintSpec>(value).is_ok(),
                "SignCheckSpec" => serde_json::from_value::<SignCheckSpec>(value).is_ok(),
                "MagicDelegationSpec" => serde_json::from_value::<MagicDelegationSpec>(value).is_ok(),
                "TaintSink" => serde_json::from_value::<TaintSink>(value).is_ok(),
                _ => unreachable!(),
            };
            assert!(!ok, "{} must reject unknown field", name);
        }
    }

    /// Enums reject an unknown `kind` / variant.
    #[test]
    fn deny_unknown_variants_rejected() {
        assert!(serde_json::from_value::<GuardAttachSpec>(json!({ "kind": "bogus" })).is_err());
        assert!(serde_json::from_value::<RouteMatchBy>(json!("bogus")).is_err());
        assert!(serde_json::from_value::<ConsumerScope>(json!("bogus")).is_err());
    }
}

