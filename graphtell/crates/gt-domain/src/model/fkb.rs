//! Domain model of the Framework Knowledge Base (FKB).
//!
//! FKB is the serialisable form of "preset framework knowledge": ThinkPHP 6, Uni-app, Laravel, CRMEB…
//! one YAML per framework, describing
//! * how to **recognise** the framework ([`Detector`])
//! * how to **resolve its root** ([`RootRule`], e.g. `autoload.psr-4` in `composer.json`)
//! * which **authoritative symbol tables** P3 should load ([`LoaderSpec`])
//! * the **rules** to run in each phase ([`Rule`] = selector + binding)
//!
//! Everything is data-driven and the kernel knows no concrete framework — this is where the **open-closed
//! principle** and **dependency inversion** land: supporting a new framework only means adding one YAML file.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::graph::{MergeStrategy, Span};
use super::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};
use crate::model::kinds::Language;

/// One framework's knowledge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameworkKnowledge {
    pub id: String,
    pub display_name: String,
    pub language: Language,
    /// Applicable-version hint, display only.
    pub version_hint: Option<String>,
    /// Recognition signals.
    pub detectors: Vec<Detector>,
    /// Rules for resolving the framework root / key paths.
    pub root_rules: Vec<RootRule>,
    /// P3 authoritative symbol-table loaders.
    pub loaders: Vec<LoaderSpec>,
    /// Rules grouped by phase.
    pub rules: Vec<Rule>,

    /// **Business-specific annotation kinds** introduced by this FKB (appended to the kernel-standard
    /// [`AnnotationKind`]).
    ///
    /// The standard kinds (pii / data.criticality / config.storage / auth.public / i18n.missing_locale …) are
    /// produced by kernel recognisers and are not listed here; only "project-specific business semantics" goes
    /// here (e.g. `entrypoint.login`).
    /// They are registered at load time into [`crate::model::kinds::register_annotation_kinds`], isomorphic to
    /// `semantic_kinds` (nodes) — adding a business annotation should not cost a kernel change.
    #[serde(default)]
    pub annotation_kinds: Vec<String>,
    /// P7 dynamic-resolution declarations: which calls are container resolutions / event triggers / facade calls.
    pub resolvers: Vec<ResolverSpec>,
    /// Additional exclusion directories (layered on top of the project / language defaults).
    pub exclude_globs: Vec<String>,
    /// Scope of the knowledge base: framework-level (default) vs project-level.
    ///
    /// * `Framework`: generic framework knowledge (e.g. `thinkphp6` / `laravel`), loaded by any project using that
    ///   framework;
    /// * `Project`: project-specific knowledge (e.g. `crmeb`), loaded **only when the project is recognised as
    ///   that project**, so project conventions (e.g. CRMEB's crontab routes) do not bleed into other projects on
    ///   the same framework.
    pub scope: KnowledgeScope,
    /// Resolution rules for **string-literal callable references** (route handlers, queue string-jobs,
    /// `invokeAction` …): how to turn a string argument naming `class::method` / a function back into a graph
    /// node. Generic — not route-only; see [`MethodRefSpec`].
    ///
    /// This is **framework knowledge**, not kernel knowledge — see [`MethodRefSpec`].
    #[serde(default)]
    pub method_ref: Option<MethodRefSpec>,
    /// Magic-method delegation: a class declares `@method getList(...)` and forwards it to some property via
    /// `__call`.
    ///
    /// "annotation declaration + `__call` forwarding" is common in the PHP ecosystem (CRMEB's `BaseServices`
    /// forwards 20-odd `get*` / `count*` / `delete*` to `$this->dao`), but **who it forwards to** is a project
    /// convention the kernel must not guess — FKB just names the property, and everything else (annotation
    /// parsing, where the type comes from, inheritance walk-back) is a generic capability.
    #[serde(default)]
    pub magic_delegation: Option<MagicDelegationSpec>,
    /// Data-model CRUD verb -> read / write classification (used together with `MapsTo`).
    ///
    /// "A model maps to a table" is **static structure**; `$model->save()` / `$model->find()` are the
    /// **actions**. Which method names count as reads and which as writes is a framework API convention (ThinkPHP's
    /// `save` / `find`, Laravel's `create`…); once FKB declares them, P7 can label the
    /// `entry -> model -> table` edge as a real `WritesDb` / `ReadsDb` instead of propagating a vague `MapsTo` all
    /// the way through.
    #[serde(default)]
    pub db_verbs: Option<DbVerbsSpec>,
    /// A callee list for **external system calls** (HTTP / SMS / email / RPC): `curl_exec`, `Http::get` …
    /// declared by FKB for the "external call inside a loop" judgement (one network round trip costs far more
    /// than one query, so putting it in a loop kills an endpoint even more reliably than N+1).
    #[serde(default)]
    pub external_calls: Vec<String>,
    /// A **"middleware class -> capability" mapping**: what capability a given middleware carries (auth /
    /// rate limiting …).
    ///
    /// This list **belongs to framework knowledge** (what counts as an auth middleware, and what name the project
    /// gave it), so it lives in FKB rather than the kernel — the kernel knows no middleware name. The judgement is
    /// anchored on the middleware's **own confirmed identity** (its class name), not on "does this endpoint look
    /// like it needs a login".
    #[serde(default)]
    pub middleware_capabilities: Vec<MiddlewareCapability>,
    /// **Route-guard recognition rules**: how to tell from the call graph "which middleware guards which route".
    ///
    /// This is **framework knowledge, not kernel knowledge** — `Route::get()->middleware(X)` is the ThinkPHP /
    /// Laravel chained form, `app.get(path, mw, handler)` is the Express positional-argument form, and
    /// `@UseGuards(X)` / `@login_required` / `@PreAuthorize` are the NestJS / Python / Spring decorator /
    /// annotation forms. The kernel knows none of them; FKB declares all of them, the generic extractor collects
    /// them from the graph per the declaration and writes them into the `route_list` symbol table (with keys of
    /// the same shape as the `HttpContract.name` synthesised in P5), so P14 can promote the guard class to a
    /// `Middleware` and attach a `PassesThrough` edge.
    ///
    /// Supporting middleware for a new language / framework = add one `route_guards` block, **no Rust change**.
    #[serde(default)]
    pub route_guards: Option<RouteGuardSpec>,
    /// **Transaction-boundary markers**: `transaction` / `startTrans` / `beginTransaction` …
    /// for the "several writes to the DB in one method but no transaction recognised" judgement (a partial
    /// success leaves dirty data).
    #[serde(default)]
    pub tx_calls: Vec<String>,
    /// Candidates for the "consumer entry method name": when connecting to a class, which of its methods to
    /// connect to first.
    ///
    /// Conventions differ per framework: Laravel / queue Jobs use `handle`, Symfony uses `__invoke`,
    /// ThinkPHP / CRMEB Jobs use `doJob`, TP5 behaviour classes use `run`.
    /// Declared by FKB so a new framework does not have to change the kernel for one method name.
    /// When undeclared it falls back to the kernel's built-in **cross-framework common entry names** default set.
    #[serde(default)]
    pub entry_methods: Vec<String>,
    /// **First-class semantic node kinds** introduced by this FKB (appended to [`NodeKind::SYNTHESIZED`]).
    ///
    /// "Which kinds count as semantic nodes" used to live only in the constant list in `kinds.rs` — so every new
    /// semantic node (the frontend's `Store`, the page perspective's `Page`…) required a kernel change, violating
    /// OCP.
    /// Now FKB can declare it itself: `semantic_kinds: [Store, Page]`, registered at load time into
    /// [`crate::model::kinds::register_semantic_kinds`], and the folded view then renders them as semantic nodes.
    #[serde(default)]
    pub semantic_kinds: Vec<String>,
    /// **First-class semantic edge kinds** introduced by this FKB (appended to the built-in [`EdgeKind::SEMANTIC`]
    /// list).
    ///
    /// Isomorphic to `semantic_kinds` (nodes): adding a semantic edge should not cost a kernel change.
    /// Example: a framework invents a `SendsWebhook` edge; after declaring
    /// `semantic_edge_kinds: [SendsWebhook]` it is counted and drawn as a semantic edge just like `ReadsDb`,
    /// with no change to `kinds.rs`.
    #[serde(default)]
    pub semantic_edge_kinds: Vec<String>,
    /// **Bridge edge kinds** introduced by this FKB (appended to the built-in [`EdgeKind::BRIDGE`] list).
    #[serde(default)]
    pub bridge_edge_kinds: Vec<String>,
    /// Whether to apply this knowledge's rules even when the framework was not recognised (default **false**).
    ///
    /// Framework-level rules carry strong framework assumptions (`Db::name` is a table name, the second argument
    /// of `Route::get` is a handler…). Applying them unconditionally to a project that **uses the same language
    /// but a different framework** means interpreting framework B's code with framework A's knowledge, producing
    /// a graph that **looks reasonable but is not trustworthy** — measured: applying ThinkPHP rules to a Laravel
    /// project conjures hundreds of `Table` / `HttpContract` nodes out of nothing.
    ///
    /// So by default they take effect only when the detector matches; this is switched on explicitly only for
    /// knowledge that really is "a generic fallback for that language" (carrying no concrete framework
    /// assumptions).
    #[serde(default)]
    pub apply_without_detection: bool,
}

/// Scope of a knowledge base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeScope {
    /// Framework knowledge: loaded along with framework recognition, applies to every project using that framework.
    #[default]
    Framework,
    /// Project knowledge: loaded along with project recognition, applies only to projects recognised as that project (its detectors matched).
    Project,
}

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
/// instead it reads `composer.json`'s PSR-4 autoload (`psr4_namespaces`) and resolves the handler against the real
/// class FQNs on the graph — see the resolver for the name-agnostic matching rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    /// PSR-4 root namespaces for the sub-project, derived at prepare time from `composer.json`'s `autoload.psr-4`.
    ///
    /// These drive the resolver: a handler string is resolved against the real class FQNs under these namespaces,
    /// so **no controller directory name is ever assumed**.
    pub psr4_namespaces: Vec<String>,
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
/// Example (CRMEB): `AuthTokenMiddleware` provides `Authentication`.
/// Matching is done on the **short name** only (the last segment after stripping the namespace) and is
/// case-insensitive — the same kind of middleware lives under different namespaces in different app directories
/// (`app\api\middleware\AuthTokenMiddleware` vs `app\kefuapi\middleware\KefuAuthTokenMiddleware`), yet the
/// semantics are expressed by the name.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[serde(default)]
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
#[serde(default)]
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
#[serde(tag = "kind", rename_all = "snake_case")]
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
#[serde(default)]
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
#[serde(default)]
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
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
    ///   no authentication at all (measured: 17 routes in the NestJS realworld sample were mislabelled as
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

fn default_true() -> bool {
    true
}

fn default_zero() -> usize {
    0
}

/// The read / write verb list of a data model (method names, case-insensitive).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DbVerbsSpec {
    /// Write verbs: `save` / `insert` / `update` / `delete` …
    #[serde(default)]
    pub write: Vec<String>,
    /// Read verbs: `find` / `select` / `value` / `count` …
    #[serde(default)]
    pub read: Vec<String>,
}

/// The forwarding target of magic methods (`@method` annotations).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MagicDelegationSpec {
    /// Name of the property to forward to (e.g. CRMEB's `dao`). Its type is inferred from how the property is injected, per the existing rules.
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
            psr4_namespaces: Vec::new(),
            controller_layer_depth: 1,
            app_anchor_dir: None,
            app_fallback: String::new(),
        }
    }
}

/// A framework recognition signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detector {
    /// A dependency exists in the manifest file.
    ManifestDependency {
        manifest: String,
        dependency: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// A characteristic file / directory exists.
    FileExists {
        path: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
}

fn default_conf() -> f32 {
    0.9
}

/// Framework root resolution rules.
///
/// Example: resolving `AppRoot = "app"` from `autoload.psr-4` in `composer.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRule {
    pub id: String,
    /// The fact key produced, e.g. `app_root`.
    pub key: String,
    pub source: RootSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// Fallback candidate directories; probed in order when resolution fails.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootSource {
    /// Read a value from a JSON manifest by pointer.
    ManifestJson {
        manifest: String,
        /// Dotted path, e.g. `autoload.psr-4`.
        pointer: String,
        /// Value-selection strategy.
        pick: PickStrategy,
    },
    /// Probe directly whether a directory exists.
    DirectoryExists { path: String },
    /// Read a value from a project manifest / config file (e.g. ThinkPHP's `config/database.php`) by
    /// dotted pointer. The *interpretation* of the file format is delegated to the tech-stack adapter
    /// (`TechStackAdapter::read_manifest`), so the kernel knows no language-specific file format.
    ///
    /// `manifest_php` is kept as a deserialization alias: existing FKB data still spells the PHP-specific
    /// tag, and it keeps loading unchanged while new data can adopt the language-agnostic `manifest`.
    #[serde(alias = "manifest_php")]
    Manifest {
        /// Path relative to the project root, e.g. `config/database.php`.
        manifest: String,
        /// Dotted path, e.g. `connections.mysql.prefix`.
        pointer: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickStrategy {
    /// Take the shallowest of all mapped directories.
    ShallowestDir,
    /// Take the first mapped directory.
    FirstDir,
    /// Take the mapped directory whose key name equals the given value.
    ByNamespaceKey,
}

/// A P3 symbol-table loader.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderSpec {
    pub id: String,
    /// Output symbol-table name: `schema` / `config_keys` / `i18n` / `facade_map` / `route_list`, etc.
    pub table: String,
    pub from: LoaderSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoaderSource {
    /// Read a single file and take values by key_path.
    File {
        path: String,
        #[serde(default)]
        key_path: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// Read in bulk by glob (e.g. `lang/*/*.php`); capture groups can extract the locale.
    Glob {
        pattern: String,
        /// Regex that extracts the locale from a path capture group (the first capture group).
        #[serde(default)]
        locale_regex: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// A constant table declared inline by FKB (e.g. FacadeMap — given by framework knowledge, not guessed).
    Inline { rows: Vec<Value> },
    /// A built-in loader (implemented by the pipeline, e.g. collecting `$table` and `Db::name` from PHP source).
    Builtin {
        name: String,
        #[serde(default)]
        params: Value,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Php,
    Json,
    Yaml,
    Sql,
    Text,
    #[default]
    Auto,
}

/// One rule: in a given phase, run a set of actions against the matched targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub phase: Phase,
    pub selector: Selector,
    pub binding: Vec<Action>,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// Per-rule language scoping. `None` inherits the owning FKB's `language` (current behaviour);
    /// `Some(list)` restricts the rule to the listed languages, where the sentinel `Language("*")`
    /// means "all languages" (used by cross-language universal rules).
    #[serde(default)]
    pub languages: Option<Vec<Language>>,
}

impl Rule {
    /// Whether this rule is effective for a sub-project of `sub_language`, given the language of the
    /// FKB that declared it (`fk_language`). See `languages` for the scoping semantics.
    pub fn applies_to(&self, fk_language: &Language, sub_language: &Language) -> bool {
        match &self.languages {
            Some(list) => list.iter().any(|x| x == sub_language || x.0 == "*"),
            None => fk_language == sub_language || fk_language.0 == "*",
        }
    }
}





/// A selector: decides what a rule acts on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    /// A call site, e.g. `Db::name('store_order')`.
    Call {
        /// Callee matching pattern; supports `|`-separated alternatives and `*` wildcards:
        /// `think\facade\Db::name|*:where|Db::raw`
        #[serde(default)]
        callee: Option<String>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },

    /// Inheritance / implementation / trait.
    Inheritance {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        with_property: Option<String>,
    },
    /// A config-file entry.
    ConfigEntry {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        key_path: Option<String>,
        /// Additional predicates (applied to config entries only).
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// A syntax declaration.
    Declaration {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        fqn_matches: Option<String>,
    },
    /// **A node on the graph** (P6 only: the selector is a node rather than source code).
    Node {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// P7 dynamic resolution: container make / event trigger / facade call / getters, etc.
    Dynamic {
        #[serde(default)]
        call: Option<String>,
        #[serde(default)]
        channel: Option<String>,
    },
}

/// A predicate (a `where` condition).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// The target (class) has a given property.
    HasProperty(String),
    /// The key exists in the authoritative symbol table.
    InSymbolTable { table: String, key_of: ValueSource },
    /// A column of the authoritative symbol table matches one of the given names.
    ColumnsMatch { table: String, names: Vec<String> },
    /// The node already carries a given annotation.
    HasAnnotation { kind: String },
    /// The given capability does not exist on the scope chain.
    NoneOfCapability(Vec<String>),
    /// Whether i18n has a missing locale.
    HasMissing(bool),
    /// fan_in is at least the threshold.
    FanInGte(u64),
    /// The argument count equals the given value.
    ArgCount(usize),
    /// The node name (or identity value) contains the given substring (case-insensitive).
    NameMatches(String),
    /// A node property equals the given value.
    PropertyIs { name: String, value: String },
    /// The `arg`-th argument of the call site (a string) starts with `prefix` (case-sensitive).
    /// Used to narrow matching by call-argument prefix, e.g. picking out only routes like `Route::get('crontab/...')`.
    ArgStartsWith { arg: usize, prefix: String },
    /// The config entry's value is an **array** with at least `n` elements (only effective for `kind: config_entry`).
    ///
    /// PHP config parsing expands array elements into independent entries (`listen.evt.0`) that exist **alongside**
    /// the parent entry (`listen.evt`). Since `key_path` is a plain substring wildcard, both `"*"` and
    /// `"listen.*"` match the parent and the child, so one event synthesises two nodes `evt` and `evt.0` (the
    /// latter is a scalar, cannot get a `HandledBy` edge, and is pure noise).
    /// This predicate admits array entries only: it drops the expanded scalar leaves and incidentally also filters
    /// out framework-level empty tags like `app_init => []`.
    EntryArityGte(usize),
    /// The node's **FQN** contains the given substring (case-insensitive).
    ///
    /// Why it is needed: many conventions hold by **namespace position**, which a node's short name cannot reveal
    /// — the semantics of the controller method `detail` come from its FQN `app\api\controller\Goods::detail`,
    /// and the auto-route rule can only be selected by the `\controller\` segment.
    FqnMatches(String),
    /// The node name is **not** in the given list (case-insensitive).
    ///
    /// A convention's scope always has to exclude language / framework hooks: `__construct` / `initialize` also
    /// live in the controller namespace but are definitely not HTTP entries. The list comes from FKB; the kernel
    /// knows no concrete name.
    NameNotIn(Vec<String>),
    /// The node is **not yet claimed**: it has neither an in-edge of the given kind nor a pending link of that
    /// kind pointing at it (see the shape comparison in [`crate::GraphWorkspace::claimed_by`]).
    ///
    /// "An explicit declaration beats a convention inference": a method already written into `Route::get` /
    /// `Route::resource` must not be scooped up a second time by a directory convention — otherwise a project like
    /// CRMEB, which registers routes exhaustively, sprouts thousands of duplicate endpoints. This is the same
    /// accounting principle as "better a missing edge than a wrong edge".
    NotClaimedBy(String),
}

/// A binding action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Action {
    /// Tag an annotation.
    Annotate(AnnotateAction),
    /// Synthesise a node.
    Synthesize(SynthesizeAction),
    /// Build an edge only.
    Link(LinkAction),
    /// **Project one class of edges onto another layer**.
    Project(ProjectAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotateAction {
    /// The phase in which it is expected to run (Pre / Post). Skipped when the rule's phase differs from this.
    pub phase: Option<Phase>,
    pub channel: AnnotationChannel,
    pub target: AnnotateTarget,
    pub annotations: Vec<AnnotationSpec>,
    pub merge: MergeStrategy,
    /// Scope: `[RouteSelf, EnclosingGroup, Global]`.
    pub scope: Option<Vec<String>>,
    pub r#where: Vec<Predicate>,
    /// Confidence decay when inheriting a capability from the scope chain.
    pub confidence_scale: Option<f32>,
}

impl Default for AnnotateAction {
    fn default() -> Self {
        Self {
            phase: None,
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            target: AnnotateTarget::Matched,
            annotations: Vec::new(),
            merge: MergeStrategy::MaxByKind,
            scope: None,
            r#where: Vec::new(),
            confidence_scale: None,
        }
    }
}

/// Annotation target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotateTarget {
    /// The node the selector matched directly.
    Matched,
    /// Derived from a field of the match result (e.g. `array_values` -> resolved into a class).
    FromField {
        source: ValueSource,
        resolve: Option<ResolveAs>,
    },
    /// Reference a node synthesised earlier by this same rule.
    SynthesizedRef(String),
}

impl Default for AnnotateTarget {
    fn default() -> Self {
        AnnotateTarget::Matched
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotationSpec {
    pub kind: String,
    pub subkind: Option<SubkindSource>,
    pub severity: Option<String>,
    pub confidence: f32,
    pub evidence: Option<Value>,
    pub channel: Option<AnnotationChannel>,
}

impl Default for AnnotationSpec {
    fn default() -> Self {
        Self {
            kind: String::new(),
            subkind: None,
            severity: None,
            confidence: 1.0,
            evidence: None,
            channel: None,
        }
    }
}

/// Where a subkind comes from: a literal / an authoritative symbol table / a computed value / a fan_in grade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubkindSource {
    Literal(String),
    FromSymbolTable {
        table: String,
        field: String,
        #[serde(default)]
        of: Option<ValueSource>,
    },
    FromFanIn {
        thresholds: FanInThresholds,
    },
    Computed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanInThresholds {
    pub high: u64,
    pub medium: u64,
    #[serde(default)]
    pub low_label: Option<String>,
    #[serde(default)]
    pub medium_label: Option<String>,
    #[serde(default)]
    pub high_label: Option<String>,
}

/// A synthesis action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthesizeAction {
    /// Node kind (an open string, e.g. `Table` / `HttpContract` / `Event` / `Queue` / `Cache` / `Topic`).
    ///
    /// If `subtype` is given too, the **subtype is promoted to kind** (`kind = subtype`);
    /// `category` then still records the final kind (a first-class semantic node equals its kind).
    pub node: NodeKind,
    /// Subtype (`Event` / `Queue` / `Cache`…), optional; when given it becomes the final kind.
    pub subtype: Option<String>,
    pub identity: IdentitySpec,
    pub fields: Vec<FieldSpec>,
    pub link: Option<LinkSpec>,
    pub confidence: f32,
    /// `MergeBy(key)` — several data sources merge into different fields of one node rather than into several nodes.
    pub modifiers: Vec<String>,
    /// Alias registration (written into by_alias automatically after synthesis).
    pub alias: Option<AliasSpec>,
    /// **Expand one call into N semantic nodes** (table-driven).
    ///
    /// Typical case: the REST resource route `Route::resource('cms', Ctrl::class)` is really 7 contracts in one
    /// statement (index / create / save / read / edit / update / delete).
    /// The expansion table comes from **FKB** (the kernel has zero framework knowledge); the kernel only: runs the
    /// same `identity` / `fields` / `link` once per variant in the table, injects the variant's `method` / `entry`
    /// into the two sources `{ expand_method: true }` / `{ expand_entry: true }`, and appends `path_suffix` to the
    /// computed path (after the `Route::group` prefix).
    pub expand: Option<ExpandSpec>,
}

/// An expansion table: one call -> N semantic nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExpandSpec {
    pub variants: Vec<ExpandVariant>,
    /// Allowlist source: the argument array of a chained call with this name **on the same statement line**
    /// (`->only(['index','delete'])`). When given, only the actions in the list are synthesised.
    pub only: Option<String>,
    /// Denylist source: `->except(['read'])`. When given, those actions are removed from the action table.
    pub except: Option<String>,
}

impl Default for ExpandSpec {
    fn default() -> Self {
        Self { variants: Vec::new(), only: None, except: None }
    }
}

/// One row of the expansion table: one action (e.g. REST's `index`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ExpandVariant {
    /// Action name (corresponding to the names written in `only` / `except`).
    pub name: String,
    /// HTTP method (read by `{ expand_method: true }`).
    pub method: Option<String>,
    /// Suffix appended to the path (e.g. `/create`, `/:id`).
    pub path_suffix: Option<String>,
    /// The handler's entry method name (read by `{ expand_entry: true }`).
    pub entry: Option<String>,
}

impl Default for SynthesizeAction {
    fn default() -> Self {
        Self {
            node: NodeKind(NodeKind::UNKNOWN.to_string()),
            subtype: None,
            identity: IdentitySpec::default(),
            fields: Vec::new(),
            link: None,
            confidence: 0.9,
            modifiers: Vec::new(),
            alias: None,
            expand: None,
            }
            }
}

/// The identity spec of a synthesised node.
///
/// **`identity` is the core of the whole Synthesize phase**: as long as three different rules compute the same
/// identity, their output merges idempotently into one node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentitySpec {
    /// `Fqn` / `Named` / `ContractId`。
    pub kind: SynthesizedKind,
    /// A single-value identity (`Fqn` / `Named`).
    pub value: Option<ValueSource>,
    /// Where the HTTP method of a `ContractId` comes from.
    pub method: Option<ValueSource>,
    /// Where the path of a `ContractId` comes from.
    pub path: Option<ValueSource>,
    #[serde(default)]
    pub normalize: Vec<NormalizeStep>,
    /// Fallback source when the primary identity cannot be obtained (or `require_class` judges it not a class).
    ///
    /// Example: a queue topic prefers `arg:0` (the Job class in the argument) and falls back to `owner_class`
    /// (the class that made the call) when that fails. When both sources compute the same identity they merge
    /// idempotently into one node.
    #[serde(default)]
    pub value_fallback: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct FieldSpec {
    pub name: String,
    pub value: Option<ValueSource>,
    /// Accumulating merge: `{ key: locale, value: text }`.
    pub accumulate: Option<AccumulateSpec>,
    /// Supplement fields from the authoritative symbol table.
    pub from_symbol_table: Option<SymbolFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccumulateSpec {
    pub key: ValueSource,
    pub value: ValueSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolFieldSpec {
    pub table: String,
    pub field: String,
    #[serde(default)]
    pub of: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkSpec {
    pub kind: EdgeKind,
    /// Source of the edge's other end (e.g. a handler string).

    pub to: Option<ValueSource>,
    /// The target **method name** (optional). When given, prefer connecting to `Class::method`.
    ///
    /// Two uses: (1) the method part of an array-style handler (item 1 of `[Ctrl::class, 'method']`); (2) letting
    /// FKB decide the "consumer entry method" instead of the kernel's hard-coded list of
    /// `handle`/`fire`/`doJob`/`__invoke`/`run`.
    #[serde(default)]
    pub to_method: Option<ValueSource>,
    /// Fallback source when `to` yields no target (e.g. falling back to `receiver_class` when a queue consumer's `arg:0` cannot be resolved).
    #[serde(default)]
    pub to_fallback: Option<ValueSource>,
    /// Direction: incoming (the source points at the new node) / outgoing (the new node points at the source) / to_target.
    pub direction: Direction,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The matched caller ——> the newly synthesised node.
    #[default]
    Incoming,
    /// The newly synthesised node ——> the matched caller.
    Outgoing,
    /// The newly synthesised node ——> the target resolved from `to`.
    ToTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AliasSpec {
    pub namespace: String,
    pub key: ValueSource,
    pub qualifier: Option<ValueSource>,
}

/// A value source (structured, for convenient YAML authoring).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ValueSource {
    /// The n-th argument.
    pub arg: Option<usize>,
    /// When the argument is an **array**, take the n-th item by index.
    ///
    /// For array-style handlers — Laravel's main form `Route::get('/x', [Ctrl::class, 'method'])` has the class
    /// name in `arg1[0]` and the method name in `arg1[1]`, which previously could not be expressed (only the whole
    /// array could be taken).
    pub element: Option<usize>,
    /// Take a field when the argument is an object literal (e.g. `uni.request({url:..})`).
    pub field: Option<String>,
    /// Read a class property (e.g. a Model's `$table`).
    pub property: Option<String>,
    /// Take the current node itself (class short name / FQN).
    #[serde(rename = "self")]
    pub self_value: Option<bool>,
    /// Skip the first n segments of key_path and take what remains (joined with dots).
    pub path_segment: Option<usize>,
    /// The call site's method name (e.g. the `post` of `Route::post`).
    pub method_name: Option<bool>,
    /// The config entry's own value.
    pub entry_value: Option<bool>,
    /// Take all values of an array (one-to-many).
    pub array_values: Option<bool>,
    /// Take the array length.
    pub array_length: Option<bool>,
    /// Take the key_path of a config-file entry.
    pub key_path: Option<bool>,
    /// Take the file-name stem.
    pub file_stem: Option<bool>,
    /// Take the current locale (when loading i18n).
    pub locale: Option<bool>,
    /// Take "the class that made the call": the class FQN after stripping the trailing `::method` from
    /// `owner_fqn`.
    ///
    /// Generic semantics: for when the identity is "the calling class" rather than some argument (e.g. in the
    /// self-enqueue pattern the queue's Job / topic is the class that produced it, typically `QueueTrait::dispatch`
    /// setting the consumer to the calling class via `->job(__CLASS__)`). This is a framework-agnostic extraction
    /// capability.
    pub owner_class: Option<bool>,
    /// Take the **member name** at the end of `owner_fqn` (method / field), complementing `owner_class`.
    ///
    /// Typical use: the call site of a Java method-level annotation (`@GetMapping`) has `owner_fqn` = the method FQN
    /// `com.example.Ctrl.list`, and `owner_member` extracts `list` so that a link's `to_method` connects the
    /// `HandledBy` edge precisely to the **handler method** node (rather than the controller class), letting a
    /// perspective keep drilling down along the method's call chain. For a class-level annotation `owner_fqn` is
    /// already a class FQN, so this yields the class short name; when no method node is found,
    /// `find_target_node` falls back to the class node, which is semantically safe.
    pub owner_member: Option<bool>,
    /// Take the call site's receiver class: resolve `receiver` into an FQN via import aliases.
    ///
    /// Unlike `owner_class` (the class the call sits in), this is "the receiver class of the callee", e.g. in
    /// `QueueThink::push()` resolving `QueueThink` through an alias into `think\facade\Queue`.
    pub receiver_class: Option<bool>,
    /// Take the "primary domain type" the call site is about (`CallSiteFact.entity`), e.g. the event type
    /// `OrderPlacedEvent`. Used to merge the publisher and the subscriber of "the same event type" onto one
    /// `Event` node (rather than naming each after its method). When it cannot be obtained (the parser did not
    /// recognise it), return `None` overall and let `value_fallback` (e.g. `owner_member`) cover it.
    pub entity: Option<bool>,
    /// With `resolve: class_const`, if the resolved result does not exist as a class node in the codebase, return
    /// `None` overall (instead of treating a variable name / literal as a class). For fallbacks like "prefer the
    /// Job class in the argument, otherwise fall back to `owner_class`", so strings like `$action` cannot pollute
    /// the semantic identity.
    pub require_class: Option<bool>,
    /// Accept **literals only** (strings / scalars); reject variables and expression text.
    ///
    /// `arg` evaluates a variable / concatenated expression into `FactValue::Unknown(Some(verbatim))` (see
    /// `gt-adapter-parser::php::value`), so trusting it directly would take source text like `$name` or
    /// `self::X . $y` as an identity and conjure garbage semantic nodes. Symmetric with `require_class`: if the
    /// test fails, return `None` overall and let `value_fallback` cover it.
    pub require_literal: Option<bool>,
    /// A literal.
    pub literal: Option<String>,
    /// A nested source: `{ source: { arg: 1 }, field: 'url' }`.
    pub source: Option<Box<ValueSource>>,
    /// A transformation (e.g. `class_to_topic`, `snake_plural`).
    pub transform: Option<TransformSpec>,
    /// A normalisation chain.
    pub normalize: Option<Vec<NormalizeStep>>,
    /// Resolution method.
    pub resolve: Option<ResolveAs>,
    /// Default value when it cannot be obtained.
    pub default: Option<String>,
    /// Multi-segment joining: `{ path: [{file_stem:true},{key_path:true}], join: '.' }`.
    pub path: Option<Vec<ValueSource>>,
    pub join: Option<String>,
    /// Take the HTTP method of the **current expansion variant** (`expand.variants[].method`).
    ///
    /// Only has a value together with `Synthesize.expand`: when one call expands into N semantic nodes, each
    /// variant has its own method / path suffix / entry method (as in a REST resource route).
    pub expand_method: Option<bool>,
    /// Take the entry method name of the **current expansion variant** (`expand.variants[].entry`), so that
    /// `link.to_method` connects the edge precisely to "the method handling that action".
    pub expand_entry: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransformSpec {
    pub snake_plural: Option<bool>,
    pub snake: Option<bool>,
    pub strip_namespace: Option<bool>,
    pub class_to_topic: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAs {
    /// `Foo::class` -> a fully qualified class name -> look up by_name.
    ClassConst,
    /// Assemble an FQN from a string reference to a `class::method` / function (e.g. `'Login/appleLogin'`).
    /// Generic string → callable resolver: route handlers, queue string-jobs, `invokeAction`, etc. — not route-only.
    MethodRef,
    /// Look up the by_alias index.
    ByAlias,
    /// Use it directly as a name.
    AsIs,
}

/// A normalisation step (the key to idempotent identity merging).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizeStep {
    StripPrefix(Vec<String>),
    Lower,
    Upper,
    /// Guarantee a leading `/`.
    LeadingSlash,
    /// Plural to singular.
    Singularize,
    /// Class name to `snake_case` plural (the Model table-name convention).
    SnakePlural,
    /// Strip the namespace, keep only the last segment.
    StripNamespace,
    /// Take the last segment of a **dot-separated** path: `app.tasks.send_email` -> `send_email`.
    ///
    /// It differs from [`Self::StripNamespace`] by only one separator, `.`: Python / Java namespaces are dot
    /// separated, while `StripNamespace` deliberately does **not** split on `.` (otherwise it would split Java
    /// auto-route package names too and change existing behaviour). Hence a separate **additive** step, aimed at
    /// scenarios like "the long and short names of one semantic entity must merge" — e.g. a Celery task's
    /// registrar only has the short name while its dispatcher restored a fully qualified name via `import`, and
    /// without normalisation they split into two nodes.
    ShortName,
    /// Path-parameter segment normalisation: every segment starting with `:` folds into `:*`.
    ///
    /// A key step of the contract bridge: the backend route writes `invoice/detail/:id` while the frontend's
    /// concatenated URL `'invoice/detail/' + id` normalises to `invoice/detail/:param` — different parameter
    /// names but the **same shape**, and HTTP matching only ever looks at the shape. Without folding, the two
    /// never merge onto one node and the route perspective "cannot see the frontend".
    ParamWildcard,
    /// Drop the query string starting at `?` (page-navigation URLs often carry `?id=1`, but a route identity only
    /// looks at the path): `uni.navigateTo({ url: '/pages/detail?id=1' })` and the `/pages/detail` in `pages.json`
    /// converge onto the same `Page` node.
    StripQuery,
    Trim,
    Replace { from: String, to: String },
}

/// An edge-only action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkAction {
    pub kind: EdgeKind,
    pub from: Option<ValueSource>,
    pub to: Option<ValueSource>,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

/// An edge-projection action: project **one class of edges** from the layer they live in onto another layer.
///
/// Walk every `along` out-edge of the matched node; the start walks along the `from` edge-kind chain and the end
/// along the `to` chain, and a `kind` edge is built between the two landing points. It is **one-to-many**: an
/// entity with several `@ManyToOne` produces several foreign-key edges (something `Link` cannot do — its two ends
/// can each take only one name).
///
/// Why the kernel has to provide this ability: "class -> the table it maps to" is essentially **walking one hop
/// along `MapsTo`**, while `ValueSource` only understands names (`self_value` / `property`) and cannot reach "the
/// far end of an edge". Which edge to walk is still declared entirely by FKB — the kernel still knows nothing
/// about TypeORM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ProjectAction {
    /// The edge kind produced.
    pub kind: EdgeKind,
    /// Walk every out-edge **of this kind** on the matched node (no edge of that kind -> no output, so unrelated nodes are skipped naturally).
    pub along: EdgeKind,

    /// Walk from the **edge's start** along this edge-kind chain to the landing point; empty means the landing point is the start itself.
    #[serde(default)]
    pub from: Vec<String>,
    /// Walk from the **edge's end** along this edge-kind chain to the landing point; empty means the landing point is the end itself.
    #[serde(default)]
    pub to: Vec<String>,
    pub confidence: Option<f32>,
}

/// Levels of the P7 resolution funnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveTier {
    /// L1 literal FQN, e.g. `app()->make(StoreOrderServices::class)`.
    Exact = 1,
    /// L2 container registry (the bindings in `provider.php`).
    Registry = 2,
    /// L3 alias index (Facade / event name / getter).
    Alias = 3,
    /// L4 convention (namespace concatenation, deriving a table name from a class name).
    Convention = 4,
    /// L5 constant propagation.
    ConstProp = 5,
    /// L6 intersection with a finite universe (the 203 tables of the schema).
    Intersection = 6,
    /// L7 completely unknown.
    Unknown = 7,
}

impl ResolveTier {
    /// The base confidence of that level.
    pub fn base_confidence(self) -> f32 {
        match self {
            Self::Exact => 1.0,
            Self::Registry => 0.95,
            Self::Alias => 0.85,
            Self::Convention => 0.8,
            Self::ConstProp => 0.6,
            Self::Intersection => 0.7,
            Self::Unknown => 0.3,
        }
    }
}

/// The product of one dynamic resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub tier: ResolveTier,
    /// Candidate nodes; empty means unresolved.
    pub candidates: Vec<super::ids::NodeId>,
    pub confidence: f32,
    pub evidence: String,
}

impl Resolution {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            tier: ResolveTier::Unknown,
            candidates: Vec::new(),
            confidence: ResolveTier::Unknown.base_confidence(),
            evidence: reason.into(),
        }
    }
    pub fn resolved(tier: ResolveTier, candidate: super::ids::NodeId, evidence: impl Into<String>) -> Self {
        Self { tier, candidates: vec![candidate], confidence: tier.base_confidence(), evidence: evidence.into() }
    }
}

/// Call-site context (used by selector matching).
#[derive(Debug, Clone)]
pub struct CallContext {
    pub owner_fqn: String,
    pub owner_node: Option<super::ids::NodeId>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<super::syntax::FactValue>,
    pub span: Span,
    pub sub_project: Option<super::ids::SubProjectId>,
    pub file_path: String,
}

/// A P7 dynamic-resolution declaration.
///
/// Letting FKB decide "which calls need dynamic resolution" too, rather than hard-coding it in the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverSpec {
    pub id: String,
    /// Matching pattern, e.g. `app()->make|app|make`.
    #[serde(default)]
    pub call: Option<String>,
    pub strategy: ResolveStrategy,
    /// Starting resolution level (containers default to Registry).
    #[serde(default)]
    pub from_tier: Option<ResolveTier>,
}

/// A concrete resolution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStrategy {
    /// `app()->make(X)` / `app('x')`: L1 literal -> L2 registry -> L4 convention -> L6 intersection.
    Container,
    /// `event('x')`: look up the L3 alias index.
    Event,
    /// `Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`:
    /// arg0 resolves to an event node (L3 alias), then a `HandledBy` edge goes from the event node to the arg1 listener class.
    EventListen,
    /// `think\facade\Cache::get()`: look up the L3 FacadeMap.
    Facade,
    /// `$order->status_text`: a composite-key accessor alias.
    Accessor,
    /// `Route::post('p','Login/appleLogin')`: handler-pattern resolution.
    Handler,
    /// `$services->appAuth()`: resolve an instance method call by variable type.
    ///
    /// Type sources: method parameter type hints (the ThinkPHP controller DI convention) and constructor property
    /// injection (`__construct(T $x){ $this->p = $x; }`), recorded by P2 and consumed by this strategy.
    VariableType,
}
