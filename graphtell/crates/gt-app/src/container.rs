//! 依赖装配容器。

use std::sync::Arc;

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

        Ok(Self { config, store, deps, views, rules })
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
    /// 若编译时启用 `model-candle` 且运行时在 `GT_BGE_MODEL` 指定的目录
    /// （默认 `models/bge-m3-safetensors`）找到权重，则使用真实的 bge-m3 语义编码器；
    /// 否则安全退回默认的本地哈希编码器（离线、零依赖）。
    pub fn recall_service(&self) -> gt_application::RecallService {
        #[cfg(feature = "model-candle")]
        if let Some(embedder) = Self::load_candle_embedder() {
            return gt_application::RecallService::with_embedder(
                self.store.clone() as Arc<dyn Persistence>,
                Arc::clone(&self.deps.fs),
                Arc::clone(&self.deps.scanner),
                embedder,
            );
        }
        gt_application::RecallService::new(
            self.store.clone() as Arc<dyn Persistence>,
            Arc::clone(&self.deps.fs),
            Arc::clone(&self.deps.scanner),
        )
    }

    /// 尝试从 `GT_BGE_MODEL`（默认 `models/bge-m3-safetensors`）加载真实 bge-m3
    /// 语义编码器；失败（缺权重 / 加载错误）返回 `None`，由调用方退回默认哈希编码器。
    #[cfg(feature = "model-candle")]
    fn load_candle_embedder() -> Option<Arc<dyn gt_application::Embedder>> {
        use gt_application::embed_model::CandleBgeEmbedder;

        let dir = std::env::var("GT_BGE_MODEL")
            .unwrap_or_else(|_| "models/bge-m3-safetensors".to_string());
        match CandleBgeEmbedder::load(&dir) {
            Ok(embedder) => {
                tracing::info!("已加载真实 bge-m3 语义编码器（{dir}）");
                Some(Arc::new(embedder))
            }
            Err(err) => {
                tracing::warn!("bge-m3 模型加载失败（{dir}），退回本地哈希编码器：{err}");
                None
            }
        }
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
