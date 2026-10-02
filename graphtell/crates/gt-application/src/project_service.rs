//! Project CRUD use cases.

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    NewProject, PhaseReport, Project, ProjectId, ProjectPatch, ProjectStatus, SourceFile,
    SubProject, SubProjectId,
};
use gt_domain::port::{Clock, Persistence, PipelineObserver, ProjectReader, ProjectWriter};
use std::sync::Arc;
use tracing::info;

/// Project use-case service.
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
            return Err(DomainError::InvalidArgument("project name must not be empty".into()));
        }
        if !new.root_path.exists() || !new.root_path.is_dir() {
            return Err(DomainError::InvalidArgument(format!(
                "invalid project root directory: {}",
                new.root_path.display()
            )));
        }
        // A project with the same name counts as a rebuild: delete the old project (with its graph data) first, then create it, so nothing accumulates twice.
        if let Some(existing) = self
            .store
            .list_projects()?
            .into_iter()
            .find(|p| p.name == new.name)
        {
            self.store.delete_project(existing.id)?;
        }
        let project = self.store.create_project(new)?;
        info!("created project {} ({})", project.name, project.id);
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

    /// The sub-projects of a project.
    pub fn sub_projects(&self, id: ProjectId) -> Result<Vec<SubProject>> {
        self.store.list_sub_projects(id)
    }

    /// The ingested source files of a project (a count may be returned instead, to control response size).
    pub fn files(&self, id: ProjectId, sub: Option<SubProjectId>) -> Result<Vec<SourceFile>> {
        self.store.list_files(id, sub)
    }

    pub fn update(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project> {
        if let Some(path) = &patch.root_path {
            if !path.exists() {
                return Err(DomainError::InvalidArgument(format!(
                    "path does not exist: {}",
                    path.display()
                )));
            }
        }
        self.store.update_project(id, patch)
    }

    pub fn delete(&self, id: ProjectId) -> Result<()> {
        self.store.delete_project(id)
    }

    /// Trigger a graph build synchronously (called by the pipeline service; this only handles the state transition).
    pub fn mark(&self, id: ProjectId, status: ProjectStatus) -> Result<()> {
        self.store.set_project_status(id, status)
    }

    pub fn now(&self) -> i64 {
        self.clock.now_millis()
    }
}

/// An in-memory record of graph-build progress (for the UI to poll).
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

/// Observer implementation that broadcasts progress to the UI.
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

/// For injecting the Reader port on its own.
pub fn reader_of(store: &Arc<dyn Persistence>) -> &dyn ProjectReader {
    store.as_ref()
}

/// For injecting the Writer port on its own.
pub fn writer_of(store: &Arc<dyn Persistence>) -> &dyn ProjectWriter {
    store.as_ref()
}
