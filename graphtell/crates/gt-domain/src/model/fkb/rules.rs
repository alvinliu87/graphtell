use super::*;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::graph::MergeStrategy;
use crate::model::kinds::{AnnotationChannel, EdgeKind, Language, NodeKind, Phase, SynthesizedKind};

// `default_conf` lives in the `detector` submodule but is also the serde default for `Rule`'s
// confidence field, so we pull it in explicitly here.
use super::detector::default_conf;
/// One rule: in a given phase, run a set of actions against the matched targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub phase: Phase,
    pub selector: Selector,
    pub binding: Vec<Action>,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// Per-rule language scoping. `None` inherits the owning FKB's `language` (current behaviour);
    /// `Some(list)` restricts the rule to the listed languages, where the sentinel `Language("*")`
    /// means "all languages" (used by cross-language universal rules).
    #[serde(default)]
    pub languages: Option<Vec<Language>>,
}

impl Rule {
    /// Whether this rule is effective for a sub-project of `sub_language`, given the language of the
    /// FKB that declared it (`fk_language`). See `languages` for the scoping semantics.
    pub fn applies_to(&self, fk_language: &Language, sub_language: &Language) -> bool {
        match &self.languages {
            Some(list) => list.iter().any(|x| x == sub_language || x.0 == "*"),
            None => fk_language == sub_language || fk_language.0 == "*",
        }
    }
}





/// A selector: decides what a rule acts on.
///
/// **Every variant must be consumed by one of the `matches_*` functions in `gt-pipeline/src/engine.rs`.**
/// A variant that no matcher reads is dead model — the rule parses, the test suite is green, and the rule
/// silently never fires (this is exactly how `exclude_globs` and the two removed variants below lived on).
/// `engine::selector_has_matcher` is the exhaustive-`match` guard that enforces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    /// A call site, e.g. `Db::name('store_order')`.
    Call {
        /// Callee matching pattern; supports `|`-separated alternatives and `*` wildcards:
        /// `think\facade\Db::name|*:where|Db::raw`
        #[serde(default)]
        callee: Option<String>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },

    /// Inheritance / implementation / trait.
    Inheritance {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        with_property: Option<String>,
    },
    /// A config-file entry.
    ConfigEntry {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        key_path: Option<String>,
        /// Additional predicates (applied to config entries only).
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// **A node on the graph** (P6 only: the selector is a node rather than source code).
    ///
    /// Note: the `declaration` and `dynamic` variants once declared here were removed — no matcher ever
    /// read them and no FKB/rule used them, so a rule carrying them parsed fine and never fired.
    Node {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
}

/// A predicate (a `where` condition).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    /// The target (class) has a given property.
    HasProperty(String),
    /// The key exists in the authoritative symbol table.
    InSymbolTable { table: String, key_of: ValueSource },
    /// A column of the authoritative symbol table matches one of the given names.
    ColumnsMatch { table: String, names: Vec<String> },
    /// The node already carries a given annotation.
    HasAnnotation { kind: String },
    /// The given capability does not exist on the scope chain.
    NoneOfCapability(Vec<String>),
    /// Whether i18n has a missing locale.
    HasMissing(bool),
    /// fan_in is at least the threshold.
    FanInGte(u64),
    /// The argument count equals the given value.
    ArgCount(usize),
    /// The node name (or identity value) contains the given substring (case-insensitive).
    NameMatches(String),
    /// A node property equals the given value.
    PropertyIs { name: String, value: String },
    /// The `arg`-th argument of the call site (a string) starts with `prefix` (case-sensitive).
    /// Used to narrow matching by call-argument prefix, e.g. picking out only routes like `Route::get('crontab/...')`.
    ArgStartsWith { arg: usize, prefix: String },
    /// The config entry's value is an **array** with at least `n` elements (only effective for `kind: config_entry`).
    ///
    /// PHP config parsing expands array elements into independent entries (`listen.evt.0`) that exist **alongside**
    /// the parent entry (`listen.evt`). Since `key_path` is a plain substring wildcard, both `"*"` and
    /// `"listen.*"` match the parent and the child, so one event synthesises two nodes `evt` and `evt.0` (the
    /// latter is a scalar, cannot get a `HandledBy` edge, and is pure noise).
    /// This predicate admits array entries only: it drops the expanded scalar leaves and incidentally also filters
    /// out framework-level empty tags like `app_init => []`.
    EntryArityGte(usize),
    /// The node's **FQN** contains the given substring (case-insensitive).
    ///
    /// Why it is needed: many conventions hold by **namespace position**, which a node's short name cannot reveal
    /// — the semantics of the controller method `detail` come from its FQN `app\api\controller\Goods::detail`,
    /// and the auto-route rule can only be selected by the `\controller\` segment.
    FqnMatches(String),
    /// The node name is **not** in the given list (case-insensitive).
    ///
    /// A convention's scope always has to exclude language / framework hooks: `__construct` / `initialize` also
    /// live in the controller namespace but are definitely not HTTP entries. The list comes from FKB; the kernel
    /// knows no concrete name.
    NameNotIn(Vec<String>),
    /// The node is **not yet claimed**: it has neither an in-edge of the given kind nor a pending link of that
    /// kind pointing at it (see the shape comparison in [`crate::GraphWorkspace::claimed_by`]).
    ///
    /// "An explicit declaration beats a convention inference": a method already written into `Route::get` /
    /// `Route::resource` must not be scooped up a second time by a directory convention — otherwise a project like
    /// sample_project, which registers routes exhaustively, sprouts thousands of duplicate endpoints. This is the same
    /// accounting principle as "better a missing edge than a wrong edge".
    NotClaimedBy(String),
    /// The node **already has an in-edge of the given kind**.
    ///
    /// Use this instead of a marker property whenever the fact you want is "somebody really linked to me":
    /// a synthesised node may be written by several parties (a contract bridge is written by both the front end
    /// and the back end), and any scalar property they all patch is decided by **whoever patches last** — so the
    /// property lies about the party set, while the edge is first-hand evidence.
    /// Example: `frontend.called` must be judged by a `CallsHttp` in-edge, not by `side = frontend`.
    HasIncoming(String),
}

/// A binding action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
pub enum Action {
    /// Tag an annotation.
    Annotate(AnnotateAction),
    /// Synthesise a node.
    Synthesize(SynthesizeAction),
    /// Build an edge only.
    Link(LinkAction),
    /// **Project one class of edges onto another layer**.
    Project(ProjectAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnnotateAction {
    /// The phase in which it is expected to run (Pre / Post). Skipped when the rule's phase differs from this.
    pub phase: Option<Phase>,
    pub channel: AnnotationChannel,
    pub target: AnnotateTarget,
    pub annotations: Vec<AnnotationSpec>,
    pub merge: MergeStrategy,
    /// Confidence decay when inheriting a capability from the scope chain.
    pub confidence_scale: Option<f32>,
    // `scope: Option<Vec<String>>` and `where: Vec<Predicate>` used to be declared here and were
    // never read: `exec_binding` gates on the *selector's* `where` (via `matches_*`) and there is no
    // scope chain to walk, so a rule declaring either parsed fine and did nothing. Removed so that
    // `deny_unknown_fields` turns such a rule into a load error instead of a silent no-op.
}

impl Default for AnnotateAction {
    fn default() -> Self {
        Self {
            phase: None,
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            target: AnnotateTarget::Matched,
            annotations: Vec::new(),
            merge: MergeStrategy::MaxByKind,
            confidence_scale: None,
        }
    }
}

/// Annotation target.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AnnotateTarget {
    /// The node the selector matched directly.
    Matched,
    /// Derived from a field of the match result (e.g. `array_values` -> resolved into a class).
    FromField {
        source: ValueSource,
        resolve: Option<ResolveAs>,
    },
    /// Reference a node synthesised earlier by this same rule.
    SynthesizedRef(String),
}

impl Default for AnnotateTarget {
    fn default() -> Self {
        AnnotateTarget::Matched
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnnotationSpec {
    pub kind: String,
    pub subkind: Option<SubkindSource>,
    pub severity: Option<String>,
    pub confidence: f32,
    pub evidence: Option<Value>,
    pub channel: Option<AnnotationChannel>,
}

impl Default for AnnotationSpec {
    fn default() -> Self {
        Self {
            kind: String::new(),
            subkind: None,
            severity: None,
            confidence: 1.0,
            evidence: None,
            channel: None,
        }
    }
}

/// Where a subkind comes from: a literal / an authoritative symbol table / a computed value / a fan_in grade.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SubkindSource {
    Literal(String),
    FromSymbolTable {
        table: String,
        field: String,
        #[serde(default)]
        of: Option<ValueSource>,
    },
    FromFanIn {
        thresholds: FanInThresholds,
    },
    Computed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FanInThresholds {
    pub high: u64,
    pub medium: u64,
    #[serde(default)]
    pub low_label: Option<String>,
    #[serde(default)]
    pub medium_label: Option<String>,
    #[serde(default)]
    pub high_label: Option<String>,
}

/// A synthesis action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynthesizeAction {
    /// Node kind (an open string, e.g. `Table` / `HttpContract` / `Event` / `Queue` / `Cache` / `Topic`).
    ///
    /// If `subtype` is given too, the **subtype is promoted to kind** (`kind = subtype`);
    /// `category` then still records the final kind (a first-class semantic node equals its kind).
    pub node: NodeKind,
    /// Subtype (`Event` / `Queue` / `Cache`…), optional; when given it becomes the final kind.
    pub subtype: Option<String>,
    pub identity: IdentitySpec,
    pub fields: Vec<FieldSpec>,
    pub link: Option<LinkSpec>,
    /// Declare that this node is **the same thing as the node another sub-project declares under the same
    /// identity** — expressed as an edge, because the two are two nodes.
    ///
    /// The canonical case is the contract bridge: a sub-project's node is owned by that sub-project (the identity
    /// scope carries it), so the front end's `POST /login` and the back end's `ANY /login` stay two nodes, and
    /// "the front end's call resolves to that route" becomes `HttpContract --ResolvesToContract--> HttpContract`.
    ///
    /// Which pair counts as "the same thing" is still knowledge (declared here); the kernel only knows "another
    /// sub-project, same identity value, and for contracts the method-agnostic `ANY` / `RULE` spelling".
    #[serde(default)]
    pub bridge: Option<BridgeSpec>,
    pub confidence: f32,
    /// `MergeBy(key)` — several data sources merge into different fields of one node rather than into several nodes.
    pub modifiers: Vec<String>,
    /// Alias registration (written into by_alias automatically after synthesis).
    pub alias: Option<AliasSpec>,
    /// **Expand one call into N semantic nodes** (table-driven).
    ///
    /// Typical case: the REST resource route `Route::resource('cms', Ctrl::class)` is really 7 contracts in one
    /// statement (index / create / save / read / edit / update / delete).
    /// The expansion table comes from **FKB** (the kernel has zero framework knowledge); the kernel only: runs the
    /// same `identity` / `fields` / `link` once per variant in the table, injects the variant's `method` / `entry`
    /// into the two sources `{ expand_method: true }` / `{ expand_entry: true }`, and appends `path_suffix` to the
    /// computed path (after the `Route::group` prefix).
    pub expand: Option<ExpandSpec>,
}

/// A cross-sub-project bridge: an edge from the synthesised node to the node another sub-project declares
/// under the same identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BridgeSpec {
    /// The edge kind produced (e.g. `ResolvesToContract`).
    pub kind: EdgeKind,
    /// Overrides the inherited confidence (`SynthesizeAction::confidence * Rule::confidence`).
    pub confidence: Option<f32>,
}

/// An expansion table: one call -> N semantic nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExpandSpec {
    pub variants: Vec<ExpandVariant>,
    /// Allowlist source: the argument array of a chained call with this name **on the same statement line**
    /// (`->only(['index','delete'])`). When given, only the actions in the list are synthesised.
    pub only: Option<String>,
    /// Denylist source: `->except(['read'])`. When given, those actions are removed from the action table.
    pub except: Option<String>,
}

impl Default for ExpandSpec {
    fn default() -> Self {
        Self { variants: Vec::new(), only: None, except: None }
    }
}

/// One row of the expansion table: one action (e.g. REST's `index`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct ExpandVariant {
    /// Action name (corresponding to the names written in `only` / `except`).
    pub name: String,
    /// HTTP method (read by `{ expand_method: true }`).
    pub method: Option<String>,
    /// Suffix appended to the path (e.g. `/create`, `/:id`).
    pub path_suffix: Option<String>,
    /// The handler's entry method name (read by `{ expand_entry: true }`).
    pub entry: Option<String>,
}

impl Default for SynthesizeAction {
    fn default() -> Self {
        Self {
            node: NodeKind(NodeKind::UNKNOWN.to_string()),
            subtype: None,
            identity: IdentitySpec::default(),
            fields: Vec::new(),
            link: None,
            bridge: None,
            confidence: 0.9,
            modifiers: Vec::new(),
            alias: None,
            expand: None,
        }
    }
}

/// The identity spec of a synthesised node.
///
/// **`identity` is the core of the whole Synthesize phase**: as long as three different rules compute the same
/// identity, their output merges idempotently into one node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdentitySpec {
    /// `Fqn` / `Named` / `ContractId`。
    pub kind: SynthesizedKind,
    /// A single-value identity (`Fqn` / `Named`).
    pub value: Option<ValueSource>,
    /// Where the HTTP method of a `ContractId` comes from.
    pub method: Option<ValueSource>,
    /// Where the path of a `ContractId` comes from.
    pub path: Option<ValueSource>,
    #[serde(default)]
    pub normalize: Vec<NormalizeStep>,
    /// Fallback source when the primary identity cannot be obtained (or `require_class` judges it not a class).
    ///
    /// Example: a queue topic prefers `arg:0` (the Job class in the argument) and falls back to `owner_class`
    /// (the class that made the call) when that fails. When both sources compute the same identity they merge
    /// idempotently into one node.
    #[serde(default)]
    pub value_fallback: Option<ValueSource>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::kinds::{Language, Phase};
    use serde_json::json;

    fn rule_with_languages(langs: Option<Vec<Language>>) -> Rule {
        Rule {
            id: "r".into(),
            phase: Phase::SYNTHESIZE.into(),
            selector: Selector::Call { callee: None, r#where: vec![] },
            binding: vec![],
            confidence: 0.9,
            languages: langs,
        }
    }

    // ------------------------------------------------------- Rule::applies_to (the only logic-bearing method)

    #[test]
    fn rule_applies_to_language_scoping() {
        let php = Language::new("php");
        let py = Language::new("python");
        let star = Language::new("*");

        // `None` inherits from the declaring FKB's language.
        let inherit = rule_with_languages(None);
        assert!(!inherit.applies_to(&php, &py), "fk=php, sub=py -> no match");
        assert!(inherit.applies_to(&php, &php), "fk=php, sub=php -> match");
        assert!(inherit.applies_to(&star, &py), "fk=* (universal) -> match any sub");

        // `Some(list)`: the FKB's own language is IGNORED — only the list decides.
        let scoped = rule_with_languages(Some(vec![php.clone()]));
        assert!(scoped.applies_to(&py, &php), "sub in list -> match even when fk differs");
        assert!(!scoped.applies_to(&php, &py), "sub not in list -> no match");

        // The `Language("*")` sentinel opts a rule into every sub-project language.
        let universal = rule_with_languages(Some(vec![star.clone()]));
        assert!(universal.applies_to(&py, &py), "list with `*` matches any sub");
    }

    // ------------------------------------------------------- Rule: confidence default + required fields + deny_unknown_fields

    #[test]
    fn rule_confidence_default_and_contract() {
        // Omitting `confidence` falls back to `default_conf()` (0.9).
        let r: Rule = serde_json::from_value(json!({
            "id": "r", "phase": "Synthesize", "selector": { "kind": "call" }, "binding": []
        }))
        .unwrap();
        assert_eq!(r.confidence, 0.9);
        assert!(r.languages.is_none());

        // A fully valid declaration with `languages` set.
        let r2: Rule = serde_json::from_value(json!({
            "id": "r", "phase": "Synthesize", "selector": { "kind": "call" },
            "binding": [], "languages": ["php"]
        }))
        .unwrap();
        assert!(r2.languages.is_some());

        // Required fields are required; unknown keys are rejected.
        assert!(serde_json::from_value::<Rule>(json!({ "phase": "Synthesize", "selector": { "kind": "call" }, "binding": [] })).is_err(), "missing id");
        assert!(serde_json::from_value::<Rule>(json!({ "id": "r", "phase": "Synthesize", "binding": [] })).is_err(), "missing selector");
        assert!(serde_json::from_value::<Rule>(json!({
            "id": "r", "phase": "Synthesize", "selector": { "kind": "call" }, "binding": [], "bogus": 1
        }))
        .is_err());
    }

    // ------------------------------------------------------- Selector: internally-tagged, snake_case, `where` key

    #[test]
    fn selector_tags_and_where_key() {
        // `r#where` serialises as `where` (raw identifier). Option/Vec fields render as explicit null, so we
        // check the `kind` tag and round-trip rather than byte-exact shape.
        let v = serde_json::to_value(Selector::Call { callee: None, r#where: vec![] }).unwrap();
        assert_eq!(v["kind"], json!("call"));
        assert!(serde_json::from_value::<Selector>(json!({ "kind": "call", "where": [] })).is_ok());

        for (s, tag) in [
            (Selector::Inheritance { base: None, with_property: None }, "inheritance"),
            (Selector::ConfigEntry { file: None, key_path: None, r#where: vec![] }, "config_entry"),
            (Selector::Node { node_kind: None, r#where: vec![] }, "node"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap()["kind"], json!(tag));
        }

        assert!(serde_json::from_value::<Selector>(json!({ "kind": "call", "bogus": 1 })).is_err());
        assert!(serde_json::from_value::<Selector>(json!({ "kind": "bogus" })).is_err());
    }

    // ------------------------------------------------------- Predicate: externally-tagged, snake_case, deny_unknown_fields

    #[test]
    fn predicate_tags_and_round_trip() {
        // Predicates that carry no `ValueSource` serialise compactly (Option fields are not nested), so the
        // exact tag+shape assertion holds; each also round-trips.
        let cases: Vec<(Predicate, serde_json::Value)> = vec![
            (Predicate::HasProperty("x".into()), json!({ "has_property": "x" })),
            (Predicate::NameMatches("c".into()), json!({ "name_matches": "c" })),
            (Predicate::FqnMatches("ns".into()), json!({ "fqn_matches": "ns" })),
            (Predicate::HasMissing(true), json!({ "has_missing": true })),
            (Predicate::FanInGte(5), json!({ "fan_in_gte": 5 })),
            (Predicate::ArgCount(2), json!({ "arg_count": 2 })),
            (Predicate::EntryArityGte(1), json!({ "entry_arity_gte": 1 })),
            (Predicate::NameNotIn(vec!["a".into()]), json!({ "name_not_in": ["a"] })),
            (Predicate::NoneOfCapability(vec!["c".into()]), json!({ "none_of_capability": ["c"] })),
            (Predicate::NotClaimedBy("e".into()), json!({ "not_claimed_by": "e" })),
            (Predicate::HasIncoming("k".into()), json!({ "has_incoming": "k" })),
            (Predicate::ColumnsMatch { table: "t".into(), names: vec!["n".into()] }, json!({ "columns_match": { "table": "t", "names": ["n"] } })),
            (Predicate::HasAnnotation { kind: "k".into() }, json!({ "has_annotation": { "kind": "k" } })),
            (Predicate::PropertyIs { name: "n".into(), value: "v".into() }, json!({ "property_is": { "name": "n", "value": "v" } })),
            (Predicate::ArgStartsWith { arg: 0, prefix: "p".into() }, json!({ "arg_starts_with": { "arg": 0, "prefix": "p" } })),
        ];
        for (p, expected) in cases {
            assert_eq!(serde_json::to_value(&p).unwrap(), expected, "serialised tag/shape");
            assert!(serde_json::from_value::<Predicate>(expected).is_ok(), "round-trip");
        }

        // `InSymbolTable` carries a `ValueSource`, which serde renders with explicit `null`s for every Option
        // field — assert the tag is present and the whole thing round-trips (rather than byte-exact shape).
        let in_sym = Predicate::InSymbolTable {
            table: "t".into(),
            key_of: ValueSource { arg: Some(0), ..Default::default() },
        };
        let v = serde_json::to_value(&in_sym).unwrap();
        assert!(v.get("in_symbol_table").is_some());
        assert!(serde_json::from_value::<Predicate>(v).is_ok());

        // `deny_unknown_fields` on a struct variant rejects stray keys.
        assert!(serde_json::from_value::<Predicate>(json!({ "in_symbol_table": { "table": "t", "key_of": { "arg": 0 }, "bogus": 1 } })).is_err());
    }

    // ------------------------------------------------------- Action: PascalCase, externally-tagged

    #[test]
    fn action_pascal_case_tag() {
        let a = serde_json::to_value(Action::Annotate(AnnotateAction::default())).unwrap();
        assert!(a.get("Annotate").is_some() && a.get("Synthesize").is_none());
        let s = serde_json::to_value(Action::Synthesize(SynthesizeAction::default())).unwrap();
        assert!(s.get("Synthesize").is_some());
        let l = serde_json::to_value(Action::Link(LinkAction::default())).unwrap();
        assert!(l.get("Link").is_some());
        let p = serde_json::to_value(Action::Project(ProjectAction::default())).unwrap();
        assert!(p.get("Project").is_some());
        // PascalCase variant tags are authorable directly.
        assert!(matches!(serde_json::from_value::<Action>(json!({ "Annotate": {} })).unwrap(), Action::Annotate(_)));
        assert!(matches!(serde_json::from_value::<Action>(json!({ "Synthesize": {} })).unwrap(), Action::Synthesize(_)));
    }

    // ------------------------------------------------------- AnnotateAction: manual Default + removed-field contract

    #[test]
    fn annotate_action_default_and_removed_fields_rejected() {
        let a: AnnotateAction = serde_json::from_value(json!({})).unwrap();
        assert_eq!(a.channel.0, AnnotationChannel::FKB_MARK, "default channel is FkbMark");
        assert!(matches!(a.target, AnnotateTarget::Matched));
        assert!(a.confidence_scale.is_none());
        assert!(a.phase.is_none());
        assert!(a.annotations.is_empty());

        // The fields that were removed because they were never read must now be a load error, not a silent no-op.
        assert!(serde_json::from_value::<AnnotateAction>(json!({ "scope": ["x"] })).is_err());
        assert!(serde_json::from_value::<AnnotateAction>(json!({ "where": [] })).is_err());
        assert!(serde_json::from_value::<AnnotateAction>(json!({ "bogus": 1 })).is_err());
    }

    // ------------------------------------------------------- other action/selector sub-types

    #[test]
    fn annotate_target_and_subkind_source_tags() {
        assert_eq!(serde_json::to_value(AnnotateTarget::Matched).unwrap(), json!("matched"));
        assert_eq!(serde_json::to_value(AnnotateTarget::SynthesizedRef("x".into())).unwrap(), json!({ "synthesized_ref": "x" }));
        assert!(serde_json::from_value::<AnnotateTarget>(json!({ "from_field": { "source": { "arg": 0 } } })).is_ok());
        assert!(matches!(AnnotateTarget::default(), AnnotateTarget::Matched));

        assert_eq!(serde_json::to_value(SubkindSource::Literal("x".into())).unwrap(), json!({ "literal": "x" }));
        assert_eq!(serde_json::to_value(SubkindSource::Computed("x".into())).unwrap(), json!({ "computed": "x" }));
        assert!(serde_json::from_value::<SubkindSource>(json!({ "from_symbol_table": { "table": "t", "field": "f" } })).is_ok());
        assert!(serde_json::from_value::<SubkindSource>(json!({ "from_fan_in": { "thresholds": { "high": 1, "medium": 1 } } })).is_ok());
        // `deny_unknown_fields` on the enum's struct variant.
        assert!(serde_json::from_value::<SubkindSource>(json!({ "literal": "x", "bogus": 1 })).is_err());
    }

    #[test]
    fn spec_defaults_and_unknown_fields() {
        // AnnotationSpec: confidence defaults to 1.0; unknown field rejected.
        let aspec: AnnotationSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(aspec.confidence, 1.0);
        assert_eq!(aspec.kind, "");
        assert!(serde_json::from_value::<AnnotationSpec>(json!({ "bogus": 1 })).is_err());

        // SynthesizeAction: confidence defaults to 0.9; unknown field rejected.
        let syn: SynthesizeAction = serde_json::from_value(json!({})).unwrap();
        assert_eq!(syn.confidence, 0.9);
        assert!(serde_json::from_value::<SynthesizeAction>(json!({ "bogus": 1 })).is_err());

        // BridgeSpec / ExpandSpec / IdentitySpec / ExpandVariant accept `{}` (struct default) and reject extras.
        assert!(serde_json::from_value::<BridgeSpec>(json!({})).is_ok());
        assert!(serde_json::from_value::<BridgeSpec>(json!({ "bogus": 1 })).is_err());
        assert!(serde_json::from_value::<ExpandSpec>(json!({})).is_ok());
        assert!(serde_json::from_value::<ExpandSpec>(json!({ "bogus": 1 })).is_err());
        assert!(serde_json::from_value::<IdentitySpec>(json!({})).is_ok());
        assert!(serde_json::from_value::<IdentitySpec>(json!({ "bogus": 1 })).is_err());
        let ev: ExpandVariant = serde_json::from_value(json!({})).unwrap();
        assert_eq!(ev.name, "");
        assert!(serde_json::from_value::<ExpandVariant>(json!({ "bogus": 1 })).is_err());

        // FanInThresholds: `high`/`medium` required.
        assert!(serde_json::from_value::<FanInThresholds>(json!({})).is_err(), "missing high/medium");
        assert!(serde_json::from_value::<FanInThresholds>(json!({ "high": 10, "medium": 3, "bogus": 1 })).is_err());
        assert!(serde_json::from_value::<FanInThresholds>(json!({ "high": 10, "medium": 3 })).is_ok());
    }
}

