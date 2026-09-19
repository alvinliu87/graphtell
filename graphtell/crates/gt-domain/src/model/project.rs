//! 工程 / 子工程 / 源文件。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::ids::{FileId, ProjectId, SubProjectId};
use super::kinds::Language;

/// 被分析的顶层工程。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    /// 工程根目录的绝对路径。
    pub root_path: PathBuf,
    pub description: Option<String>,
    /// 附加配置（排除模式、需要的 locale 列表等），避免硬编码。
    pub config: ProjectConfig,
    pub status: ProjectStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 新建工程的入参（不含服务端生成的字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewProject {
    pub name: String,
    pub root_path: PathBuf,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// 更新工程的入参；`None` 表示"不修改该字段"。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectPatch {
    pub name: Option<String>,
    pub root_path: Option<PathBuf>,
    pub description: Option<String>,
    pub config: Option<ProjectConfig>,
}

/// 工程级配置。全部可覆盖，杜绝硬编码。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectConfig {
    /// 额外的目录排除 glob（在语言默认排除规则之上叠加）。
    pub exclude_globs: Vec<String>,
    /// i18n 覆盖检查要求的 locale 列表。
    pub required_locales: Vec<String>,
    /// 数据库表前缀（用于 identity 归一化，如 `eb_`）。
    ///
    /// 默认空：前缀应在工程配置里显式给出，或在 P3 由 FKB 从框架配置
    /// （如 ThinkPHP 的 `config/database.php`）自动探测。绝不内置任何
    /// 项目特定的默认值（CRMEB 的 `eb_` 不应泄漏到通用层）。
    pub table_prefixes: Vec<String>,
    /// 是否启用全阶段流水线（关闭则只跑 Ingest + CfAst）。
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

/// 工程状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// 已创建，尚未建图。
    Created,
    /// 建图中。
    Indexing,
    /// 建图完成。
    Ready,
    /// 建图失败。
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

/// 子工程：一个工程内的独立可分析单元。
///
/// 例如 `CRMEB-master` 下既有 ThinkPHP 后端，也有 Uni-app 前端，
/// 二者语言不同、FKB 不同，必须作为两个子工程分别分析，
/// 再在 `HttpContract` 这类契约节点上跨工程汇聚。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubProject {
    pub id: SubProjectId,
    pub project_id: ProjectId,
    pub name: String,
    pub root_path: PathBuf,
    pub language: Language,
    /// 子工程角色，形如 `tier` 或 `tier:kind`（如 `backend` / `frontend:admin` / `backend:worker`）。
    /// `tier` 为 `frontend` / `backend` / `library` / `unknown`；`kind` 进一步区分类型
    /// （小程序 / 管理后台 / 移动端 / API / Worker …），由 Ingest 按目录名识别。
    pub role: String,
    /// 识别依据，例如 "composer.json"。
    pub detected_by: String,
    /// 框架标识列表（由 FKB 匹配得出，如 `["thinkphp6", "uni-app"]`）。
    pub frameworks: Vec<String>,
    /// 框架根信息（AppRoot 等），由 Prepare 阶段按 FKB 解析后回填。
    pub facts: serde_json::Value,
}

/// 新建子工程。
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

/// 待分析的源文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceFile {
    pub id: FileId,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    /// 相对工程根的路径（统一用 `/` 分隔）。
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}

/// 新建源文件（批量插入用）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSourceFile {
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub path: String,
    pub language: Language,
    pub size_bytes: u64,
    pub content_hash: String,
}
