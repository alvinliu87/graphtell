//! 依赖装配容器。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::OnceLock;

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_http::build_router;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_rules::YamlRuleSet;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_views::YamlViewRegistry;
use gt_application::pipeline_runner::PipelineDeps;
use gt_domain::error::Result;
use gt_domain::port::{
    FileScanner, FileSystem, KnowledgeProvider, ParserRegistry, Persistence, RuleProvider,
    ViewRegistryProvider,
};
use tracing::info;

use crate::config::AppConfig;

/// 应用容器：持有全部适配器实例并对外暴露组装好的服务。
pub struct Container {
    pub config: AppConfig,
    pub store: Arc<SqliteStore>,
    pub deps: Arc<PipelineDeps>,
    pub views: Arc<YamlViewRegistry>,
    pub rules: Arc<YamlRuleSet>,
    /// 共享语义编码器（进程内懒加载一次：编译并配置了 bge-m3 时为 `Some`，否则 `None`）。
    semantic_embedder: OnceLock<Option<Arc<dyn gt_application::Embedder>>>,
    /// 共享节点向量缓存：首次召回预热后，后续召回只编码查询一次。
    node_embed_cache: Arc<Mutex<HashMap<i64, Vec<f32>>>>,
}

impl Container {
    /// 依据配置构建容器。
    pub fn new(config: AppConfig) -> Result<Self> {
        std::fs::create_dir_all(&config.data_dir).ok();
        let store = Arc::new(SqliteStore::open(config.database_path())?);

        let kb = YamlKnowledgeBase::load_dir(&config.resolve_fkb_dir())?;
        info!("已装载 {} 份框架知识", kb.len());

        let views = Arc::new(YamlViewRegistry::load_dir(&config.resolve_views_dir())?);
        info!("已装载 {} 个视角声明", views.registry().perspectives.len());

        let rules = Arc::new(YamlRuleSet::load_dir(&config.resolve_rules_dir())?);
        info!("已装载 {} 条检查规则", rules.len());

        let deps = Arc::new(PipelineDeps {
            fs: Arc::new(StdFileSystem::new()),
            scanner: Arc::new(WalkDirScanner::new(Vec::new())),
            parsers: Arc::new(DefaultParserRegistry::new()),
            kb: Arc::new(kb),
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

    /// 支持的编程语言。
    pub fn languages(&self) -> Vec<String> {
        self.deps
            .parsers
            .supported_languages()
            .into_iter()
            .map(|l| l.to_string())
            .collect()
    }

    /// 视角注册表（供 Tauri / CLI 直接使用）。
    pub fn views(&self) -> Arc<dyn ViewRegistryProvider> {
        Arc::clone(&self.views) as Arc<dyn ViewRegistryProvider>
    }

    /// 已装载的框架数量。
    pub fn framework_count(&self) -> usize {
        self.deps.kb.all().len()
    }

    /// 构建 HTTP 路由。
    pub fn router(&self) -> axum::Router {
        let state = gt_adapter_http::router::state(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.deps),
            self.framework_count(),
            Arc::clone(&self.views) as Arc<dyn ViewRegistryProvider>,
            Arc::clone(&self.rules) as Arc<dyn RuleProvider>,
        );
        build_router(state)
    }

    /// 合规检查服务（供 Tauri / CLI 直接使用）。
    pub fn rule_service(&self) -> gt_application::RuleService {
        gt_application::RuleService::new(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.rules) as Arc<dyn RuleProvider>,
        )
    }

    /// 代码召回服务（供 Tauri / CLI 直接使用）。
    ///
    /// 编码器由 [`gt_application::resolve_recall_embedder`] 解析：编译了 `model-candle`
    /// 且 `GT_BGE_MODEL`（默认 `models/bge-m3-safetensors`）权重可用时走真实 bge-m3，
    /// 否则安全退回默认本地哈希编码器。节点向量缓存跨多次召回复用 —— 首次召回预热
    /// （对全图编码一次），之后每次只对查询编码一次 + 全图点积，亚秒级返回。
    pub fn recall_service(&self) -> gt_application::RecallService {
        // CLI 召回：优先真实 bge-m3（权重可用时），缺权重则无语义编码器（退回词面 / 快速路）。
        // 这里**不**开启后台异步预热（避免 CLI 进程提前退出杀掉线程）；持久化走手动 `embed` 命令。
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

    /// 各端口的只读引用（供 Tauri / CLI 直接使用）。
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
