//! `em-domain` —— EntropyMate 的领域内核（六边形架构最内层）。
//!
//! 本 crate 只描述"是什么"与"要什么能力"（端口 trait），
//! **不包含任何 IO 实现**：不碰文件系统、不碰数据库、不碰 tree-sitter。
//! 所有外部能力都通过 [`port`] 中定义的 trait 反向注入。
//!
//! 分层约定：
//! * [`model`] —— 实体与值对象（语言无关、框架无关）
//! * [`port`] —— 出站/入站端口
//! * [`error`] —— 领域错误

pub mod error;
pub mod model;
pub mod port;

pub use error::{DomainError, Result};
