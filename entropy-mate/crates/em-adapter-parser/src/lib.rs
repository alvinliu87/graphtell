//! `em-adapter-parser` —— 源码解析适配器（出站端口 `ParserRegistry` 的实现）。
//!
//! 本 crate 是**唯一**接触 tree-sitter 的地方。它把各语言的具体语法树
//! 翻译成领域定义的、语言无关的 [`SyntaxFacts`]，从而使流水线与语言无关。

pub mod java;
pub mod php;
pub mod registry;

pub use registry::{require_parser, DefaultParserRegistry};
