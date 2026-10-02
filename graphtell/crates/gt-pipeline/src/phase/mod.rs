//! Implementations of the pipeline phases.

pub mod annotate;
pub mod cf_ast;
pub mod columns;
pub mod cors;
pub mod ingest;
pub mod prepare;
pub mod propagate;
pub mod external;
pub mod guard;
pub mod resolve;
pub mod sign;
pub mod taint;
pub mod tx;
