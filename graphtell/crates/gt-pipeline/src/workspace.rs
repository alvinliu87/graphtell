//! Pipeline in-memory workspace.
//!
//! Each phase builds, queries, and annotates the graph in memory; after all phases the application layer persists the accumulated
//! [`GraphDelta`] in one shot (guaranteeing phase-level atomicity while keeping domain logic free of transaction APIs).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use gt_domain::model::{
    AliasEntry, Annotation, Diagnostic, Edge, EdgeKind, FactValue, GraphDelta, IdentityKey,
    Language, MergeStrategy, NewAnnotation, NewEdge, NewNode, Node, NodeId, NodeKind, Phase,
    ProjectId, Severity, Span, SubProjectId, SynthesizedKind,
};
use gt_domain::model::syntax::{SignCompareFact, VariableAssignFact};
use serde_json::{json, Value};

use crate::engine::NS_SEPARATORS;

/// One call site's record in the workspace.
#[derive(Debug, Clone)]
pub struct CallRecord {
    /// The `CallSite` node of this call site itself (Taint's precise landing point).
    pub node: NodeId,
    /// The node of the owning method (for building semantic edges, e.g. `Method --ReadsDb--> Table`).
    pub owner: NodeId,
    pub owner_fqn: String,
    /// The **class** FQN this call site belongs to (propagated by parser from `CallSiteFact.owner_class`).
    /// Used directly by the `owner_class` binding; PHP side is `None` (falls back to string splitting).
    pub owner_class: Option<String>,
    pub callee: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<FactValue>,
    /// The target table name propagated through a chained facade call (`Db::name('goods')->insert()`),
    /// so P7 can lay the terminal verb as `WritesDb` / `ReadsDb`.
    pub db_table: Option<String>,
    /// Whether this call site is inside a loop body (parser fact, same source as the CallSite node's `in_loop` property).
    /// Used by "loop / batch" criteria like P12/P13 — the rule side reads the node property, the phase side reads here.
    pub in_loop: bool,
    /// The "primary domain type" this call site concerns (propagated from `CallSiteFact.entity`), e.g. event type
    /// `OrderPlacedEvent`. Used by `value: { entity: true }` to merge same-kind publish / subscribe into
    /// the same semantic node.
    pub entity: Option<String>,
    pub span: Span,
    pub file: String,
    pub sub: Option<SubProjectId>,
    pub language: Language,
}

/// Config-entry record (from `return [...]`-style PHP files).
#[derive(Debug, Clone)]
pub struct ConfigRecord {
    pub file: String,
    pub key_path: String,
    pub value: FactValue,
    pub span: Span,
    pub sub: Option<SubProjectId>,
    pub locale: Option<String>,
    pub file_stem: Option<String>,
}

/// Inheritance / implementation / trait record.
#[derive(Debug, Clone)]
pub struct InheritRecord {
    pub child: NodeId,
    pub child_fqn: String,
    pub base: String,
    pub kind: EdgeKind,
    pub sub: Option<SubProjectId>,
    pub file: String,
    pub span: Span,
}

/// Route-group range: `Route::group('v2', function () { ... })`.
///
/// ThinkPHP appends the group prefix to the paths of **all** routes inside the group:
/// `Route::group('v2', fn(){ Route::get('order/x') })`'s real path is `/v2/order/x`.
/// If we build the contract ID only from `arg:0`, we'd drop `v2`, causing:
/// * the backend contract mismatches the real request path and can't converge with the frontend `CallsHttp`;
/// * same-named sub-paths under different versions / groups would be merged idempotently into one node.
#[derive(Debug, Clone)]
pub struct RouteGroup {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub prefix: String,
}

/// **One** middleware hung on a route / route group (one item of `->middleware(X::class, true)`).
#[derive(Debug, Clone)]
pub struct RouteGuard {
    /// The middleware class's fully-qualified name (`app\api\middleware\AuthTokenMiddleware`).
    pub class: String,
    /// The **second arg** at mount time: CRMEB uses it to distinguish "must login / optional login"
    /// (`AuthTokenMiddleware::class, false` == reachable without auth), the most informative signal for `auth.public`.
    /// Laravel's `->middleware('auth:api')`-style aliases also land here first; alias → class restoration is left to later phases.
    pub arg: Option<String>,
}

/// A "route range with middleware": could be `Route::group(fn){...}->middleware(...)`,
/// or a single route registered on its own with `->middleware(...)`.
///
/// Deliberately **two separate structures** from [`RouteGroup`]: the latter only serves "group-prefix fill-in" and is long stable;
/// this one covers a wider range (**incl. prefix-less groups** and single routes), so any regression won't touch the prefix backbone.
#[derive(Debug, Clone)]
pub struct RouteGuardScope {
    pub file: String,
    pub start_line: u32,
    pub end_line: u32,
    pub guards: Vec<RouteGuard>,
}

/// Whether it's a **type node** participating in the "short name → FQN" index.
///
/// Only classes / interfaces / traits / enums participate: the short-name index's semantics is exactly
/// "`StoreOrderServices` → `app\services\order\StoreOrderServices`".
/// If methods and functions also entered the index, high-frequency global function names (`config`
fn is_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        NodeKind::CLASS | NodeKind::INTERFACE | NodeKind::TRAIT | NodeKind::ENUM
    )
}

/// Unresolved dynamic link (produced by P5, resolved by P7).
#[derive(Debug, Clone)]
pub struct PendingLink {
    pub from: NodeId,
    pub kind: EdgeKind,
    pub raw: String,
    /// **Target member name** (optional): the entry method explicitly given by FKB in `link.to_method`,
    /// typically the `index` / `delete` expanded from a resource route (`{ expand_entry: true }`).
    ///
    /// Why it must reach P7: the `handler` often only writes the class (`Route::resource('level','v1.agent.AgentLevel')`),
    /// the real entry method comes from the **expansion variant** (framework knowledge); at the P5 link stage we don't yet know the class's FQN
    /// (its FQN is derived from PSR-4 + the real class nodes on the graph, not a hard-coded template), so passing `to_method` straight to
    /// `find_target_node` would miss; it can only degrade to a PendingLink. If we didn't carry the method name,
    /// the `raw` P7 gets would be only the class name — whether or not the class has a matching method, the edge degrades to class-level
    /// (measured: CRMEB had 164 resource routes falling to `Class`, but `AgentLevel::delete` was clearly in the graph).
    pub method: Option<String>,
    pub resolve: gt_domain::model::ResolveAs,
    pub confidence: f32,
    pub sub: Option<SubProjectId>,
    pub file: String,
    pub line: u32,
}

/// Per-project exclusive node-id segment size.
///
/// A node's `id` is a cross-project shared primary key, persisted with `INSERT OR REPLACE` (`SqliteStore::apply`).
/// Originally we avoided collisions by "reading global `MAX(id)` once at pipeline start": fine for a single pipeline,
/// but under concurrent build several pipelines read the same MAX at the same start — none has flushed yet,
/// so they allocate completely overlapping id ranges, and the later-committed project REPLACEs the earlier's node rows one by one.
/// (Measured: after creating 10 projects in a row, `nodes` only had the last-committed project's data,
/// while `edges` looked fine because its unique key includes `project_id` and has no FK.)
///
/// After switching to per-project segmentation, the id range depends only on `project_id`, independent of runtime order and concurrency,
/// so it neither conflicts nor needs to serialize builds. Single-project cap is 1 billion nodes.
const NODE_ID_STRIDE: i64 = 1_000_000_000;

/// The start of this project's node-id segment (in-segment local id starts at 1).
fn node_id_base(project_id: ProjectId) -> i64 {
    project_id.get().saturating_mul(NODE_ID_STRIDE) + 1
}

/// Graph workspace.
pub struct GraphWorkspace {
    project_id: ProjectId,
    next_node: i64,
    next_edge: i64,
    next_ann: i64,
    nodes: BTreeMap<i64, Node>,
    edges: Vec<Edge>,
    edge_keys: HashSet<(String, i64, i64)>,
    annotations: Vec<Annotation>,
    by_fqn: HashMap<String, i64>,
    by_identity: HashMap<String, i64>,
    /// Contract bridge (HttpContract) indexed by `path` to node id, and tags whether the node is a wildcard method
    /// (`ANY` / `RULE`). Used to converge "method-agnostic" auto-route endpoints with the frontend's concrete method
    /// (`POST` / `GET`...) onto the same node, otherwise the route view can't align frontend and backend.
    contract_path_index: HashMap<String, (i64, bool)>,
    by_alias: BTreeMap<(String, String, String), i64>,
    fan_in: HashMap<i64, u32>,
    fan_out: HashMap<i64, u32>,
    pub calls: Vec<CallRecord>,
    /// **Same-line call index**: `(file, start line) → [(method name, string in first arg)]`.

    /// Chained modifiers (`Route::resource('x', C::class)->except(['read'])`) are parsed into
    /// **another call site on the same line**; the expansion table must fetch its args by line to know which actions take effect.
    /// Why a separate index instead of iterating `calls`: at P4/P5 execution `calls` is
    /// `std::mem::take`n out of the workspace (to avoid borrow conflicts), so iterating now yields an empty table.
    /// Hence register alongside it when P2 builds call sites (only calls with args take space).
    chained: HashMap<(String, u32), Vec<(String, Vec<String>)>>,
    pub configs: Vec<ConfigRecord>,
    /// Signature-value equality comparison (fed by `cf_ast` from parse facts), for `phase::sign` to judge signature quality.
    pub sign_compares: Vec<SignCompareFact>,
    pub inherits: Vec<InheritRecord>,
    pub pending_links: Vec<PendingLink>,
    /// Route-group ranges (`Route::group('v2', ...)`), for contract-ID group-prefix fill-in.
    pub route_groups: Vec<RouteGroup>,
    /// Route-guard ranges (`Route::xxx(...)->middleware(...)`), for querying "which middleware this route passed".
    pub route_guard_scopes: Vec<RouteGuardScope>,
    /// Whether FKB authorizes "building a `Middleware` node even when the guarded class isn't found in the graph".
    ///
    /// Declared by `route_guards.synthesize_unresolved` (JS / Python function-value middleware = true,
    /// PHP class middleware = false); P14 decides whether to build the node or skip per this.
    pub synthesize_unresolved_guards: bool,
    pub symbols: BTreeMap<String, BTreeMap<String, Value>>,
    /// Class property default values: `(class_node_id, property_name) → value`.
    prop_values: HashMap<(i64, String), FactValue>,
    /// File path → File node.
    file_nodes: HashMap<String, i64>,
    /// File path → source-file id (used by `resolve_name_in_file` to look up the import table by path).
    file_id_by_path: HashMap<String, i64>,
    /// Source-file id → path (reverse of `file_id_by_path`).
    ///
    /// P6's selector acts on **graph nodes**; a node's `file_id` must be restored to `file:line`
    /// to give resolvable / synthesized nodes a jumpable origin.
    source_path_by_id: HashMap<i64, String>,
    /// **Each file's own** `use` import table: `source-file id → (short name lowercased → FQN)`.
    ///
    /// Why it must be per-file: PHP short names resolve **per file** (`use think\facade\Cache;`
    /// and `use app\model\other\Cache;` mean completely different things in different files). A single global
    /// short-name index and a first-come-first-served global `imports` symbol table would treat `Cache::tag()` as
    /// `app\model\other\Cache` (a Model), spuriously creating a `Model --MapsTo--> Table(cache)` class-level
    /// semantic edge and polluting the whole call chain.
    file_imports: HashMap<i64, HashMap<String, String>>,
    /// Out-edge adjacency: `from → [(kind, to)]`, for ancestor-chain judgment.
    out_edges: HashMap<i64, Vec<(String, i64)>>,
    /// In-edge adjacency: `to → [(kind, from)]`.
    ///
    /// Convention rules use this to judge "does this node already have an authoritative source": when an explicitly registered route
    /// points at some controller method, we shouldn't also cover it with a convention-inferred contract (explicit beats inferred).
    in_edges: HashMap<i64, Vec<(String, i64)>>,
    /// Short-name index: `short name (lowercased) → [node_id]`, replacing full-table linear scan.
    by_short: HashMap<String, Vec<i64>>,
    /// Table-name index: `(normalized) table name → Table node id`, for P7 to look up the synthesized Table node by the name propagated from `Db::name('x')` (synthesized nodes have no fqn, can't go through `by_fqn`).
    by_table_name: HashMap<String, i64>,
    /// DB table prefix (from project config, for identity normalization and symbol-table lookup).
    table_prefixes: Vec<String>,
    /// Parent-type-name index: `child FQN → [parent FQN]`.
    ///
    /// Must use **names** not node edges: CRMEB's chain is
    /// `StoreOrder → crmeb\basic\BaseModel → think\Model`,
    /// and `think\Model` is in vendor (excluded by P0), so the edge isn't on the graph.
    supertypes: HashMap<String, Vec<String>>,
    /// Subtype-name index (inheritance chain downstream): `parent FQN → [child FQN]`, reversed from `supertypes`.
    ///
    /// Used to reverse-lookup the tables mapped by instances (subclasses) for "read / write verbs in base-class methods": yoshop / CRMEB's
    /// read verbs (`$this->select` / `getAll`) are often written in base classes like `app\common\model\X`,
    /// while `MapsTo` edges only hang on concrete subclasses (`app\api\model\X`) — without walking to subclasses, these verbs
    /// can never produce `ReadsDb`, and the route can only fall back to a vague "maps to".
    subtypes: HashMap<String, Vec<String>>,
    /// Method param types: `method FQN → [(var name, type FQN)]`, for P7 to resolve `$var->method()`.
    param_types: HashMap<String, Vec<(String, String)>>,
    /// Class property types: `class FQN → {prop name → type FQN}` (from constructor injection `$this->p = $param`,
    /// typed-property declarations, and right-side-typed assignments like `$this->p = new Y()`).
    prop_types: HashMap<String, HashMap<String, String>>,
    /// Class magic-method names declared via `@method` (`class FQN → {method name}`, for `__call` forwarding resolution).
    magic_methods: HashMap<String, HashSet<String>>,
    /// In-method local-variable types: `method FQN → {var name → type FQN}` (`$x = new Y()` / `Y::make()`).
    ///
    /// Serves "temporary-object calls" like `$x->m()`: they are neither param-type hints nor fields,
    /// without this layer the whole chain breaks on the most common single line.
    local_types: HashMap<String, HashMap<String, String>>,
    /// Sub-project facts (app_root, etc.), keyed by sub_project_id.
    pub facts: BTreeMap<i64, BTreeMap<String, Value>>,
    pub diagnostics: Vec<Diagnostic>,
    /// Local-variable assignment (`$sql = ...;`), by (owner_fqn, var) → right-side source index,
    /// for `phase::taint` to trace variable origin backward within the same function.
    pub variable_assignments: Vec<VariableAssignFact>,
    delta: GraphDelta,
}

impl GraphWorkspace {
    pub fn new(project_id: ProjectId) -> Self {
        Self {
            project_id,
            // Node ids are segmented per project, without relying on "read global MAX once at start" (see `NODE_ID_STRIDE`).
            next_node: node_id_base(project_id),
            next_edge: 1,
            next_ann: 1,
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            edge_keys: HashSet::new(),
            annotations: Vec::new(),
            by_fqn: HashMap::new(),
            by_identity: HashMap::new(),
            contract_path_index: HashMap::new(),
            by_alias: BTreeMap::new(),
            fan_in: HashMap::new(),
            fan_out: HashMap::new(),
            calls: Vec::new(),
            chained: HashMap::new(),
            configs: Vec::new(),
            sign_compares: Vec::new(),
            inherits: Vec::new(),
            pending_links: Vec::new(),
            route_groups: Vec::new(),
            route_guard_scopes: Vec::new(),
            synthesize_unresolved_guards: false,
            symbols: BTreeMap::new(),
            prop_values: HashMap::new(),
            file_nodes: HashMap::new(),
            file_id_by_path: HashMap::new(),
            source_path_by_id: HashMap::new(),
            file_imports: HashMap::new(),
            out_edges: HashMap::new(),
            in_edges: HashMap::new(),
            by_short: HashMap::new(),
            by_table_name: HashMap::new(),
            supertypes: HashMap::new(),
            subtypes: HashMap::new(),
            param_types: HashMap::new(),
            prop_types: HashMap::new(),
            magic_methods: HashMap::new(),
            local_types: HashMap::new(),
            table_prefixes: Vec::new(),
            facts: BTreeMap::new(),
            diagnostics: Vec::new(),
            variable_assignments: Vec::new(),
            delta: GraphDelta::new(project_id),
        }
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    /// Register a call site's "chained modifier" info (called when P2 builds the call site).
    ///
    /// Only record (method name, string in first arg): `->except(['read'])` → `("except", ["read"])`.
    pub fn index_chained(&mut self, file: &str, line: u32, method: Option<&str>, args: &[FactValue]) {
        let Some(m) = method else { return };
        let mut strings: Vec<String> = Vec::new();
        for a in args {
            match a {
                FactValue::String(s) => strings.push(s.clone()),
                FactValue::Array(items) => {
                    for (_, v) in items {
                        if let FactValue::String(s) = v {
                            strings.push(s.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if strings.is_empty() {
            return;
        }
        self.chained
            .entry((file.to_string(), line))
            .or_default()
            .push((m.to_string(), strings));
    }

    /// Get the arg array of a chained call **on the same line** (used by `expanded_actions`).
    pub fn chained_strings(&self, file: &str, line: u32, method: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (m, vals) in self
            .chained
            .get(&(file.to_string(), line))
            .into_iter()
            .flatten()
        {
            if m == method {
                out.extend(vals.iter().cloned());
            }
        }
        out
    }

    /// Record a param's type: `method FQN → (var name, type FQN)`.
    pub fn add_param_type(&mut self, owner_fqn: &str, var: &str, type_fqn: &str) {
        let entry = self.param_types.entry(owner_fqn.to_string()).or_default();
        if !entry.iter().any(|(v, _)| v == var) {
            entry.push((var.to_string(), type_fqn.to_string()));
        }
    }

    /// Look up a variable's type within a method (for `$var->method()` resolution).
    pub fn param_type(&self, owner_fqn: &str, var: &str) -> Option<&str> {
        self.param_types
            .get(owner_fqn)
            .and_then(|list| list.iter().find(|(v, _)| v == var))
            .map(|(_, t)| t.as_str())
    }

    /// Record a class property's type (from constructor injection `$this->p = $param`).
    pub fn set_prop_type(&mut self, class_fqn: &str, prop: &str, type_fqn: &str) {
        self.prop_types
            .entry(class_fqn.to_string())
            .or_default()
            .insert(prop.to_string(), type_fqn.to_string());
    }

    /// Look up a class property's type (for `$this->prop->method()` resolution).
    pub fn prop_type(&self, class_fqn: &str, prop: &str) -> Option<&str> {
        self.prop_types
            .get(class_fqn)
            .and_then(|m| m.get(prop))
            .map(|s| s.as_str())
    }

    /// Register a class's `@method` magic-method name.
    pub fn set_magic_methods(&mut self, class_fqn: &str, names: &[String]) {
        if names.is_empty() {
            return;
        }
        self.magic_methods
            .entry(class_fqn.to_string())
            .or_default()
            .extend(names.iter().cloned());
    }

    /// All table nodes this class (incl. ancestors) maps to via `MapsTo`.
    ///
    /// "class maps to table" is a model class's static identity; P7 uses this to upgrade FKB-declared read / write verb calls like `$model->save()` into real `WritesDb` / `ReadsDb`.
    pub fn mapped_tables(&self, class_fqn: &str, maps_to_kind: &str) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        let mut stack = vec![class_fqn.to_string()];
        let mut visited: HashSet<String> = HashSet::new();
        let mut steps = 0;
        while let Some(c) = stack.pop() {
            steps += 1;
            if steps > 50 || !visited.insert(c.clone()) {
                continue;
            }
            if let Some(edges) = self.out_edges.get(&{
                // class FQN → node id: go through short / full name index
                self.find_by_name(&c).map(|id| id.get()).unwrap_or(0)
            }) {
                for (k, to) in edges {
                    if k == maps_to_kind {
                        out.push(NodeId(*to));
                    } else if k == EdgeKind::RESOLVES_TO {
                        if let Some(tf) = self.node(NodeId(*to)).and_then(|n| n.fqn.clone()) {
                            stack.push(tf);
                        }
                    }
                }
            }
            for p in self.parents_of(&c) {
                stack.push(p);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// Whether this class (or its ancestor) declared a magic method named `method`.
    pub fn declares_magic_method(&self, class_fqn: &str, method: &str) -> bool {
        let mut stack = vec![class_fqn.to_string()];
        let mut visited: HashSet<String> = HashSet::new();
        let mut steps = 0;
        while let Some(c) = stack.pop() {
            steps += 1;
            if steps > 50 || !visited.insert(c.clone()) {
                continue;
            }
            if self
                .magic_methods
                .get(&c)
                .map(|s| s.contains(method))
                .unwrap_or(false)
            {
                return true;
            }
            for p in self.parents_of(&c) {
                stack.push(p);
            }
        }
        false
    }

    /// Record an in-method local variable's type (`$x = new Y()`).
    pub fn set_local_type(&mut self, owner_fqn: &str, var: &str, type_fqn: &str) {
        self.local_types
            .entry(owner_fqn.to_string())
            .or_default()
            .insert(var.to_string(), type_fqn.to_string());
    }

    /// Look up an in-method local variable's type (for `$x->method()` resolution).
    pub fn local_type(&self, owner_fqn: &str, var: &str) -> Option<&str> {
        self.local_types
            .get(owner_fqn)
            .and_then(|m| m.get(var))
            .map(|s| s.as_str())
    }

    // ------------------------------------------------------------ nodes

    /// Add a syntax node.
    ///
    /// id comes from this project's exclusive segment (see [`NODE_ID_STRIDE`]), no collision across projects or processes.
    pub fn add_node(&mut self, mut new: NewNode) -> NodeId {
        let id = NodeId(self.next_node);
        self.next_node += 1;
        let node = Node {
            id,
            project_id: new.project_id,
            sub_project_id: new.sub_project_id,
            kind: new.kind.clone(),
            name: new.name.clone(),
            fqn: new.fqn.clone(),
            identity: new.identity.clone(),
            file_id: new.file_id,
            span: new.span,
            language: new.language.clone(),
            phase: new.phase.clone(),
            confidence: new.confidence,
            properties: new.properties.clone(),
        };
        if let Some(fqn) = &new.fqn {
            self.by_fqn.entry(fqn.clone()).or_insert(id.get());
            if is_type_kind(new.kind.as_str()) {
                // `NS_SEPARATORS` (not a hand-written `\` / `:` / `/` list): `.` is the namespace separator
                // for Java (`com.x.User`) and Python (`app.api.users.User`), so without it their short name
                // would be indexed as the whole FQN and never found again.
                if let Some(short) = fqn.rsplit(NS_SEPARATORS).next() {
                    if !short.is_empty() {
                        self.by_short
                            .entry(short.to_ascii_lowercase())
                            .or_default()
                            .push(id.get());
                    }
                }
            }
        }
        // Synthesized nodes (Table / HttpContract / ConfigKey / I18nKey) have no fqn, indexed by name,
        // for P7 to look up by the table name propagated from `Db::name('x')`. Only Table needs lookup, ignore the rest.
        if new.kind.as_str() == NodeKind::TABLE && !new.name.is_empty() {
            self.by_table_name.entry(new.name.clone()).or_insert(id.get());
        }
        new.id = Some(id);
        self.delta.nodes.push(new);
        self.nodes.insert(id.get(), node);
        id
    }

    /// Add or reuse a synthesized node (**idempotent merge**: same identity built only once).
    ///
    /// Merge is two-layered:
    /// 1. **exact identity** (`Method /path`): same endpoint same method builds only one node.
    /// 2. **wildcard-method aware**: a backend "auto route / `Route::rule`" with no method restriction (method = `ANY`
    ///    / `RULE`) should converge with the frontend's concrete method (`POST` / `GET`...) onto the same contract-bridge node,
    ///    otherwise in the route view "frontend caller ↔ backend handler" would land on two nodes, misaligned.
    ///    As long as one side is a wildcard method, reuse the same node (whoever builds first).
    pub fn get_or_create_synthesized(&mut self, new: NewNode) -> (NodeId, bool) {
        if let Some(identity) = &new.identity {
            let key = identity.key();
            // 1) exact-identity merge
            if let Some(existing) = self.by_identity.get(&key).copied() {
                self.merge_synthesized(existing, &new);
                return (NodeId(existing), false);
            }
            // 2) wildcard-method-aware merge
            if new.kind.as_str() == NodeKind::HTTP_CONTRACT
                && identity.kind.as_str() == SynthesizedKind::CONTRACT_ID
            {
                if let Some((method, path)) = identity.contract_parts() {
                    let new_is_wild = gt_domain::model::is_wildcard_http_method(&method);
                    if let Some(&(eid, e_is_wild)) = self.contract_path_index.get(&path) {
                        if e_is_wild || new_is_wild {
                            self.merge_synthesized(eid, &new);
                            return (NodeId(eid), false);
                        }
                    }
                }
            }
            // Register the contract bridge into the path index (wildcard or concrete both registered, for wildcard-merge hits).
            // Must be computed before `add_node` moves `new` (path and wildcard flag are both owned values).
            let contract_entry = if new.kind.as_str() == NodeKind::HTTP_CONTRACT {
                identity
                    .contract_parts()
                    .map(|(method, path)| (path, gt_domain::model::is_wildcard_http_method(&method)))
            } else {
                None
            };
            let id = self.add_node(new);
            self.by_identity.insert(key, id.get());
            if let Some((path, is_wild)) = contract_entry {
                self.contract_path_index.entry(path).or_insert((id.get(), is_wild));
            }
            return (NodeId(id.get()), true);
        }
        let id = self.add_node(new);
        (id, true)
    }

    /// When reusing an existing synthesized node, merge confidence (take max) and properties.
    fn merge_synthesized(&mut self, existing: i64, new: &NewNode) {
        if let Some(node) = self.nodes.get_mut(&existing) {
            if new.confidence > node.confidence {
                node.confidence = new.confidence;
            }
            merge_properties(&mut node.properties, &new.properties);
        }
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id.0)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id.0)
    }

    /// Modify a node's properties (also sync to the pending delta).
    pub fn patch_properties(&mut self, id: NodeId, patch: Value) {
        let key = id.get();
        if let Some(node) = self.nodes.get_mut(&key) {
            merge_properties(&mut node.properties, &patch);
        }
        // Nodes in the delta already carry properties at insert; later patches record a separate upsert
        self.delta.property_patches.push((id, patch));
    }

    /// Record one party's claim on a synthesised node: `sides` grows as a **sorted set** (so the result is
    /// independent of the order in which the parties happen to run), and `side` stays a plain scalar label for
    /// display / UI colouring — one party ⇒ that party, several parties ⇒ `bridge`.
    ///
    /// Why a set and not an overwrite: a node like an `HttpContract` is deliberately synthesised **twice**, once
    /// from the backend route and once from the frontend call site, and both rules declare `side`. Overwriting
    /// made the node claim to belong to whichever side ran last (and silently broke every consumer reading
    /// `side`, e.g. `frontend-mark-called`). See [`crate::engine::exec_synthesize_one`].
    pub fn record_side(&mut self, id: NodeId, side: &str) {
        if side.is_empty() {
            return;
        }
        let mut sides: BTreeSet<String> = self
            .nodes
            .get(&id.0)
            .and_then(|n| n.properties.get("sides"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if !sides.insert(side.to_string()) {
            return;
        }
        let list: Vec<String> = sides.into_iter().collect();
        let label = if list.len() > 1 { "bridge".to_string() } else { list[0].clone() };
        self.patch_properties(id, json!({ "sides": list, "side": label }));
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn node_ids(&self) -> Vec<NodeId> {
        self.nodes.keys().map(|k| NodeId(*k)).collect()
    }

    pub fn nodes_of_kind(&self, kind: &str) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.kind.as_str() == kind)
            .map(|(k, _)| NodeId(*k))
            .collect()
    }

    pub fn find_by_name(&self, fqn: &str) -> Option<NodeId> {
        self.by_fqn.get(fqn).copied().map(NodeId)
    }

    /// Look up the synthesized Table node by table name.
    ///
    /// Writings like `Db::name('goods')` get normalized via `strip_prefix → singularize → ...` at P5, so
    /// the table node's `name` may differ from the raw literal (`goods` → `good`). Here we align the same normalization:
    /// try in order the raw string, singularize, strip prefix, strip-prefix-then-singularize, return on hit.
    pub fn find_table_by_name(&self, raw: &str) -> Option<NodeId> {
        use crate::normalize::{singularize, strip_prefixes};
        let prefixes = self.table_prefixes();
        let candidates = [
            raw.to_string(),
            singularize(raw),
            strip_prefixes(raw, &prefixes),
            singularize(&strip_prefixes(raw, &prefixes)),
        ];
        for c in candidates {
            let key = c.trim();
            if key.is_empty() {
                continue;
            }
            if let Some(id) = self.by_table_name.get(key) {
                return Some(NodeId(*id));
            }
            // case-insensitive fallback
            if let Some(id) = self
                .by_table_name
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| *v)
            {
                return Some(NodeId(id));
            }
        }
        None
    }

    /// Short-name resolution: `StoreOrderServices` → `app\services\order\StoreOrderServices`.
    ///
    /// Go through the [`Self::by_short`] index, O(1); otherwise on a big DB it degrades to O(n) full-table scan
    /// (CRMEB has ~50k FQNs, each linear scan would slow P5/P7 by tens of seconds). **Ambiguous short names are always rejected**: only adopt when there's a unique candidate.
    ///
    /// Previously we returned the first candidate "first-come-first-served", but candidate order depended on node insertion order (non-deterministic).
    /// Among this repo's 897 type short names, 112 are duplicated (`User` / `StoreProduct` / `Login` each have
    /// 3~5 candidates across `adminapi` / `api` / `model` namespaces) — the guessed one
    /// was often a controller not a model, so `Model --MapsTo--> Table` class-level semantic edges got hung on completely unrelated classes.
    ///
    /// When we can't guess, return `None` (leave an `UnresolvedLink` diagnostic) — **better than connecting a wrong edge**.
    /// For precise results use `resolve_name_in_file` (resolve by that file's `use`).
    pub fn resolve_short_name(&self, short: &str) -> Option<String> {
        let target = short.trim_start_matches('\\').to_ascii_lowercase();
        let ids = self.by_short.get(&target)?;
        let mut found: Option<&str> = None;
        for id in ids {
            let Some(fqn) = self.nodes.get(id).and_then(|n| n.fqn.as_deref()) else {
                continue;
            };
            match found {
                Some(prev) if prev == fqn => {}
                Some(_) => return None, // Multiple distinct FQNs -> ambiguity, reject
                None => found = Some(fqn),
            }
        }
        found.map(|f| f.to_string())
    }

    /// Resolve a `method_ref` controller reference to a real class FQN — **without assuming any controller
    /// directory name** (no `controller` / `Http/Controllers` hard-coded).
    ///
    /// Strategy (name-agnostic, driven by the stack's root namespaces + the real class FQNs on the graph):
    /// 1. If `controller_part` is already a fully-qualified FQN that exists on the graph, return it directly.
    ///    (Covers Laravel/ThinkPHP handlers written as full class names, e.g. `App\Http\Controllers\UserController`.)
    /// 2. Otherwise it is a short name. Look up every class whose last segment equals the controller's short name
    ///    (the `by_short` index) and keep the ones that live under `<root_ns><ns_sep><module><ns_sep>` whose
    ///    remaining path is exactly `<controller_layer_depth> segment(s)` followed by the (hierarchy-expanded)
    ///    `controller_part`.
    ///    The layer *name* is never inspected — only its depth — so `controller` / `Http/Controllers` / anything
    ///    the user chose all work equally. Ambiguous matches (more than one distinct FQN under the constraints)
    ///    are rejected rather than connecting the wrong edge.
    ///
    /// `ns_separator` is this language's namespace separator ([`gt_domain::model::NamespacePolicy`]): the kernel
    /// never assumes `\`, so the same rule serves PHP (`\`), Java (`.`) and whatever a stack declares.
    pub fn resolve_controller(
        &self,
        controller_part: &str,
        root_namespaces: &[String],
        module: &str,
        controller_layer_depth: usize,
        ns_separator: char,
    ) -> Option<String> {
        // 1. direct fully-qualified FQN
        if let Some(id) = self.find_by_name(controller_part) {
            return self.nodes.get(&id.get()).and_then(|n| n.fqn.clone());
        }
        let sep = ns_separator.to_string();
        let class_short = controller_part.rsplit(ns_separator).next().unwrap_or(controller_part);
        let lower = class_short.to_ascii_lowercase();
        let ids = self.by_short.get(&lower)?;
        let tail: Vec<&str> = controller_part.split(ns_separator).collect();
        if tail.is_empty() {
            return None;
        }
        let depth = controller_layer_depth.max(1);
        let mut matched: Option<String> = None;
        for ns in root_namespaces {
            let mut prefix = ns.trim_end_matches(ns_separator).to_string();
            prefix.push(ns_separator);
            if !module.is_empty() {
                // The app module comes from a route *file path*, so it arrives with `/` separators.
                prefix.push_str(&module.replace('/', &sep));
                prefix.push(ns_separator);
            }
            for id in ids {
                let Some(fqn) = self.nodes.get(id).and_then(|n| n.fqn.as_deref()) else {
                    continue;
                };
                if !fqn.starts_with(&prefix) {
                    continue;
                }
                let rest: Vec<&str> = fqn[prefix.len()..].split(ns_separator).collect();
                // exactly `depth` layer segments between the module and the (hierarchy-expanded) controller_part
                if rest.len() != tail.len() + depth {
                    continue;
                }
                let ok = rest[depth..]
                    .iter()
                    .zip(tail.iter())
                    .all(|(a, b)| a.eq_ignore_ascii_case(b));
                if ok {
                    match &matched {
                        Some(prev) if prev.eq_ignore_ascii_case(fqn) => {}
                        Some(_) => return None, // ambiguous -> reject, never synthesize a ghost
                        None => matched = Some(fqn.to_string()),
                    }
                }
            }
        }
        matched
    }

    // ------------------------------------------------------------ edges

    pub fn add_edge(&mut self, new: NewEdge) -> bool {
        let key = (new.kind.to_string(), new.from_id.get(), new.to_id.get());
        if !self.edge_keys.insert(key) {
            return false;
        }
        let id = self.next_edge;
        self.next_edge += 1;
        self.out_edges.entry(new.from_id.get()).or_default().push((new.kind.to_string(), new.to_id.get()));
        self.in_edges
            .entry(new.to_id.get())
            .or_default()
            .push((new.kind.to_string(), new.from_id.get()));
        *self.fan_out.entry(new.from_id.get()).or_insert(0) += 1;
        *self.fan_in.entry(new.to_id.get()).or_insert(0) += 1;
        self.delta.edges.push(new.clone());
        self.edges.push(Edge {
            id: gt_domain::model::EdgeId(id),
            project_id: new.project_id,
            kind: new.kind.clone(),
            from_id: new.from_id,
            to_id: new.to_id,
            phase: new.phase.clone(),
            confidence: new.confidence,
            properties: new.properties.clone(),
        });
        true
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// All edges (read-only). Propagation phase builds the reverse call index from this.
    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    pub fn fan_in(&self, id: NodeId) -> u32 {
        self.fan_in.get(&id.get()).copied().unwrap_or(0)
    }

    pub fn fan_out(&self, id: NodeId) -> u32 {
        self.fan_out.get(&id.get()).copied().unwrap_or(0)
    }

    /// A node's **out-edge** adjacency `(edge kind, target)` (read-only view).
    ///
    /// For `Project` to traverse "each matching out-edge of a node" — one-to-many projection, not just the first.
    pub fn out_edges_of(&self, id: NodeId) -> &[(String, i64)] {
        self.out_edges
            .get(&id.get())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Walk from `from` to the landing node along the **edge-kind chain**; each hop takes the first matching out-edge.
    ///
    /// Empty chain means the landing is `from` itself ("no projection"). Typical: `[MapsTo]` — from an entity class to the table it maps to.
    pub fn follow(&self, from: NodeId, kinds: &[String]) -> Option<NodeId> {
        let mut cur = from;
        for k in kinds {
            let next = self
                .out_edges
                .get(&cur.get())?
                .iter()
                .find(|(kind, _)| kind == k)?
                .1;
            cur = NodeId(next);
        }
        Some(cur)
    }

    /// Transitive-closure judgment: does `child` (directly or indirectly) inherit/implement `base_fqn`.
    ///
    /// CRMEB's models are `StoreOrder extends BaseModel extends Model`,
    /// matching only the direct base class would miss almost all tables.
    pub fn has_ancestor(&self, child: NodeId, base_fqn: &str) -> bool {
        let base = base_fqn.trim_start_matches('\\').to_ascii_lowercase();
        if base.is_empty() {
            return false;
        }
        let mut visited: HashSet<i64> = HashSet::new();
        let mut stack = vec![child.get()];
        let mut depth = 0;
        while let Some(cur) = stack.pop() {
            if depth > 200 { break; }
            depth += 1;
            if !visited.insert(cur) {
                continue;
            }
            let Some(outs) = self.out_edges.get(&cur) else { continue };
            for (kind, to) in outs {
                if kind != "Extends" && kind != "Implements" {
                    continue;
                }
                let Some(node) = self.nodes.get(to) else { continue };
                let name = node
                    .fqn
                    .clone()
                    .unwrap_or_else(|| node.name.clone())
                    .trim_start_matches('\\')
                    .to_ascii_lowercase();
                if name == base || name.ends_with(&format!("\\{}", base)) {
                    return true;
                }
                stack.push(*to);
            }
        }
        false
    }

    // ------------------------------------------------------------ annotations

    /// Annotate by merge strategy.
    pub fn annotate(&mut self, new: NewAnnotation) {
        let key = new.node_id.get();
        match new.merge {
            MergeStrategy::MaxByKind => {
                let idx = self.annotations.iter().position(|a| {
                    a.node_id == new.node_id && a.channel == new.channel && a.kind == new.kind
                });
                if let Some(idx) = idx {
                    if new.confidence > self.annotations[idx].confidence {
                        let updated = self.materialize(&new);
                        self.annotations[idx] = updated;
                    }
                    return;
                }
            }
            MergeStrategy::Replace => {
                self.annotations
                    .retain(|a| !(a.node_id == new.node_id && a.channel == new.channel && a.kind == new.kind));
            }
            MergeStrategy::Coexist | MergeStrategy::Accumulate => {}
        }
        let ann = self.materialize(&new);
        self.delta.annotations.push(new);
        self.annotations.push(ann);
        let _ = key;
    }

    fn materialize(&mut self, new: &NewAnnotation) -> Annotation {
        let id = self.next_ann;
        self.next_ann += 1;
        Annotation {
            id,
            node_id: new.node_id,
            channel: new.channel.clone(),
            kind: new.kind.clone(),
            subkind: new.subkind.clone(),
            confidence: new.confidence,
            evidence: new.evidence.clone(),
            phase: new.phase.clone(),
        }
    }

    pub fn annotations_of(&self, id: NodeId) -> Vec<&Annotation> {
        self.annotations.iter().filter(|a| a.node_id == id).collect()
    }

    pub fn has_annotation(&self, id: NodeId, kind: &str) -> bool {
        self.annotations.iter().any(|a| a.node_id == id && a.kind == kind)
    }

    pub fn annotation_count(&self) -> usize {
        self.annotations.len()
    }

    // ------------------------------------------------------------ aliases

    pub fn put_alias(&mut self, entry: AliasEntry) {
        self.by_alias.insert(
            (
                entry.namespace.clone(),
                entry.key.clone(),
                entry.qualifier.clone().unwrap_or_default(),
            ),
            entry.node_id.get(),
        );
        self.delta.aliases.push(entry);
    }

    pub fn find_by_alias(&self, ns: &str, key: &str, qualifier: Option<&str>) -> Option<NodeId> {
        self.by_alias
            .get(&(ns.to_string(), key.to_string(), qualifier.unwrap_or_default().to_string()))
            .copied()
            .map(NodeId)
            .or_else(|| {
                // When no qualifier, degrade to "same namespace + key unique hit"
                self.by_alias
                    .iter()
                    .find(|((n, k, _), _)| n == ns && k == key)
                    .map(|(_, v)| NodeId(*v))
            })
    }

    pub fn alias_count(&self) -> usize {
        self.by_alias.len()
    }

    /// Append a "co-occurrence location".
    ///
    /// A synthesized node (e.g. `Table:user`) may come from `crmeb.sql` or from a Model's
    /// `$table` definition. We must **keep all**, so the frontend can give a multi-location list for the user to jump and verify,
    /// rather than fabricating a single location.
    pub fn append_location(
        &mut self,
        id: NodeId,
        file: impl Into<String>,
        line: u32,
        symbol: Option<String>,
        note: Option<String>,
        snippet: Option<String>,
    ) {
        const CAP: usize = 50;
        let loc = gt_domain::model::SourceLocation {
            file: file.into(),
            line,
            symbol,
            note,
            snippet,
        };
        let dup = self
            .nodes
            .get(&id.get())
            .and_then(|n| n.properties.get("locations"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|item| {
                    item.get("file").and_then(|v| v.as_str()) == Some(loc.file.as_str())
                        && item.get("line").and_then(|v| v.as_u64()) == Some(loc.line as u64)
                })
            })
            .unwrap_or(false);
        if dup {
            return;
        }
        if let Some(node) = self.nodes.get_mut(&id.get()) {
            let existing = node
                .properties
                .as_object()
                .and_then(|o| o.get("locations"))
                .cloned();
            let mut arr = match existing {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            if arr.len() < CAP {
                arr.push(serde_json::to_value(&loc).unwrap_or(Value::Null));
            }
            if let Some(obj) = node.properties.as_object_mut() {
                obj.insert("locations".into(), Value::Array(arr));
            }
        }
        self.delta.location_patches.push((id, loc));
    }

    /// Promote an existing node to another kind (syntax node → semantic node).
    ///
    /// The difference from `Synthesize` is decisive: **no new node**, only change this one node's kind.
    /// Middleware comes this way — P2 already built it as a `Class`, P14 changes its kind after confirming it hangs on a route,
    /// so it's visible by default in the folded view, while the graph **always has only one** `AuthTokenMiddleware`
    /// (if we "build another synthesized node", the same code becomes two nodes: fan-in splits,
    /// Inspector shows duplicate entries, `Node → definition location` gives two incomplete locations).
    ///
    /// Only change kind, don't rebuild indexes: `by_short` / `by_fqn` were registered at `add_node`,
    /// promotion happens at P14, after which all resolution is done; keeping the existing index is actually safer (the name still resolves).
    pub fn patch_kind(&mut self, id: NodeId, kind: &str) {
        let Some(node) = self.nodes.get_mut(&id.get()) else {
            return;
        };
        if node.kind.as_str() == kind {
            return;
        }
        node.kind = NodeKind(kind.to_string());
        self.delta
            .kind_patches
            .push((id, NodeKind(kind.to_string())));
    }

    // ------------------------------------------------------------ config

    pub fn set_table_prefixes(&mut self, prefixes: Vec<String>) {
        self.table_prefixes = prefixes;
    }

    pub fn table_prefixes(&self) -> &[String] {
        &self.table_prefixes
    }

    /// Register a route-group range (P3 collects from `Route::group('v2', ...)` call sites).
    ///
    /// Use **append** not overwrite: loaders call per sub-project, but collection walks all call sites,
    /// overwriting would drop data from sub-projects processed earlier (deduped later by `route_group_prefix`).
    pub fn add_route_groups(&mut self, groups: Vec<RouteGroup>) {
        self.route_groups.extend(groups);
    }

    /// Get the route-group prefix containing a call site (file + line number), outer first (e.g. `v2` / `v2/inner`).
    ///
    /// Use **line-range containment** not AST traversal: a call site's `span` naturally covers the whole
    /// `Route::group(...)` expression (incl. closure body), judging range containment restores nesting levels.
    pub fn route_group_prefix(&self, file: &str, line: u32) -> String {
        let mut matched: Vec<&RouteGroup> = self
            .route_groups
            .iter()
            .filter(|g| g.file == file && g.start_line <= line && line <= g.end_line)
            .collect();
        if matched.is_empty() {
            return String::new();
        }
        // Outer group has smaller start_line, larger end_line: order by (start asc, end desc) = outer-to-inner.
        matched.sort_by_key(|g| (g.start_line, std::cmp::Reverse(g.end_line), g.prefix.clone()));
        // Count a repeated registration of the same range only once (loaders re-walk all call sites per sub-project).
        matched.dedup_by_key(|g| (g.start_line, g.end_line, g.prefix.clone()));
        matched
            .iter()
            .map(|g| g.prefix.trim().trim_matches('/'))
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Register and query route-guard ranges (P3 collects from `Route::xxx(...)->middleware(...)`).
    ///
    /// Same **append** as [`Self::add_route_groups`]: loaders call per sub-project,
    /// but collection walks all call sites, overwriting would drop data from sub-projects processed earlier.
    pub fn add_route_guard_scopes(&mut self, scopes: Vec<RouteGuardScope>) {
        self.route_guard_scopes.extend(scopes);
    }

    pub fn route_guard_scopes(&self) -> &[RouteGuardScope] {
        &self.route_guard_scopes
    }

    /// Get which middleware guard a call site (file + line number) is under, **outer first**.
    ///
    /// Use line-range containment (not AST traversal), same play as [`Self::route_group_prefix`]:
    /// a call site's `span` naturally covers the whole `Route::group(...)` expression (incl. closure body).
    ///
    /// Same-named middleware merge by **inner overrides outer**: a group hangs `AuthToken(true)`, a route inside hangs `AuthToken(false)`, the latter should win.
    pub fn route_guards(&self, file: &str, line: u32) -> Vec<RouteGuard> {
        let mut matched: Vec<&RouteGuardScope> = self
            .route_guard_scopes
            .iter()
            .filter(|g| g.file == file && g.start_line <= line && line <= g.end_line)
            .collect();
        if matched.is_empty() {
            return Vec::new();
        }
        matched.sort_by_key(|g| (g.start_line, std::cmp::Reverse(g.end_line)));
        let mut out: Vec<RouteGuard> = Vec::new();
        for scope in matched {
            for guard in &scope.guards {
                match out.iter_mut().find(|g| g.class == guard.class) {
                    Some(existing) => {
                        if guard.arg.is_some() {
                            existing.arg = guard.arg.clone();
                        }
                    }
                    None => out.push(guard.clone()),
                }
            }
        }
        out
    }

    /// Strip known table prefixes; also try several generic prefixes.
    pub fn strip_table_prefix(&self, name: &str) -> String {
        crate::normalize::strip_prefixes(name, &self.table_prefixes)
    }

    // ------------------------------------------------------------ inheritance chain

    pub fn record_supertype(&mut self, child_fqn: &str, base: &str) {
        self.supertypes
            .entry(child_fqn.to_string())
            .or_default()
            .push(base.to_string());
        // Maintain the reverse index in sync (subclass → parent direction), for base-class methods to reverse-lookup subclass mapping tables.
        self.subtypes
            .entry(base.to_string())
            .or_default()
            .push(child_fqn.to_string());
    }

    /// Direct parent-type-name list (`child FQN → [parent FQN]`).
    pub fn parents_of(&self, fqn: &str) -> Vec<String> {
        self.supertypes.get(fqn).cloned().unwrap_or_default()
    }

    /// Direct subtype-name list (`parent FQN → [child FQN]`), inheritance-chain downstream.
    pub fn children_of(&self, fqn: &str) -> Vec<String> {
        self.subtypes.get(fqn).cloned().unwrap_or_default()
    }

    /// From `root`, do a bounded BFS downstream (subtypes) the inheritance chain, returning all reachable subclass FQNs.
    ///
    /// `max_depth` bounds the descent depth, `max_nodes` bounds total visited nodes, to avoid a shared generic base class
    /// (like `BaseModel`) instantly expanding to dozens of tables and blowing up action edges. Only subclasses that hit `MapsTo` are truly useful; here we just hand out the candidate subclasses.
    pub fn subtypes_bfs(&self, root: &str, max_depth: usize, max_nodes: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<(String, usize)> = vec![(root.to_string(), 0)];
        while let Some((cur, depth)) = stack.pop() {
            if out.len() >= max_nodes {
                break;
            }
            if !visited.insert(cur.clone()) {
                continue;
            }
            if depth > 0 {
                out.push(cur.clone());
            }
            if depth >= max_depth {
                continue;
            }
            for c in self.children_of(&cur) {
                stack.push((c, depth + 1));
            }
        }
        out
    }

    /// Whether `child` (transitively) inherits/implements a type with the same name as `base`.
    ///
    /// Comparison uses "lowercase-equal or ends with `\base`", so `Model` matches `think\Model`.
    pub fn has_supertype(&self, child_fqn: &str, base: &str) -> bool {
        let want = base.trim_start_matches('\\').to_ascii_lowercase();
        if want.is_empty() {
            return false;
        }
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![child_fqn.to_string()];
        let mut steps = 0;
        while let Some(cur) = stack.pop() {
            steps += 1;
            if steps > 200 || !visited.insert(cur.clone()) {
                continue;
            }
            let Some(parents) = self.supertypes.get(&cur) else { continue };
            for p in parents {
                let name = p.trim_start_matches('\\').to_ascii_lowercase();
                if name == want || name.ends_with(&format!("\\{}", want)) {
                    return true;
                }
                stack.push(p.clone());
            }
        }
        false
    }

    // ------------------------------------------------------------ file nodes

    pub fn record_file_node(&mut self, path: &str, id: NodeId) {
        self.file_nodes.insert(path.to_string(), id.get());
    }

    pub fn file_node(&self, path: &str) -> Option<NodeId> {
        self.file_nodes.get(path).copied().map(NodeId)
    }

    // ------------------------------------------------------------ property defaults

    pub fn record_property(&mut self, class_node: NodeId, name: &str, value: FactValue) {
        self.prop_values.insert((class_node.get(), name.to_string()), value);
    }

    pub fn property_of(&self, node: NodeId, name: &str) -> Option<FactValue> {
        self.prop_values.get(&(node.get(), name.to_string())).cloned()
    }

    // ------------------------------------------------------------ symbol tables / facts

    pub fn put_symbol(&mut self, project_id: ProjectId, table: &str, key: &str, value: Value) {
        self.symbols
            .entry(table.to_string())
            .or_default()
            .insert(key.to_string(), value.clone());
        self.delta.symbols.push(gt_domain::model::SymbolEntry {
            project_id,
            table: table.to_string(),
            key: key.to_string(),
            value,
        });
    }

    pub fn get_symbol(&self, table: &str, key: &str) -> Option<&Value> {
        self.symbols.get(table).and_then(|m| m.get(key))
    }

/// Restore a short name to an FQN via import alias (generic mechanism, **bound to no framework**).
///
/// `use think\facade\Queue as QueueThink;` is written into the `imports` symbol table by P2,
/// so `QueueThink` restores to `think\facade\Queue`, thus hitting FKB patterns like
/// `Queue::push` that "end in an umbrella name".
///
/// This is a global index (by lowercased short name), the same heuristic trade-off as `resolve_short_name`:
/// the same alias name in different files may point to different FQNs, but matching is best-effort and composable.
/// Register **some file**'s `use` import table (called by P2, one per file).
    pub fn record_file_imports(
        &mut self,
        file_id: i64,
        path: &str,
        imports: HashMap<String, String>,
    ) {
        self.file_id_by_path.insert(path.to_string(), file_id);
        self.source_path_by_id.insert(file_id, path.to_string());
        self.file_imports.insert(file_id, imports);
    }

    /// Source-file id → path.
    pub fn source_path_of(&self, file_id: i64) -> Option<String> {
        self.source_path_by_id.get(&file_id).cloned()
    }

    /// Whether this node is **already claimed**: has an in-edge of this kind, or a pending link of this kind points at it.
    ///
    /// Why in-edges alone aren't enough: most route-handler edges are laid at **P7** (P5 can only queue a
    /// `PendingLink`, because the class name must be built from FKB's template), while "convention inference" rules run at **P6**,
    /// when the edge doesn't exist yet; looking only at in-edges would let explicitly-registered routes also be covered by before/after conventions.
    ///
    /// The handler shape is the generic `Class/method` (with method name) or `Class` (REST resource route,
    /// equivalent to claiming the whole class's standard actions), so compare by shape; the kernel need not know the concrete framework.
    /// Whether the node already has an in-edge of the given kind ([`Predicate::HasIncoming`]).
    ///
    /// Same source as [`Self::claimed_by`] minus its fqn-scope fallback: this is pure "does the edge exist",
    /// used when the fact to establish is "somebody really linked to me" rather than "somebody will link to me".
    pub fn has_incoming_edge(&self, node: NodeId, kind: &str) -> bool {
        self.in_edges
            .get(&node.get())
            .map(|v| v.iter().any(|(k, _)| k == kind))
            .unwrap_or(false)
    }

    pub fn claimed_by(&self, node: NodeId, kind: &str) -> bool {
        if self
            .in_edges
            .get(&node.get())
            .map(|v| v.iter().any(|(k, _)| k == kind))
            .unwrap_or(false)
        {
            return true;
        }
        let Some(n) = self.node(node) else { return false };
        let Some(fqn) = n.fqn.clone() else { return false };
        let (class_part, member) = match fqn.rfind("::") {
            Some(i) => (&fqn[..i], Some(fqn[i + 2..].to_string())),
            None => (fqn.as_str(), None),
        };
        let short_name_of = |fqn: &str| -> String {
            fqn.rsplit(['\\', '.', '/', ':'])
                .next()
                .unwrap_or(fqn)
                .to_string()
        };
        let self_class = short_name_of(class_part).to_ascii_lowercase();
        if self_class.is_empty() {
            return false;
        }
        self.pending_links.iter().filter(|p| p.kind.0 == kind).any(|p| {
            let raw = p.raw.trim_start_matches('\\');
            let (head, tail_method) = match raw.rsplit_once('/') {
                Some((h, m)) => (h, Some(m.to_ascii_lowercase())),
                None => (raw, None),
            };
            let claimed_class = short_name_of(head).to_ascii_lowercase();
            if claimed_class != self_class {
                return false;
            }
            match tail_method {
                // Wrote a method name → claim only this one method
                Some(m) => member.as_ref().map(|x| x.to_ascii_lowercase()) == Some(m),
                // No method name (resource route) → the whole controller is taken over by explicit routing
                None => true,
            }
        })
    }

    /// Get **some file**'s `use` import table: short name (lowercased) → FQN.
    ///
    /// P7 must query this first when resolving a `Class::method` receiver — this is PHP's real resolution rule,
    /// framework-independent. Only when not found may it fall back to the global short-name index.
    pub fn imports_of_file(&self, file_id: i64) -> Option<&HashMap<String, String>> {
        self.file_imports.get(&file_id)
    }

    pub fn imports_of_path(&self, path: &str) -> Option<&HashMap<String, String>> {
        self.file_id_by_path
            .get(path)
            .and_then(|id| self.file_imports.get(id))
    }

    /// Every FQN that **any** file imported: `(source-file id, FQN)`.
    ///
    /// Deliberately not the global `imports` symbol table: that one is keyed by **short name** and
    /// first-come-first-served (see `cf_ast`), so `GuzzleHttp\Client` is silently dropped as soon as any
    /// earlier file imported a different `Client` — the very collision that forced `file_imports` to be
    /// per-file. Detection asks "does this FQN appear anywhere", which must not lose entries, so it walks
    /// the per-file tables. Pair with [`Self::source_path_of`] to scope the answer to one sub-project.
    pub fn all_imported_fqns(&self) -> impl Iterator<Item = (i64, &str)> + '_ {
        self.file_imports
            .iter()
            .flat_map(|(file_id, imports)| {
                imports.values().map(move |fqn| (*file_id, fqn.as_str()))
            })
    }

    /// Restore a short name to an FQN **within some file** — everywhere that needs to "guess a class name" should go through here.
    ///
    /// Rules (consistent with PHP, bound to no framework):
    /// * the file `use`d this short name → recognize **only** the FQN it imported, even if that class isn't in the graph
    ///   (a framework class, vendor excluded by P0). Then **never** fall back to the global index to guess a same-named project class;
    /// * otherwise fall back to the global short-name index, and **reject ambiguous short names outright** (see `resolve_short_name`).
    pub fn resolve_name_in_file(&self, file: Option<&str>, raw: &str) -> Option<String> {
        if let Some(path) = file {
            if let Some(fqn) = self
                .imports_of_path(path)
                .and_then(|m| m.get(&raw.to_ascii_lowercase()))
            {
                return Some(fqn.clone());
            }
        }
        self.resolve_short_name(raw)
    }

    /// Restore a short name to an FQN **within the file a node belongs to** (first `use`, then global short-name index).
    ///
    /// For resolution points that only have a "caller node" but no ready file path (receiver type, free function).
    pub fn resolve_name_at(&self, owner: NodeId, raw: &str) -> Option<String> {
        if let Some(fqn) = self
            .node(owner)
            .and_then(|n| n.file_id)
            .and_then(|id| self.file_imports.get(&id.get()))
            .and_then(|m| m.get(&raw.to_ascii_lowercase()))
        {
            return Some(fqn.clone());
        }
        self.resolve_short_name(raw)
    }

    pub fn resolve_import_alias(&self, name: &str) -> Option<String> {
        let key = name.trim_start_matches('\\').to_ascii_lowercase();
        self.get_symbol("imports", &key)
            .and_then(|v| v.get("fqn"))
            .and_then(|f| f.as_str())
            .map(|s| s.to_string())
    }

    pub fn symbol_table(&self, table: &str) -> Option<&BTreeMap<String, Value>> {
        self.symbols.get(table)
    }

    pub fn set_fact(&mut self, sub: SubProjectId, key: &str, value: Value) {
        self.facts.entry(sub.get()).or_default().insert(key.to_string(), value);
    }

    pub fn get_fact(&self, sub: SubProjectId, key: &str) -> Option<&Value> {
        self.facts.get(&sub.get()).and_then(|m| m.get(key))
    }

    /// A sub-project's full fact snapshot (for writing back to the DB).
    pub fn facts_snapshot(&self, sub: SubProjectId) -> Option<Value> {
        self.facts
            .get(&sub.get())
            .map(|m| Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
    }

    // ------------------------------------------------------------ diagnostics

    pub fn diagnose(
        &mut self,
        phase: &Phase,
        code: &str,
        severity: Severity,
        message: impl Into<String>,
        location: Option<String>,
    ) {
        self.diagnostics.push(Diagnostic {
            project_id: self.project_id,
            sub_project_id: None,
            phase: phase.clone(),
            code: code.to_string(),
            severity,
            message: message.into(),
            location,
            payload: Value::Null,
        });
    }

    // ------------------------------------------------------------ persist

    /// Take out and clear the accumulated changes.
    pub fn take_delta(&mut self) -> GraphDelta {
        std::mem::replace(&mut self.delta, GraphDelta::new(self.project_id))
    }

    pub fn remaining_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }
}

/// Shallow-merge two JSON objects (the latter overrides the former).
fn merge_properties(base: &mut Value, patch: &Value) {
    match (base.as_object_mut(), patch.as_object()) {
        (Some(b), Some(p)) => {
            for (k, v) in p {
                match (b.get(k), v.as_object()) {
                    (Some(Value::Object(_)), Some(_)) => {
                        let mut cur = b.get(k).cloned().unwrap_or(Value::Null);
                        merge_properties(&mut cur, v);
                        b.insert(k.clone(), cur);
                    }
                    _ => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        _ => {
            if !patch.is_null() {
                *base = patch.clone();
            }
        }
    }
}

/// Convenience constructor: a synthesized node with an identity.
pub fn synthesized_node(
    project_id: ProjectId,
    kind: &str,
    identity: IdentityKey,
    sub: Option<SubProjectId>,
    phase: &Phase,
    confidence: f32,
    language: &Language,
    span: Span,
) -> NewNode {
    NewNode {
        id: None,
        project_id,
        sub_project_id: sub,
        kind: NodeKind(kind.to_string()),
        name: identity.value.clone(),
        fqn: None,
        identity: Some(identity),
        file_id: None,
        span,
        language: language.clone(),
        phase: phase.clone(),
        confidence,
        properties: Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{NewNode, ProjectId};

    /// The short-name index must split on **every** namespace separator, not only PHP's `\`.
    ///
    /// Java (`com.example.UserController`) and Python (`app.api.users.User`) use `.`; with a
    /// hand-written `\` / `:` / `/` list their short name was indexed as the whole FQN, so
    /// `resolve_controller` (which looks up by short name) could never find them.
    #[test]
    fn short_name_index_respects_every_language_separator() {
        let mut ws = GraphWorkspace::new(ProjectId(1));
        for fqn in [
            r"app\admin\controller\Login",
            "com.example.UserController",
            "app.api.users.User",
        ] {
            let mut node = NewNode::new(ProjectId(1), NodeKind(NodeKind::CLASS.to_string()), "x");
            node.fqn = Some(fqn.to_string());
            ws.add_node(node);
        }
        assert!(ws.by_short.get("login").is_some(), "PHP: `\\` separator");
        assert!(ws.by_short.get("usercontroller").is_some(), "Java: `.` separator");
        assert!(ws.by_short.get("user").is_some(), "Python: `.` separator");
    }
}
