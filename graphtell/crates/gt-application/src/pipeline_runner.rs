//! Graph-build use cases: orchestrate the pipeline and handle state transitions and background execution.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    EdgeKind, GraphDelta, NewEdge, Node, NodeId, NodeKind, Phase, PhaseReport, Project, ProjectId,
    ProjectStatus, SymbolEntry,
};
use gt_domain::port::{
    EdgeDirection, FileScanner, FileSystem, GraphSink, KnowledgeProvider, MarkerProvider, NodeFilter,
    ParserRegistry, Persistence, PipelineObserver, ProjectWriter, ResourceAdapterRegistry,
    RuleProvider, TechStackRegistry,
};
use gt_pipeline::runner::PipelineOutcome;
use tracing::{error, info, warn};

use crate::RuleService;

/// Aggregate of the infrastructure the pipeline needs (injected at the composition root, dependency inversion).
pub struct PipelineDeps {
    pub fs: Arc<dyn FileSystem>,
    pub scanner: Arc<dyn FileScanner>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub kb: Arc<dyn KnowledgeProvider>,
    /// Tech-stack-specific prepare logic (PSR-4 / PHP config / Laravel migrations / …), injected from the composition root.
    pub techstack: Arc<dyn TechStackRegistry>,
    /// Sub-project detection markers (bootstrap: needed before any FKB is loaded).
    pub markers: Arc<dyn MarkerProvider>,
    /// Resource adapters: non-source files (MyBatis mapper XML, …) turned into pseudo facts.
    pub resources: Arc<dyn ResourceAdapterRegistry>,
}

/// Adapt [`PipelineDeps`] and persistence into the [`PipelineInfrastructure`] that `gt-pipeline` needs.
struct Infra<'a> {
    deps: &'a PipelineDeps,
    store: &'a dyn Persistence,
}

impl<'a> gt_pipeline::runner::PipelineInfrastructure for Infra<'a> {
    fn fs(&self) -> &dyn FileSystem {
        self.deps.fs.as_ref()
    }
    fn scanner(&self) -> &dyn FileScanner {
        self.deps.scanner.as_ref()
    }
    fn parsers(&self) -> &dyn ParserRegistry {
        self.deps.parsers.as_ref()
    }
    fn techstack(&self) -> &dyn TechStackRegistry {
        self.deps.techstack.as_ref()
    }
    fn markers(&self) -> &dyn MarkerProvider {
        self.deps.markers.as_ref()
    }
    fn resources(&self) -> &dyn ResourceAdapterRegistry {
        self.deps.resources.as_ref()
    }
    fn kb(&self) -> &dyn KnowledgeProvider {
        self.deps.kb.as_ref()
    }
    fn projects(&self) -> &dyn ProjectWriter {
        self.store
    }
    fn graph(&self) -> &dyn GraphSink {
        self.store
    }
}

/// Cap on concurrently running graph-build tasks.
///
/// Each pipeline must keep the whole project's nodes resident in the in-memory workspace (large projects have 100k-level nodes),
/// and all writes ultimately funnel onto the same single SQLite connection. Concurrency itself is safe after the segment allocation was fixed,
/// this only sets a resource ceiling: extra projects queue instead of all storming in at once.
const MAX_CONCURRENT_BUILDS: usize = 2;

/// Graph-build use-case service.
pub struct PipelineService {
    store: Arc<dyn Persistence>,
    deps: Arc<PipelineDeps>,
    /// Auto-run a compliance check after graph build (see [`Self::run_check`]).
    checks: RuleService,
    /// Only one graph-build task per project at a time.
    running: Arc<Mutex<std::collections::HashSet<i64>>>,
    /// Cross-project concurrency slots (counter + condition variable).
    slots: Arc<(Mutex<usize>, Condvar)>,
}

impl PipelineService {
    pub fn new(
        store: Arc<dyn Persistence>,
        deps: Arc<PipelineDeps>,
        rules: Arc<dyn RuleProvider>,
    ) -> Self {
        let checks = RuleService::new(Arc::clone(&store), rules);
        Self {
            store,
            checks,
            deps,
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
            slots: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    /// Take a graph-build slot; wait if full.
    ///
    /// Use `unwrap_or_else(poisoned.into_inner())`: after the lock-holder thread panics the lock is poisoned,
    /// if we kept `unwrap()`-ing, every later build would panic too, and the failure would keep spreading.
    fn acquire_slot(&self) {
        let (lock, cv) = &*self.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_CONCURRENT_BUILDS {
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
    }

    fn release_slot(&self) {
        let (lock, cv) = &*self.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        cv.notify_one();
    }

    /// Run the graph build synchronously.
    pub fn run(
        &self,
        project_id: ProjectId,
        observer: &dyn PipelineObserver,
    ) -> Result<PipelineOutcome> {
        {
            let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.insert(project_id.get()) {
                return Err(DomainError::Conflict(format!(
                    "project {project_id} is being graphed"
                )));
            }
        }
        self.acquire_slot();
        let result = self.run_inner(project_id, observer);
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&project_id.get());
        self.release_slot();
        result
    }

    fn run_inner(
        &self,
        project_id: ProjectId,
        observer: &dyn PipelineObserver,
    ) -> Result<PipelineOutcome> {
        let project: Project = self
            .store
            .get_project(project_id)?
            .ok_or_else(|| DomainError::NotFound(format!("project {project_id}")))?;

        self.store.set_project_status(project_id, ProjectStatus::Indexing)?;
        let infra = Infra { deps: &self.deps, store: self.store.as_ref() };
        let outcome = gt_pipeline::runner::run(&project, &infra, observer);

        match outcome {
            Ok(out) => {
                self.store.set_project_status(project_id, ProjectStatus::Ready)?;
                info!(
                    "project {} graphed: {} sub-projects / {} files",
                    project.name,
                    out.sub_projects.len(),
                    out.files.len()
                );
                self.run_check(project_id, &project.name);
                self.attach_declared_middleware(project_id);
                Ok(out)
            }
            Err(e) => {
                error!("project {} failed to build the graph: {e}", project.name);
                self.store.set_project_status(project_id, ProjectStatus::Failed)?;
                Err(e)
            }
        }
    }

    /// **Automatically** run a compliance check after graph build.
    ///
    /// # Why automatic
    ///
    /// Rules are the graph's only acceptance device. If triggered manually, when the user builds and sees "0 violations"
    /// they can't tell "really no violation" from "never ran" — and the latter is exactly the pitfall this project already hit (silent zeroing).
    /// Running automatically is what makes warnings like `rules_unavailable` / `rules_silent` actually visible.
    ///
    /// # Why "after build" and not "a pipeline stage"
    ///
    /// Three reasons; the second is decisive:
    /// 1. Rules must read the **complete graph** — annotations like `auth.public` only appear at P6, folding only finishes at P8;
    ///    squeezing it into a stage would imply it's on par with P0–P8 and reorderable.
    /// 2. **Editing one rule shouldn't trigger re-parsing the whole repo**. Rules are YAML; if checks belonged to build,
    ///    editing one YAML line would re-run tens of seconds to minutes of parsing; after build, re-running the check takes just 1 second.
    /// 3. **Failure isolation**: a rule-engine error should only make the "conclusion" missing, not make the "graph" fail to build —
    ///    the graph is the far more expensive asset.
    ///
    /// So here we deliberately **swallow check failures**: log a warn, but the build still counts as success.
    fn run_check(&self, project_id: ProjectId, project_name: &str) {
        match self.checks.check(project_id, None, true) {
            Ok(report) => {
                info!(
                    "project {}: automatic compliance check done — {} rules matched {} violations ({} ms)",
                    project_name,
                    report.rules_run,
                    report.violations.len(),
                    report.duration_ms
                );
                for s in &report.rules_unavailable {
                    warn!("rule disabled (its criterion does not hold in this project): {}", s);
                }
                for s in &report.rules_silent {
                    warn!("rule hit 0 times (confirm whether the code is clean or the rule is blind): {}", s);
                }
            }
            Err(e) => {
                // Don't propagate upward: the graph is already built; a failed "conclusion" must not mark the build as failed.
                warn!("auto compliance check failed for project {} (the graph is still usable): {}", project_name, e);
            }
        }
    }

    /// After build and rule synthesis of HttpContract nodes, hang "declarative middleware" as `PassesThrough` edges.
    ///
    /// # Why after build, not P14
    ///
    /// In auto-routing projects like likeadmin, most HttpContracts are synthesized by the `frontend-js-common-http-contract` rule only at
    /// the `run_check` stage; when P14 runs they don't exist yet, so hanging the chain would fall through entirely. Here we read the declarations
    /// written into the `declared_middleware` symbol table during prepare (file name / key / scope all live in FKB, kernel has zero framework strings),
    /// hang edges onto **all** contract nodes by scope, and promote the corresponding `Class` nodes to `Middleware`.
    ///
    /// * `global`: hang onto all contract nodes;
    /// * `per_app`: only onto contract nodes whose name contains `prefix` (e.g. `/adminapi`).
    ///
    /// Node lookup is by class name (FQN / short name); if the class isn't in the graph (e.g. a vendor class) skip, no dangling edge.
    fn attach_declared_middleware(&self, project_id: ProjectId) {
        let Ok(declared) = self.store.list_symbols(project_id, "declared_middleware") else {
            return;
        };
        if declared.is_empty() {
            return;
        }
        let Ok(contracts) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("HttpContract")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };
        if contracts.is_empty() {
            return;
        }
        let Ok(classes) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("Class")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };
        let Ok(mws) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("Middleware")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };

        // Pre-read existing PassesThrough edges for deduplication (so re-runs don't pile up duplicate edges).
        let mut existing: HashSet<(NodeId, NodeId)> = HashSet::new();
        for c in &contracts {
            if let Ok(es) = self.store.edges_of(c.id, EdgeDirection::Outgoing) {
                for e in es {
                    if e.kind.as_str() == "PassesThrough" {
                        existing.insert((e.from_id, e.to_id));
                    }
                }
            }
        }

        let delta = build_middleware_delta(project_id, &declared, &contracts, &classes, &mws, &existing);

        if delta.edges.is_empty() && delta.kind_patches.is_empty() {
            return;
        }
        let edge_n = delta.edges.len();
        let promoted_n = delta.kind_patches.len();
        if let Err(e) = self.store.apply(&delta) {
            warn!("declarative middleware chain failed for project {project_id}: {e}");
        } else {
            info!(
                "project {project_id}: declarative middleware chaining done — {} edges / {} promoted to Middleware",
                edge_n, promoted_n
            );
        }
    }

    /// Run the graph build on a background thread (used by HTTP / Tauri callers).
    pub fn spawn(
        self: &Arc<Self>,
        project_id: ProjectId,
        observer: Arc<dyn PipelineObserver>,
    ) -> Result<()> {
        let svc = Arc::clone(self);
        std::thread::spawn(move || {
            if let Err(e) = svc.run(project_id, observer.as_ref()) {
                error!("background graph build failed: {e}");
            }
        });
        Ok(())
    }

    pub fn reports(&self, project_id: ProjectId) -> Vec<PhaseReport> {
        // The latest run's stage report is cached by the caller; here we read a summary from the run-records table
        let _ = project_id;
        Vec::new()
    }
}

/// Pure policy for [`PipelineService::attach_declared_middleware`]: build the `GraphDelta` that hangs `PassesThrough`
/// edges from each middleware onto the matching `HttpContract` nodes and promotes the referenced `Class` nodes to
/// `Middleware`. `existing` is the pre-read set of `(contract_id, mw_id)` PassesThrough edges to dedupe against.
///
/// Kept as a free function so the (subtle) scope/prefix/name-resolution/dedup logic is testable without a store.
fn build_middleware_delta(
    project_id: ProjectId,
    declared: &[SymbolEntry],
    contracts: &[Node],
    classes: &[Node],
    mws: &[Node],
    existing: &HashSet<(NodeId, NodeId)>,
) -> GraphDelta {
    // class name (FQN / short name) → node id, preferring to reuse an existing Middleware node.
    let mut by_name: HashMap<String, NodeId> = HashMap::new();
    for n in mws.iter().chain(classes.iter()) {
        by_name.entry(n.name.clone()).or_insert(n.id);
        if let Some(fqn) = &n.fqn {
            by_name.entry(fqn.clone()).or_insert(n.id);
        }
        let short = n.name.rsplit(['\\', '/']).next().unwrap_or(&n.name).to_string();
        by_name.entry(short).or_insert(n.id);
    }

    let mut existing = existing.clone();
    // Track which middleware nodes have already been queued for promotion, so a node shared by many contracts is
    // promoted exactly once (the `kind_patches` Vec is otherwise fanned out per edge — redundant, though idempotent).
    let mut promoted: HashSet<NodeId> = HashSet::new();
    let mut delta = GraphDelta::new(project_id);
    for entry in declared.iter() {
        let scope = entry
            .value
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("global");
        let prefix = entry.value.get("prefix").and_then(|v| v.as_str());
        let classes_arr = entry
            .value
            .get("classes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for cv in &classes_arr {
            let Some(class) = cv.as_str().map(|s| s.to_string()) else {
                continue;
            };
            if class.is_empty() {
                continue;
            }
            let Some(&mw_id) = by_name.get(&class).or_else(|| {
                let short = class.rsplit(['\\', '/']).next().unwrap_or(&class);
                by_name.get(short)
            }) else {
                continue;
            };
            let targets: Vec<NodeId> = if scope == "per_app" {
                contracts
                    .iter()
                    .filter(|c| prefix.map_or(true, |p| c.name.contains(p)))
                    .map(|c| c.id)
                    .collect()
            } else {
                contracts.iter().map(|c| c.id).collect()
            };
            for tid in targets {
                if existing.contains(&(tid, mw_id)) {
                    continue;
                }
                existing.insert((tid, mw_id));
                if !mws.iter().any(|m| m.id == mw_id) && promoted.insert(mw_id) {
                    delta.kind_patches.push((mw_id, NodeKind::from("Middleware")));
                }
                let mut e = NewEdge::new(project_id, EdgeKind::from("PassesThrough"), tid, mw_id);
                e.phase = Phase::from("P14");
                e.confidence = 1.0;
                delta.edges.push(e);
            }
        }
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use gt_adapter_fkb::YamlKnowledgeBase;
    use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
    use gt_adapter_parser::DefaultParserRegistry;
    use gt_adapter_sqlite::SqliteStore;
    use gt_adapter_techstack::{DefaultMarkerProvider, JsTechStackAdapter};
    use gt_domain::model::{
        CheckRule, GraphDelta, Language, NewNode, NewProject, Node, NodeId, NodeKind, Phase,
        ProjectConfig, ProjectId, Span, SymbolEntry,
    };
    use gt_domain::port::{
        DefaultResourceAdapterRegistry, DefaultTechStackRegistry, EdgeDirection, NodeFilter,
        Persistence, RuleProvider,
    };
    use serde_json::Value;

    fn node(id: i64, kind: &str, name: &str, fqn: Option<&str>) -> Node {
        Node {
            id: NodeId::new(id),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            kind: NodeKind::from(kind),
            name: name.to_string(),
            fqn: fqn.map(|s| s.to_string()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    fn decl(classes: &[&str], scope: &str, prefix: Option<&str>) -> SymbolEntry {
        let mut v = serde_json::Map::new();
        v.insert(
            "classes".into(),
            Value::Array(classes.iter().map(|c| Value::String(c.to_string())).collect()),
        );
        if scope != "global" {
            v.insert("scope".into(), Value::String(scope.to_string()));
        }
        if let Some(p) = prefix {
            v.insert("prefix".into(), Value::String(p.to_string()));
        }
        SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: Value::Object(v),
        }
    }

    /// `global` scope hangs the middleware onto **every** contract and promotes the referenced Class to Middleware.
    #[test]
    fn global_scope_hangs_all_contracts_and_promotes_class() {
        let contracts = vec![node(1, "HttpContract", "/adminapi/order", None), node(2, "HttpContract", "/api/user", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", Some("App\\Http\\Middleware\\AuthMiddleware"))];
        let declared = vec![decl(&["App\\Http\\Middleware\\AuthMiddleware"], "global", None)];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 2, "global is attached to every contract");
        for e in &d.edges {
            assert_eq!(e.kind.as_str(), "PassesThrough");
        }
        // the single Class is promoted exactly once
        assert_eq!(d.kind_patches.len(), 1);
        assert_eq!(d.kind_patches[0], (NodeId::new(10), NodeKind::from("Middleware")));
    }

    /// `per_app` scope only hangs onto contracts whose name contains `prefix`.
    #[test]
    fn per_app_scope_filters_by_prefix() {
        let contracts = vec![node(1, "HttpContract", "/adminapi/order", None), node(2, "HttpContract", "/api/user", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", Some("App\\Http\\Middleware\\AuthMiddleware"))];
        let declared = vec![decl(&["App\\Http\\Middleware\\AuthMiddleware"], "per_app", Some("/adminapi"))];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 1, "per_app hits only contracts carrying the prefix");
        assert_eq!(d.edges[0].from_id, NodeId::new(1));
    }

    /// per_app with no prefix matches every contract (the `map_or(true, …)` branch).
    #[test]
    fn per_app_without_prefix_matches_all() {
        let contracts = vec![node(1, "HttpContract", "/adminapi/order", None), node(2, "HttpContract", "/api/user", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![decl(&["AuthMiddleware"], "per_app", None)];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 2, "per_app with no prefix degrades to every contract");
    }

    /// A class already resolved as `Middleware` is hung but **not** re-promoted (no duplicate kind_patch).
    #[test]
    fn existing_middleware_is_not_promoted_again() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let mws = vec![node(10, "Middleware", "AuthMiddleware", None)];
        let declared = vec![decl(&["AuthMiddleware"], "global", None)];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &[], &mws, &Default::default());
        assert_eq!(d.edges.len(), 1);
        assert!(d.kind_patches.is_empty(), "a node that is already Middleware must not be promoted again");
    }

    /// An `existing` (contract, mw) edge must not be re-created — re-runs are idempotent.
    #[test]
    fn existing_edge_is_not_duplicated() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![decl(&["AuthMiddleware"], "global", None)];
        let mut existing = HashSet::new();
        existing.insert((NodeId::new(1), NodeId::new(10)));

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &existing);
        assert!(d.edges.is_empty(), "an existing edge must not be added twice");
        assert!(d.kind_patches.is_empty());
    }

    /// A declared class with no matching node is silently skipped (no dangling edge).
    #[test]
    fn missing_class_is_skipped() {
        let contracts = vec![node(1, "HttpContract", "/x", None), node(2, "HttpContract", "/y", None)];
        let declared = vec![decl(&["GhostMiddleware"], "global", None)];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &[], &[], &Default::default());
        assert!(d.edges.is_empty());
        assert!(d.kind_patches.is_empty());
    }

    /// Empty declaration yields an empty delta (the caller's early-return guard).
    #[test]
    fn empty_declared_yields_empty_delta() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let d = build_middleware_delta(ProjectId::new(1), &[], &contracts, &classes, &[], &Default::default());
        assert!(d.is_empty());
    }

    fn new_node(pid: ProjectId, id: i64, kind: &str, name: &str, fqn: Option<&str>) -> NewNode {
        let mut n = NewNode::new(pid, NodeKind::from(kind), name);
        n.id = Some(NodeId::new(id));
        n.fqn = fqn.map(|s| s.to_string());
        n
    }

    /// `classes` being a scalar (not an array) must be treated as "no declarations": the `as_array()` fallback yields an
    /// empty list, so no edges are produced rather than panicking or assuming a single entry.
    #[test]
    fn declared_classes_not_an_array_is_ignored() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": "AuthMiddleware", "scope": "global" }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert!(d.is_empty(), "a non-array classes must be ignored");
    }

    /// A non-string entry inside the `classes` array must be skipped while a valid sibling entry still applies.
    #[test]
    fn declared_class_entry_not_a_string_is_skipped() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": ["AuthMiddleware", 123], "scope": "global" }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 1, "a non-string class entry is skipped while the valid one still applies");
        assert_eq!(d.kind_patches.len(), 1);
    }

    /// An empty-string class entry must be skipped (not resolved as an empty node name).
    #[test]
    fn declared_empty_class_string_is_skipped() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": ["", "AuthMiddleware"], "scope": "global" }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 1, "an empty-string class must be skipped");
    }

    /// A non-string `scope` (e.g. a numeric id from a malformed probe) must fall back to `global` via
    /// `as_str().unwrap_or("global")`, not be treated as a literal scope name — so it still attaches to every contract.
    #[test]
    fn non_string_scope_defaults_to_global() {
        let contracts = vec![node(1, "HttpContract", "/x", None), node(2, "HttpContract", "/y", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": ["AuthMiddleware"], "scope": 123 }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 2, "a non-string scope must degrade to global (every contract)");
        assert_eq!(d.kind_patches.len(), 1);
    }

    /// In `per_app`, a non-string `prefix` yields `None`, so `prefix.map_or(true, …)` matches every contract rather
    /// than silently filtering to nothing.
    #[test]
    fn non_string_prefix_in_per_app_matches_all() {
        let contracts = vec![node(1, "HttpContract", "/adminapi/order", None), node(2, "HttpContract", "/api/user", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": ["AuthMiddleware"], "scope": "per_app", "prefix": 123 }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 2, "a non-string per_app prefix must degrade to every contract");
    }

    /// A declared class given as an FQN only matches a node whose **short name** (namespace stripped) lines up, via the
    /// `.or_else(|| short rsplit)` branch — not its full name / fqn.
    #[test]
    fn declared_fqn_resolves_by_node_short_name() {
        let contracts = vec![node(1, "HttpContract", "/x", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)]; // no fqn; short name == "AuthMiddleware"
        let declared = vec![SymbolEntry {
            project_id: ProjectId::new(1),
            table: "declared_middleware".into(),
            key: "k".into(),
            // FQN that is not the node's name/fqn, but shares the short name after stripping `App\`
            value: serde_json::json!({ "classes": ["App\\AuthMiddleware"], "scope": "global" }),
        }];
        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert_eq!(d.edges.len(), 1, "the FQN should resolve through the node's short name");
        assert_eq!(d.kind_patches, vec![(NodeId::new(10), NodeKind::from("Middleware"))]);
    }

    /// A `per_app` prefix that matches no contract yields an empty target set — no edges are produced (never an error).
    #[test]
    fn per_app_prefix_matching_no_contract_yields_no_edges() {
        let contracts = vec![node(1, "HttpContract", "/api/user", None)];
        let classes = vec![node(10, "Class", "AuthMiddleware", None)];
        let declared = vec![decl(&["AuthMiddleware"], "per_app", Some("/adminapi"))];

        let d = build_middleware_delta(ProjectId::new(1), &declared, &contracts, &classes, &[], &Default::default());
        assert!(d.edges.is_empty(), "a per_app prefix with no match must produce no edges");
        assert!(d.kind_patches.is_empty());
    }

    // ---- `attach_declared_middleware` (store-only orchestration) ----
    // The dependency bundle is only needed to satisfy `PipelineService::new`; `attach_declared_middleware` touches nothing
    // but `self.store`, so the real adapters are inert here.

    const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

    fn attach_test_deps() -> PipelineDeps {
        let kb = YamlKnowledgeBase::load_dir(Path::new(FKB_DIR)).expect("fkb must load");
        PipelineDeps {
            fs: Arc::new(StdFileSystem::new()),
            scanner: Arc::new(WalkDirScanner::new(Vec::new())),
            parsers: Arc::new(DefaultParserRegistry::new()),
            kb: Arc::new(kb),
            techstack: Arc::new(
                DefaultTechStackRegistry::new().register(Box::new(JsTechStackAdapter::new())),
            ),
            markers: Arc::new(DefaultMarkerProvider::new()),
            resources: Arc::new(DefaultResourceAdapterRegistry::new()),
        }
    }

    struct NoopRules;
    impl RuleProvider for NoopRules {
        fn rules(&self) -> &[CheckRule] {
            &[]
        }
    }

    /// `attach_declared_middleware` reads the `declared_middleware` symbol table, hangs `PassesThrough` edges from every
    /// matching `HttpContract` onto the middleware `Class`, and promotes that `Class` to `Middleware` — the full happy
    /// path the empty-project integration test never reaches (it has no such symbols / contracts).
    #[test]
    fn attach_declared_middleware_hangs_edges_and_promotes_class() {
        let store: Arc<dyn Persistence> = Arc::new(SqliteStore::in_memory().expect("store"));
        let pid = store
            .create_project(NewProject {
                name: "mw".into(),
                root_path: std::env::temp_dir().join("gt_mw_test"),
                description: None,
                config: Some(ProjectConfig::default()),
            })
            .expect("create_project")
            .id;

        let mut d = GraphDelta::new(pid);
        d.nodes.push(new_node(pid, 1, "HttpContract", "/adminapi/order", None));
        d.nodes.push(new_node(pid, 2, "HttpContract", "/api/user", None));
        d.nodes.push(new_node(
            pid,
            3,
            "Class",
            "AuthMiddleware",
            Some("App\\Http\\Middleware\\AuthMiddleware"),
        ));
        d.symbols.push(SymbolEntry {
            project_id: pid,
            table: "declared_middleware".into(),
            key: "k".into(),
            value: serde_json::json!({ "classes": ["App\\Http\\Middleware\\AuthMiddleware"], "scope": "global" }),
        });
        store.apply(&d).expect("seed");

        let svc = PipelineService::new(store.clone(), Arc::new(attach_test_deps()), Arc::new(NoopRules));
        // private, but callable from this descendant `tests` module
        svc.attach_declared_middleware(pid);

        let contracts = store
            .query_nodes(&NodeFilter {
                project_id: pid,
                kind: Some(NodeKind::from("HttpContract")),
                ..Default::default()
            })
            .expect("query contracts");
        assert_eq!(contracts.len(), 2);
        for c in &contracts {
            let es = store.edges_of(c.id, EdgeDirection::Outgoing).expect("edges_of");
            let pt: Vec<_> = es.iter().filter(|e| e.kind.as_str() == "PassesThrough").collect();
            assert_eq!(pt.len(), 1, "contract {} must get one PassesThrough", c.id.get());
            assert_eq!(pt[0].to_id, NodeId::new(3));
        }
        let promoted = store
            .query_nodes(&NodeFilter {
                project_id: pid,
                kind: Some(NodeKind::from("Middleware")),
                ..Default::default()
            })
            .expect("query middleware");
        assert!(promoted.iter().any(|n| n.id == NodeId::new(3)), "Class(3) must be promoted to Middleware");
    }
}
