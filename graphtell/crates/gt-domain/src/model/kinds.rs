//! Extensible "kind" value objects: `NodeKind` / `EdgeKind` / `Phase` / `AnnotationChannel`.
//!
//! # Why a `String` newtype instead of an `enum`
//!
//! GraphTell aims to cover every mainstream tech stack. Languages, frameworks and phases keep growing through
//! FKB (Framework Knowledge Base, YAML); hard-coding them as a Rust `enum` would mean recompiling the kernel for
//! every new node type — a violation of the **open-closed principle (OCP)**.
//!
//! So this uses "open strings + constant shorthands": the constants are only **documentation of the known
//! kinds**, not a restriction; FKB may freely introduce new ones.

macro_rules! declare_open_kind {
    ($name:ident => $doc:literal; $($const:ident = $lit:literal => $cdoc:literal),* $(,)?) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            $(
                #[doc = $cdoc]
                pub const $const: &'static str = $lit;
            )*

            pub fn new<S: Into<String>>(s: S) -> Self { Self(s.into()) }

            pub fn as_str(&self) -> &str { &self.0 }

            /// Whether it is one of the known kinds.
            pub fn is(&self, other: &str) -> bool { self.0 == other }

            /// Case-insensitive comparison (FKB usually writes lower case).
            pub fn eq_ignore_ascii_case(&self, other: &str) -> bool {
                self.0.eq_ignore_ascii_case(other)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.to_string()) }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl Default for $name {
            fn default() -> Self { Self("Unknown".to_string()) }
        }
    };
}

declare_open_kind! { NodeKind => "Graph node kinds (syntax nodes + synthetic nodes, open and extensible)";
    FILE       = "File"       => "source file",
    DIRECTORY  = "Directory"  => "directory",
    NAMESPACE  = "Namespace"  => "namespace / module",
    CLASS      = "Class"      => "class",
    INTERFACE  = "Interface"  => "interface",
    TRAIT      = "Trait"      => "PHP trait / Rust trait / mixin",
    ENUM       = "Enum"       => "enum",
    ENUM_CASE  = "EnumCase"   => "enum case",
    METHOD     = "Method"     => "class method",
    FUNCTION   = "Function"   => "free function",
    PROPERTY   = "Property"   => "class property / field",
    CONST      = "Const"      => "constant",
    CALL_SITE  = "CallSite"   => "one call site inside a method body (refined in P2)",
    // ---- synthetic nodes (materialised by the Synthesize phase per FKB rules) ----
    TABLE      = "Table"         => "database table (Mediator; converges 200-odd references)",
    COLUMN     = "Column"        => "table column (the landing point of field-level impact)",
    HTTP_CONTRACT = "HttpContract" => "HTTP contract (one endpoint as declared or called by one sub-project; two parties are two nodes joined by `ResolvesToContract`)",
    CONFIG_KEY = "ConfigKey"   => "config key",
    I18N_KEY   = "I18nKey"     => "i18n key",
    // Out-of-process mediators: each is its own kind — the naming granularity matches Table / ConfigKey, and
    // perspectives switch directly by kind.
    EVENT      = "Event"       => "event-bus node (an out-of-process mediator)",
    QUEUE      = "Queue"       => "message-queue node (an out-of-process mediator)",
    // The **consumer-side** semantic role in the event / queue perspectives: listener / consumer classes are
    // relabelled to this kind on the canvas, same family as Event / Queue (view-layer relabelling only; the DB
    // still stores `Class`, so nothing needs rebuilding).
    EVENT_HANDLER = "EventHandler" => "event / queue handler (a listener / consumer class)",
    CACHE      = "Cache"       => "cache node (an out-of-process mediator)",
    HEADER_ASSIGNMENT = "HeaderAssignment" => "response-header assignment (e.g. `$header['Access-Control-Allow-Origin'] = ...`); promotes a parse fact into a matchable node so rules can judge it",
    TOPIC      = "Topic"       => "message-topic node (an out-of-process mediator)",
    SCHEDULE   = "Schedule"    => "scheduled-job node (cron / scheduler)",
    // Front-end route node: the page routes declared by uni-app's `pages.json`, isomorphic to the backend
    // `Route` (in the route perspective front-end "page <-> endpoint" can be compared directly).
    PAGE       = "Page"        => "page / route node (a route declared in the frontend pages.json)",
    // Front-end event-bus node: decoupled inter-component communication via `uni.$emit('evt')` /
    // `bus.$emit('evt')` etc., isomorphic to the backend `Event` — same event name means same node, and both
    // emitters and listeners connect to it.
    EVENT_BUS  = "EventBus"    => "event-bus node (frontend uni.$emit / bus.$emit component communication)",
    MIDDLEWARE   = "Middleware"   => "middleware (a guard class attached to a route, promoted from Class)",
    UNKNOWN       = "Unknown"       => "unclassified, or a new kind introduced dynamically by FKB",
}

impl NodeKind {
    /// **First-class** synthetic (semantic) nodes: domain assets that are semantics in themselves, each with its
    /// own identity and perspective.
    ///
    /// The folded view shows **only** semantic nodes by default; everything else (File / Class / Method /
    /// Function / CallSite…) is an implementation detail belonging to the "syntax chain you expand by clicking".
    ///
    /// Out-of-process mediators like `Event` / `Queue` / `Cache` / `Topic` sit at the same level as Table /
    /// ConfigKey — every semantic node takes its concrete kind as `kind`.
    pub const SYNTHESIZED: &'static [&'static str] = &[
        Self::TABLE,
        Self::HTTP_CONTRACT,
        Self::CONFIG_KEY,
        Self::I18N_KEY,
        Self::EVENT,
        Self::QUEUE,
        Self::CACHE,
        Self::TOPIC,
        Self::SCHEDULE,
        Self::PAGE,
        Self::EVENT_BUS,
        Self::MIDDLEWARE,
    ];

    /// Whether this is a "first-class" semantic node (the kind itself is the semantics).
    ///
    /// Besides the built-in list this also covers the kinds **declared by FKB** (see
    /// [`crate::model::FrameworkKnowledge::semantic_kinds`] + [`register_semantic_kinds`]) — adding a semantic
    /// node should not cost a kernel change.
    pub fn is_semantic(&self) -> bool {
        if Self::SYNTHESIZED.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_SEMANTIC
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }
}

/// Semantic node kinds additionally registered by FKB (an in-process singleton, filled as FKB loads).
static EXTRA_SEMANTIC: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// Register the semantic node kinds declared by FKB (may be called repeatedly; merges idempotently).
pub fn register_semantic_kinds(kinds: impl IntoIterator<Item = String>) {
    let mut set = EXTRA_SEMANTIC
        .get_or_init(Default::default)
        .write()
        .expect("the semantic-kind registry is not corrupted");
    set.extend(kinds);
}

/// The FKB semantic node kinds currently registered (for diagnostics / test observation).
pub fn extra_semantic_kinds() -> Vec<String> {
    let mut out: Vec<String> = EXTRA_SEMANTIC
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    out.sort();
    out
}

declare_open_kind! { EdgeKind => "Graph edge kinds (open and extensible)";
    CONTAINS      = "Contains"      => "containment",
    DECLARES      = "Declares"      => "declares",
    EXTENDS       = "Extends"       => "inherits",
    IMPLEMENTS    = "Implements"    => "implements an interface",
    USES_TRAIT    = "UsesTrait"     => "uses a trait",
    CALLS         = "Calls"         => "calls",
    HAS_CALL_SITE = "HasCallSite"   => "the method body contains a call site",
    IMPORTS       = "Imports"       => "file import",
    // ---- semantic edges (built by Synthesize / Resolve) ----
    HANDLED_BY    = "HandledBy"     => "handled by (contract bridge -> handler)",
    CALLS_HTTP    = "CallsHttp"     => "the frontend calls an HTTP contract",
    TRIGGERS      = "Triggers"      => "triggers an event",
    PUBLISHES_TO  = "PublishesTo"   => "publishes to a queue topic",
    READS_DB      = "ReadsDb"       => "reads the database",
    WRITES_DB     = "WritesDb"      => "writes the database",
    WRITES_CACHE  = "WritesCache"   => "writes the cache",
    READS_CACHE   = "ReadsCache"    => "reads the cache",
    MAPS_TO       = "MapsTo"        => "a model maps to a table",
    HAS_COLUMN    = "HasColumn"     => "owns a column (table / model -> column)",
    READS_CONFIG  = "ReadsConfig"   => "reads config",
    MUTATES       = "Mutates"       => "mutates a state container (frontend Store / Vuex, Pinia…)",
    // Front-end page navigation: `uni.navigateTo` / `redirectTo` / `reLaunch` / `switchTab`, etc., from the
    // initiator (a function / component method) to the target `Page` node.
    NAVIGATES_TO  = "NavigatesTo"   => "frontend page navigation (uni.navigateTo etc.)",
    // Front-end event bus: emitter `--Emits-->` event node `<--ListensTo--` listener, isomorphic to the backend
    // `Event`'s `Triggers` — the event node acts as a "via" bridge in the folded view.
    EMITS       = "Emits"       => "the frontend emits an event-bus event (uni.$emit / bus.$emit)",
    LISTENS_TO  = "ListensTo"   => "the frontend listens for an event-bus event (uni.$on / bus.$on)",
    PASSES_THROUGH = "PassesThrough" => "passes through (contract -> middleware)",
    RESOLVES_TO   = "ResolvesTo"    => "dynamic resolution result",
    UNKNOWN       = "Unknown"       => "unclassified, or a new edge kind introduced dynamically by FKB",
}

impl EdgeKind {
    /// Semantic edges (business-resource dependencies): business dependencies built by Synthesize / Resolve
    /// (reading the DB / reading config / cache / events / cross-service…). The folded view shows only these by
    /// default; **both of their ends should be semantic nodes**, so they are both drawable on the canvas and
    /// counted in the candidate badge's "semantic in-edges N".
    ///
    /// Note: `HandledBy` / `CallsHttp` are **not in this list** — they are "bridge edges" (see [`Self::BRIDGE`]):
    /// one end is a semantic node and the other a syntax node (a handler / front-end function), so they are not a
    /// resource dependency, do not count towards "in-edges N", and are never drawn as canvas edges; their syntax
    /// endpoint degrades into the `orphans` tally (expandable in the front-end drawer / Inspector). The rest
    /// (Contains / Declares / Calls / HasCallSite …) is implementation structure, i.e. the syntax chain.
    pub const SEMANTIC: &'static [&'static str] = &[
        Self::TRIGGERS,
        Self::PUBLISHES_TO,
        Self::READS_DB,
        Self::WRITES_DB,
        Self::MAPS_TO,
        Self::READS_CONFIG,
        Self::RESOLVES_TO,
        Self::WRITES_CACHE,
        Self::MUTATES,
        Self::NAVIGATES_TO,
        Self::EMITS,
        Self::LISTENS_TO,
        Self::PASSES_THROUGH,
        Self::READS_CACHE,
    ];

    /// Bridge edges (discovered connectors between semantic and syntax): `HandledBy` (contract -> handler),
    /// `CallsHttp` (front-end function -> contract). They are not business-resource dependencies — they do not
    /// count towards "in-edges N" and are not canvas edges — but they must stay in `is_chain_edge` so discovery
    /// traversal can connect the syntax implementation to the semantic resource; their syntax endpoint (handler /
    /// caller) degrades into the `orphans` tally, and the canvas is always semantic nodes.
    /// `HasColumn` (table / model -> column) is in this list too, and what that buys is precisely the combination
    /// "it does not count towards semantic in/out-edges N, but it stays in `is_chain_edge` and is traversable":
    /// it is a **composition relation**, not a resource dependency, so counting it as a semantic edge would add
    /// +15 to the out-edge count of a 15-column table and blur it together with "how many tables are read /
    /// written" right next to it, making the number meaningless.
    /// Classified as a bridge edge: fan does not count ✓, impact can still be drilled down from table to field ✓.
    pub const BRIDGE: &'static [&'static str] =
        &[Self::HANDLED_BY, Self::CALLS_HTTP, Self::HAS_COLUMN];

    /// Whether this is a "semantic edge meaningful to a human" (a business-resource dependency, see
    /// [`Self::SEMANTIC`]).
    ///
    /// Besides the built-in list this also covers the edge kinds **declared by FKB** (see [`register_edge_kinds`])
    /// — adding a semantic edge should not cost a kernel change (isomorphic to
    /// [`crate::model::kinds::EXTRA_SEMANTIC`] for node kinds).
    pub fn is_semantic(&self) -> bool {
        if Self::SEMANTIC.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_SEMANTIC_EDGE
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }

    /// Whether this is a "bridge edge" (a discovered connector between semantic and syntax, see [`Self::BRIDGE`]).
    ///
    /// Besides the built-in list this also covers the bridge edge kinds **declared by FKB** (see
    /// [`register_edge_kinds`]).
    pub fn is_bridge(&self) -> bool {
        if Self::BRIDGE.iter().any(|k| self.0 == *k) {
            return true;
        }
        EXTRA_BRIDGE_EDGE
            .get_or_init(Default::default)
            .read()
            .map(|set| set.contains(&self.0))
            .unwrap_or(false)
    }
}

/// Semantic-edge test (authority: [`EdgeKind::SEMANTIC`]).
///
/// How it differs from `is_chain_edge`: a chain edge is "can we walk along the call chain" (including syntactic
/// call edges like `Calls` / `HasCallSite`), while a semantic edge is "this edge is meaningful to a human in
/// itself" (reading / writing a table, triggering an event…). Counting must use semantic edges — otherwise
/// `Calls` leaks into "in-edges N" and the number agrees neither with the canvas nor with any business meaning.
pub fn is_semantic_edge(kind: &str) -> bool {
    EdgeKind(kind.to_string()).is_semantic()
}

/// Bridge-edge test (authority: [`EdgeKind::BRIDGE`]).
///
/// A bridge edge connects "semantic node <-> syntax node" and is not a business-resource dependency: it does not
/// count towards "in-edges N" and is not a canvas edge, but it is kept in discovery traversal. Together with
/// `is_semantic_edge`, the folding logic uses it to decide "passes the drawability gate but still degrades into
/// the `orphans` tally" — see `gt_application::view_service`.
pub fn is_bridge_edge(kind: &str) -> bool {
    EdgeKind(kind.to_string()).is_bridge()
}

/// Semantic / bridge edge kinds additionally registered by FKB (an in-process singleton, filled as FKB loads).
///
/// Isomorphic to [`EXTRA_SEMANTIC`] for nodes: "adding an edge kind" also only means writing FKB, not changing the
/// kernel — this is how the "FKB only, zero code" promise lands on the **edge** dimension (the same mechanism for
/// nodes has been in place for a while).
static EXTRA_SEMANTIC_EDGE: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();
static EXTRA_BRIDGE_EDGE: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// Register the semantic / bridge edge kinds declared by FKB (may be called repeatedly; merges idempotently).
///
/// Caller: `gt_adapter_fkb::loader` feeds `semantic_edge_kinds` / `bridge_edge_kinds` in as it loads FKB.
/// After that `is_semantic` / `is_bridge` / `is_chain_edge` treat them as first-class automatically, with no need
/// to touch the `SEMANTIC` / `BRIDGE` lists in `kinds.rs` again.
pub fn register_edge_kinds(
    semantic: impl IntoIterator<Item = String>,
    bridge: impl IntoIterator<Item = String>,
) {
    {
        let mut set = EXTRA_SEMANTIC_EDGE
            .get_or_init(Default::default)
            .write()
            .expect("the semantic-edge registry is not corrupted");
        set.extend(semantic);
    }
    {
        let mut set = EXTRA_BRIDGE_EDGE
            .get_or_init(Default::default)
            .write()
            .expect("the bridge-edge registry is not corrupted");
        set.extend(bridge);
    }
}

/// The FKB edge kinds currently registered (for diagnostics / tests / `validate` observation).
pub fn extra_edge_kinds() -> (Vec<String>, Vec<String>) {
    let sem: Vec<String> = EXTRA_SEMANTIC_EDGE
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    let bri: Vec<String> = EXTRA_BRIDGE_EDGE
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    (sem, bri)
}

/// Call-chain edges: the folded view does "forward discovery" along these, treating syntax nodes as pass-through.
///
/// The authority is [`EdgeKind::is_semantic`] + [`EdgeKind::is_bridge`] (including FKB-declared kinds) plus a few
/// purely syntactic chain edges (`Calls` / `HasCallSite`). Centralised here as the single source of truth — a new
/// edge kind only needs registering into `SEMANTIC` / `BRIDGE` to become traversable automatically, no listing
/// here.
pub fn is_chain_edge(kind: &str) -> bool {
    matches!(kind, "Calls" | "HasCallSite")
        || EdgeKind(kind.to_string()).is_semantic()
        || EdgeKind(kind.to_string()).is_bridge()
}

declare_open_kind! { Phase => "Pipeline phases";
    INGEST        = "Ingest"        => "P0 ingest: detect sub-projects and the files to analyse",
    CF_AST        = "CfAst"         => "P2 syntax-level graph building",
    PREPARE       = "Prepare"       => "P3 load FKB and the authoritative symbol tables",
    ANNOTATE_PRE  = "AnnotatePre"   => "P4 tag by source-code selectors",
    SYNTHESIZE    = "Synthesize"    => "P5 synthesise non-code semantic nodes",
    ANNOTATE_POST = "AnnotatePost"  => "P6 tag the aggregated result / register aliases",
    RESOLVE       = "Resolve"       => "P7 dynamic resolution (funnel + fixed point)",
    PROPAGATE     = "Propagate"     => "P8 propagate semantics upward along the call chain (generic, framework-agnostic)",
    CHECK         = "Check"         => "compliance check (runs rules read-only after the graph is built, producing violations; diagnostic code prefix rule:)",
}

declare_open_kind! { AnnotationChannel => "Annotation channels (channels do not interfere with each other and can coexist)";
    FKB_MARK    = "FkbMark"    => "framework semantic tag: listener / pii / auth.public / data.criticality",
    TAINT       = "Taint"      => "taint semantics: source / sanitizer / sink",
    CAPABILITY  = "Capability" => "capability declared by a scope: RateLimiting / Authentication",
    ALIAS       = "Alias"      => "alias registration: event_name / facade / accessor",
}

declare_open_kind! { SynthesizedKind => "Identity type of a synthesised node";
    FQN     = "Fqn"     => "fully qualified name (e.g. `Table:store_order`)",
    NAMED   = "Named"   => "named (e.g. the event name `order.pay_success`)",
    CONTRACT_ID = "ContractId" => "HTTP contract `METHOD /path`",
}

declare_open_kind! { AnnotationKind => "Annotation kinds (semantic tags FKB puts on nodes; open and extensible)";
    PII                = "pii"                  => "personally identifiable information (privacy / compliance)",
    DATA_CRITICALITY   = "data.criticality"     => "data criticality (high / medium / low judged after fan_in aggregation)",
    CONFIG_STORAGE     = "config.storage"       => "config storage medium (abstract interpretation can only yield ⊤ for it)",
    CONFIG_MUTABILITY  = "config.mutability"    => "config mutability (mutable at runtime vs read-only)",
    AUTH_PUBLIC        = "auth.public"          => "public endpoint (no authentication middleware attached)",
    I18N_MISSING_LOCALE = "i18n.missing_locale" => "i18n locale coverage missing",
    // Business-specific annotation kinds are appended by FKB via `register_annotation_kinds`; the kernel knows no concrete names.
}

/// Annotation kinds additionally registered by FKB (an in-process singleton, filled as FKB loads).
///
/// Isomorphic to [`EXTRA_SEMANTIC`] for nodes / [`EXTRA_SEMANTIC_EDGE`] for edges: "adding an annotation
/// semantic" also only means writing FKB, not changing the kernel. The kernel-standard kinds live in the
/// [`AnnotationKind`] constant list.
static EXTRA_ANNOTATION: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<String>>> =
    std::sync::OnceLock::new();

/// Register the annotation kinds declared by FKB (may be called repeatedly; merges idempotently).
///
/// Caller: `gt_adapter_fkb::loader` feeds `annotation_kinds` in as it loads FKB.
pub fn register_annotation_kinds(kinds: impl IntoIterator<Item = String>) {
    let mut set = EXTRA_ANNOTATION
        .get_or_init(Default::default)
        .write()
        .expect("the annotation-kind registry is not corrupted");
    set.extend(kinds);
}

/// The FKB annotation kinds currently registered (for diagnostics / test observation).
pub fn extra_annotation_kinds() -> Vec<String> {
    let mut out: Vec<String> = EXTRA_ANNOTATION
        .get_or_init(Default::default)
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default();
    out.sort();
    out
}

/// Tech-stack language (open and extensible).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, Default)]
#[serde(transparent)]
pub struct Language(pub String);

impl Language {
    pub const PHP: &'static str = "php";
    pub const JAVASCRIPT: &'static str = "javascript";
    pub const TYPESCRIPT: &'static str = "typescript";
    pub const JAVA: &'static str = "java";
    pub const PYTHON: &'static str = "python";
    pub const RUST: &'static str = "rust";
    pub const UNKNOWN: &'static str = "unknown";

    pub fn new<S: Into<String>>(s: S) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn is_php(&self) -> bool {
        self.0 == Self::PHP
    }
}

impl From<&str> for Language {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_kind_registry_extends_classification() {
        // A built-in semantic edge: recognised before registration.
        assert!(EdgeKind("ReadsDb".to_string()).is_semantic());
        assert!(is_chain_edge("ReadsDb"));

        // A brand-new semantic edge: not recognised before registration, recognised after (and automatically traversable along the call chain).
        assert!(!EdgeKind("SendsWebhook".to_string()).is_semantic());
        assert!(!is_chain_edge("SendsWebhook"));
        register_edge_kinds(vec!["SendsWebhook".to_string()], vec![]);
        assert!(EdgeKind("SendsWebhook".to_string()).is_semantic());
        assert!(is_chain_edge("SendsWebhook"));

        // A brand-new bridge edge: not recognised before registration, recognised as a bridge edge after and traversable.
        assert!(!EdgeKind("MyBridge".to_string()).is_bridge());
        register_edge_kinds(vec![], vec!["MyBridge".to_string()]);
        assert!(EdgeKind("MyBridge".to_string()).is_bridge());
        assert!(is_chain_edge("MyBridge"));

        // The old hard-coded path is unaffected: built-in bridge edges are still recognised.
        assert!(EdgeKind("HandledBy".to_string()).is_bridge());
        assert!(is_chain_edge("HandledBy"));
        // Purely syntactic chain edges still go through the built-in branch.
        assert!(is_chain_edge("Calls"));
        assert!(is_chain_edge("HasCallSite"));
    }

    #[test]
    fn annotation_kind_registry_extends_classification() {
        // Kernel-standard kinds (constants, i.e. `&'static str`): compared directly against literals.
        assert_eq!(AnnotationKind::PII, "pii");
        assert_eq!(AnnotationKind::AUTH_PUBLIC, "auth.public");
        // Business-specific kinds: first-class once registered (the OCP escape hatch, isomorphic to edge kinds).

        register_annotation_kinds(vec!["entrypoint.login".to_string()]);
        assert!(extra_annotation_kinds().contains(&"entrypoint.login".to_string()));
    }

    /// The constants and the classification lists must agree.
    ///
    /// `SEMANTIC` used to carry a bare `"ReadsCache"` literal with no constant behind it, so renaming either
    /// side produced no compile-time signal — the two could drift silently.
    #[test]
    fn edge_kind_constants_agree_with_the_classification_lists() {
        for k in [
            EdgeKind::TRIGGERS,
            EdgeKind::PUBLISHES_TO,
            EdgeKind::READS_DB,
            EdgeKind::WRITES_DB,
            EdgeKind::WRITES_CACHE,
            EdgeKind::READS_CACHE,
            EdgeKind::READS_CONFIG,
            EdgeKind::MAPS_TO,
        ] {
            assert!(EdgeKind::from(k).is_semantic(), "{k} must be in the semantic edge list");
            assert!(is_semantic_edge(k));
        }
        for k in [EdgeKind::HANDLED_BY, EdgeKind::CALLS_HTTP, EdgeKind::HAS_COLUMN] {
            assert!(EdgeKind::from(k).is_bridge(), "{k} must be in the bridging edge list");
        }
        // A bridge edge is deliberately **not** semantic, and vice versa.
        assert!(!EdgeKind::from(EdgeKind::HANDLED_BY).is_semantic());
        assert!(!EdgeKind::from(EdgeKind::READS_DB).is_bridge());
    }

    /// The node-kind half of the same OCP mechanism the two tests above cover for edges / annotations:
    /// a kind FKB declares becomes first-class without a kernel change.
    ///
    /// Note the registries are **process-global** and tests run in parallel, so the kind registered here must
    /// be unique to this test and assertions may only claim *membership*, never exact contents.
    #[test]
    fn node_kind_registry_extends_classification() {
        // Built-in first-class semantic nodes: recognised without any registration.
        assert!(NodeKind::from(NodeKind::TABLE).is_semantic());
        assert!(NodeKind::from(NodeKind::EVENT).is_semantic());
        assert!(NodeKind::from(NodeKind::MIDDLEWARE).is_semantic());
        // Syntax nodes are never semantic in themselves.
        assert!(!NodeKind::from(NodeKind::CLASS).is_semantic());
        assert!(!NodeKind::from(NodeKind::METHOD).is_semantic());
        assert!(!NodeKind::from(NodeKind::CALL_SITE).is_semantic());

        // A brand-new kind: unrecognised first, first-class after registration.
        let fresh = NodeKind("InvoiceBatch".to_string());
        assert!(!fresh.is_semantic());
        register_semantic_kinds(vec!["InvoiceBatch".to_string()]);
        assert!(fresh.is_semantic());
        assert!(extra_semantic_kinds().contains(&"InvoiceBatch".to_string()));
    }

    /// `Default` is macro-generated for the open kinds but **derived** for `Language`, so the two mean
    /// different things — and neither is "the same as the UNKNOWN constant" in `Language`'s case.
    #[test]
    fn defaults_differ_between_open_kinds_and_language() {
        assert_eq!(NodeKind::default(), NodeKind::from(NodeKind::UNKNOWN));
        assert_eq!(NodeKind::default().as_str(), "Unknown");
        assert_eq!(EdgeKind::default(), EdgeKind::from(EdgeKind::UNKNOWN));

        // `Language::default()` is the **empty** string, which is not `Language::UNKNOWN`.
        assert_eq!(Language::default().as_str(), "");
        assert_ne!(Language::default(), Language::new(Language::UNKNOWN));
        assert_eq!(Language::new(Language::UNKNOWN).as_str(), "unknown");

        // `is_php` compares exactly (values come from parsers, which already normalise case).
        assert!(Language::new("php").is_php());
        assert!(!Language::new("PHP").is_php());
    }

    /// `#[serde(transparent)]`: every kind and the language serialize as a bare string, not an object —
    /// persisted rows and API responses depend on that shape.
    #[test]
    fn kinds_serialize_as_bare_strings_and_compare_case_insensitively_on_request() {
        assert_eq!(
            serde_json::to_value(NodeKind::from("Table")).unwrap(),
            serde_json::json!("Table")
        );
        assert_eq!(
            serde_json::to_value(EdgeKind::from("ReadsDb")).unwrap(),
            serde_json::json!("ReadsDb")
        );
        assert_eq!(
            serde_json::to_value(Language::new("php")).unwrap(),
            serde_json::json!("php")
        );
        let back: NodeKind = serde_json::from_value(serde_json::json!("Table")).unwrap();
        assert_eq!(back, NodeKind::from("Table"));
        let back: Language = serde_json::from_value(serde_json::json!("java")).unwrap();
        assert_eq!(back, Language::new("java"));

        // `is` is exact; `eq_ignore_ascii_case` is the one FKB should use (it writes lower case).
        assert!(NodeKind::from("Table").is("Table"));
        assert!(!NodeKind::from("Table").is("table"));
        assert!(NodeKind::from("Table").eq_ignore_ascii_case("table"));
        assert!(!NodeKind::from("Table").eq_ignore_ascii_case("tabl"));
    }

    // ===== Below: what the six tests above leave out — the *lists* (`SYNTHESIZED` / `SEMANTIC` / `BRIDGE`) are
    // the single source of truth for folding, counting and drawing, yet only a handful of their entries were
    // asserted; the invariants between the two edge lists, the `is_chain_edge` negative side, registry
    // idempotency, and three open kinds (`Phase` / `AnnotationChannel` / `SynthesizedKind`) had no test at all. =====

    /// Every entry of `SYNTHESIZED` must be classified as semantic, and the kinds that are **deliberately**
    /// absent must stay out: `Column` is a component of a Table rather than a resource of its own,
    /// `EventHandler` is a *view-layer relabelling* of what is still a `Class` in storage, and
    /// `HeaderAssignment` is a matchable parse fact, not a business asset.
    #[test]
    fn every_synthesized_node_kind_is_semantic_and_the_omissions_stay_omitted() {
        assert!(!NodeKind::SYNTHESIZED.is_empty());
        for k in NodeKind::SYNTHESIZED {
            assert!(NodeKind::from(*k).is_semantic(), "{k} declared in the first-class semantic list must be a first-class semantic node");
        }
        // A repeated entry would be a copy-paste slip that no compiler catches.
        let mut seen = std::collections::HashSet::new();
        for k in NodeKind::SYNTHESIZED {
            assert!(seen.insert(*k), "{k} is duplicated in SYNTHESIZED");
        }

        for k in [NodeKind::COLUMN, NodeKind::EVENT_HANDLER, NodeKind::HEADER_ASSIGNMENT] {
            assert!(!NodeKind::from(k).is_semantic(), "{k} is deliberately absent from the first-class semantic list");
        }
        // Syntax nodes and the unclassified default are never semantic.
        for k in [NodeKind::FILE, NodeKind::CLASS, NodeKind::PROPERTY, NodeKind::UNKNOWN] {
            assert!(!NodeKind::from(k).is_semantic(), "{k} is not a semantic node");
        }
        assert!(!NodeKind::default().is_semantic());
    }

    /// The two edge lists are **mutually exclusive by design**: a bridge edge must not inflate "semantic
    /// in-edges N" (that is exactly why `HasColumn` moved from semantic to bridge), and a semantic edge must
    /// never degrade into the `orphans` tally.
    #[test]
    fn semantic_and_bridge_edge_lists_are_disjoint_and_exhaustively_classified() {
        for k in EdgeKind::SEMANTIC {
            assert!(EdgeKind::from(*k).is_semantic(), "{k} is in the semantic edge list");
            assert!(!EdgeKind::from(*k).is_bridge(), "{k} must not be a bridging edge at the same time");
            assert!(is_semantic_edge(k));
            assert!(!is_bridge_edge(k));
        }
        for k in EdgeKind::BRIDGE {
            assert!(EdgeKind::from(*k).is_bridge(), "{k} is in the bridging edge list");
            assert!(!EdgeKind::from(*k).is_semantic(), "{k} must not be a semantic edge at the same time");
            assert!(is_bridge_edge(k));
            assert!(!is_semantic_edge(k));
        }
        let mut seen = std::collections::HashSet::new();
        for k in EdgeKind::SEMANTIC.iter().chain(EdgeKind::BRIDGE.iter()) {
            assert!(seen.insert(*k), "{k} appears in both lists");
        }
    }

    /// Only the previous test's coverage was positive cases. A `Contains` / `Imports` leaking into
    /// `is_chain_edge` would silently turn the folded view into the whole syntax tree.
    #[test]
    fn chain_edges_cover_semantic_bridge_and_two_syntactic_ones_only() {
        for k in EdgeKind::SEMANTIC.iter().chain(EdgeKind::BRIDGE.iter()) {
            assert!(is_chain_edge(k), "{k} takes part in call-chain exploration");
        }
        assert!(is_chain_edge("Calls"));
        assert!(is_chain_edge("HasCallSite"));

        for k in [
            EdgeKind::CONTAINS,
            EdgeKind::DECLARES,
            EdgeKind::EXTENDS,
            EdgeKind::IMPLEMENTS,
            EdgeKind::USES_TRAIT,
            EdgeKind::IMPORTS,
            EdgeKind::UNKNOWN,
        ] {
            assert!(!is_chain_edge(k), "{k} must not appear in a call chain");
            assert!(!is_semantic_edge(k), "{k} must not be counted as a semantic edge");
        }
        assert!(!is_chain_edge(""));
        // An invented kind not (yet) declared by FKB is nothing at all.
        assert!(!is_chain_edge("TotallyMadeUp"));
        assert!(!is_semantic_edge("TotallyMadeUp"));
        assert!(!is_bridge_edge("TotallyMadeUp"));
    }

    /// The registries are process-global singletons filled as FKB loads, so loading twice (two sub-projects, two
    /// runs in one process) must not duplicate anything, and the read accessor must stay sorted.
    #[test]
    fn registries_merge_idempotently_and_the_accessors_report_what_was_registered() {
        let node_kind = "WarehouseSlotLocator".to_string();
        assert!(!NodeKind(node_kind.clone()).is_semantic());
        register_semantic_kinds(vec![node_kind.clone()]);
        register_semantic_kinds(vec![node_kind.clone()]);
        let registered = extra_semantic_kinds();
        let occurrences = registered.iter().filter(|k| **k == node_kind).count();
        assert_eq!(occurrences, 1, "registering the same kind twice must not produce duplicate entries");
        let sorted = {
            let mut s = registered.clone();
            s.sort();
            s
        };
        assert_eq!(registered, sorted, "extra_semantic_kinds returns a sorted result");

        assert!(!EdgeKind("FanoutWebhookSender".to_string()).is_semantic());
        assert!(!EdgeKind("BridgeRedirect".to_string()).is_bridge());
        register_edge_kinds(vec!["FanoutWebhookSender".to_string()], vec!["BridgeRedirect".to_string()]);
        register_edge_kinds(vec!["FanoutWebhookSender".to_string()], Vec::new());
        let (sem, bridge) = extra_edge_kinds();
        assert_eq!(sem.iter().filter(|k| **k == "FanoutWebhookSender").count(), 1);
        assert!(bridge.contains(&"BridgeRedirect".to_string()));
        // Registration is only additive — an empty call must not clear anything.
        register_edge_kinds(Vec::new(), Vec::new());
        let (sem2, bridge2) = extra_edge_kinds();
        assert!(sem2.contains(&"FanoutWebhookSender".to_string()) && bridge2.contains(&"BridgeRedirect".to_string()));
    }

    /// `Language` is the one open kind that is **not** macro-generated (hand-written `Default`, extra
    /// `is_php`), so its conversions and constants need pinning of their own.
    #[test]
    fn language_covers_its_constants_conversions_and_display() {
        for (c, text) in [
            (Language::PHP, "php"),
            (Language::JAVASCRIPT, "javascript"),
            (Language::TYPESCRIPT, "typescript"),
            (Language::JAVA, "java"),
            (Language::PYTHON, "python"),
            (Language::RUST, "rust"),
            (Language::UNKNOWN, "unknown"),
        ] {
            assert_eq!(Language::new(c), Language::from(c), "the two construction paths of {text} must agree");
            assert_eq!(Language::from(c).as_str(), text);
            assert_eq!(Language::from(c).to_string(), text, "Display writes the bare string directly");
        }
        assert!(Language::from(Language::PHP).is_php());
        assert!(!Language::from(Language::TYPESCRIPT).is_php());
        assert!(!Language::from(Language::UNKNOWN).is_php());
    }

    /// The values of `Phase` / `AnnotationChannel` / `SynthesizedKind` are **persisted** (every node, edge and
    /// annotation row carries them) — a renamed constant would orphan stored rows, so the spelling is part of
    /// the contract, not an implementation detail.
    #[test]
    fn phase_channel_and_identity_kind_spellings_are_stable() {
        for (c, text) in [
            (Phase::INGEST, "Ingest"),
            (Phase::CF_AST, "CfAst"),
            (Phase::PREPARE, "Prepare"),
            (Phase::ANNOTATE_PRE, "AnnotatePre"),
            (Phase::SYNTHESIZE, "Synthesize"),
            (Phase::ANNOTATE_POST, "AnnotatePost"),
            (Phase::RESOLVE, "Resolve"),
            (Phase::PROPAGATE, "Propagate"),
            (Phase::CHECK, "Check"),
        ] {
            assert_eq!(Phase::from(c).as_str(), text);
            let round: Phase = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(round, Phase::from(c), "{text} must be read back verbatim");
        }
        for (c, text) in [
            (AnnotationChannel::FKB_MARK, "FkbMark"),
            (AnnotationChannel::TAINT, "Taint"),
            (AnnotationChannel::CAPABILITY, "Capability"),
            (AnnotationChannel::ALIAS, "Alias"),
        ] {
            assert_eq!(AnnotationChannel::from(c).as_str(), text);
            let round: AnnotationChannel = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(round, AnnotationChannel::from(c));
        }
        for (c, text) in [
            (SynthesizedKind::FQN, "Fqn"),
            (SynthesizedKind::NAMED, "Named"),
            (SynthesizedKind::CONTRACT_ID, "ContractId"),
        ] {
            assert_eq!(SynthesizedKind::from(c).as_str(), text);
            let round: SynthesizedKind = serde_json::from_value(serde_json::json!(text)).unwrap();
            assert_eq!(round, SynthesizedKind::from(c));
        }
    }

    /// The macro derives `Hash` / `Ord` because kinds are used as map keys and sorted for stable output; those
    /// implementations must agree with `Eq` on the inner string (the same kind built three different ways is
    /// one entry in a set, not three).
    #[test]
    fn derived_hash_and_ordering_follow_the_inner_string() {
        let mut set = std::collections::HashSet::new();
        assert!(set.insert(NodeKind::from("Table")));
        assert!(!set.insert(NodeKind::new("Table")), "different constructors, but the Hash must be equal");
        assert!(!set.insert(NodeKind("Table".to_string())));
        assert_eq!(set.len(), 1);

        let mut kinds = vec![NodeKind::from("Class"), NodeKind::from("Table")];
        kinds.sort();
        assert_eq!(kinds.first().unwrap().as_str(), "Class", "Ord follows string order so the output stays stable");
        assert!(EdgeKind::from("ReadsDb") < EdgeKind::from("WritesDb"));
    }
}
