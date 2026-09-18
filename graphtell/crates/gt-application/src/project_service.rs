//! 工程 CRUD 用例。

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    NewProject, PhaseReport, Project, ProjectId, ProjectPatch, ProjectStatus, SourceFile,
    SubProject, SubProjectId,
};
use gt_domain::port::{Clock, Persistence, PipelineObserver, ProjectReader, ProjectWriter};
use std::sync::Arc;
use tracing::info;

/// 工程用例服务。
pub struct ProjectService {
    store: Arc<dyn Persistence>,
    clock: Arc<dyn Clock>,
}

impl ProjectService {
    pub fn new(store: Arc<dyn Persistence>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    pub fn create(&self, new: NewProject) -> Result<Project> {
        if new.name.trim().is_empty() {
            return Err(DomainError::InvalidArgument("工程名不能为空".into()));
        }
        if !new.root_path.exists() || !new.root_path.is_dir() {
            return Err(DomainError::InvalidArgument(format!(
                "工程根目录无效: {}",
                new.root_path.display()
            )));
        }
        // 同名工程视为重建：先删除旧工程（含其图数据），再创建，避免重复累积。
        if let Some(existing) = self
            .store
            .list_projects()?
            .into_iter()
            .find(|p| p.name == new.name)
        {
            self.store.delete_project(existing.id)?;
        }
        let project = self.store.create_project(new)?;
        info!("创建工程 {} ({})", project.name, project.id);
        Ok(project)
    }

    pub fn get(&self, id: ProjectId) -> Result<Project> {
        self.store
            .get_project(id)?
            .ok_or_else(|| DomainError::NotFound(format!("project {id}")))
    }

    pub fn list(&self) -> Result<Vec<Project>> {
        self.store.list_projects()
    }

    /// 某工程下的子工程。
    pub fn sub_projects(&self, id: ProjectId) -> Result<Vec<SubProject>> {
        self.store.list_sub_projects(id)
    }

    /// 某工程下的已摄取源文件（可返回数量以控制响应体大小）。
    pub fn files(&self, id: ProjectId, sub: Option<SubProjectId>) -> Result<Vec<SourceFile>> {
        self.store.list_files(id, sub)
    }

    pub fn update(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project> {
        if let Some(path) = &patch.root_path {
            if !path.exists() {
                return Err(DomainError::InvalidArgument(format!(
                    "路径不存在: {}",
                    path.display()
                )));
            }
        }
        self.store.update_project(id, patch)
    }

    pub fn delete(&self, id: ProjectId) -> Result<()> {
        self.store.delete_project(id)
    }

    /// 同步触发建图（由流水线服务调用；这里只负责状态流转）。
    pub fn mark(&self, id: ProjectId, status: ProjectStatus) -> Result<()> {
        self.store.set_project_status(id, status)
    }

    pub fn now(&self) -> i64 {
        self.clock.now_millis()
    }
}

/// 建图进度的内存记录（供 UI 轮询）。
#[derive(Debug, Clone, Default)]
pub struct RunProgress {
    pub project_id: ProjectId,
    pub current_phase: Option<String>,
    pub reports: Vec<PhaseReport>,
    pub finished: bool,
}

impl RunProgress {
    pub fn new(project_id: ProjectId) -> Self {
        Self { project_id, ..Default::default() }
    }
}

/// 把进度广播给 UI 的观察者实现。
pub struct ProgressObserver {
    progress: std::sync::Mutex<RunProgress>,
}

impl ProgressObserver {
    pub fn new(project_id: ProjectId) -> Self {
        Self { progress: std::sync::Mutex::new(RunProgress::new(project_id)) }
    }
    pub fn snapshot(&self) -> RunProgress {
        self.progress.lock().unwrap().clone()
    }
}

impl PipelineObserver for ProgressObserver {
    fn on_phase_start(&self, project_id: ProjectId, phase: &gt_domain::model::Phase) {
        let mut p = self.progress.lock().unwrap();
        p.project_id = project_id;
        p.current_phase = Some(phase.to_string());
    }

    fn on_phase_end(&self, _project_id: ProjectId, report: &PhaseReport) {
        let mut p = self.progress.lock().unwrap();
        p.reports.push(report.clone());
    }
}

/// 供 Reader 端口单独注入时使用。
pub fn reader_of(store: &Arc<dyn Persistence>) -> &dyn ProjectReader {
    store.as_ref()
}

/// 供 Writer 端口单独注入时使用。
pub fn writer_of(store: &Arc<dyn Persistence>) -> &dyn ProjectWriter {
    store.as_ref()
}
