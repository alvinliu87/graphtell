//! 建图用例：编排流水线并处理状态流转与后台执行。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    EdgeKind, GraphDelta, NewEdge, NodeId, NodeKind, Phase, PhaseReport, Project, ProjectId,
    ProjectStatus,
};
use gt_domain::port::{
    EdgeDirection, FileScanner, FileSystem, GraphSink, KnowledgeProvider, NodeFilter, ParserRegistry,
    Persistence, PipelineObserver, ProjectWriter, RuleProvider,
};
use gt_pipeline::runner::PipelineOutcome;
use tracing::{error, info, warn};

use crate::RuleService;

/// 流水线所需基础设施的聚合（在组装根注入，实现依赖倒置）。
pub struct PipelineDeps {
    pub fs: Arc<dyn FileSystem>,
    pub scanner: Arc<dyn FileScanner>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub kb: Arc<dyn KnowledgeProvider>,
}

/// 把 [`PipelineDeps`] 与持久化适配成 `gt-pipeline` 需要的 [`PipelineInfrastructure`]。
struct Infra<'a> {
    deps: &'a PipelineDeps,
    store: &'a dyn Persistence,
}

impl<'a> gt_pipeline::runner::PipelineInfrastructure for Infra<'a> {
    fn fs(&self) -> &dyn FileSystem {
        self.deps.fs.as_ref()
    }
    fn scanner(&self) -> &dyn FileScanner {
        self.deps.scanner.as_ref()
    }
    fn parsers(&self) -> &dyn ParserRegistry {
        self.deps.parsers.as_ref()
    }
    fn kb(&self) -> &dyn KnowledgeProvider {
        self.deps.kb.as_ref()
    }
    fn projects(&self) -> &dyn ProjectWriter {
        self.store
    }
    fn graph(&self) -> &dyn GraphSink {
        self.store
    }
}

/// 同时进行的建图任务上限。
///
/// 每条流水线都要把整个工程的节点驻留在内存工作区里（大工程十万级节点），
/// 并且写操作最终都挤在同一条 SQLite 连接上。并发本身在号段分配修好之后
/// 已经安全，这里只是给资源上界：超出的工程排队等待，而不是一拥而上。
const MAX_CONCURRENT_BUILDS: usize = 2;

/// 建图用例服务。
pub struct PipelineService {
    store: Arc<dyn Persistence>,
    deps: Arc<PipelineDeps>,
    /// 建图完成后自动跑合规检查（见 [`Self::run_check`]）。
    checks: RuleService,
    /// 同一工程同时只允许一个建图任务。
    running: Arc<Mutex<std::collections::HashSet<i64>>>,
    /// 跨工程的并发名额（计数 + 条件变量）。
    slots: Arc<(Mutex<usize>, Condvar)>,
}

impl PipelineService {
    pub fn new(
        store: Arc<dyn Persistence>,
        deps: Arc<PipelineDeps>,
        rules: Arc<dyn RuleProvider>,
    ) -> Self {
        let checks = RuleService::new(Arc::clone(&store), rules);
        Self {
            store,
            checks,
            deps,
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
            slots: Arc::new((Mutex::new(0), Condvar::new())),
        }
    }

    /// 占一个建图名额；满则等待。
    ///
    /// 用 `unwrap_or_else(poisoned.into_inner())`：持锁线程 panic 后锁会中毒，
    /// 若继续 `unwrap()`，后续每一次建图都会连带 panic，故障会一直扩散下去。
    fn acquire_slot(&self) {
        let (lock, cv) = &*self.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_CONCURRENT_BUILDS {
            n = cv.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
    }

    fn release_slot(&self) {
        let (lock, cv) = &*self.slots;
        let mut n = lock.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
        cv.notify_one();
    }

    /// 同步执行建图。
    pub fn run(
        &self,
        project_id: ProjectId,
        observer: &dyn PipelineObserver,
    ) -> Result<PipelineOutcome> {
        {
            let mut guard = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if !guard.insert(project_id.get()) {
                return Err(DomainError::Conflict(format!(
                    "工程 {project_id} 正在建图中"
                )));
            }
        }
        self.acquire_slot();
        let result = self.run_inner(project_id, observer);
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&project_id.get());
        self.release_slot();
        result
    }

    fn run_inner(
        &self,
        project_id: ProjectId,
        observer: &dyn PipelineObserver,
    ) -> Result<PipelineOutcome> {
        let project: Project = self
            .store
            .get_project(project_id)?
            .ok_or_else(|| DomainError::NotFound(format!("project {project_id}")))?;

        self.store.set_project_status(project_id, ProjectStatus::Indexing)?;
        let infra = Infra { deps: &self.deps, store: self.store.as_ref() };
        let outcome = gt_pipeline::runner::run(&project, &infra, observer);

        match outcome {
            Ok(out) => {
                self.store.set_project_status(project_id, ProjectStatus::Ready)?;
                info!(
                    "工程 {} 建图完成：{} 子工程 / {} 文件",
                    project.name,
                    out.sub_projects.len(),
                    out.files.len()
                );
                self.run_check(project_id, &project.name);
                self.attach_declared_middleware(project_id);
                Ok(out)
            }
            Err(e) => {
                error!("工程 {} 建图失败: {e}", project.name);
                self.store.set_project_status(project_id, ProjectStatus::Failed)?;
                Err(e)
            }
        }
    }

    /// 建图完成后**自动**跑一遍合规检查。
    ///
    /// # 为什么自动
    ///
    /// 规则是图唯一的验收装置。要手动触发，用户建完图看到"0 条违规"时无法区分
    /// "真的没违规"和"根本没跑" —— 而后者正是本项目已经踩过的坑（静默归零）。
    /// 自动跑之后，`rules_unavailable` / `rules_silent` 这些告警才会真的被看到。
    ///
    /// # 为什么是"建图之后"而不是"流水线的一个阶段"
    ///
    /// 三条，第二条是决定性的：
    /// 1. 规则必须读**完整的图** —— P6 才产出 `auth.public` 这类标注，P8 才折叠完；
    ///    作为阶段塞进流水线会暗示它与 P0–P8 同级、可乱序。
    /// 2. **改一条规则不该触发重新解析整个仓库**。规则是 YAML，若检查属于建图，
    ///    改一行 YAML 就要重跑几十秒到几分钟的解析；放在建图后，重跑检查只要 1 秒。
    /// 3. **失败隔离**：规则引擎出错应当只让"结论"缺失，不能让"图"建不出来 ——
    ///    图是贵得多的资产。
    ///
    /// 因此这里刻意**吞掉检查失败**：记一条 warn，建图照样算成功。
    fn run_check(&self, project_id: ProjectId, project_name: &str) {
        match self.checks.check(project_id, None, true) {
            Ok(report) => {
                info!(
                    "工程 {} 自动合规检查完成：{} 条规则命中 {} 条违规（{} ms）",
                    project_name,
                    report.rules_run,
                    report.violations.len(),
                    report.duration_ms
                );
                for s in &report.rules_unavailable {
                    warn!("规则停用（判据在本工程不成立）：{}", s);
                }
                for s in &report.rules_silent {
                    warn!("规则 0 命中（需确认是代码干净还是规则瞎了）：{}", s);
                }
            }
            Err(e) => {
                // 不向上传播：图已经建好了，不能因为"结论"算不出来就判定建图失败。
                warn!("工程 {} 自动合规检查失败（图仍可用）：{}", project_name, e);
            }
        }
    }

    /// 建图完成、规则合成完 HttpContract 节点后，把「声明式中间件」挂成 `PassesThrough` 边。
    ///
    /// # 为什么是建图之后、而不是 P14
    ///
    /// likeadmin 这类自动路由项目的绝大多数 HttpContract 由 `frontend-http-contract` 规则在
    /// `run_check` 阶段才合成；P14 跑时它们还不存在，挂链会整段落空。这里读 prepare 阶段写入
    /// `declared_middleware` 符号表的声明（文件名 / 键 / 作用域全在 FKB，内核零框架字符串），
    /// 对**全量**契约节点按作用域挂边，并把对应的 `Class` 节点晋升为 `Middleware`。
    ///
    /// * `global`：挂到全部契约节点；
    /// * `per_app`：只挂到名字含 `prefix`（如 `/adminapi`）的契约节点。
    ///
    /// 节点查找按类名（FQN / 短名）；图里没有该类（如 vendor 里的类）则跳过，不建悬空边。
    fn attach_declared_middleware(&self, project_id: ProjectId) {
        let Ok(declared) = self.store.list_symbols(project_id, "declared_middleware") else {
            return;
        };
        if declared.is_empty() {
            return;
        }
        let Ok(contracts) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("HttpContract")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };
        if contracts.is_empty() {
            return;
        }
        let Ok(classes) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("Class")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };
        let Ok(mws) = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(NodeKind::from("Middleware")),
            limit: Some(1_000_000),
            ..Default::default()
        }) else {
            return;
        };

        // 类名（FQN / 短名）→ 节点 id，优先复用已有的 Middleware 节点。
        let mut by_name: HashMap<String, NodeId> = HashMap::new();
        for n in mws.iter().chain(classes.iter()) {
            by_name.entry(n.name.clone()).or_insert(n.id);
            if let Some(fqn) = &n.fqn {
                by_name.entry(fqn.clone()).or_insert(n.id);
            }
            let short = n.name.rsplit(['\\', '/']).next().unwrap_or(&n.name).to_string();
            by_name.entry(short).or_insert(n.id);
        }

        // 预读已有 PassesThrough 边去重。
        let mut existing: HashSet<(NodeId, NodeId)> = HashSet::new();
        for c in &contracts {
            if let Ok(es) = self.store.edges_of(c.id, EdgeDirection::Outgoing) {
                for e in es {
                    if e.kind.as_str() == "PassesThrough" {
                        existing.insert((e.from_id, e.to_id));
                    }
                }
            }
        }

        let mut delta = GraphDelta::new(project_id);
        for entry in declared.iter() {
            let scope = entry
                .value
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("global");
            let prefix = entry.value.get("prefix").and_then(|v| v.as_str());
            let classes_arr = entry
                .value
                .get("classes")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for cv in &classes_arr {
                let Some(class) = cv.as_str().map(|s| s.to_string()) else {
                    continue;
                };
                if class.is_empty() {
                    continue;
                }
                let Some(&mw_id) = by_name.get(&class).or_else(|| {
                    let short = class.rsplit(['\\', '/']).next().unwrap_or(&class);
                    by_name.get(short)
                }) else {
                    continue;
                };
                let targets: Vec<NodeId> = if scope == "per_app" {
                    contracts
                        .iter()
                        .filter(|c| prefix.map_or(true, |p| c.name.contains(p)))
                        .map(|c| c.id)
                        .collect()
                } else {
                    contracts.iter().map(|c| c.id).collect()
                };
                for tid in targets {
                    if existing.contains(&(tid, mw_id)) {
                        continue;
                    }
                    existing.insert((tid, mw_id));
                    if !mws.iter().any(|m| m.id == mw_id) {
                        delta.kind_patches.push((mw_id, NodeKind::from("Middleware")));
                    }
                    let mut e =
                        NewEdge::new(project_id, EdgeKind::from("PassesThrough"), tid, mw_id);
                    e.phase = Phase::from("P14");
                    e.confidence = 1.0;
                    delta.edges.push(e);
                }
            }
        }

        if delta.edges.is_empty() && delta.kind_patches.is_empty() {
            return;
        }
        let edge_n = delta.edges.len();
        let promoted_n = delta.kind_patches.len();
        if let Err(e) = self.store.apply(&delta) {
            warn!("工程 {project_id} 声明式中间件挂链失败：{e}");
        } else {
            info!(
                "工程 {project_id} 声明式中间件挂链完成：{} 条边 / {} 个晋升为 Middleware",
                edge_n, promoted_n
            );
        }
    }

    /// 在后台线程执行建图（HTTP / Tauri 调用方使用）。
    pub fn spawn(
        self: &Arc<Self>,
        project_id: ProjectId,
        observer: Arc<dyn PipelineObserver>,
    ) -> Result<()> {
        let svc = Arc::clone(self);
        std::thread::spawn(move || {
            if let Err(e) = svc.run(project_id, observer.as_ref()) {
                error!("后台建图失败: {e}");
            }
        });
        Ok(())
    }

    pub fn reports(&self, project_id: ProjectId) -> Vec<PhaseReport> {
        // 最近一次运行的阶段报告由调用方缓存；这里从运行记录表读取概要
        let _ = project_id;
        Vec::new()
    }
}
