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
/// are equivalent to "accept any HTTP method". This kind of contract bridge should be treated as matching any
/// front-end call method rather than as a concrete verb — during graph building it converges with front-end `POST` /
/// `GET` calls onto one node, and the read/write heuristics use that to skip the "unknown -> read" misjudgement.
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
        assert!(n.id.is_none(), "id 由流水线分配");
        assert!(n.sub_project_id.is_none());
        assert!(n.fqn.is_none());
        assert!(n.identity.is_none());
        assert!(n.file_id.is_none());
        assert_eq!(n.span, Span::default());
        assert_eq!(n.language.0, Language::UNKNOWN, "语言必须显式声明，不猜");
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
        assert_ne!(fe.key(), be.key(), "两端不能合并成同一节点");
        assert_ne!(fe, be);
        assert_eq!(fe.value, "token", "显示名不受 scope 影响");
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
        assert!(v.get("scope").is_none(), "未设置 scope 时不应写出该字段");
        assert_eq!(serde_json::from_value::<IdentityKey>(v).unwrap(), plain);

        let scoped = IdentityKey::named_scoped("token", "frontend");
        let back: IdentityKey =
            serde_json::from_value(serde_json::to_value(&scoped).unwrap()).unwrap();
        assert_eq!(back, scoped, "scope 必须参与往返");
        assert_eq!(back.key(), scoped.key());
    }

    /// ThinkPHP auto-routing and `Route::rule` bind no concrete method: `ANY` / `RULE` mean "any method".
    #[test]
    fn is_wildcard_http_method_recognises_any_and_rule_case_insensitively() {
        for m in ["ANY", "any", "Any", "RULE", "rule"] {
            assert!(is_wildcard_http_method(m), "{m} 应视为通配方法");
        }
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH", "ANYTHING", ""] {
            assert!(!is_wildcard_http_method(m), "{m} 不是通配方法");
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
}
