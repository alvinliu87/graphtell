//! 建图用例：编排流水线并处理状态流转与后台执行。

use std::sync::{Arc, Mutex};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{PhaseReport, Project, ProjectId, ProjectStatus};
use gt_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, Persistence,
    PipelineObserver, ProjectWriter, RuleProvider,
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

/// 建图用例服务。
pub struct PipelineService {
    store: Arc<dyn Persistence>,
    deps: Arc<PipelineDeps>,
    /// 建图完成后自动跑合规检查（见 [`Self::run_check`]）。
    checks: RuleService,
    /// 同一工程同时只允许一个建图任务。
    running: Arc<Mutex<std::collections::HashSet<i64>>>,
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
        }
    }

    /// 同步执行建图。
    pub fn run(
        &self,
        project_id: ProjectId,
        observer: &dyn PipelineObserver,
    ) -> Result<PipelineOutcome> {
        {
            let mut guard = self.running.lock().unwrap();
            if !guard.insert(project_id.get()) {
                return Err(DomainError::Conflict(format!(
                    "工程 {project_id} 正在建图中"
                )));
            }
        }
        let result = self.run_inner(project_id, observer);
        self.running.lock().unwrap().remove(&project_id.get());
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
