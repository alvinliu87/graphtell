//! `gt-application` —— 应用层：用例编排与事务边界。
//!
//! 只依赖 `gt-domain` 的端口 trait，不依赖任何具体技术。
//! 每个用例是一个独立的服务，遵循单一职责原则（SRP）。

pub mod graph_query;
pub mod location;
pub mod view_service;
pub mod pipeline_runner;
pub mod project_service;
pub mod rule_service;
pub mod recall_service;
pub mod embedding;
// 真实神经网络嵌入适配器：仅在 `model` feature 开启时编译，离线构建不受影响。
#[cfg(feature = "model")]
pub mod embed_model;

pub use graph_query::{DiagnosticSummary, GraphQueryService};
pub use view_service::ViewService;
pub use pipeline_runner::PipelineService;
pub use project_service::ProjectService;
pub use rule_service::{CheckSummary, RuleService};
pub use recall_service::{RecallHit, RecallQuery, RecallResult, RecallService, SeedInfo};
pub use embedding::{Embedder, LocalHashingEmbedder};
pub use gt_domain::model::{CheckReport, CheckRule, Violation};
