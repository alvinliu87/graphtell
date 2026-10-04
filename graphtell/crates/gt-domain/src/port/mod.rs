//! Ports: the kernel's **abstract requirements** for capabilities from the outside world.
//!
//! In a hexagonal architecture ports are defined by the kernel and implemented by adapters (dependency inversion).
//! The kernel is decoupled from every concrete technology (SQLite / tree-sitter / filesystem / YAML).

pub mod filesystem;
pub mod knowledge;
pub mod observability;
pub mod parsing;
pub mod persistence;
pub mod resource;
pub mod rules;
pub mod techstack;

pub use filesystem::{FileScanner, FileSystem, ScanRequest, ScannedFile};
pub use knowledge::KnowledgeProvider;
pub use resource::{
    DefaultResourceAdapterRegistry, PseudoCall, ResourceAdapter, ResourceAdapterRegistry,
    ResourceFact,
};
pub use rules::RuleProvider;
pub use techstack::{
    AdapterFact, DefaultTechStackRegistry, ManifestEntries, ManifestEntry, Marker, MarkerProvider,
    TechStackAdapter, TechStackRegistry,
};
pub use observability::{Clock, NoopObserver, PipelineObserver, SystemClock};
pub use parsing::{LanguageParser, ParserRegistry};
pub use views::ViewRegistryProvider;
pub use persistence::{
    DiagnosticSink, EdgeDirection, GraphDelta, GraphQuery, GraphSink, GraphStats, NodeFilter,
    Persistence, ProjectReader, ProjectWriter, RuleConfigStore, SymbolTableReader,
};
pub mod views;
