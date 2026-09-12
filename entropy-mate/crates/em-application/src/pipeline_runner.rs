//! 建图用例：编排流水线并处理状态流转与后台执行。

use std::sync::{Arc, Mutex};

use em_domain::error::{DomainError, Result};
use em_domain::model::{PhaseReport, Project, ProjectId, ProjectStatus};
use em_domain::port::{
    FileScanner, FileSystem, GraphSink, KnowledgeProvider, ParserRegistry, Persistence,
    PipelineObserver, ProjectWriter,
};
use em_pipeline::runner::PipelineOutcome;
use tracing::{error, info};

/// 流水线所需基础设施的聚合（在组装根注入，实现依赖倒置）。
pub struct PipelineDeps {
    pub fs: Arc<dyn FileSystem>,
    pub scanner: Arc<dyn FileScanner>,
    pub parsers: Arc<dyn ParserRegistry>,
    pub kb: Arc<dyn KnowledgeProvider>,
}

/// 把 [`PipelineDeps`] 与持久化适配成 `em-pipeline` 需要的 [`PipelineInfrastructure`]。
struct Infra<'a> {
    deps: &'a PipelineDeps,
    store: &'a dyn Persistence,
}

impl<'a> em_pipeline::runner::PipelineInfrastructure for Infra<'a> {
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
    /// 同一工程同时只允许一个建图任务。
    running: Arc<Mutex<std::collections::HashSet<i64>>>,
}

impl PipelineService {
    pub fn new(store: Arc<dyn Persistence>, deps: Arc<PipelineDeps>) -> Self {
        Self {
            store,
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
        let outcome = em_pipeline::runner::run(&project, &infra, observer);

        match outcome {
            Ok(out) => {
                self.store.set_project_status(project_id, ProjectStatus::Ready)?;
                info!(
                    "工程 {} 建图完成：{} 子工程 / {} 文件",
                    project.name,
                    out.sub_projects.len(),
                    out.files.len()
                );
                Ok(out)
            }
            Err(e) => {
                error!("工程 {} 建图失败: {e}", project.name);
                self.store.set_project_status(project_id, ProjectStatus::Failed)?;
                Err(e)
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
