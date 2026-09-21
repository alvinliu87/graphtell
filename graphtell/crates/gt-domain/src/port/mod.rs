//! 端口（Port）：内核对外界能力的**抽象需求**。
//!
//! 六边形架构中，端口由内核定义、由适配器实现（依赖倒置）。
//! 内核与所有具体技术（SQLite / tree-sitter / 文件系统 / YAML）解耦。

pub mod filesystem;
pub mod knowledge;
pub mod observability;
pub mod parsing;
pub mod persistence;
pub mod rules;

pub use filesystem::{FileScanner, FileSystem, ScanRequest, ScannedFile};
pub use knowledge::KnowledgeProvider;
pub use rules::RuleProvider;
pub use observability::{Clock, NoopObserver, PipelineObserver, SystemClock};
pub use parsing::{LanguageParser, ParserRegistry};
pub use views::ViewRegistryProvider;
pub use persistence::{
    DiagnosticSink, EdgeDirection, GraphDelta, GraphQuery, GraphSink, GraphStats, NodeFilter,
    Persistence, ProjectReader, ProjectWriter, RuleConfigStore, SymbolTableReader,
};
pub mod views;
