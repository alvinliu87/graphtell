//! `em-application` —— 应用层：用例编排与事务边界。
//!
//! 只依赖 `em-domain` 的端口 trait，不依赖任何具体技术。
//! 每个用例是一个独立的服务，遵循单一职责原则（SRP）。

pub mod graph_query;
pub mod view_service;
pub mod pipeline_runner;
pub mod project_service;

pub use graph_query::GraphQueryService;
pub use view_service::ViewService;
pub use pipeline_runner::PipelineService;
pub use project_service::ProjectService;
