//! `gt-adapter-parser` — the source-parsing adapter (implementation of the outbound port `ParserRegistry`).
//!
//! This crate is the **only** place that touches tree-sitter. It translates each language's concrete
//! syntax tree into the language-agnostic [`SyntaxFacts`] defined by the domain, making the pipeline
//! language-independent.

pub mod java;
pub mod js;
/// Shared tree-sitter helpers (`text` / `opt_text` / `span_of`) used by every language adapter.
pub mod ts_util;
pub mod json;
pub mod php;
pub mod python;
pub mod rust;
pub mod registry;

pub use registry::{require_parser, DefaultParserRegistry};
