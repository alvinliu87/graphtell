//! Integration tests for `PipelineService` orchestration.
//!
//! These exercise the use-case layer the way a real HTTP / Tauri caller would: a graph build is driven
//! **end-to-end** through `gt_pipeline::runner::run` (real adapters over an in-memory SQLite store), so the
//! status transitions, the per-project "one build at a time" conflict guard, the cross-project concurrency
//! slot cap, and the background-thread `spawn` are all observed against the actual pipeline.
//!
//! A deliberately slow `MarkerProvider` stretches each build past the scheduling quantum, so concurrent builds
//! reliably overlap — that is what lets the guard / cap become observable (without it the pipeline would finish
//! before the second call could race in).
//!
//! `run_check` failure-swallowing is structurally guaranteed by `PipelineService::run_check` (a `match Err => warn`
//! inside `run_inner`'s success branch), and `run` returning `Ok` here also confirms the auto check does not turn a
//! good build into a failure. Failure-injecting the rule engine is out of scope for this harness.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_techstack::PhpTechStackAdapter;
use gt_adapter_resource::MyBatisMapperAdapter;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_techstack::{DefaultMarkerProvider, JsTechStackAdapter};
use gt_application::pipeline_runner::{PipelineDeps, PipelineService};
use std::collections::HashMap;

use serde_json::Value;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Annotation, CheckRule, Diagnostic, Edge, EdgeId, FileId, GraphDelta, NewProject, NewSourceFile,
    NewSubProject, Node, NodeId, NodeKind, Project, ProjectConfig, ProjectId, ProjectPatch,
    ProjectRuleConfig, ProjectStatus, SourceFile, SubProject, SubProjectId, SymbolEntry,
};
use gt_domain::model::graph::{CoverageReport, NodeSummary};
use gt_domain::port::persistence::GraphStats;
use gt_domain::port::{
    DefaultResourceAdapterRegistry, DefaultTechStackRegistry, DiagnosticSink, EdgeDirection,
    GraphQuery, GraphSink, Marker, MarkerProvider, NodeFilter, NoopObserver, Persistence,
    ProjectReader, ProjectWriter, RuleConfigStore, RuleProvider, SymbolTableReader,
};

const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// A `MarkerProvider` that sleeps `ms` on every `markers()` call, so a build takes visibly longer than the
/// scheduling quantum and concurrent builds overlap enough for the guard / slot cap to be observable.
///
/// `calls` counts how many times the pipeline actually entered a build (`markers()` runs once per build, before any
/// slow work) — used to make "exactly one build ran" observable and deterministic in the `spawn`-twice test.
struct SlowMarkerProvider {
    inner: DefaultMarkerProvider,
    ms: u64,
    calls: Arc<AtomicUsize>,
}
impl MarkerProvider for SlowMarkerProvider {
    fn markers(&self) -> Vec<Marker> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(self.ms));
        self.inner.markers()
    }
}

/// A `RuleProvider` with no rules: `run_check` runs and succeeds, the build still counts as success.
struct NoopRuleProvider;
impl RuleProvider for NoopRuleProvider {
    fn rules(&self) -> &[CheckRule] {
        &[]
    }
}

/// Build the dependency bundle for an integration build. `slow_markers_ms > 0` makes the build artificially slow.
fn deps(slow_markers_ms: u64) -> PipelineDeps {
    let kb = YamlKnowledgeBase::load_dir(Path::new(FKB_DIR))
        .expect("the FKB directory must load, otherwise the test environment is broken");
    PipelineDeps {
        fs: Arc::new(StdFileSystem::new()),
        scanner: Arc::new(WalkDirScanner::new(Vec::new())),
        parsers: Arc::new(DefaultParserRegistry::new()),
        kb: Arc::new(kb),
        techstack: Arc::new(
            DefaultTechStackRegistry::new()
                .register(Box::new(PhpTechStackAdapter::new()))
                .register(Box::new(JsTechStackAdapter::new())),
        ),
        markers: Arc::new(SlowMarkerProvider {
            inner: DefaultMarkerProvider::new(),
            ms: slow_markers_ms,
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        resources: Arc::new(
            DefaultResourceAdapterRegistry::new().register(Box::new(MyBatisMapperAdapter::default())),
        ),
    }
}

/// A fresh empty temp dir (with a `composer.json` so at least one sub-project is detected and the build proceeds
/// like a real, just-empty, project).
fn scratch_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("gt_pipe_it_{}_{}", std::process::id(), n));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::write(dir.join("composer.json"), "{}").expect("write composer.json");
    dir
}

/// Create a project in `store` with the given `root_path` and return its id.
fn create_it(store: &Arc<dyn Persistence>, root_path: PathBuf) -> ProjectId {
    store
        .create_project(NewProject {
            name: "it".into(),
            root_path,
            description: None,
            config: Some(ProjectConfig::default()),
        })
        .expect("create_project")
        .id
}

/// In-memory store + one project + a `PipelineService` (all in `Arc` so it can be shared across threads).
fn setup(slow_markers_ms: u64) -> (Arc<dyn Persistence>, Arc<PipelineService>, ProjectId) {
    let store: Arc<dyn Persistence> =
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"));
    let project = store
        .create_project(NewProject {
            name: "it".into(),
            root_path: scratch_dir(),
            description: None,
            config: Some(ProjectConfig::default()),
        })
        .expect("create_project");
    let svc = Arc::new(PipelineService::new(
        store.clone(),
        Arc::new(deps(slow_markers_ms)),
        Arc::new(NoopRuleProvider),
    ));
    (store, svc, project.id)
}

/// Like [`setup`] but with `n` distinct projects.
fn setup_n(n: usize, slow_markers_ms: u64) -> (Arc<dyn Persistence>, Arc<PipelineService>, Vec<ProjectId>) {
    let store: Arc<dyn Persistence> =
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"));
    let mut pids = Vec::new();
    for i in 0..n {
        let p = store
            .create_project(NewProject {
                name: format!("it{i}"),
                root_path: scratch_dir(),
                description: None,
                config: Some(ProjectConfig::default()),
            })
            .expect("create_project");
        pids.push(p.id);
    }
    let svc = Arc::new(PipelineService::new(
        store.clone(),
        Arc::new(deps(slow_markers_ms)),
        Arc::new(NoopRuleProvider),
    ));
    (store, svc, pids)
}

/// A successful build must flip the project from `Indexing` to `Ready`, and `run` returns `Ok` (confirming the
/// auto compliance check does not fail an otherwise-good build — its errors are swallowed by `run_check`).
#[test]
fn run_flips_status_to_ready_and_returns_ok() {
    let (store, svc, pid) = setup(0);
    let out = svc.run(pid, &NoopObserver).expect("build should succeed");
    assert!(!out.sub_projects.is_empty(), "composer.json must yield at least one sub-project");
    let status = store.get_project(pid).unwrap().unwrap().status;
    assert_eq!(status, ProjectStatus::Ready, "after success the status must be Ready");
}

/// Two concurrent builds of the **same** project must not both run: exactly one wins and the other is rejected
/// with `Conflict` (the `running` set guard, checked before the pipeline starts).
#[test]
fn concurrent_builds_of_same_project_conflict() {
    let (store, svc, pid) = setup(150);

    let s2 = Arc::clone(&svc);
    let handle = std::thread::spawn(move || s2.run(pid, &NoopObserver));
    // Run a second build on the same project immediately (the first is still inside its slow markers()).
    let main_res = svc.run(pid, &NoopObserver);
    let thread_res = handle.join().expect("thread joined");

    let conflicts = [&main_res, &thread_res]
        .iter()
        .filter(|r| matches!(r, Err(DomainError::Conflict(_))))
        .count();
    assert_eq!(conflicts, 1, "concurrent graphing of the same project must yield exactly one Conflict");
    assert!(main_res.is_ok() || thread_res.is_ok(), "one of them must succeed at graphing");
    let status = store.get_project(pid).unwrap().unwrap().status;
    assert_eq!(status, ProjectStatus::Ready, "the winner must leave the project Ready");
}

/// Three concurrent builds of **different** projects must serialize through `MAX_CONCURRENT_BUILDS` (2) slots:
/// the third waits for a slot to free, so total wall time is ~2× a single build, not ~1×.
#[test]
fn concurrent_builds_respect_slot_cap() {
    let (store, svc, pids) = setup_n(3, 150);

    let handles: Vec<_> = pids
        .iter()
        .map(|&pid| {
            let s = Arc::clone(&svc);
            std::thread::spawn(move || s.run(pid, &NoopObserver))
        })
        .collect();

    let t0 = Instant::now();
    for h in handles {
        h.join().expect("thread joined").expect("build should succeed");
    }
    let elapsed = t0.elapsed();

    // 3 builds / 2 slots → at least 2 waves → strictly more than a single build's time.
    assert!(
        elapsed >= Duration::from_millis(150 * 3 / 2),
        "the concurrency slot must make the total time > 1.5x a single run (measured {:?})",
        elapsed
    );
    for pid in pids {
        assert_eq!(
            store.get_project(pid).unwrap().unwrap().status,
            ProjectStatus::Ready
        );
    }
}

/// `spawn` returns immediately and the build actually completes on a background thread (status reaches `Ready`).
#[test]
fn spawn_runs_build_in_background() {
    let (store, svc, pid) = setup(0);

    svc.spawn(pid, Arc::new(NoopObserver)).expect("spawn must enqueue");
    // The call returns before the build finishes; poll until the background thread lands it on Ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match store.get_project(pid).unwrap() {
            Some(p) if p.status == ProjectStatus::Ready => break,
            Some(p) if p.status == ProjectStatus::Failed => panic!("background build failed"),
            _ => {
                if Instant::now() > deadline {
                    panic!("timed out waiting for background build");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Like [`setup`] but exposes the `SlowMarkerProvider` call counter (number of builds that actually entered the
/// pipeline) so a test can assert "exactly N builds ran" deterministically.
fn setup_counted(slow_markers_ms: u64) -> (Arc<dyn Persistence>, Arc<PipelineService>, ProjectId, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let store: Arc<dyn Persistence> =
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"));
    let project = store
        .create_project(NewProject {
            name: "it".into(),
            root_path: scratch_dir(),
            description: None,
            config: Some(ProjectConfig::default()),
        })
        .expect("create_project");
    let svc = Arc::new(PipelineService::new(
        store.clone(),
        Arc::new(PipelineDeps {
            fs: Arc::new(StdFileSystem::new()),
            scanner: Arc::new(WalkDirScanner::new(Vec::new())),
            parsers: Arc::new(DefaultParserRegistry::new()),
            kb: Arc::new(
                YamlKnowledgeBase::load_dir(Path::new(FKB_DIR))
                    .expect("the FKB directory must load, otherwise the test environment is broken"),
            ),
            techstack: Arc::new(
                DefaultTechStackRegistry::new()
                    .register(Box::new(PhpTechStackAdapter::new()))
                    .register(Box::new(JsTechStackAdapter::new())),
            ),
            markers: Arc::new(SlowMarkerProvider {
                inner: DefaultMarkerProvider::new(),
                ms: slow_markers_ms,
                calls: Arc::clone(&calls),
            }),
            resources: Arc::new(
                DefaultResourceAdapterRegistry::new()
                    .register(Box::new(MyBatisMapperAdapter::default())),
            ),
        }),
        Arc::new(NoopRuleProvider),
    ));
    (store, svc, project.id, calls)
}

/// Two `spawn`s (the HTTP / Tauri entry point) of the **same** project must result in exactly one real build: the
/// second `spawn` launches a background `run` that finds the `running`-set slot already taken and is rejected
/// before it ever touches the pipeline. We count `markers()` calls (one per build) to make this observable and
/// deterministic — a regression that let two background builds race the same project would double-write the graph
/// and bump the count to 2.
#[test]
fn spawn_twice_same_project_runs_exactly_one_build() {
    let (store, svc, pid, calls) = setup_counted(150);

    svc.spawn(pid, Arc::new(NoopObserver)).expect("spawn must enqueue");
    svc.spawn(pid, Arc::new(NoopObserver)).expect("spawn must enqueue");

    // Poll until the single winning build reaches Ready.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match store.get_project(pid).unwrap() {
            Some(p) if p.status == ProjectStatus::Ready => break,
            Some(p) if p.status == ProjectStatus::Failed => panic!("background build failed"),
            _ => {
                if Instant::now() > deadline {
                    panic!("timed out waiting for the spawned build");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "two spawns of the same project must run exactly one build \
         (the second must be rejected by the running-set guard before markers)"
    );
}

/// Rebuilding a project that is already `Ready` (a plain, non-concurrent second `run`) must re-flip the status
/// through `Indexing` and land back on `Ready` — not error, not leave it stuck, and not be (wrongly) flagged as a
/// `Conflict` just because it was built before. This is the normal "re-index" user action the suite otherwise never
/// exercises (every other test builds each project exactly once).
#[test]
fn rebuild_runs_again_and_stays_ready() {
    let (store, svc, pid) = setup(0);
    svc.run(pid, &NoopObserver).expect("first build");
    assert_eq!(
        store.get_project(pid).unwrap().unwrap().status,
        ProjectStatus::Ready
    );

    let out = svc
        .run(pid, &NoopObserver)
        .expect("rebuild must succeed (not a Conflict)");
    assert!(
        !out.sub_projects.is_empty(),
        "the rebuild must still detect the composer.json sub-project"
    );
    assert_eq!(
        store.get_project(pid).unwrap().unwrap().status,
        ProjectStatus::Ready,
        "after a rebuild the status must be Ready again"
    );
}

/// A persistence wrapper that delegates **every** method to a real in-memory store, except
/// `get_rule_configs` which always fails. This is the only deterministic way to make
/// `PipelineService::run_check` (which is called after a successful build) return `Err`, so we can
/// prove that failure is *swallowed* and does not turn a good build into `Failed`.
struct FailRuleConfigs {
    inner: Arc<dyn Persistence>,
}

impl ProjectReader for FailRuleConfigs {
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>> {
        self.inner.get_project(id)
    }
    fn list_projects(&self) -> Result<Vec<Project>> {
        self.inner.list_projects()
    }
    fn list_sub_projects(&self, project_id: ProjectId) -> Result<Vec<SubProject>> {
        self.inner.list_sub_projects(project_id)
    }
    fn list_files(&self, project_id: ProjectId, sub: Option<SubProjectId>) -> Result<Vec<SourceFile>> {
        self.inner.list_files(project_id, sub)
    }
}

impl ProjectWriter for FailRuleConfigs {
    fn create_project(&self, new: NewProject) -> Result<Project> {
        self.inner.create_project(new)
    }
    fn update_project(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project> {
        self.inner.update_project(id, patch)
    }
    fn delete_project(&self, id: ProjectId) -> Result<()> {
        self.inner.delete_project(id)
    }
    fn set_project_status(&self, id: ProjectId, status: ProjectStatus) -> Result<()> {
        self.inner.set_project_status(id, status)
    }
    fn replace_sub_projects(
        &self,
        project_id: ProjectId,
        subs: Vec<NewSubProject>,
    ) -> Result<Vec<SubProject>> {
        self.inner.replace_sub_projects(project_id, subs)
    }
    fn update_sub_project_facts(&self, id: SubProjectId, facts: Value) -> Result<()> {
        self.inner.update_sub_project_facts(id, facts)
    }
    fn set_sub_project_frameworks(&self, id: SubProjectId, frameworks: Vec<String>) -> Result<()> {
        self.inner.set_sub_project_frameworks(id, frameworks)
    }
    fn replace_files(
        &self,
        project_id: ProjectId,
        files: Vec<NewSourceFile>,
    ) -> Result<Vec<SourceFile>> {
        self.inner.replace_files(project_id, files)
    }
}

impl GraphSink for FailRuleConfigs {
    fn apply(&self, delta: &GraphDelta) -> Result<()> {
        self.inner.apply(delta)
    }
}

impl GraphQuery for FailRuleConfigs {
    fn query_nodes(&self, filter: &NodeFilter) -> Result<Vec<Node>> {
        self.inner.query_nodes(filter)
    }
    fn get_node(&self, id: NodeId) -> Result<Option<Node>> {
        self.inner.get_node(id)
    }
    fn get_nodes(&self, ids: &[NodeId]) -> Result<HashMap<i64, Node>> {
        self.inner.get_nodes(ids)
    }
    fn edges_of(&self, node: NodeId, direction: EdgeDirection) -> Result<Vec<Edge>> {
        self.inner.edges_of(node, direction)
    }
    fn nodes_summary(&self, project_id: ProjectId) -> Result<HashMap<i64, NodeSummary>> {
        self.inner.nodes_summary(project_id)
    }
    fn edges_outgoing(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> {
        self.inner.edges_outgoing(ids)
    }
    fn edges_incoming(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>> {
        self.inner.edges_incoming(ids)
    }
    fn chain_adjacency(
        &self,
        project_id: ProjectId,
    ) -> Result<(HashMap<i64, Vec<i64>>, HashMap<i64, Vec<i64>>, HashMap<i64, Vec<i64>>)> {
        self.inner.chain_adjacency(project_id)
    }
    fn edge_kinds(&self, project_id: ProjectId) -> Result<Vec<String>> {
        self.inner.edge_kinds(project_id)
    }
    fn node_kinds(&self, project_id: ProjectId) -> Result<Vec<String>> {
        self.inner.node_kinds(project_id)
    }
    fn annotation_kinds(&self, project_id: ProjectId) -> Result<Vec<(String, String)>> {
        self.inner.annotation_kinds(project_id)
    }
    fn annotations_of(&self, node: NodeId) -> Result<Vec<Annotation>> {
        self.inner.annotations_of(node)
    }
    fn annotations_of_project(
        &self,
        project_id: ProjectId,
    ) -> Result<HashMap<i64, Vec<Annotation>>> {
        self.inner.annotations_of_project(project_id)
    }
    fn file_paths(&self, project_id: ProjectId) -> Result<HashMap<i64, String>> {
        self.inner.file_paths(project_id)
    }
    fn stats(&self, project_id: ProjectId) -> Result<GraphStats> {
        self.inner.stats(project_id)
    }
    fn count_nodes(
        &self,
        project_id: ProjectId,
        kind: Option<&NodeKind>,
        side: Option<&str>,
    ) -> Result<u64> {
        self.inner.count_nodes(project_id, kind, side)
    }
    fn coverage(&self, project_id: ProjectId) -> Result<CoverageReport> {
        self.inner.coverage(project_id)
    }
    fn find_edge(&self, id: EdgeId) -> Result<Option<Edge>> {
        self.inner.find_edge(id)
    }
    fn file_path(&self, id: FileId) -> Result<Option<String>> {
        self.inner.file_path(id)
    }
}

impl SymbolTableReader for FailRuleConfigs {
    fn get_symbol(&self, project_id: ProjectId, table: &str, key: &str) -> Result<Option<Value>> {
        self.inner.get_symbol(project_id, table, key)
    }
    fn list_symbols(&self, project_id: ProjectId, table: &str) -> Result<Vec<SymbolEntry>> {
        self.inner.list_symbols(project_id, table)
    }
}

impl DiagnosticSink for FailRuleConfigs {
    fn push_diagnostics(&self, items: &[Diagnostic]) -> Result<()> {
        self.inner.push_diagnostics(items)
    }
    fn list_diagnostics(&self, project_id: ProjectId, limit: u32) -> Result<Vec<Diagnostic>> {
        self.inner.list_diagnostics(project_id, limit)
    }
    fn list_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
        limit: u32,
    ) -> Result<Vec<Diagnostic>> {
        self.inner
            .list_diagnostics_excluding(project_id, exclude_prefix, limit)
    }
    fn list_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
        limit: u32,
    ) -> Result<Vec<Diagnostic>> {
        self.inner
            .list_diagnostics_by_code(project_id, code_prefix, sub_project_id, limit)
    }
    fn clear_diagnostics(&self, project_id: ProjectId, code_prefix: &str) -> Result<u64> {
        self.inner.clear_diagnostics(project_id, code_prefix)
    }
    fn count_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<Vec<(String, u64)>> {
        self.inner
            .count_diagnostics_by_code(project_id, code_prefix, sub_project_id)
    }
    fn count_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, u64)>> {
        self.inner.count_diagnostics_excluding(project_id, exclude_prefix)
    }
    fn count_diagnostics_by_code_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, String, u64)>> {
        self.inner
            .count_diagnostics_by_code_excluding(project_id, exclude_prefix)
    }
}

impl RuleConfigStore for FailRuleConfigs {
    fn get_rule_configs(&self, _: ProjectId) -> Result<HashMap<String, ProjectRuleConfig>> {
        // The only method that fails: `run_check` reads this first, so a failing store makes the
        // auto compliance check return `Err` without touching the graph.
        Err(DomainError::infra("injected rule-config read failure"))
    }
    fn set_rule_config(&self, cfg: &ProjectRuleConfig) -> Result<()> {
        self.inner.set_rule_config(cfg)
    }
    fn delete_rule_config(&self, project_id: ProjectId, rule_id: &str) -> Result<()> {
        self.inner.delete_rule_config(project_id, rule_id)
    }
}

/// Running a non-existent project id must surface `NotFound` (the project lookup in `run_inner`
/// happens before any status flip, so it must not be mistaken for `Conflict` or `Failed`).
#[test]
fn run_on_unknown_project_is_not_found() {
    let (_, svc, _) = setup(0);
    let res = svc.run(ProjectId::new(9_999_999), &NoopObserver);
    assert!(
        matches!(res, Err(DomainError::NotFound(_))),
        "running a non-existent project must be NotFound, got {res:?}"
    );
}

/// A build whose `root_path` does not exist must fail the pipeline and leave the project `Failed`
/// (not stuck at `Indexing`, not silently `Ready`).
#[test]
fn build_of_missing_root_path_fails_and_marks_failed() {
    let store: Arc<dyn Persistence> =
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"));
    let pid = create_it(
        &store,
        std::env::temp_dir()
            .join(format!("gt_missing_root_{}_{}", std::process::id(), pid_counter())),
    );
    let svc = Arc::new(PipelineService::new(
        store.clone(),
        Arc::new(deps(0)),
        Arc::new(NoopRuleProvider),
    ));

    let res = svc.run(pid, &NoopObserver);
    assert!(res.is_err(), "a non-existent root_path must fail the build");
    let status = store.get_project(pid).unwrap().unwrap().status;
    assert_eq!(
        status,
        ProjectStatus::Failed,
        "a failed build must be marked Failed (not left at Indexing)"
    );
}

/// A good build must still succeed and reach `Ready` even when the automatic compliance check
/// fails: `PipelineService::run_check` deliberately swallows check errors so a flaky/down rule
/// engine cannot invalidate an already-built graph.
#[test]
fn run_succeeds_when_auto_check_fails() {
    let inner: Arc<dyn Persistence> =
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"));
    let store: Arc<dyn Persistence> =
        Arc::new(FailRuleConfigs { inner: Arc::clone(&inner) });
    let pid = create_it(&store, scratch_dir());
    let svc = Arc::new(PipelineService::new(
        store.clone(),
        Arc::new(deps(0)),
        Arc::new(NoopRuleProvider),
    ));

    let out = svc
        .run(pid, &NoopObserver)
        .expect("a good build must succeed even if the auto compliance check fails");
    assert!(
        !out.sub_projects.is_empty(),
        "the graph is still produced despite the check failure"
    );
    let status = store.get_project(pid).unwrap().unwrap().status;
    assert_eq!(
        status,
        ProjectStatus::Ready,
        "an auto-check failure must be swallowed, not turn the build Failed"
    );
}

/// Process-unique counter so the (intentionally) non-existent root path is distinct per call.
fn pid_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static C: AtomicU64 = AtomicU64::new(0);
    C.fetch_add(1, Ordering::SeqCst)
}
