//! Shared fixtures for the integration tests.
//!
//! # Where the samples come from
//!
//! The tests use `samples/thinkphp-projects/CRMEB-master` as their material. So that CI can run on a machine
//! without samples, a missing sample makes the test **skip** rather than fail:
//! * the environment variable `GRAPHTELL_SAMPLE_DIR` points at it explicitly, or
//! * the in-repo relative path `samples/thinkphp-projects/CRMEB-master`

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_php::PhpTechStackAdapter;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_techstack::DefaultMarkerProvider;
use gt_domain::model::{Project, ProjectConfig};
use gt_domain::port::{
    DefaultTechStackRegistry, FileScanner, FileSystem, GraphSink, KnowledgeProvider, MarkerProvider,
    ParserRegistry, ProjectWriter, TechStackRegistry,
};
use gt_pipeline::runner::{PipelineInfrastructure, PipelineOutcome};

pub const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// Locate the CRMEB sample under `dir/samples`.
///
/// The real layout is **classified by tech stack over several levels** (e.g.
/// `samples/php-projects/thinkphp/CRMEB`), and the directory name may or may not carry a `-master` suffix. An early
/// version only matched `samples/*/CRMEB-master` (one level + suffix), which did not fit the real layout — the
/// sample was on disk yet never matched, so the tests silently skipped and CI was all green with zero coverage.
/// This now does a **bounded-depth** recursive search under `samples/` for a directory named `CRMEB` /
/// `CRMEB-master`, no longer depending on a specific depth or naming.
fn under_samples(dir: &Path) -> Option<PathBuf> {
    /// Search at most `depth` levels under `dir`; return the lexicographically first hit (for a stable result).
    fn search(dir: &Path, depth: usize) -> Option<PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut hits: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "CRMEB" || name == "CRMEB-master" {
                hits.push(path);
            } else if let Some(found) = search(&path, depth - 1) {
                hits.push(found);
            }
        }
        hits.sort();
        hits.into_iter().next()
    }
    search(&dir.join("samples"), 3)
}

/// Locate the CRMEB sample root directory.
///
/// Walks upward from `CARGO_MANIFEST_DIR` looking for `samples/**/CRMEB-master`, supporting both layouts —
/// "the repo root is the workspace" and "the workspace is nested in a subdirectory".
pub fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("GRAPHTELL_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if let Some(candidate) = under_samples(&cur) {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

pub struct TestInfra {
    fs: StdFileSystem,
    scanner: WalkDirScanner,
    parsers: DefaultParserRegistry,
    kb: YamlKnowledgeBase,
    techstack: DefaultTechStackRegistry,
    markers: DefaultMarkerProvider,
    store: Arc<SqliteStore>,
}

impl TestInfra {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        let kb = YamlKnowledgeBase::load_dir(Path::new(FKB_DIR))
            .expect("the FKB directory must load, otherwise the test environment is broken");
        assert!(!kb.is_empty(), "FKB must load at least 1 framework knowledge base");
        Self {
            fs: StdFileSystem::new(),
            scanner: WalkDirScanner::new(Vec::new()),
            parsers: DefaultParserRegistry::new(),
            kb,
            techstack: DefaultTechStackRegistry::new()
                .register(Box::new(PhpTechStackAdapter::new())),
            markers: DefaultMarkerProvider::new(),
            store,
        }
    }
}

impl PipelineInfrastructure for TestInfra {
    fn fs(&self) -> &dyn FileSystem {
        &self.fs
    }
    fn scanner(&self) -> &dyn FileScanner {
        &self.scanner
    }
    fn parsers(&self) -> &dyn ParserRegistry {
        &self.parsers
    }
    fn techstack(&self) -> &dyn TechStackRegistry {
        &self.techstack
    }
    fn markers(&self) -> &dyn MarkerProvider {
        &self.markers
    }
    fn kb(&self) -> &dyn KnowledgeProvider {
        &self.kb
    }
    fn projects(&self) -> &dyn ProjectWriter {
        self.store.as_ref()
    }
    fn graph(&self) -> &dyn GraphSink {
        self.store.as_ref()
    }
}

pub struct Built {
    pub store: Arc<SqliteStore>,
    pub project: Project,
    pub outcome: PipelineOutcome,
}

/// Run one complete graph build on the sample (the result is cached in-process so each case need not re-run it).
pub fn graph() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let root = sample_root()?;
            let store = Arc::new(SqliteStore::in_memory().ok()?);
            let project = store
                .create_project(gt_domain::model::NewProject {
                    name: "CRMEB".into(),
                    root_path: root,
                    description: None,
                    config: Some(ProjectConfig {
                        table_prefixes: vec!["eb_".into()],
                        ..Default::default()
                    }),
                })
                .ok()?;
            let infra = TestInfra::new(Arc::clone(&store));
            let outcome =
                gt_pipeline::runner::run(&project, &infra, &gt_domain::port::NoopObserver)
                    .expect("the graph build should not fail");
            Some(Arc::new(Built { store, project, outcome }))
        })
        .clone()
}

/// Take the report of one phase.
pub fn phase_report(built: &Built, phase: &str) -> Option<gt_domain::model::PhaseReport> {
    built.outcome.reports.iter().find(|r| r.phase == phase).cloned()
}

/// Run one complete graph build against **any** project root (for self-checks on synthetic samples, no external CRMEB sample needed).
pub fn graph_with_root(root: &Path, config: ProjectConfig) -> Option<Arc<Built>> {
    let store = Arc::new(SqliteStore::in_memory().ok()?);
    let project = store
        .create_project(gt_domain::model::NewProject {
            name: "Synthetic".into(),
            root_path: root.to_path_buf(),
            description: None,
            config: Some(config),
        })
        .ok()?;
    let infra = TestInfra::new(Arc::clone(&store));
    let outcome =
        gt_pipeline::runner::run(&project, &infra, &gt_domain::port::NoopObserver)
            .expect("the graph build should not fail");
    Some(Arc::new(Built { store, project, outcome }))
}

/// The standard skip message when the sample is missing.
pub fn skip_reason() -> &'static str {
    "skip: CRMEB sample not found (point GRAPHTELL_SAMPLE_DIR at it)"
}
