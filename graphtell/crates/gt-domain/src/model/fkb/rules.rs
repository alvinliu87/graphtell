use super::*;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::graph::MergeStrategy;
use crate::model::kinds::{AnnotationChannel, Language, NodeKind, Phase, SynthesizedKind};

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
    /// CRMEB, which registers routes exhaustively, sprouts thousands of duplicate endpoints. This is the same
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
    /// Scope: `[RouteSelf, EnclosingGroup, Global]`.
    pub scope: Option<Vec<String>>,
    pub r#where: Vec<Predicate>,
    /// Confidence decay when inheriting a capability from the scope chain.
    pub confidence_scale: Option<f32>,
}

impl Default for AnnotateAction {
    fn default() -> Self {
        Self {
            phase: None,
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            target: AnnotateTarget::Matched,
            annotations: Vec::new(),
            merge: MergeStrategy::MaxByKind,
            scope: None,
            r#where: Vec::new(),
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
