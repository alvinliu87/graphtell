//! 领域错误类型。
//!
//! 遵循 SOLID 中的「依赖倒置」：错误在内核定义，适配器只负责把自身错误
//! 转换成 [`DomainError::Infrastructure`]，绝不把 `rusqlite::Error` 之类的
//! 具体类型泄漏到上层。

use std::path::PathBuf;

/// 领域层统一结果类型。
pub type Result<T> = std::result::Result<T, DomainError>;

/// 领域错误。
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("资源不存在: {0}")]
    NotFound(String),

    #[error("参数非法: {0}")]
    InvalidArgument(String),

    #[error("配置/知识库损坏: {0}")]
    InvalidKnowledge(String),

    #[error("解析失败: {file}: {reason}")]
    Parse { file: PathBuf, reason: String },

    #[error("路径越界: {0}")]
    PathEscape(PathBuf),

    #[error("冲突: {0}")]
    Conflict(String),

    #[error("未支持的能力: {0}")]
    Unsupported(String),

    /// 适配器把具体技术错误装箱后透传，保留 `Display` 信息但不泄漏类型。
    #[error("基础设施错误: {0}")]
    Infrastructure(String),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl DomainError {
    /// 适配器专用：把任意 `Display` 错误包装为领域错误。
    pub fn infra<E: std::fmt::Display>(e: E) -> Self {
        DomainError::Infrastructure(e.to_string())
    }
}
