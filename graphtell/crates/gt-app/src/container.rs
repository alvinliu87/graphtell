//! 依赖装配容器。

use std::sync::Arc;

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_http::build_router;
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_sqlite::SqliteStore;
use gt_adapter_views::YamlViewRegistry;
use gt_application::pipeline_runner::PipelineDeps;
use gt_domain::error::Result;
use gt_domain::port::{
    FileScanner, FileSystem, KnowledgeProvider, ParserRegistry, Persistence, ViewRegistryProvider,
};
use tracing::info;

use crate::config::AppConfig;

/// 应用容器：持有全部适配器实例并对外暴露组装好的服务。
pub struct Container {
    pub config: AppConfig,
    pub store: Arc<SqliteStore>,
    pub deps: Arc<PipelineDeps>,
    pub views: Arc<YamlViewRegistry>,
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

        let deps = Arc::new(PipelineDeps {
            fs: Arc::new(StdFileSystem::new()),
            scanner: Arc::new(WalkDirScanner::new(Vec::new())),
            parsers: Arc::new(DefaultParserRegistry::new()),
            kb: Arc::new(kb),
        });

        Ok(Self { config, store, deps, views })
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
        );
        build_router(state)
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
