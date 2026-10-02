//! `gt-pipeline` — phase implementations of the graph-building pipeline (the domain service layer).
//!
//! Dependency inversion: this crate depends only on the port traits of `gt-domain` and never touches SQLite /
//! tree-sitter / the filesystem directly.
//!
//! Phase order (strict dependency, the order cannot be rearranged):
//! ```text
//! P0 Ingest -> P2 CfAst -> P3 Prepare -> P4 AnnotatePre
//!          -> P5 Synthesize -> P6 AnnotatePost -> P7 Resolve
//! ```
//! P5 must follow P3 (it queries the authoritative symbol tables), P6 must follow P5 (it queries the aggregated
//! result), and P7 must follow P6 (it queries the alias index).

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
