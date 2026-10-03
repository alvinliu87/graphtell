//! Data transfer objects for the HTTP layer.
//!
//! DTOs are separate from the domain model: the domain model can evolve freely while the API contract stays stable.

use serde::{Deserialize, Serialize};

use gt_domain::model::{Project, ProjectConfig, ProjectStatus, SubProject};

/// Project view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDto {
    pub id: i64,
    pub name: String,
    pub root_path: String,
    pub description: Option<String>,
    pub status: String,
    pub config: ProjectConfig,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<Project> for ProjectDto {
    fn from(p: Project) -> Self {
        Self {
            id: p.id.get(),
            name: p.name,
            root_path: p.root_path.to_string_lossy().to_string(),
            description: p.description,
            status: p.status.to_string(),
            config: p.config,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

/// Create-project request.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub root_path: String,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// Update-project request.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateProjectRequest {
    pub name: Option<String>,
    pub root_path: Option<String>,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

impl UpdateProjectRequest {
    pub fn into_patch(self) -> gt_domain::model::ProjectPatch {
        gt_domain::model::ProjectPatch {
            name: self.name,
            root_path: self.root_path.map(std::path::PathBuf::from),
            description: self.description,
            config: self.config,
        }
    }
}

/// Sub-project view.
#[derive(Debug, Clone, Serialize)]
pub struct SubProjectDto {
    pub id: i64,
    pub name: String,
    pub root_path: String,
    pub language: String,
    pub role: String,
    pub detected_by: String,
    pub frameworks: Vec<String>,
    pub facts: serde_json::Value,
}

impl From<SubProject> for SubProjectDto {
    fn from(s: SubProject) -> Self {
        Self {
            id: s.id.get(),
            name: s.name,
            root_path: s.root_path.to_string_lossy().to_string(),
            language: s.language.to_string(),
            role: s.role,
            detected_by: s.detected_by,
            frameworks: s.frameworks,
            facts: s.facts,
        }
    }
}

/// Unified response envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ApiResponse<T: Serialize> {
    pub ok: bool,
    pub data: Option<T>,
    pub error: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn success(data: T) -> Self {
        Self { ok: true, data: Some(data), error: None }
    }
    pub fn failure(msg: impl Into<String>) -> Self {
        Self { ok: false, data: None, error: Some(msg.into()) }
    }
}

/// Graph-build status.
#[derive(Debug, Clone, Serialize, Default)]
pub struct RunStatusDto {
    pub project_id: i64,
    pub current_phase: Option<String>,
    pub phases: Vec<PhaseReportDto>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhaseReportDto {
    pub phase: String,
    pub nodes_created: u64,
    pub edges_created: u64,
    pub annotations_created: u64,
    pub aliases_created: u64,
    pub duration_ms: u64,
    pub diagnostics: usize,
}

/// Health check.
#[derive(Debug, Clone, Serialize)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
    pub languages: Vec<String>,
    pub frameworks: usize,
    /// Whether the backend process runs inside WSL (the front end applies WSL path mapping automatically from this).
    pub is_wsl: bool,
    /// WSL distro name (meaningful only when `is_wsl` is true; defaults to Ubuntu).
    pub wsl_distro: String,
}

/// Graph-build trigger response.
#[derive(Debug, Clone, Serialize)]
pub struct RunAcceptedDto {
    pub project_id: i64,
    pub accepted: bool,
}

/// Directory browsing entry (for the front-end directory picker).
#[derive(Debug, Clone, Serialize)]
pub struct DirEntryDto {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Status literal.
pub fn status_of(s: &ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Created => "created",
        ProjectStatus::Indexing => "indexing",
        ProjectStatus::Ready => "ready",
        ProjectStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use gt_domain::model::{Language, ProjectConfig, ProjectStatus, SubProject, Project, ProjectId, SubProjectId};

    fn sample_project() -> Project {
        Project {
            id: ProjectId(7),
            name: "demo".into(),
            root_path: PathBuf::from("/tmp/demo"),
            description: Some("a demo".into()),
            config: ProjectConfig::default(),
            status: ProjectStatus::Ready,
            created_at: 100,
            updated_at: 200,
        }
    }

    fn sample_sub_project() -> SubProject {
        SubProject {
            id: SubProjectId(3),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: PathBuf::from("/tmp/demo/app"),
            language: Language::new("php"),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Null,
        }
    }

    /// The `{ ok, data, error }` envelope is the contract the front end unpacks (`body.data`); the success / failure
    /// shape must stay stable.
    #[test]
    fn api_response_envelope_shape() {
        let ok: ApiResponse<i32> = ApiResponse::success(42);
        assert!(ok.ok);
        assert_eq!(ok.data, Some(42));
        assert!(ok.error.is_none());

        let fail: ApiResponse<i32> = ApiResponse::failure("boom");
        assert!(!fail.ok);
        assert!(fail.data.is_none());
        assert_eq!(fail.error.as_deref(), Some("boom"));
    }

    #[test]
    fn project_dto_maps_fields() {
        let dto: ProjectDto = sample_project().into();
        assert_eq!(dto.id, 7);
        assert_eq!(dto.name, "demo");
        assert_eq!(dto.root_path, "/tmp/demo");
        assert_eq!(dto.status, "ready"); // Display of ProjectStatus, not status_of
        assert_eq!(dto.created_at, 100);
        assert_eq!(dto.updated_at, 200);
    }

    #[test]
    fn sub_project_dto_maps_fields() {
        let dto: SubProjectDto = sample_sub_project().into();
        assert_eq!(dto.id, 3);
        assert_eq!(dto.name, "app");
        assert_eq!(dto.language, "php");
        assert_eq!(dto.role, "backend");
        assert_eq!(dto.detected_by, "composer.json");
        assert_eq!(dto.frameworks, vec!["thinkphp".to_string()]);
    }

    #[test]
    fn status_of_maps_every_variant() {
        assert_eq!(status_of(&ProjectStatus::Created), "created");
        assert_eq!(status_of(&ProjectStatus::Indexing), "indexing");
        assert_eq!(status_of(&ProjectStatus::Ready), "ready");
        assert_eq!(status_of(&ProjectStatus::Failed), "failed");
    }

    /// `UpdateProjectRequest::into_patch` must forward only the provided fields (`None` => "do not change").
    #[test]
    fn update_request_into_patch_forwards_optionals() {
        let req = UpdateProjectRequest {
            name: Some("new".into()),
            root_path: Some("/x".into()),
            description: None,
            config: None,
        };
        let patch = req.into_patch();
        assert_eq!(patch.name.as_deref(), Some("new"));
        assert_eq!(patch.root_path, Some(PathBuf::from("/x")));
        assert!(patch.description.is_none());
        assert!(patch.config.is_none());
    }
}
