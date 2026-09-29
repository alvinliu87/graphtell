//! 集成测试公共装置。
//!
//! # 样本从哪来
//!
//! 测试以 `samples/thinkphp-projects/CRMEB-master` 为材料。为了在没带样本的机器上也能跑 CI，
//! 样本缺失时测试**跳过**而不是失败：
//! * 环境变量 `GRAPHTELL_SAMPLE_DIR` 显式指定，或
//! * 仓库内的相对路径 `samples/thinkphp-projects/CRMEB-master`

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use gt_adapter_fkb::YamlKnowledgeBase;
use gt_adapter_fs::{StdFileSystem, WalkDirScanner};
use gt_adapter_parser::DefaultParserRegistry;
use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{Project, ProjectConfig};
use gt_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, ProjectWriter,
};
use gt_pipeline::runner::{PipelineInfrastructure, PipelineOutcome};

pub const FKB_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fkb");

/// 在 `dir/samples` 下定位 CRMEB 样本。
///
/// 样本实际布局是**按技术栈多层分类**的（如 `samples/php-projects/thinkphp/CRMEB`），
/// 且目录名可能带或不带 `-master` 后缀。早期只匹配 `samples/*/CRMEB-master`（一层 + 后缀），
/// 与真实布局不符 → 样本明明在磁盘上却匹配不到 → 测试静默跳过、CI 全绿但零覆盖。
/// 这里改为在 `samples/` 下**有限深度**递归查找名为 `CRMEB` / `CRMEB-master` 的目录，
/// 不再依赖具体的层级与命名。
fn under_samples(dir: &Path) -> Option<PathBuf> {
    /// 在 `dir` 内最多找 `depth` 层；返回字典序第一个命中（保证结果稳定）。
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

/// 定位 CRMEB 样本根目录。
///
/// 从 `CARGO_MANIFEST_DIR` 向上逐层查找 `samples/**/CRMEB-master`，
/// 兼容「仓库根即工作区」与「工作区嵌套在子目录」两种布局。
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
                .create_project(gt_domain::model::NewProject {
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
                gt_pipeline::runner::run(&project, &infra, &gt_domain::port::NoopObserver)
                    .expect("建图不应失败");
            Some(Arc::new(Built { store, project, outcome }))
        })
        .clone()
}

/// 取某阶段的报告。
pub fn phase_report(built: &Built, phase: &str) -> Option<gt_domain::model::PhaseReport> {
    built.outcome.reports.iter().find(|r| r.phase == phase).cloned()
}

/// 对**任意**工程根目录执行一次完整建图（用于合成样本的自检，无需外部 CRMEB 样本）。
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
            .expect("建图不应失败");
    Some(Arc::new(Built { store, project, outcome }))
}

/// 样本缺失时的标准跳过提示。
pub fn skip_reason() -> &'static str {
    "跳过：未找到 CRMEB 样本（可用 GRAPHTELL_SAMPLE_DIR 指定）"
}
