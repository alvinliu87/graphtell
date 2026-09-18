//! `gt-pipeline` —— 建图流水线的阶段实现（领域服务层）。
//!
//! 依赖倒置：本 crate 只依赖 `gt-domain` 的端口 trait，
//! 不直接接触 SQLite / tree-sitter / 文件系统。
//!
//! 阶段顺序（严格依赖，顺序不可颠倒）：
//! ```text
//! P0 Ingest → P2 CfAst → P3 Prepare → P4 AnnotatePre
//!          → P5 Synthesize → P6 AnnotatePost → P7 Resolve
//! ```
//! P5 必须在 P3 之后（要查权威符号表），P6 必须在 P5 之后（要查汇聚结果），
//! P7 必须在 P6 之后（要查别名索引）。

pub mod context;
pub mod engine;
pub mod eval;
pub mod mybatis;
pub mod normalize;
pub mod phase;
pub mod runner;
pub mod workspace;

pub use context::PipelineContext;
pub use runner::{run, PipelineInfrastructure, PipelineOutcome};
pub use workspace::GraphWorkspace;
