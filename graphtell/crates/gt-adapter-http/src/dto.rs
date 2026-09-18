//! HTTP 层的数据传输对象。
//!
//! DTO 与领域模型分离：领域模型可以自由演进，API 契约保持稳定。

use serde::{Deserialize, Serialize};

use gt_domain::model::{Project, ProjectConfig, ProjectStatus, SubProject};

/// 工程视图。
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

/// 创建工程请求。
#[derive(Debug, Clone, Deserialize)]
pub struct CreateProjectRequest {
    pub name: String,
    pub root_path: String,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// 更新工程请求。
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

/// 子工程视图。
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

/// 统一响应包装。
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

/// 建图状态。
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

/// 健康检查。
#[derive(Debug, Clone, Serialize)]
pub struct HealthDto {
    pub status: String,
    pub version: String,
    pub languages: Vec<String>,
    pub frameworks: usize,
    /// 后端进程是否运行在 WSL 中（前端据此自动套用 WSL 路径映射）。
    pub is_wsl: bool,
    /// WSL 发行版名（仅 `is_wsl` 为 true 时有意义，默认 Ubuntu）。
    pub wsl_distro: String,
}

/// 建图触发响应。
#[derive(Debug, Clone, Serialize)]
pub struct RunAcceptedDto {
    pub project_id: i64,
    pub accepted: bool,
}

/// 目录浏览条目（供前端目录选择器使用）。
#[derive(Debug, Clone, Serialize)]
pub struct DirEntryDto {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// 状态字面量。
pub fn status_of(s: &ProjectStatus) -> &'static str {
    match s {
        ProjectStatus::Created => "created",
        ProjectStatus::Indexing => "indexing",
        ProjectStatus::Ready => "ready",
        ProjectStatus::Failed => "failed",
    }
}
