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

#[cfg(test)]
mod tests {
    use super::*;
    use gt_adapter_sqlite::SqliteStore;
    use gt_domain::model::Phase;
    use std::path::PathBuf;

    /// `create`/`update` never read the clock, so a frozen clock is enough to build a service.
    struct StubClock;
    impl Clock for StubClock {
        fn now_millis(&self) -> i64 {
            0
        }
    }

    fn store() -> Arc<dyn Persistence> {
        Arc::new(SqliteStore::in_memory().expect("in-memory store must construct"))
    }
    fn svc() -> ProjectService {
        ProjectService::new(store(), Arc::new(StubClock))
    }
    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gt_proj_it_{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn create_rejects_empty_name() {
        let s = svc();
        let r = s.create(NewProject {
            name: "   ".into(),
            root_path: tmpdir("empty"),
            description: None,
            config: None,
        });
        assert!(
            matches!(r, Err(DomainError::InvalidArgument(_))),
            "空名（仅空白）必须被拒"
        );
    }

    #[test]
    fn create_rejects_missing_root_dir() {
        let s = svc();
        let r = s.create(NewProject {
            name: "x".into(),
            root_path: PathBuf::from("/no/such/dir/here"),
            description: None,
            config: None,
        });
        assert!(
            matches!(r, Err(DomainError::InvalidArgument(_))),
            "不存在的 root 目录必须被拒"
        );
    }

    #[test]
    fn create_succeeds_and_get_finds_it() {
        let s = svc();
        let p = s
            .create(NewProject {
                name: "x".into(),
                root_path: tmpdir("ok"),
                description: None,
                config: None,
            })
            .expect("create should succeed");
        assert_eq!(s.get(p.id).unwrap().id, p.id);
    }

    /// Re-creating a project with the same name must delete the old one (graph data included) and keep the new one —
    /// this is the "rebuild" contract; without it, graph data would accumulate across rebuilds.
    #[test]
    fn create_rebuilds_existing_project_with_same_name() {
        let s = svc();
        let dir_a = tmpdir("rebuild_a");
        let dir_b = tmpdir("rebuild_b");
        let _first = s
            .create(NewProject {
                name: "x".into(),
                root_path: dir_a.clone(),
                description: None,
                config: None,
            })
            .unwrap();
        let _second = s
            .create(NewProject {
                name: "x".into(),
                root_path: dir_b.clone(),
                description: None,
                config: None,
            })
            .unwrap();

        let all = s.list().unwrap();
        assert_eq!(all.len(), 1, "同名重建必须只保留一个工程（不累积）");
        // SQLite reuses the deleted rowid on the next insert, so `first.id == second.id` is expected; the rebuild
        // contract is proven by the single surviving row pointing at the *new* root path.
        assert_eq!(all[0].root_path, dir_b, "重建后保留的是新 root 的工程");
    }

    #[test]
    fn get_returns_not_found_for_missing_id() {
        let s = svc();
        assert!(matches!(
            s.get(ProjectId::new(99999)),
            Err(DomainError::NotFound(_))
        ));
    }

    #[test]
    fn update_rejects_missing_root_path() {
        let s = svc();
        let p = s
            .create(NewProject {
                name: "u".into(),
                root_path: tmpdir("u"),
                description: None,
                config: None,
            })
            .unwrap();
        let r = s.update(
            p.id,
            ProjectPatch {
                root_path: Some(PathBuf::from("/no/such/dir/here")),
                ..Default::default()
            },
        );
        assert!(
            matches!(r, Err(DomainError::InvalidArgument(_))),
            "update 的非法 root 必须被拒"
        );
    }

    /// `ProgressObserver` must capture the current phase on start and accumulate phase reports on end (the UI polls
    /// this snapshot), without touching the store.
    #[test]
    fn progress_observer_records_phase_and_report() {
        let pid = ProjectId::new(7);
        let obs = ProgressObserver::new(pid);
        obs.on_phase_start(pid, &Phase::from("Ingest"));
        obs.on_phase_end(
            pid,
            &PhaseReport {
                phase: "Ingest".into(),
                ..Default::default()
            },
        );
        let snap = obs.snapshot();
        assert_eq!(snap.project_id, pid);
        assert_eq!(snap.current_phase.as_deref(), Some("Ingest"));
        assert_eq!(snap.reports.len(), 1);
        assert_eq!(snap.reports[0].phase, "Ingest");
        assert!(!snap.finished);
    }
}
