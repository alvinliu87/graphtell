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
pub mod embed_remote;
/// 源码变更监听（轮询 + 防抖 → 整库安全重建 + 自动合规）。
pub mod watch;
// 真实神经网络嵌入适配器：仅在特定 `model-*` feature 开启时编译，离线构建不受影响。
#[cfg(feature = "model-candle")]
pub mod embed_model;
#[cfg(feature = "model-ort")]
pub mod embed_ort;

pub use graph_query::{DiagnosticSummary, GraphQueryService};
pub use view_service::ViewService;
pub use pipeline_runner::PipelineService;
pub use project_service::ProjectService;
pub use rule_service::{CheckSummary, RuleService};
pub use recall_service::{RecallHit, RecallQuality, RecallQuery, RecallResult, RecallService, SeedInfo};
pub use embedding::{
    default_embedder, Embedder, LocalHashingEmbedder, resolve_recall_embedder,
    try_real_recall_embedder, embedding_backend_info, embedding_dim,
};
pub use gt_domain::model::{CheckReport, CheckRule, Violation};
