//! Dependency assembly container.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::OnceLock;

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_http::build_router;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_php::PhpTechStackAdapter;
use gt_adapter_rules::YamlRuleSet;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_techstack::{DefaultMarkerProvider, JsTechStackAdapter};
use gt_adapter_resource::MyBatisMapperAdapter;
use gt_adapter_views::YamlViewRegistry;
use gt_application::pipeline_runner::PipelineDeps;
use gt_domain::error::Result;
use gt_domain::port::{
    DefaultResourceAdapterRegistry, DefaultTechStackRegistry, FileScanner, FileSystem,
    KnowledgeProvider, ParserRegistry, Persistence, RuleProvider, TechStackRegistry,
    ViewRegistryProvider,
};
use tracing::info;

use crate::config::AppConfig;

/// The application container: holds every adapter instance and exposes the assembled services.
pub struct Container {
    pub config: AppConfig,
    pub store: Arc<SqliteStore>,
    pub deps: Arc<PipelineDeps>,
    pub views: Arc<YamlViewRegistry>,
    pub rules: Arc<YamlRuleSet>,
    /// Shared semantic encoder (loaded lazily once per process: `Some` when bge-m3 is compiled in and configured, `None` otherwise).
    semantic_embedder: OnceLock<Option<Arc<dyn gt_application::Embedder>>>,
    /// Shared node-vector cache: after the first recall warms it up, later recalls only encode the query once.
    node_embed_cache: Arc<Mutex<HashMap<u64, Vec<f32>>>>,
}

impl Container {
    /// Build the container from the configuration.
    pub fn new(config: AppConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir).ok();
        let store = Arc::new(SqliteStore::open(config.database_path())?);

        let kb = YamlKnowledgeBase::load_dir(&config.resolve_fkb_dir())?;
        info!("Loaded {} framework-knowledge files", kb.len());

        let views = Arc::new(YamlViewRegistry::load_dir(&config.resolve_views_dir())?);
        info!("Loaded {} perspective declarations", views.registry().perspectives.len());

        let rules = Arc::new(YamlRuleSet::load_dir(&config.resolve_rules_dir())?);
        info!("Loaded {} check rules", rules.len());

        let techstack: Arc<dyn TechStackRegistry> = Arc::new(
            DefaultTechStackRegistry::new()
                .register(Box::new(PhpTechStackAdapter::new()))
                .register(Box::new(JsTechStackAdapter::new())),
        );
        let deps = Arc::new(PipelineDeps {
            fs: Arc::new(StdFileSystem::new()),
            scanner: Arc::new(WalkDirScanner::new(Vec::new())),
            parsers: Arc::new(DefaultParserRegistry::new()),
            kb: Arc::new(kb),
            techstack,
            markers: Arc::new(DefaultMarkerProvider::new()),
            resources: Arc::new(
                DefaultResourceAdapterRegistry::new()
                    .register(Box::new(MyBatisMapperAdapter::default())),
            ),
        });

        Ok(Self {
            config,
            store,
            deps,
            views,
            rules,
            semantic_embedder: OnceLock::new(),
            node_embed_cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Supported programming languages.
    pub fn languages(&self) -> Vec<String> {
        self.deps
            .parsers
            .supported_languages()
            .into_iter()
            .map(|l| l.to_string())
            .collect()
    }

    /// The perspective registry (for direct use by Tauri / the CLI).
    pub fn views(&self) -> Arc<dyn ViewRegistryProvider> {
        Arc::clone(&self.views) as Arc<dyn ViewRegistryProvider>
    }

    /// Number of frameworks loaded.
    pub fn framework_count(&self) -> usize {
        self.deps.kb.all().len()
    }

    /// Build the HTTP router.
    pub fn router(&self) -> axum::Router {
        let state = gt_adapter_http::router::state(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.deps),
            self.framework_count(),
            Arc::clone(&self.views) as Arc<dyn ViewRegistryProvider>,
            Arc::clone(&self.rules) as Arc<dyn RuleProvider>,
        );
        build_router(state, self.config.resolve_ui_dir())
    }

    /// The compliance-check service (for direct use by Tauri / the CLI).
    pub fn rule_service(&self) -> gt_application::RuleService {
        gt_application::RuleService::new(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.rules) as Arc<dyn RuleProvider>,
        )
    }

    /// The prompt-augmentation service (code recall + prompt composition; for direct use by Tauri / the CLI).
    ///
    /// The encoder is resolved by [`gt_application::resolve_recall_embedder`]: when `model-candle` is compiled in
    /// and the `GT_BGE_MODEL` weights (default `models/bge-m3-safetensors`) are available it uses the real bge-m3,
    /// otherwise it safely falls back to the default local hash encoder. The node-vector cache is reused across
    /// recalls — the first recall warms it up (encoding the whole graph once), after which each recall only encodes
    /// the query once plus a dot product over the whole graph, returning in sub-second time.
    pub fn recall_service(&self) -> gt_application::RecallService {
        // CLI recall: prefer the real bge-m3 (when weights are available); without weights there is no semantic
        // encoder (falling back to the lexical / fast path). Background async warm-up is deliberately **not**
        // enabled here (so the CLI process cannot exit early and kill the thread); persistence goes through the manual `embed` command.
        let semantic = self
            .semantic_embedder
            .get_or_init(gt_application::try_real_recall_embedder)
            .clone();
        gt_application::RecallService::with_embedder_and_cache(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.deps.fs),
            Arc::clone(&self.deps.scanner),
            semantic,
            Arc::clone(&self.node_embed_cache),
            Some(std::path::PathBuf::from("data/embeddings")),
            None,
        )
    }

    /// Read-only references to each port (for direct use by Tauri / the CLI).
    pub fn filesystem(&self) -> Arc<dyn FileSystem> {
        Arc::clone(&self.deps.fs)
    }

    pub fn scanner(&self) -> Arc<dyn FileScanner> {
        Arc::clone(&self.deps.scanner)
    }

    pub fn parsers(&self) -> Arc<dyn ParserRegistry> {
        Arc::clone(&self.deps.parsers)
    }

    pub fn knowledge(&self) -> Arc<dyn KnowledgeProvider> {
        Arc::clone(&self.deps.kb)
    }
}
