//! Graph model: nodes, edges, annotations, aliases, symbol table.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::{EdgeId, FileId, NodeId, ProjectId, SubProjectId};
use super::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};
use crate::model::kinds::Language;

/// A source location.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Span {
    pub start_line: u32,
    pub end_line: u32,
    pub start_byte: u32,
    pub end_byte: u32,
}

/// A graph node.
///
/// A *syntax node* is created directly from language syntax by the CfAst phase;
/// a *synthetic node* is aggregated from semantics by the Synthesize phase according to FKB rules (e.g. `Table`,
/// `HttpContract`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub kind: NodeKind,
    /// Short name, e.g. `StoreOrderServices`.
    pub name: String,
    /// Fully qualified name, e.g. `app\services\order\StoreOrderServices`.
    /// A synthetic node has no FQN and uses [`Node::identity`].
    pub fqn: Option<String>,
    /// The idempotent identity key of a synthetic node; `None` for a syntax node.
    pub identity: Option<IdentityKey>,
    pub file_id: Option<FileId>,
    pub span: Span,
    pub language: Language,
    /// The phase that produced this node.
    pub phase: Phase,
    pub confidence: f32,
    /// Language / framework-specific extension fields (e.g. `I18nKey.texts`, `Table.columns`).
    pub properties: Value,
}

impl Node {
    /// The qualified name for display: FQN first, then identity, then the short name.
    pub fn display_name(&self) -> String {
        if let Some(fqn) = &self.fqn {
            return fqn.clone();
        }
        if let Some(id) = &self.identity {
            return id.value.clone();
        }
        self.name.clone()
    }
}

/// Input for creating a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewNode {
    /// Primary key assigned by the pipeline; `None` means the database generates it.
    pub id: Option<NodeId>,
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub kind: NodeKind,
    pub name: String,
    pub fqn: Option<String>,
    pub identity: Option<IdentityKey>,
    pub file_id: Option<FileId>,
    pub span: Span,
    pub language: Language,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

impl NewNode {
    pub fn new(project_id: ProjectId, kind: NodeKind, name: impl Into<String>) -> Self {
        Self {
            id: None,
            project_id,
            sub_project_id: None,
            kind,
            name: name.into(),
            fqn: None,
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language(Language::UNKNOWN.to_string()),
            phase: Phase(Phase::CF_AST.to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }
}

/// The **idempotent identity key** of a synthetic node.
///
/// This is the core of the whole Synthesize phase:
/// three different rules (`Db::name('store_order')`, a Model's `$table` property, and a class-name convention)
/// must merge into one node whenever they compute the same `IdentityKey`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IdentityKey {
    /// Identity type: `Fqn` / `Named` / `ContractId`, extensible.
    pub kind: SynthesizedKind,
    /// The normalised value, e.g. `store_order`, `order.pay_success`, `POST /apple_login`.
    pub value: String,
    /// An optional scope prefix (e.g. a cache's `side`: `frontend` / `backend`).
    ///
    /// Used only for **idempotent-merge distinction** and the persistence index (`key()`); it does **not affect the
    /// display name `value`**. For example, the front end's `uni.setStorageSync('token')` and the back end's
    /// `Cache::get('token')` share a name and must become two independent nodes; folding `side` into `scope` achieves
    /// that while the node's display name stays cleanly `token`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl IdentityKey {
    pub fn fqn(value: impl Into<String>) -> Self {
        Self { kind: SynthesizedKind(SynthesizedKind::FQN.to_string()), value: value.into(), scope: None }
    }
    pub fn named(value: impl Into<String>) -> Self {
        Self { kind: SynthesizedKind(SynthesizedKind::NAMED.to_string()), value: value.into(), scope: None }
    }
    /// An identity with a scope (the display name is unchanged, only the merge key differs).
    pub fn named_scoped(value: impl Into<String>, scope: impl Into<String>) -> Self {
        Self {
            kind: SynthesizedKind(SynthesizedKind::NAMED.to_string()),
            value: value.into(),
            scope: Some(scope.into()),
        }
    }
    pub fn contract(method: &str, path: &str) -> Self {
        Self {
            kind: SynthesizedKind(SynthesizedKind::CONTRACT_ID.to_string()),
            value: format!("{} {}", method.to_uppercase(), path),
            scope: None,
        }
    }
    /// Attach a scope to an existing identity (chainable).
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = Some(scope.into());
        self
    }
    /// The string form used for the database unique index and for cross-rule comparison.
    ///
    /// Once `scope` exists it is folded into the key, so nodes with the same `value` on different ends never merge.
    pub fn key(&self) -> String {
        match &self.scope {
            Some(s) => format!("{}:{}:{}", self.kind, s, self.value),
            None => format!("{}:{}", self.kind, self.value),
        }
    }

    /// Valid only for `ContractId`: split out `(METHOD, /path)`.
    ///
    /// The identity value is `"{METHOD} {path}"` (see [`IdentityKey::contract`]); the method is already upper-case and
    /// the path contains no spaces.
    pub fn contract_parts(&self) -> Option<(String, String)> {
        if self.kind.as_str() != SynthesizedKind::CONTRACT_ID {
            return None;
        }
        let mut it = self.value.splitn(2, ' ');
        let method = it.next()?;
        let path = it.next()?;
        Some((method.to_string(), path.to_string()))
    }
}

/// The wildcard HTTP method: means "no method restriction".
///
/// ThinkPHP auto-routing (the PATH_INFO convention) and `Route::rule` bind no concrete method in the source, so they
/// are equivalent to "accept any HTTP method". This kind of contract should be treated as matching any call method
/// rather than as a concrete verb. Two places honour that: a wildcard and a concrete method **inside one sub-project**
/// still merge onto one node, and the `ResolvesToContract` bridge falls back to the wildcard spelling when another
/// sub-project's call names a concrete method that only an auto-route declares.
pub fn is_wildcard_http_method(method: &str) -> bool {
    matches!(method.to_ascii_uppercase().as_str(), "ANY" | "RULE")
}

impl std::fmt::Display for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.key())
    }
}

/// A graph edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: EdgeId,
    pub project_id: ProjectId,
    pub kind: EdgeKind,
    pub from_id: NodeId,
    pub to_id: NodeId,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

/// A node summary (so the view layer can preload in one shot instead of round-tripping `get_node` one by one).
#[derive(Debug, Clone)]
pub struct NodeSummary {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub fqn: Option<String>,
    pub sub_project_id: Option<i64>,
}

/// Input for creating an edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEdge {
    pub project_id: ProjectId,
    pub kind: EdgeKind,
    pub from_id: NodeId,
    pub to_id: NodeId,
    pub phase: Phase,
    pub confidence: f32,
    pub properties: Value,
}

impl NewEdge {
    pub fn new(project_id: ProjectId, kind: EdgeKind, from_id: NodeId, to_id: NodeId) -> Self {
        Self {
            project_id,
            kind,
            from_id,
            to_id,
            phase: Phase(Phase::CF_AST.to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }
}

/// Annotation merge strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum MergeStrategy {
    /// Keep only the highest-confidence entry per kind.
    MaxByKind,
    /// Several entries of the same kind may coexist (e.g. a class both sanitised by `where` and tainted by `whereRaw`).
    Coexist,
    /// Accumulate into an array (capability projection / merging `texts`).
    Accumulate,
    /// Overwrite directly.
    Replace,
}

impl Default for MergeStrategy {
    fn default() -> Self {
        MergeStrategy::MaxByKind
    }
}

/// An annotation: a semantic tag attached to a node.
///
/// Stored per channel ([`AnnotationChannel`]) so they do not interfere — one node can have
/// `FkbMark(pii.phone)`、`Taint(sink:RawSql)`、`Alias(event_name)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    pub id: i64,
    pub node_id: NodeId,
    pub channel: AnnotationChannel,
    pub kind: String,
    pub subkind: Option<String>,
    pub confidence: f32,
    /// Evidence: `{ hook, location, source }`.
    pub evidence: Value,
    pub phase: Phase,
}

/// Input for creating an annotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAnnotation {
    pub node_id: NodeId,
    pub channel: AnnotationChannel,
    pub kind: String,
    pub subkind: Option<String>,
    pub confidence: f32,
    pub evidence: Value,
    pub phase: Phase,
    pub merge: MergeStrategy,
}

/// An alias index entry (registered by P3/P6, queried during P7 resolution).
///
/// Supports a composite key: `(namespace, key, qualifier)`.
/// For example an accessor must be `("accessor", "status_text", Some("app\\model\\order\\StoreOrder"))`, otherwise
/// `StoreOrder::status_text` and `User::status_text` collide.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasEntry {
    pub project_id: ProjectId,
    pub namespace: String,
    pub key: String,
    pub qualifier: Option<String>,
    pub node_id: NodeId,
    pub confidence: f32,
    pub evidence: Value,
}

/// An authoritative symbol-table entry (loaded by P3, queried by P5/P6/P7).
///
/// For example `schema` (203 tables with their columns), `config_keys`, `i18n`, `facade_map`, `route_list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolEntry {
    pub project_id: ProjectId,
    /// Table name, e.g. `schema` / `config_keys` / `i18n`.
    pub table: String,
    /// Primary key within the table.
    pub key: String,
    /// Arbitrary JSON payload.
    pub value: Value,
}

/// Pipeline diagnostic information (conflicts, missing pieces, unresolved references, etc.).
///
/// Designed as a **first-class product**: `AnnotateTargetMissing`, `UnresolvedLink` and others are themselves
/// valuable findings (for example "a route points at a handler that does not exist" = a click returns 500).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub project_id: ProjectId,
    pub sub_project_id: Option<SubProjectId>,
    pub phase: Phase,
    pub code: String,
    pub severity: Severity,
    pub message: String,
    pub location: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

/// One call site that the graph's FKB rules never turned into a semantic edge.
///
/// The `callee` is the call site node's name (the called symbol as parsed); `file` / `line` point back
/// to the source so an LLM (or a human) can write the missing FKB rule without re-reading the whole repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UncoveredCall {
    pub callee: String,
    pub file: Option<String>,
    pub line: u32,
}

/// Per-sub-project FKB coverage: how much of its call sites actually became semantic edges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubCoverage {
    pub sub_project_id: i64,
    pub name: String,
    pub language: String,
    /// Sub-project role, e.g. `backend` / `frontend:admin`.
    pub role: String,
    /// Frameworks recognized by the loaded FKB (empty means "no FKB claimed this stack").
    pub frameworks: Vec<String>,
    pub total_calls: u64,
    pub covered_calls: u64,
    /// `covered_calls / total_calls`, in `0.0..=1.0` (`1.0` when there are no call sites).
    pub coverage_ratio: f64,
    /// Gap flags: `language_unknown` / `no_framework` are unambiguous knowledge gaps;
    /// `low_coverage` is advisory only (see `sub_projects_with_gaps`).
    pub flags: Vec<String>,
    /// A small sample of call sites that no rule extracted (capped, for diagnosis only).
    pub uncovered_samples: Vec<UncoveredCall>,
}

/// Project-wide totals for the coverage report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageTotals {
    pub total_calls: u64,
    pub covered_calls: u64,
    pub coverage_ratio: f64,
    pub sub_projects: u64,
    /// How many sub-projects carry an **unambiguous** knowledge gap (`language_unknown` or
    /// `no_framework`). `low_coverage` is deliberately excluded: most call sites in real code are
    /// utility calls no rule should capture, and non-literal URLs are rejected on purpose, so a low
    /// ratio alone does not prove the loaded FKB is missing anything.
    pub sub_projects_with_gaps: u64,
}

/// FKB completeness report: "how much of the code the loaded knowledge base actually sees".
///
/// This is the machine-readable fuel for the "let the LLM fill the FKB gaps" loop — it tells you
/// *which* sub-project and *which* callees are invisible, instead of guessing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub project_id: i64,
    pub totals: CoverageTotals,
    pub sub_projects: Vec<SubCoverage>,
}

/// Statistics for one pipeline run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PhaseReport {
    pub phase: String,
    pub nodes_created: u64,
    pub edges_created: u64,
    pub annotations_created: u64,
    pub aliases_created: u64,
    pub diagnostics: Vec<Diagnostic>,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(name: &str, fqn: Option<&str>, identity: Option<IdentityKey>) -> Node {
        Node {
            id: NodeId(1),
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind("Method".to_string()),
            name: name.to_string(),
            fqn: fqn.map(|s| s.to_string()),
            identity,
            file_id: None,
            span: Span::default(),
            language: Language::new("php"),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: Value::Null,
        }
    }

    /// FQN first, then the identity's value, then the short name.
    #[test]
    fn node_display_name_falls_back_fqn_then_identity_then_name() {
        assert_eq!(
            node("short", Some("app\\Svc::run"), None).display_name(),
            "app\\Svc::run"
        );
        // A synthesised node has no FQN: its display name is the clean identity value.
        assert_eq!(
            node("ignored", None, Some(IdentityKey::named("token"))).display_name(),
            "token"
        );
        assert_eq!(node("short", None, None).display_name(), "short");
        // The FQN wins when both are present.
        assert_eq!(
            node("n", Some("fqn"), Some(IdentityKey::named("ident"))).display_name(),
            "fqn"
        );
    }

    /// `NewNode::new` must not bake in any stack's language or a non-default phase.
    #[test]
    fn new_node_defaults_to_unknown_language_and_cf_ast() {
        let n = NewNode::new(ProjectId(7), NodeKind("Class".to_string()), "User");
        assert_eq!(n.project_id, ProjectId(7));
        assert_eq!(n.name, "User");
        assert!(n.id.is_none(), "the id is assigned by the pipeline");
        assert!(n.sub_project_id.is_none());
        assert!(n.fqn.is_none());
        assert!(n.identity.is_none());
        assert!(n.file_id.is_none());
        assert_eq!(n.span, Span::default());
        assert_eq!(n.language.0, Language::UNKNOWN, "the language must be declared explicitly, never guessed");
        assert_eq!(n.phase.0, Phase::CF_AST);
        assert!((n.confidence - 1.0).abs() < 1e-6);
        assert!(n.properties.is_null());
    }

    #[test]
    fn identity_key_constructors_produce_the_declared_kinds() {
        let f = IdentityKey::fqn("app\\model\\User");
        assert_eq!(f.kind.as_str(), SynthesizedKind::FQN);
        assert_eq!(f.value, "app\\model\\User");
        assert!(f.scope.is_none());

        let n = IdentityKey::named("store_order");
        assert_eq!(n.kind.as_str(), SynthesizedKind::NAMED);
        assert_eq!(n.value, "store_order");

        // The HTTP method is normalised to upper case, so `post` and `POST` are one identity.
        let c = IdentityKey::contract("post", "/user/login");
        assert_eq!(c.kind.as_str(), SynthesizedKind::CONTRACT_ID);
        assert_eq!(c.value, "POST /user/login");
        assert_eq!(IdentityKey::contract("POST", "/user/login"), c);
    }

    /// `scope` exists only to keep two same-named nodes apart (frontend vs backend `token`): it folds into
    /// the merge key but never into the display value.
    #[test]
    fn identity_key_folds_scope_into_the_merge_key_but_not_the_value() {
        let plain = IdentityKey::named("token");
        let fe = IdentityKey::named_scoped("token", "frontend");
        let be = IdentityKey::named_scoped("token", "backend");

        assert_eq!(plain.key(), format!("{}:token", SynthesizedKind::NAMED));
        assert_eq!(fe.key(), format!("{}:frontend:token", SynthesizedKind::NAMED));
        assert_ne!(fe.key(), be.key(), "the two ends must not merge into the same node");
        assert_ne!(fe, be);
        assert_eq!(fe.value, "token", "the display name is not affected by scope");
        assert_eq!(be.value, "token");

        // `with_scope` is the chainable equivalent of `named_scoped`.
        assert_eq!(plain.clone().with_scope("frontend"), fe);
        assert_eq!(plain.with_scope("frontend").key(), fe.key());
    }

    /// `contract_parts` is the inverse of `IdentityKey::contract`, and is valid only for `ContractId`.
    #[test]
    fn contract_parts_splits_only_contract_identities() {
        let c = IdentityKey::contract("post", "/user/login");
        assert_eq!(
            c.contract_parts(),
            Some(("POST".to_string(), "/user/login".to_string()))
        );
        // A path containing spaces still splits on the *first* space only.
        assert_eq!(
            IdentityKey::contract("GET", "/a b").contract_parts(),
            Some(("GET".to_string(), "/a b".to_string()))
        );

        assert_eq!(IdentityKey::named("x").contract_parts(), None);
        assert_eq!(IdentityKey::fqn("x").contract_parts(), None);
        // A contract value with no separator cannot be split.
        let broken = IdentityKey {
            kind: SynthesizedKind(SynthesizedKind::CONTRACT_ID.to_string()),
            value: "nospace".to_string(),
            scope: None,
        };
        assert_eq!(broken.contract_parts(), None);
    }

    #[test]
    fn identity_key_display_matches_the_merge_key() {
        let k = IdentityKey::named_scoped("token", "frontend");
        assert_eq!(k.to_string(), k.key());
        assert_eq!(IdentityKey::contract("GET", "/x").to_string(), IdentityKey::contract("GET", "/x").key());
    }

    /// A missing `scope` is not serialised, so stored identities stay comparable with freshly built ones.
    #[test]
    fn identity_key_round_trips_and_omits_an_empty_scope() {
        let plain = IdentityKey::named("token");
        let v = serde_json::to_value(&plain).unwrap();
        assert!(v.get("scope").is_none(), "the field must not be written when scope is unset");
        assert_eq!(serde_json::from_value::<IdentityKey>(v).unwrap(), plain);

        let scoped = IdentityKey::named_scoped("token", "frontend");
        let back: IdentityKey =
            serde_json::from_value(serde_json::to_value(&scoped).unwrap()).unwrap();
        assert_eq!(back, scoped, "scope must take part in the round-trip");
        assert_eq!(back.key(), scoped.key());
    }

    /// ThinkPHP auto-routing and `Route::rule` bind no concrete method: `ANY` / `RULE` mean "any method".
    #[test]
    fn is_wildcard_http_method_recognises_any_and_rule_case_insensitively() {
        for m in ["ANY", "any", "Any", "RULE", "rule"] {
            assert!(is_wildcard_http_method(m), "{m} must be treated as a wildcard method");
        }
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH", "ANYTHING", ""] {
            assert!(!is_wildcard_http_method(m), "{m} is not a wildcard method");
        }
    }

    #[test]
    fn new_edge_defaults_match_new_node() {
        let e = NewEdge::new(
            ProjectId(1),
            EdgeKind("Calls".to_string()),
            NodeId(1),
            NodeId(2),
        );
        assert_eq!(e.from_id, NodeId(1));
        assert_eq!(e.to_id, NodeId(2));
        assert_eq!(e.kind.as_str(), "Calls");
        assert_eq!(e.phase.0, Phase::CF_AST);
        assert!((e.confidence - 1.0).abs() < 1e-6);
        assert!(e.properties.is_null());
    }

    /// `MaxByKind` is the conservative default (a repeat annotation only replaces a weaker one).
    #[test]
    fn merge_strategy_defaults_to_max_by_kind_and_uses_pascal_case() {
        assert_eq!(MergeStrategy::default(), MergeStrategy::MaxByKind);

        for (variant, text) in [
            (MergeStrategy::MaxByKind, "MaxByKind"),
            (MergeStrategy::Coexist, "Coexist"),
            (MergeStrategy::Accumulate, "Accumulate"),
            (MergeStrategy::Replace, "Replace"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), json!(text));
            assert_eq!(
                serde_json::from_value::<MergeStrategy>(json!(text)).unwrap(),
                variant
            );
        }
    }

    // ===== Below: what the 11 tests above left out — the **persisted** record types (nothing pinned a renamed
    // field until a reload silently dropped it), the enums that cross the API boundary, and the idempotent-merge
    // guarantee `IdentityKey` exists for. =====

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        serde_json::from_value(serde_json::to_value(v).expect("serialize")).expect("deserialize")
    }

    /// `Span` is embedded in every persisted node, so both its zero default and its field names must hold.
    #[test]
    fn span_defaults_to_zero_and_round_trips() {
        let zero = Span::default();
        assert_eq!(zero, Span { start_line: 0, end_line: 0, start_byte: 0, end_byte: 0 });

        let s = Span { start_line: 10, end_line: 20, start_byte: 120, end_byte: 340 };
        assert_eq!(round_trip(&s), s, "Span relies on PartialEq: the round-trip must not lose fields");
        let v = serde_json::to_value(&s).unwrap();
        for f in ["start_line", "end_line", "start_byte", "end_byte"] {
            assert!(v.get(f).is_some(), "renaming the persisted field {f} would silently drop coordinates on reload");
        }
    }

    /// `Severity` crosses into the HTTP API and the DB, so its snake_case spelling is part of the contract.
    #[test]
    fn severity_round_trips_in_snake_case() {
        for (variant, text) in [
            (Severity::Info, "info"),
            (Severity::Warning, "warning"),
            (Severity::Error, "error"),
            (Severity::Critical, "critical"),
        ] {
            assert_eq!(serde_json::to_value(variant).unwrap(), json!(text));
            assert_eq!(serde_json::from_value::<Severity>(json!(text)).unwrap(), variant);
        }
        assert!(serde_json::from_value::<Severity>(json!("Error")).is_err(), "a misspelling must error, not degrade to Info");
    }

    /// Diagnostics are themselves a product surface (`location` / `payload` carry the click-through evidence),
    /// so nothing may fall off on the way through storage.
    #[test]
    fn diagnostic_round_trips_with_and_without_a_location() {
        let d = Diagnostic {
            project_id: ProjectId(1),
            sub_project_id: Some(SubProjectId(2)),
            phase: Phase("Check".to_string()),
            code: "unresolved-link".to_string(),
            severity: Severity::Warning,
            message: "handler not found".to_string(),
            location: Some("app/api/route/pc.php:42".to_string()),
            payload: json!({ "target": "Login/appleLogin" }),
        };
        let back: Diagnostic = round_trip(&d);
        assert_eq!(back.code, "unresolved-link");
        assert_eq!(back.severity, Severity::Warning);
        assert_eq!(back.location.as_deref(), Some("app/api/route/pc.php:42"));
        assert_eq!(back.payload, json!({ "target": "Login/appleLogin" }));
        assert_eq!(back.sub_project_id, Some(SubProjectId(2)));

        let bare = Diagnostic { location: None, payload: Value::Null, ..d };
        let back: Diagnostic = round_trip(&bare);
        assert!(back.location.is_none() && back.payload.is_null());
    }

    /// A run report starts at zero — any non-zero default would invent stats for a phase that did nothing.
    #[test]
    fn phase_report_defaults_to_zero_counters() {
        let r = PhaseReport::default();
        assert!(r.phase.is_empty());
        assert_eq!(r.nodes_created, 0);
        assert_eq!(r.edges_created, 0);
        assert_eq!(r.annotations_created, 0);
        assert_eq!(r.aliases_created, 0);
        assert_eq!(r.duration_ms, 0);
        assert!(r.diagnostics.is_empty());

        let mut counted = PhaseReport { phase: "CfAst".to_string(), nodes_created: 3, ..Default::default() };
        counted.diagnostics.push(Diagnostic {
            project_id: ProjectId(1),
            sub_project_id: None,
            phase: Phase("CfAst".to_string()),
            code: "parse-error".to_string(),
            severity: Severity::Error,
            message: "syntax error".to_string(),
            location: None,
            payload: Value::Null,
        });
        let back: PhaseReport = round_trip(&counted);
        assert_eq!(back.nodes_created, 3);
        assert_eq!(back.diagnostics.len(), 1);
        assert_eq!(back.diagnostics[0].severity, Severity::Error);
    }

    /// The record types that are written to storage and read back: a renamed field would otherwise reload as a
    /// *successfully parsed but emptied* record.
    #[test]
    fn persisted_records_survive_a_round_trip() {
        let edge = Edge {
            id: EdgeId(9),
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: NodeId(2),
            to_id: NodeId(3),
            phase: Phase("Propagate".to_string()),
            confidence: 0.75,
            properties: json!({ "line": 12 }),
        };
        let back: Edge = round_trip(&edge);
        assert_eq!(back.id, EdgeId(9));
        assert_eq!((back.from_id, back.to_id), (NodeId(2), NodeId(3)));
        assert_eq!(back.kind.as_str(), "Calls");
        assert!((back.confidence - 0.75).abs() < 1e-6);
        assert_eq!(back.properties, json!({ "line": 12 }));

        let ann = Annotation {
            id: 42,
            node_id: NodeId(5),
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            kind: "pii".to_string(),
            subkind: Some("pii.phone".to_string()),
            confidence: 0.9,
            evidence: json!({ "hook": "cf_ast" }),
            phase: Phase("AnnotatePre".to_string()),
        };
        let back: Annotation = round_trip(&ann);
        assert_eq!(back.id, 42, "the id assigned by the database must not be lost");
        assert_eq!(back.subkind.as_deref(), Some("pii.phone"));
        assert_eq!(back.channel.as_str(), AnnotationChannel::FKB_MARK);
        assert_eq!(back.evidence, json!({ "hook": "cf_ast" }));

        let new_ann = NewAnnotation {
            node_id: NodeId(5),
            channel: AnnotationChannel(AnnotationChannel::TAINT.to_string()),
            kind: "sink".to_string(),
            subkind: None,
            confidence: 0.6,
            evidence: Value::Null,
            phase: Phase("Check".to_string()),
            merge: MergeStrategy::Accumulate,
        };
        let back: NewAnnotation = round_trip(&new_ann);
        assert_eq!(back.merge, MergeStrategy::Accumulate, "the merge strategy must be stored and read together with the action");
        assert!(back.subkind.is_none() && back.evidence.is_null());

        // The alias index key is composite: `status_text` alone would collide across classes.
        let alias = AliasEntry {
            project_id: ProjectId(1),
            namespace: "accessor".to_string(),
            key: "status_text".to_string(),
            qualifier: Some("app\\model\\order\\StoreOrder".to_string()),
            node_id: NodeId(7),
            confidence: 1.0,
            evidence: Value::Null,
        };
        let back: AliasEntry = round_trip(&alias);
        assert_eq!(back.namespace, "accessor");
        assert_eq!(back.key, "status_text");
        assert_eq!(back.qualifier.as_deref(), Some("app\\model\\order\\StoreOrder"));
        let unqualified = AliasEntry { qualifier: None, ..alias };
        assert!(round_trip::<AliasEntry>(&unqualified).qualifier.is_none(), "an entry with no qualifier must not be written as Some");

        let sym = SymbolEntry {
            project_id: ProjectId(1),
            table: "schema".to_string(),
            key: "store_order".to_string(),
            value: json!({ "columns": ["id", "order_id"] }),
        };
        let back: SymbolEntry = round_trip(&sym);
        assert_eq!(back.table, "schema");
        assert_eq!(back.value, json!({ "columns": ["id", "order_id"] }));
    }

    /// `Node` is the widest record: language / phase / properties / scoped identity all have to come back.
    #[test]
    fn node_round_trips_including_its_scoped_identity() {
        let mut n = node("token", None, Some(IdentityKey::named_scoped("token", "frontend")));
        n.properties = json!({ "side": "frontend" });
        n.file_id = Some(FileId(4));
        let back: Node = round_trip(&n);
        assert_eq!(back.display_name(), n.display_name());
        assert_eq!(back.identity.map(|i| i.key()), Some("Named:frontend:token".to_string()));
        assert_eq!(back.language.0, "php");
        assert_eq!(back.file_id, Some(FileId(4)));
        assert_eq!(back.properties, json!({ "side": "frontend" }));
    }

    /// `IdentityKey` is what makes three different rules converge on one node — the set semantics (same key
    /// collapses, different kind / scope stays apart) are the whole point of `key()`.
    #[test]
    fn identity_keys_collapse_only_on_the_full_merge_key() {
        let mut set: std::collections::HashSet<IdentityKey> = std::collections::HashSet::new();
        set.insert(IdentityKey::named("token"));
        set.insert(IdentityKey::named("token"));
        set.insert(IdentityKey::named_scoped("token", "frontend"));
        set.insert(IdentityKey::fqn("token"));
        assert_eq!(set.len(), 3, "the same name with different kind / scope must be three independent nodes");

        assert_eq!(IdentityKey::contract("get", "/x").key(), format!("{}:GET /x", SynthesizedKind::CONTRACT_ID));
        assert_eq!(IdentityKey::fqn("A").with_scope("frontend").key(), format!("{}:frontend:A", SynthesizedKind::FQN));
    }

    /// `IdentityKey.kind` flows **straight from FKB** (`IdentitySpec.kind`), and `contract_parts` compares it
    /// exactly — so a rule whose identity kind is spelled `contract_id` would silently stop being a contract.
    /// Pinned so the coupling stays visible (every FKB currently spells it `ContractId`); a deliberate switch to
    /// case-insensitive matching has to update this test and `engine::compute_identity` together.
    #[test]
    fn contract_parts_matches_the_kind_exactly() {
        let lower = IdentityKey {
            kind: SynthesizedKind(SynthesizedKind::CONTRACT_ID.to_ascii_lowercase()),
            value: "GET /x".to_string(),
            scope: None,
        };
        assert_eq!(lower.contract_parts(), None, "the comparison is exact, so the kind must be spelled exactly like the ContractId");
        assert!(IdentityKey::contract("GET", "/x").contract_parts().is_some());
    }

    /// Identities written before `scope` existed must still load; today's writer omits the field when it is
    /// unset (see the existing round-trip test), so both spellings have to work.
    #[test]
    fn identity_key_loads_records_stored_without_a_scope() {
        let stored = json!({ "kind": "Named", "value": "order.pay_success" });
        let k: IdentityKey = serde_json::from_value(stored).unwrap();
        assert_eq!(k, IdentityKey::named("order.pay_success"), "old data missing scope must be filled in with None");
        assert_eq!(k.key(), format!("{}:order.pay_success", SynthesizedKind::NAMED));
    }

    /// Precedence, including the degenerate case: the identity wins even when its value is empty, because a
    /// synthetic node's name is derived from the identity rather than replacing it.
    #[test]
    fn display_name_prefers_the_identity_even_when_its_value_is_empty() {
        let n = node("short", None, Some(IdentityKey::named("")));
        assert_eq!(n.display_name(), "", "identity takes precedence over the short name");
    }

    /// `merge` is the one FKB-facing enum spelled `PascalCase` (every sibling enum is snake_case) — writing
    /// `max_by_kind` must be rejected rather than silently falling back to the default.
    #[test]
    fn merge_strategy_rejects_the_snake_case_spelling() {
        assert!(serde_json::from_value::<MergeStrategy>(json!("max_by_kind")).is_err());
        assert!(serde_json::from_value::<MergeStrategy>(json!("MAXBYKIND")).is_err());
    }

    /// Like the persisted records, the *input* records (`NewNode` / `NewEdge`) are written then read back by the
    /// store, so a renamed field would also silently drop. Pin their round-trip plus the on-wire field names.
    #[test]
    fn new_node_and_new_edge_round_trip() {
        let mut nn = NewNode::new(ProjectId(7), NodeKind("Class".to_string()), "User");
        nn.id = Some(NodeId(3));
        nn.sub_project_id = Some(SubProjectId(9));
        nn.fqn = Some("app\\User".to_string());
        nn.identity = Some(IdentityKey::named("user"));
        nn.file_id = Some(FileId(4));
        nn.language = Language::new("php");
        nn.phase = Phase("Synthesize".to_string());
        nn.confidence = 0.8;
        nn.properties = json!({ "cols": 2 });
        let back: NewNode = round_trip(&nn);
        assert_eq!(back.id, Some(NodeId(3)));
        assert_eq!(back.sub_project_id, Some(SubProjectId(9)));
        assert_eq!(back.name, "User");
        assert_eq!(back.fqn.as_deref(), Some("app\\User"));
        assert_eq!(back.kind.as_str(), "Class");
        assert_eq!(back.identity.map(|i| i.value), Some("user".to_string()));
        assert_eq!(back.confidence, 0.8);
        assert_eq!(back.properties, json!({ "cols": 2 }));
        // The field names must be exactly these on the wire — a rename reloads as an emptied record.
        let v = serde_json::to_value(&nn).unwrap();
        for f in [
            "project_id", "kind", "name", "fqn", "identity", "file_id", "span", "language", "phase",
            "confidence", "properties",
        ] {
            assert!(v.get(f).is_some(), "NewNode field {f} must be present on the wire");
        }

        let ne = NewEdge::new(ProjectId(1), EdgeKind("Calls".to_string()), NodeId(1), NodeId(2));
        let back: NewEdge = round_trip(&ne);
        assert_eq!((back.from_id, back.to_id), (NodeId(1), NodeId(2)));
        assert_eq!(back.kind.as_str(), "Calls");
        assert_eq!(back.phase.0, Phase::CF_AST);
    }

    /// `Node.sub_project_id` is the partition key; it must survive a round-trip when set (the `node` helper above
    /// leaves it `None`, so that path was untested).
    #[test]
    fn node_keeps_sub_project_id_through_round_trip() {
        let mut n = node("Svc", Some("app\\Svc"), None);
        n.sub_project_id = Some(SubProjectId(5));
        let back: Node = round_trip(&n);
        assert_eq!(back.sub_project_id, Some(SubProjectId(5)));
        assert_eq!(back.display_name(), "app\\Svc");
    }

    /// `Severity` crosses the API/DB boundary; only the exact lower-case spellings are valid, so a typo or any
    /// unknown level must error rather than degrade to `Info` (the four valid spellings are pinned by
    /// `severity_round_trips_in_snake_case`).
    #[test]
    fn severity_rejects_unknown_and_misspelled_variants() {
        for bad in ["fatal", "warn", "Error", "WARNING", "trace", ""] {
            assert!(serde_json::from_value::<Severity>(json!(bad)).is_err(), "{bad} is not a valid Severity");
        }
    }
}
