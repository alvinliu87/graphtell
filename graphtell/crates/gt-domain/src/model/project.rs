//! Projects / sub-projects / source files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ids::{FileId, ProjectId, SubProjectId};
use super::kinds::Language;

/// A top-level project under analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// The absolute path of the project root.
    pub root_path: PathBuf,
    pub description: Option<String>,
    /// Extra configuration (exclude patterns, the list of required locales, etc.) so nothing has to be hard-coded.
    pub config: ProjectConfig,
    pub status: ProjectStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Input for creating a project (excluding server-generated fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProject {
    pub name: String,
    pub root_path: PathBuf,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// Input for updating a project; `None` means "do not change this field".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectPatch {
    pub name: Option<String>,
    pub root_path: Option<PathBuf>,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// Project-level configuration. Everything is overridable; nothing is hard-coded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// Extra directory-exclude globs (layered on top of the language defaults).
    pub exclude_globs: Vec<String>,
    /// The list of locales required by the i18n coverage check.
    pub required_locales: Vec<String>,
    /// Database table prefix (used for identity normalisation, e.g. `eb_`).
    ///
    /// Empty by default: the prefix should be given explicitly in the project config, or detected automatically by
    /// P3 from framework config (such as ThinkPHP's `config/database.php`). Never bake in any project-specific
    /// default (CRMEB's `eb_` must not leak into the generic layer).
    pub table_prefixes: Vec<String>,
    /// Whether to run the full pipeline (when off, only Ingest + CfAst run).
    pub full_pipeline: bool,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            exclude_globs: Vec::new(),
            required_locales: vec!["zh-cn".into(), "en-us".into()],
            table_prefixes: Vec::new(),
            full_pipeline: true,
        }
    }
}

/// Project status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// Created, not graphed yet.
    Created,
    /// Graphing in progress.
    Indexing,
    /// Graphing finished.
    Ready,
    /// Graphing failed.
    Failed,
}

impl std::fmt::Display for ProjectStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Created => "created",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Failed => "failed",
        };
        f.write_str(s)
    }
}

/// A sub-project: an independently analysable unit inside a project.
///
/// For example, `CRMEB-master` contains both a ThinkPHP backend and a Uni-app frontend; they differ in language and
/// in FKB, so they must be analysed as two sub-projects and only converge across projects on contract nodes such as
/// `HttpContract`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubProject {
    pub id: SubProjectId,
    pub project_id: ProjectId,
    pub name: String,
    pub root_path: PathBuf,
    pub language: Language,
    /// Sub-project role, of the form `tier` or `tier:kind` (e.g. `backend` / `frontend:admin` / `backend:worker`).
    /// `tier` is one of `frontend` / `backend` / `library` / `unknown`; `kind` further distinguishes the type
    /// (mini program / admin console / mobile / API / Worker …), recognised by Ingest from the directory name.
    pub role: String,
    /// The recognition evidence, e.g. "composer.json".
    pub detected_by: String,
    /// Identifier list of the frameworks (matched by FKB, e.g. `["thinkphp6", "uni-app"]`).
    pub frameworks: Vec<String>,
    /// Framework root information (AppRoot, etc.), back-filled by the Prepare phase after resolution via FKB.
    pub facts: serde_json::Value,
}

/// A new sub-project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSubProject {
    pub project_id: ProjectId,
    pub name: String,
    pub root_path: PathBuf,
    pub language: Language,
    pub role: String,
    pub detected_by: String,
    pub frameworks: Vec<String>,
    pub facts: serde_json::Value,
}

/// A source file to analyse.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: FileId,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    /// Path relative to the project root (always `/`-separated).
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// A new source file (for batch insertion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSourceFile {
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}
