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
use gt_domain::error::DomainError;
use gt_domain::model::{CheckRule, NewProject, ProjectConfig, ProjectId, ProjectStatus};
use gt_domain::port::{
    DefaultResourceAdapterRegistry, DefaultTechStackRegistry, Marker, MarkerProvider, NoopObserver,
    Persistence, RuleProvider,
};

const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// A `MarkerProvider` that sleeps `ms` on every `markers()` call, so a build takes visibly longer than the
/// scheduling quantum and concurrent builds overlap enough for the guard / slot cap to be observable.
struct SlowMarkerProvider {
    inner: DefaultMarkerProvider,
    ms: u64,
}
impl MarkerProvider for SlowMarkerProvider {
    fn markers(&self) -> Vec<Marker> {
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
    assert!(!out.sub_projects.is_empty() || out.files.is_empty(), "build produced an outcome");
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
