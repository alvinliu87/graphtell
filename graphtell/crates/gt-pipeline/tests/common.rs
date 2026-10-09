//! Shared fixtures for the integration tests.
//!
//! # Where the samples come from
//!
//! The tests use the real `sample_project` checkout as material, located by
//! `gt_sample_support::sample_root` (the `GRAPHTELL_SAMPLE_DIR` env var, or a bounded-depth search
//! under `samples/`). So that CI can run on a machine without samples, a missing sample makes the
//! test **skip** rather than fail.

#![allow(dead_code)]

use std::path::Path;
use std::sync::{Arc, OnceLock};

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_techstack::PhpTechStackAdapter;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_techstack::{
    DefaultMarkerProvider, DotnetTechStackAdapter, JavaTechStackAdapter, JsTechStackAdapter,
    PythonTechStackAdapter, RubyTechStackAdapter, RustTechStackAdapter,
};
use gt_adapter_resource::MyBatisMapperAdapter;
use gt_domain::model::{Project, ProjectConfig};
use gt_domain::port::{
    DefaultResourceAdapterRegistry, DefaultTechStackRegistry, FileScanner, FileSystem, GraphSink,
    KnowledgeProvider, MarkerProvider, ParserRegistry, ProjectWriter, ResourceAdapterRegistry,
    TechStackRegistry,
};
use gt_pipeline::runner::{PipelineInfrastructure, PipelineOutcome};

pub const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// Locate the `sample_project` sample root directory.
///
/// Delegates to `gt_sample_support::sample_root`, the single source of truth for where the real,
/// oversized sample checkout lives (the `GRAPHTELL_SAMPLE_DIR` env var, or a bounded-depth search
/// under `samples/`). Keeping the discovery in one crate means a layout/name change touches only
/// that crate, not every test file.
pub use gt_sample_support::sample_root;

pub struct TestInfra {
    fs: StdFileSystem,
    scanner: WalkDirScanner,
    parsers: DefaultParserRegistry,
    kb: YamlKnowledgeBase,
    techstack: DefaultTechStackRegistry,
    markers: DefaultMarkerProvider,
    resources: DefaultResourceAdapterRegistry,
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
            // Same registration as the production composition root (`gt-app/src/container.rs`): every adapter
            // belongs here. Missing one does not fail — it silently degrades `manifest_has` / `lock_has` to
            // their *text-probe fallback*, so a broken adapter (dependency extraction) could not fail a single
            // integration test. Java, Python and Rust were all absent until they were added one by one.
            techstack: DefaultTechStackRegistry::new()
                .register(Box::new(PhpTechStackAdapter::new()))
                .register(Box::new(JsTechStackAdapter::new()))
                .register(Box::new(JavaTechStackAdapter::new()))
                .register(Box::new(PythonTechStackAdapter::new()))
                .register(Box::new(RustTechStackAdapter::new()))
                .register(Box::new(RubyTechStackAdapter::new()))
                .register(Box::new(DotnetTechStackAdapter::new())),
            markers: DefaultMarkerProvider::new(),
            resources: DefaultResourceAdapterRegistry::new()
                .register(Box::new(MyBatisMapperAdapter::default())),
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
    fn resources(&self) -> &dyn ResourceAdapterRegistry {
        &self.resources
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
                    name: "sample_project".into(),
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

/// Run one complete graph build against **any** project root (for self-checks on synthetic samples, no external sample_project sample needed).
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
    "skip: sample_project sample not found (point GRAPHTELL_SAMPLE_DIR at it)"
}
