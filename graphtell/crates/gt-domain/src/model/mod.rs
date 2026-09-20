//! 领域模型。

pub mod fkb;
pub mod graph;
pub mod ids;
pub mod kinds;
pub mod project;
pub mod syntax;
pub mod view;

pub use fkb::{
    Action, AliasSpec, AnnotateAction, AnnotateTarget, AnnotationSpec, CallContext, Detector,
    Direction, ExpandSpec, ExpandVariant, FanInThresholds, FieldSpec, FileFormat,
    FrameworkKnowledge, HandlerSpec, MagicDelegationSpec,
    IdentitySpec, KnowledgeScope, LinkAction, LinkSpec, LoaderSource, LoaderSpec, NormalizeStep,
    PickStrategy, Predicate,
    ResolveAs, ResolveStrategy, ResolveTier, Resolution, ResolverSpec, RootRule, RootSource, Rule,
    Selector, SubkindSource, SynthesizeAction, TransformSpec, ValueSource,
};
pub use graph::{
    AliasEntry, Annotation, Diagnostic, Edge, IdentityKey, MergeStrategy, NewAnnotation, NewEdge,
    NewNode, Node, PhaseReport, Severity, Span, SymbolEntry,
};
pub use ids::{EdgeId, FileId, NodeId, ProjectId, SubProjectId};
pub use kinds::{AnnotationChannel, EdgeKind, Language, NodeKind, Phase, SynthesizedKind};
pub use project::{
    NewProject, NewSourceFile, NewSubProject, Project, ProjectConfig, ProjectPatch, ProjectStatus,
    SourceFile, SubProject,
};
pub use crate::port::persistence::GraphDelta;

pub use view::{
    AggregateView, Candidate, Cluster, EdgeEvidence, EdgeView, GroupBy, HiddenInfo, LayoutMode,
    MatrixView, NodeLocationEntry, NodeLocations, NodeView, ObjectView, PerspectiveSpec,
    SourceLocation, UnresolvedInfo, ViaNode, ViewMode, ViewRegistry,
};

pub use syntax::{
    CallSiteFact, ConfigEntryFact, Declaration, FactValue, FieldTypeFact, ImportFact,
    InheritanceFact, NamespacePolicy, SyntaxFacts,
};
