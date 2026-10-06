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
    use gt_domain::model::{
        Language, NewSourceFile, NewSubProject, Phase, ProjectPatch, ProjectStatus,
    };
    use serde_json::Value;
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
            "an empty (whitespace-only) name must be rejected"
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
            "a non-existent root directory must be rejected"
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
        assert_eq!(all.len(), 1, "recreating with the same name must keep exactly one project (no accumulation)");
        // SQLite reuses the deleted rowid on the next insert, so `first.id == second.id` is expected; the rebuild
        // contract is proven by the single surviving row pointing at the *new* root path.
        assert_eq!(all[0].root_path, dir_b, "after the rebuild the project with the new root is the one kept");
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
            "an illegal root in update must be rejected"
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

    /// `sub_projects` delegates to `list_sub_projects`; seed two and confirm they all come back.
    #[test]
    fn sub_projects_delegates_to_store() {
        let store = store();
        let s = ProjectService::new(store.clone(), Arc::new(StubClock));
        let p = s
            .create(NewProject {
                name: "sp".into(),
                root_path: tmpdir("sp"),
                description: None,
                config: None,
            })
            .unwrap();
        store
            .replace_sub_projects(
                p.id,
                vec![
                    NewSubProject {
                        project_id: p.id,
                        name: "be".into(),
                        root_path: p.root_path.clone(),
                        language: Language::new("php"),
                        role: "backend".into(),
                        detected_by: "composer.json".into(),
                        frameworks: vec![],
                        facts: Value::Null,
                    },
                    NewSubProject {
                        project_id: p.id,
                        name: "fe".into(),
                        root_path: p.root_path.clone(),
                        language: Language::new("js"),
                        role: "frontend".into(),
                        detected_by: "package.json".into(),
                        frameworks: vec![],
                        facts: Value::Null,
                    },
                ],
            )
            .unwrap();
        let sps = s.sub_projects(p.id).unwrap();
        assert_eq!(sps.len(), 2, "two sub-projects must be returned");
        assert!(sps.iter().all(|x| x.project_id == p.id));
    }

    /// `files` delegates to `list_files`; seed two source files and confirm they all come back.
    #[test]
    fn files_delegates_to_store() {
        let store = store();
        let s = ProjectService::new(store.clone(), Arc::new(StubClock));
        let p = s
            .create(NewProject {
                name: "fl".into(),
                root_path: tmpdir("fl"),
                description: None,
                config: None,
            })
            .unwrap();
        store
            .replace_files(
                p.id,
                vec![
                    NewSourceFile {
                        project_id: p.id,
                        sub_project_id: None,
                        path: "a.php".into(),
                        language: Language::new("php"),
                        size_bytes: 10,
                        content_hash: "h1".into(),
                    },
                    NewSourceFile {
                        project_id: p.id,
                        sub_project_id: None,
                        path: "b.php".into(),
                        language: Language::new("php"),
                        size_bytes: 20,
                        content_hash: "h2".into(),
                    },
                ],
            )
            .unwrap();
        let fs = s.files(p.id, None).unwrap();
        assert_eq!(fs.len(), 2, "two source files must be returned");
        assert!(fs.iter().all(|x| x.project_id == p.id));
    }

    /// A valid patch (description change, no root override) must apply and return the updated project — the `update`
    /// path that skips the `root_path` validation guard.
    #[test]
    fn update_applies_valid_patch() {
        let s = svc();
        let p = s
            .create(NewProject {
                name: "u2".into(),
                root_path: tmpdir("u2"),
                description: Some("old".into()),
                config: None,
            })
            .unwrap();
        let updated = s
            .update(
                p.id,
                ProjectPatch {
                    description: Some("new".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(updated.description.as_deref(), Some("new"));
    }

    /// `delete` removes the project so a subsequent `get` is `NotFound`.
    #[test]
    fn delete_removes_project() {
        let s = svc();
        let p = s
            .create(NewProject {
                name: "d".into(),
                root_path: tmpdir("d"),
                description: None,
                config: None,
            })
            .unwrap();
        s.delete(p.id).unwrap();
        assert!(
            matches!(s.get(p.id), Err(DomainError::NotFound(_))),
            "after deletion get must be NotFound"
        );
    }

    /// `mark` delegates to `set_project_status`; the UI reads the status to drive the build-state badge.
    #[test]
    fn mark_sets_status() {
        let s = svc();
        let p = s
            .create(NewProject {
                name: "m".into(),
                root_path: tmpdir("m"),
                description: None,
                config: None,
            })
            .unwrap();
        s.mark(p.id, ProjectStatus::Ready).unwrap();
        assert_eq!(s.get(p.id).unwrap().status, ProjectStatus::Ready);
    }

    /// `now` is a straight delegation to the clock (frozen at 0 here).
    #[test]
    fn now_reads_clock() {
        let s = svc();
        assert_eq!(s.now(), 0, "the frozen clock must return 0");
    }

    /// The port-injection helpers must hand back trait objects that actually work against the store.
    #[test]
    fn reader_and_writer_of_resolve_traits() {
        let store = store();
        let s = ProjectService::new(store.clone(), Arc::new(StubClock));
        let p = s
            .create(NewProject {
                name: "rw".into(),
                root_path: tmpdir("rw"),
                description: None,
                config: None,
            })
            .unwrap();
        let listed = reader_of(&store).list_projects().unwrap();
        assert!(listed.iter().any(|x| x.id == p.id), "the reader port must be able to list projects");
        writer_of(&store)
            .set_project_status(p.id, ProjectStatus::Ready)
            .unwrap();
        assert_eq!(s.get(p.id).unwrap().status, ProjectStatus::Ready);
    }
}
