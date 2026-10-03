//! Graph-build use cases: orchestrate the pipeline and handle state transitions and background execution.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    EdgeKind, GraphDelta, NewEdge, NodeId, NodeKind, Phase, PhaseReport, Project, ProjectId,
    ProjectStatus,
};
use gt_domain::port::{
    EdgeDirection, FileScanner, FileSystem, GraphSink, KnowledgeProvider, MarkerProvider, NodeFilter,
    ParserRegistry, Persistence, PipelineObserver, ProjectWriter, RuleProvider, TechStackRegistry,
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
                    "工程 {project_id} 正在建图中"
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
                    "工程 {} 建图完成：{} 子工程 / {} 文件",
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
                    "工程 {} 自动合规检查完成：{} 条规则命中 {} 条违规（{} ms）",
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
    /// In auto-routing projects like likeadmin, most HttpContracts are synthesized by the `frontend-http-contract` rule only at
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

        // Pre-read existing PassesThrough edges for deduplication.
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
                    if !mws.iter().any(|m| m.id == mw_id) {
                        delta.kind_patches.push((mw_id, NodeKind::from("Middleware")));
                    }
                    let mut e =
                        NewEdge::new(project_id, EdgeKind::from("PassesThrough"), tid, mw_id);
                    e.phase = Phase::from("P14");
                    e.confidence = 1.0;
                    delta.edges.push(e);
                }
            }
        }

        if delta.edges.is_empty() && delta.kind_patches.is_empty() {
            return;
        }
        let edge_n = delta.edges.len();
        let promoted_n = delta.kind_patches.len();
        if let Err(e) = self.store.apply(&delta) {
            warn!("declarative middleware chain failed for project {project_id}: {e}");
        } else {
            info!(
                "工程 {project_id} 声明式中间件挂链完成：{} 条边 / {} 个晋升为 Middleware",
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
