//! `gt-application` — the application layer: use-case orchestration and transaction boundaries.
//!
//! It depends only on the port traits of `gt-domain`, never on any concrete technology.
//! Each use case is an independent service, following the single-responsibility principle (SRP).

pub mod graph_query;
pub mod location;
pub mod view_service;
pub mod pipeline_runner;
pub mod project_service;
pub mod rule_service;
pub mod recall_service;
pub mod embedding;
pub mod embed_remote;
/// Source-change watching (polling + debounce -> safe whole-database rebuild + automatic compliance).
pub mod watch;
// Real neural-network embedding adapter: compiled only when a specific `model-*` feature is on; offline builds are unaffected.
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
