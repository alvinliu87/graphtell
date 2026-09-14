//! 集成测试公共装置。
//!
//! # 样本从哪来
//!
//! 测试以 `samples/CRMEB-master` 为材料。为了在没带样本的机器上也能跑 CI，
//! 样本缺失时测试**跳过**而不是失败：
//! * 环境变量 `ENTROPY_MATE_SAMPLE_DIR` 显式指定，或
//! * 仓库内的相对路径 `samples/CRMEB-master`

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use em_adapter_fkb::YamlKnowledgeBase;
use em_adapter_fs::{StdFileSystem, WalkDirScanner};
use em_adapter_parser::DefaultParserRegistry;
use em_adapter_sqlite::SqliteStore;
use em_domain::model::{Project, ProjectConfig};
use em_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, ProjectWriter,
};
use em_pipeline::runner::{PipelineInfrastructure, PipelineOutcome};

pub const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// 定位 CRMEB 样本根目录。
///
/// 从 `CARGO_MANIFEST_DIR` 向上逐层查找 `samples/CRMEB-master`，
/// 兼容「仓库根即工作区」与「工作区嵌套在子目录」两种布局。
pub fn sample_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ENTROPY_MATE_SAMPLE_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        let candidate = cur.join("samples/CRMEB-master");
        if candidate.is_dir() {
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
    store: Arc<SqliteStore>,
}

impl TestInfra {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        let kb = YamlKnowledgeBase::load_dir(Path::new(FKB_DIR))
            .expect("FKB 目录必须可加载，否则测试环境有问题");
        assert!(!kb.is_empty(), "FKB 至少要装载 1 份框架知识");
        Self {
            fs: StdFileSystem::new(),
            scanner: WalkDirScanner::new(Vec::new()),
            parsers: DefaultParserRegistry::new(),
            kb,
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

/// 对样本执行一次完整建图（结果在进程内缓存，避免每个用例都跑一遍）。
pub fn graph() -> Option<Arc<Built>> {
    static CACHE: OnceLock<Option<Arc<Built>>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            let root = sample_root()?;
            let store = Arc::new(SqliteStore::in_memory().ok()?);
            let project = store
                .create_project(em_domain::model::NewProject {
                    name: "CRMEB".into(),
                    root_path: root,
                    description: None,
                    // CRMEB 的表前缀为 `eb_`，在此显式声明（不再依赖通用默认值）。
                    // 通用层 `ProjectConfig::default()` 已不再内置任何前缀，
                    // 前缀改由 P3 从 `config/database.php` 自动探测，
                    // 或像这里一样由工程配置给出。
                    config: Some(ProjectConfig {
                        table_prefixes: vec!["eb_".into()],
                        ..Default::default()
                    }),
                })
                .ok()?;
            let infra = TestInfra::new(Arc::clone(&store));
            let outcome =
                em_pipeline::runner::run(&project, &infra, &em_domain::port::NoopObserver)
                    .expect("建图不应失败");
            Some(Arc::new(Built { store, project, outcome }))
        })
        .clone()
}

/// 取某阶段的报告。
pub fn phase_report(built: &Built, phase: &str) -> Option<em_domain::model::PhaseReport> {
    built.outcome.reports.iter().find(|r| r.phase == phase).cloned()
}

/// 样本缺失时的标准跳过提示。
pub fn skip_reason() -> &'static str {
    "跳过：未找到 CRMEB 样本（可用 ENTROPY_MATE_SAMPLE_DIR 指定）"
}
