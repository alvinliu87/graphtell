//! Domain model.

pub mod fkb;
pub mod graph;
pub mod ids;
pub mod kinds;
pub mod project;
pub mod rules;
pub mod syntax;
pub mod view;

pub use fkb::{
    Action, AliasSpec, AnnotateAction, AnnotateTarget, AnnotationSpec, CallContext, ChainGuardSpec,
    ConsumerGuardSpec, ConsumerScope, DecoratorGuardSpec, Detector, GuardAttach, Direction, ExpandSpec, ExpandVariant, FanInThresholds, FieldSpec,
    FileFormat, FrameworkKnowledge, GuardAttachSpec, MethodRefSpec, MagicDelegationSpec,
    DbVerbsSpec, MiddlewareCapability, IdentitySpec, KnowledgeScope, LinkAction, LinkSpec,
    LoaderSource, LoaderSpec, NormalizeStep, PickStrategy, Predicate, ProjectAction,
    ResolveAs, ResolveStrategy, ResolveTier, Resolution, ResolverSpec, RootRule, RootSource,
    RouteCallSpec, RouteGuardSpec, RouteMatchBy, Rule, Selector, SubkindSource,
    SynthesizeAction, TransformSpec, ValueSource,
};
pub use graph::{
    AliasEntry, Annotation, Diagnostic, Edge, IdentityKey, MergeStrategy, NewAnnotation, NewEdge,
    NewNode, Node, PhaseReport, Severity, Span, SymbolEntry,
};
pub use graph::is_wildcard_http_method;
pub use ids::{EdgeId, FileId, NodeId, ProjectId, SubProjectId};
pub use kinds::{AnnotationChannel, EdgeKind, Language, NodeKind, Phase, SynthesizedKind};
pub use project::{
    NewProject, NewSourceFile, NewSubProject, Project, ProjectConfig, ProjectPatch, ProjectStatus,
    SourceFile, SubProject,
};
pub use crate::port::persistence::GraphDelta;

pub use view::{
    AggregateView, Candidate, Cluster, EdgeEvidence, EdgeView, GroupBy, HiddenInfo, LayoutMode,
    MatrixView, NodeLocationEntry, NodeLocations, NodeView, ObjectView, OrphanAccess,
    PerspectiveSpec, SourceLocation, UnresolvedInfo, ViaNode, ViewMode, ViewRegistry,
};

pub use rules::{
    check_phase, CheckPredicate, CheckReport, CheckRule, NumOrParam, ParamKind, ParamValues,
    ProjectRuleConfig, resolve_num, resolve_param_values, resolve_str, resolve_str_opt,
    RuleConfigPatch, RuleParam, RuleRequirements, RuleScope, StrOrParam, Violation,
    RULE_CODE_PREFIX,
};

pub use syntax::{
    CallSiteFact, ConfigEntryFact, Declaration, FactValue, FieldTypeFact, ImportFact,
    InheritanceFact, NamespacePolicy, SyntaxFacts,
};
