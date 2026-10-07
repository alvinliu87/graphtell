use serde::{Deserialize, Serialize};

use crate::model::graph::Span;
use crate::model::kinds::EdgeKind;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct FieldSpec {
    pub name: String,
    pub value: Option<ValueSource>,
    /// Accumulating merge: `{ key: locale, value: text }`.
    pub accumulate: Option<AccumulateSpec>,
    /// Supplement fields from the authoritative symbol table.
    pub from_symbol_table: Option<SymbolFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccumulateSpec {
    pub key: ValueSource,
    pub value: ValueSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolFieldSpec {
    pub table: String,
    pub field: String,
    #[serde(default)]
    pub of: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct LinkSpec {
    pub kind: EdgeKind,
    /// Source of the edge's other end (e.g. a handler string).

    pub to: Option<ValueSource>,
    /// The target **method name** (optional). When given, prefer connecting to `Class::method`.
    ///
    /// Two uses: (1) the method part of an array-style handler (item 1 of `[Ctrl::class, 'method']`); (2) letting
    /// FKB decide the "consumer entry method" instead of the kernel's hard-coded list of
    /// `handle`/`fire`/`doJob`/`__invoke`/`run`.
    #[serde(default)]
    pub to_method: Option<ValueSource>,
    /// Fallback source when `to` yields no target (e.g. falling back to `receiver_class` when a queue consumer's `arg:0` cannot be resolved).
    #[serde(default)]
    pub to_fallback: Option<ValueSource>,
    /// Direction: incoming (the source points at the new node) / outgoing (the new node points at the source) / to_target.
    pub direction: Direction,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// The matched caller ——> the newly synthesised node.
    #[default]
    Incoming,
    /// The newly synthesised node ——> the matched caller.
    Outgoing,
    /// The newly synthesised node ——> the target resolved from `to`.
    ToTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct AliasSpec {
    pub namespace: String,
    pub key: ValueSource,
    pub qualifier: Option<ValueSource>,
}

/// A value source (structured, for convenient YAML authoring).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ValueSource {
    /// The n-th argument.
    pub arg: Option<usize>,
    /// When the argument is an **array**, take the n-th item by index.
    ///
    /// For array-style handlers — Laravel's main form `Route::get('/x', [Ctrl::class, 'method'])` has the class
    /// name in `arg1[0]` and the method name in `arg1[1]`.
    pub element: Option<usize>,
    /// Take a field when the argument is an object literal (e.g. `uni.request({url:..})`).
    pub field: Option<String>,
    /// Read a class property (e.g. a Model's `$table`).
    pub property: Option<String>,
    /// Take the current node itself (class short name / FQN).
    #[serde(rename = "self")]
    pub self_value: Option<bool>,
    /// Skip the first n segments of key_path and take what remains (joined with dots).
    pub path_segment: Option<usize>,
    /// The call site's method name (e.g. the `post` of `Route::post`).
    pub method_name: Option<bool>,
    /// The config entry's own value.
    pub entry_value: Option<bool>,
    /// Take all values of an array (one-to-many).
    pub array_values: Option<bool>,
    /// Take the array length.
    pub array_length: Option<bool>,
    /// Take the key_path of a config-file entry.
    pub key_path: Option<bool>,
    /// Take the file-name stem.
    pub file_stem: Option<bool>,
    /// Take the current locale (when loading i18n).
    pub locale: Option<bool>,
    /// Take "the class that made the call": the class FQN after stripping the trailing `::method` from
    /// `owner_fqn`.
    ///
    /// Generic semantics: for when the identity is "the calling class" rather than some argument (e.g. in the
    /// self-enqueue pattern the queue's Job / topic is the class that produced it, typically `QueueTrait::dispatch`
    /// setting the consumer to the calling class via `->job(__CLASS__)`). This is a framework-agnostic extraction
    /// capability.
    pub owner_class: Option<bool>,
    /// Take the **member name** at the end of `owner_fqn` (method / field), complementing `owner_class`.
    ///
    /// Typical use: the call site of a Java method-level annotation (`@GetMapping`) has `owner_fqn` = the method FQN
    /// `com.example.Ctrl.list`, and `owner_member` extracts `list` so that a link's `to_method` connects the
    /// `HandledBy` edge precisely to the **handler method** node (rather than the controller class), letting a
    /// perspective keep drilling down along the method's call chain. For a class-level annotation `owner_fqn` is
    /// already a class FQN, so this yields the class short name; when no method node is found,
    /// `find_target_node` falls back to the class node, which is semantically safe.
    pub owner_member: Option<bool>,
    /// Take the call site's receiver class: resolve `receiver` into an FQN via import aliases.
    ///
    /// Unlike `owner_class` (the class the call sits in), this is "the receiver class of the callee", e.g. in
    /// `QueueThink::push()` resolving `QueueThink` through an alias into `think\facade\Queue`.
    pub receiver_class: Option<bool>,
    /// Take the "primary domain type" the call site is about (`CallSiteFact.entity`), e.g. the event type
    /// `OrderPlacedEvent`. Used to merge the publisher and the subscriber of "the same event type" onto one
    /// `Event` node (rather than naming each after its method). When it cannot be obtained (the parser did not
    /// recognise it), return `None` overall and let `value_fallback` (e.g. `owner_member`) cover it.
    pub entity: Option<bool>,
    /// With `resolve: class_const`, if the resolved result does not exist as a class node in the codebase, return
    /// `None` overall (instead of treating a variable name / literal as a class). For fallbacks like "prefer the
    /// Job class in the argument, otherwise fall back to `owner_class`", so strings like `$action` cannot pollute
    /// the semantic identity.
    pub require_class: Option<bool>,
    /// Accept **literals only** (strings / scalars); reject variables and expression text.
    ///
    /// `arg` evaluates a variable / concatenated expression into `FactValue::Unknown(Some(verbatim))` (see
    /// `gt-adapter-parser::php::value`), so trusting it directly would take source text like `$name` or
    /// `self::X . $y` as an identity and conjure garbage semantic nodes. Symmetric with `require_class`: if the
    /// test fails, return `None` overall and let `value_fallback` cover it.
    pub require_literal: Option<bool>,
    /// Reject the value when, **after** normalisation, it contains any of these substrings; the whole source
    /// then counts as unavailable (so `value_fallback` / `default` can cover it).
    ///
    /// The point is scope, not syntax: a value can be a perfectly good literal and still name something outside
    /// this repository. A front end's `fetch('https://api.paypal.com/v2/checkout/orders')` is a literal URL, but
    /// it names a **third-party** endpoint — synthesising an `HttpContract` for it invents an endpoint no route
    /// in the project declares, and those invented endpoints then compete with the real ones in every list and
    /// every recall. Which substrings mean "out of scope" is declared by the FKB (`reject_contains: ["://"]`),
    /// the kernel only performs the test.
    #[serde(default)]
    pub reject_contains: Option<Vec<String>>,
    /// A literal.
    pub literal: Option<String>,
    /// A nested source: `{ source: { arg: 1 }, field: 'url' }`.
    pub source: Option<Box<ValueSource>>,
    /// A transformation (e.g. `class_to_topic`, `snake_plural`).
    pub transform: Option<TransformSpec>,
    /// A normalisation chain.
    pub normalize: Option<Vec<NormalizeStep>>,
    /// Resolution method.
    pub resolve: Option<ResolveAs>,
    /// Default value when it cannot be obtained.
    pub default: Option<String>,
    /// Multi-segment joining: `{ path: [{file_stem:true},{key_path:true}], join: '.' }`.
    pub path: Option<Vec<ValueSource>>,
    pub join: Option<String>,
    /// Take the HTTP method of the **current expansion variant** (`expand.variants[].method`).
    ///
    /// Only has a value together with `Synthesize.expand`: when one call expands into N semantic nodes, each
    /// variant has its own method / path suffix / entry method (as in a REST resource route).
    pub expand_method: Option<bool>,
    /// Take the entry method name of the **current expansion variant** (`expand.variants[].entry`), so that
    /// `link.to_method` connects the edge precisely to "the method handling that action".
    pub expand_entry: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransformSpec {
    pub snake_plural: Option<bool>,
    pub snake: Option<bool>,
    pub strip_namespace: Option<bool>,
    pub class_to_topic: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAs {
    /// `Foo::class` -> a fully qualified class name -> look up by_name.
    ClassConst,
    /// Assemble an FQN from a string reference to a `class::method` / function (e.g. `'Login/appleLogin'`).
    /// Generic string → callable resolver: route handlers, queue string-jobs, `invokeAction`, etc. — not route-only.
    MethodRef,
    /// Look up the by_alias index.
    ByAlias,
    /// Use it directly as a name.
    AsIs,
}

/// A normalisation step (the key to idempotent identity merging).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NormalizeStep {
    StripPrefix(Vec<String>),
    Lower,
    Upper,
    /// Guarantee a leading `/`.
    LeadingSlash,
    /// Plural to singular.
    Singularize,
    /// Class name to `snake_case` plural (the Model table-name convention).
    SnakePlural,
    /// Strip the namespace, keep only the last segment.
    StripNamespace,
    /// Take the last segment of a **dot-separated** path: `app.tasks.send_email` -> `send_email`.
    ///
    /// It differs from [`Self::StripNamespace`] by only one separator, `.`: Python / Java namespaces are dot
    /// separated, while `StripNamespace` deliberately does **not** split on `.` (otherwise it would split Java
    /// auto-route package names too and change existing behaviour). Hence a separate **additive** step, aimed at
    /// scenarios like "the long and short names of one semantic entity must merge" — e.g. a Celery task's
    /// registrar only has the short name while its dispatcher restored a fully qualified name via `import`, and
    /// without normalisation they split into two nodes.
    ShortName,
    /// Path-parameter segment normalisation: every segment starting with `:` folds into `:*`.
    ///
    /// A key step of the contract bridge: the backend route writes `invoice/detail/:id` while the frontend's
    /// concatenated URL `'invoice/detail/' + id` normalises to `invoice/detail/:param` — different parameter
    /// names but the **same shape**, and HTTP matching only ever looks at the shape. Without folding, the two
    /// never merge onto one node and the route perspective "cannot see the frontend".
    ParamWildcard,
    /// Drop the query string starting at `?` (page-navigation URLs often carry `?id=1`, but a route identity only
    /// looks at the path): `uni.navigateTo({ url: '/pages/detail?id=1' })` and the `/pages/detail` in `pages.json`
    /// converge onto the same `Page` node.
    StripQuery,
    Trim,
    Replace { from: String, to: String },
}

/// An edge-only action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct LinkAction {
    pub kind: EdgeKind,
    pub from: Option<ValueSource>,
    pub to: Option<ValueSource>,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

/// An edge-projection action: project **one class of edges** from the layer they live in onto another layer.
///
/// Walk every `along` out-edge of the matched node; the start walks along the `from` edge-kind chain and the end
/// along the `to` chain, and a `kind` edge is built between the two landing points. It is **one-to-many**: an
/// entity with several `@ManyToOne` produces several foreign-key edges (something `Link` cannot do — its two ends
/// can each take only one name).
///
/// Why the kernel has to provide this ability: "class -> the table it maps to" is essentially **walking one hop
/// along `MapsTo`**, while `ValueSource` only understands names (`self_value` / `property`) and cannot reach "the
/// far end of an edge". Which edge to walk is still declared entirely by FKB — the kernel still knows nothing
/// about TypeORM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
#[serde(deny_unknown_fields)]
pub struct ProjectAction {
    /// The edge kind produced.
    pub kind: EdgeKind,
    /// Walk every out-edge **of this kind** on the matched node (no edge of that kind -> no output, so unrelated nodes are skipped naturally).
    pub along: EdgeKind,

    /// Walk from the **edge's start** along this edge-kind chain to the landing point; empty means the landing point is the start itself.
    #[serde(default)]
    pub from: Vec<String>,
    /// Walk from the **edge's end** along this edge-kind chain to the landing point; empty means the landing point is the end itself.
    #[serde(default)]
    pub to: Vec<String>,
    pub confidence: Option<f32>,
}

/// Levels of the P7 resolution funnel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveTier {
    /// L1 literal FQN, e.g. `app()->make(StoreOrderServices::class)`.
    Exact = 1,
    /// L2 container registry (the bindings in `provider.php`).
    Registry = 2,
    /// L3 alias index (Facade / event name / getter).
    Alias = 3,
    /// L4 convention (namespace concatenation, deriving a table name from a class name).
    Convention = 4,
    /// L5 constant propagation.
    ConstProp = 5,
    /// L6 intersection with a finite universe (the 203 tables of the schema).
    Intersection = 6,
    /// L7 completely unknown.
    Unknown = 7,
}

impl ResolveTier {
    /// The base confidence of that level.
    pub fn base_confidence(self) -> f32 {
        match self {
            Self::Exact => 1.0,
            Self::Registry => 0.95,
            Self::Alias => 0.85,
            Self::Convention => 0.8,
            Self::ConstProp => 0.6,
            Self::Intersection => 0.7,
            Self::Unknown => 0.3,
        }
    }
}

/// The product of one dynamic resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub tier: ResolveTier,
    /// Candidate nodes; empty means unresolved.
    pub candidates: Vec<crate::model::ids::NodeId>,
    pub confidence: f32,
    pub evidence: String,
}

impl Resolution {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            tier: ResolveTier::Unknown,
            candidates: Vec::new(),
            confidence: ResolveTier::Unknown.base_confidence(),
            evidence: reason.into(),
        }
    }
    pub fn resolved(tier: ResolveTier, candidate: crate::model::ids::NodeId, evidence: impl Into<String>) -> Self {
        Self { tier, candidates: vec![candidate], confidence: tier.base_confidence(), evidence: evidence.into() }
    }
}

/// Call-site context (used by selector matching).
#[derive(Debug, Clone)]
pub struct CallContext {
    pub owner_fqn: String,
    pub owner_node: Option<crate::model::ids::NodeId>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<crate::model::syntax::FactValue>,
    pub span: Span,
    pub sub_project: Option<crate::model::ids::SubProjectId>,
    pub file_path: String,
}

/// A P7 dynamic-resolution declaration.
///
/// Letting FKB decide "which calls need dynamic resolution" too, rather than hard-coding it in the kernel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolverSpec {
    pub id: String,
    /// Matching pattern, e.g. `app()->make|app|make`.
    #[serde(default)]
    pub call: Option<String>,
    pub strategy: ResolveStrategy,
    /// Starting resolution level (containers default to Registry).
    #[serde(default)]
    pub from_tier: Option<ResolveTier>,
}

/// A concrete resolution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStrategy {
    /// `app()->make(X)` / `app('x')`: L1 literal -> L2 registry -> L4 convention -> L6 intersection.
    Container,
    /// `event('x')`: look up the L3 alias index.
    Event,
    /// `Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`:
    /// arg0 resolves to an event node (L3 alias), then a `HandledBy` edge goes from the event node to the arg1 listener class.
    EventListen,
    /// `think\facade\Cache::get()`: look up the L3 FacadeMap.
    Facade,
    /// `$order->status_text`: a composite-key accessor alias.
    Accessor,
    /// `Route::post('p','Login/appleLogin')`: handler-pattern resolution.
    Handler,
    /// `$services->appAuth()`: resolve an instance method call by variable type.
    ///
    /// Type sources: method parameter type hints (the ThinkPHP controller DI convention) and constructor property
    /// injection (`__construct(T $x){ $this->p = $x; }`), recorded by P2 and consumed by this strategy.
    VariableType,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ids::NodeId;
    use serde_json::json;

    // ------------------------------------------------------- ResolveTier: base confidence + funnel ordering

    #[test]
    fn resolve_tier_base_confidence_each_level() {
        assert_eq!(ResolveTier::Exact.base_confidence(), 1.0);
        assert_eq!(ResolveTier::Registry.base_confidence(), 0.95);
        assert_eq!(ResolveTier::Alias.base_confidence(), 0.85);
        assert_eq!(ResolveTier::Convention.base_confidence(), 0.8);
        assert_eq!(ResolveTier::ConstProp.base_confidence(), 0.6);
        assert_eq!(ResolveTier::Intersection.base_confidence(), 0.7);
        assert_eq!(ResolveTier::Unknown.base_confidence(), 0.3);
    }

    /// The funnel levels are explicitly numbered 1..=7; ordering (used by the resolver to pick the highest
    /// tier that resolved) must follow that numbering.
    #[test]
    fn resolve_tier_discriminant_and_ordering() {
        assert_eq!(ResolveTier::Exact as u8, 1);
        assert_eq!(ResolveTier::Registry as u8, 2);
        assert_eq!(ResolveTier::Alias as u8, 3);
        assert_eq!(ResolveTier::Convention as u8, 4);
        assert_eq!(ResolveTier::ConstProp as u8, 5);
        assert_eq!(ResolveTier::Intersection as u8, 6);
        assert_eq!(ResolveTier::Unknown as u8, 7);

        assert!(ResolveTier::Exact < ResolveTier::Registry);
        assert!(ResolveTier::Registry < ResolveTier::Alias);
        assert!(ResolveTier::Alias < ResolveTier::Convention);
        assert!(ResolveTier::Convention < ResolveTier::ConstProp);
        assert!(ResolveTier::ConstProp < ResolveTier::Intersection);
        assert!(ResolveTier::Intersection < ResolveTier::Unknown);
    }

    // ------------------------------------------------------- Resolution constructors

    #[test]
    fn resolution_unknown_and_resolved() {
        let u = Resolution::unknown("cannot resolve");
        assert_eq!(u.tier, ResolveTier::Unknown);
        assert!(u.candidates.is_empty());
        assert_eq!(u.confidence, 0.3);
        assert_eq!(u.evidence, "cannot resolve");

        let r = Resolution::resolved(ResolveTier::Exact, NodeId::new(7), "literal FQN");
        assert_eq!(r.tier, ResolveTier::Exact);
        assert_eq!(r.candidates, vec![NodeId::new(7)]);
        assert_eq!(r.confidence, 1.0);
        assert_eq!(r.evidence, "literal FQN");
    }

    // ------------------------------------------------------- snake_case enum tags (positive round-trips)

    #[test]
    fn direction_snake_case_and_default() {
        assert_eq!(serde_json::to_value(Direction::Incoming).unwrap(), json!("incoming"));
        assert_eq!(serde_json::to_value(Direction::Outgoing).unwrap(), json!("outgoing"));
        assert_eq!(serde_json::to_value(Direction::ToTarget).unwrap(), json!("to_target"));
        assert_eq!(Direction::default(), Direction::Incoming);
        assert_eq!(serde_json::from_value::<Direction>(json!("to_target")).unwrap(), Direction::ToTarget);
    }

    #[test]
    fn resolve_as_snake_case() {
        for (v, tag) in [
            (ResolveAs::ClassConst, "class_const"),
            (ResolveAs::AsIs, "as_is"),
            (ResolveAs::ByAlias, "by_alias"),
            (ResolveAs::MethodRef, "method_ref"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(tag));
        }
        assert_eq!(serde_json::from_value::<ResolveAs>(json!("method_ref")).unwrap(), ResolveAs::MethodRef);
    }

    #[test]
    fn resolve_strategy_snake_case() {
        for (v, tag) in [
            (ResolveStrategy::Container, "container"),
            (ResolveStrategy::Event, "event"),
            (ResolveStrategy::EventListen, "event_listen"),
            (ResolveStrategy::Facade, "facade"),
            (ResolveStrategy::Accessor, "accessor"),
            (ResolveStrategy::Handler, "handler"),
            (ResolveStrategy::VariableType, "variable_type"),
        ] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(tag));
        }
        assert_eq!(
            serde_json::from_value::<ResolveStrategy>(json!("variable_type")).unwrap(),
            ResolveStrategy::VariableType
        );
    }

    #[test]
    fn normalize_step_tags_and_round_trip() {
        // `NormalizeStep` is **externally tagged** (no `tag`), so a unit variant serialises to a bare string,
        // a newtype variant to `{"tag": payload}`, and a struct variant to `{"tag": {...}}`.
        assert_eq!(serde_json::to_value(NormalizeStep::Lower).unwrap(), json!("lower"));
        assert_eq!(
            serde_json::to_value(NormalizeStep::StripPrefix(vec!["a".into(), "b".into()])).unwrap(),
            json!({ "strip_prefix": ["a", "b"] })
        );
        assert_eq!(
            serde_json::to_value(NormalizeStep::Replace { from: "x".into(), to: "y".into() }).unwrap(),
            json!({ "replace": { "from": "x", "to": "y" } })
        );
        // Every variant round-trips (exercises the newtype `StripPrefix` payload and the `Replace` struct).
        let steps = vec![
            NormalizeStep::Lower,
            NormalizeStep::Upper,
            NormalizeStep::LeadingSlash,
            NormalizeStep::Singularize,
            NormalizeStep::SnakePlural,
            NormalizeStep::StripNamespace,
            NormalizeStep::ShortName,
            NormalizeStep::ParamWildcard,
            NormalizeStep::StripQuery,
            NormalizeStep::Trim,
            NormalizeStep::StripPrefix(vec!["a".into(), "b".into()]),
            NormalizeStep::Replace { from: "x".into(), to: "y".into() },
        ];
        for s in steps {
            assert_eq!(serde_json::from_value::<NormalizeStep>(serde_json::to_value(&s).unwrap()).unwrap(), s);
        }
        // Unknown variant -> error.
        assert!(serde_json::from_value::<NormalizeStep>(json!("nope")).is_err());
        // `Replace`'s `deny_unknown_fields` rejects extra keys inside the payload object.
        assert!(serde_json::from_value::<NormalizeStep>(json!({ "replace": { "from": "x", "to": "y", "bogus": 1 } })).is_err());
    }

    // ------------------------------------------------------- ValueSource: `self` rename + defaults + deny_unknown_fields

    #[test]
    fn value_source_self_rename_and_defaults() {
        // `self_value` is serialised as the YAML/JSON key `self` (reserved word).
        let vs = ValueSource { self_value: Some(true), arg: Some(0), ..Default::default() };
        let v = serde_json::to_value(&vs).unwrap();
        assert_eq!(v["self"], json!(true));
        assert!(v.get("self_value").is_none(), "serialised under `self`, not `self_value`");
        assert_eq!(v["arg"], json!(0));

        let back: ValueSource = serde_json::from_value(json!({ "self": true })).unwrap();
        assert_eq!(back.self_value, Some(true));
        assert!(back.arg.is_none());

        assert!(ValueSource::default().arg.is_none() && ValueSource::default().self_value.is_none());
    }

    #[test]
    fn value_source_rejects_unknown_field() {
        assert!(serde_json::from_value::<ValueSource>(json!({ "bogus": 1 })).is_err());
    }

    // ------------------------------------------------------- required fields + deny_unknown_fields on each struct

    /// Structs with `#[serde(deny_unknown_fields)]` reject unknown keys, and those with non-`Option` required
    /// fields reject a missing required field.
    #[test]
    fn struct_default_and_unknown_fields() {
        // Structs WITHOUT a struct-level default: required fields must be present, unknown keys rejected.
        assert!(serde_json::from_value::<AccumulateSpec>(json!({})).is_err(), "AccumulateSpec requires key+value");
        assert!(serde_json::from_value::<AccumulateSpec>(json!({
            "key": { "arg": 0 }, "value": { "arg": 1 }, "bogus": 1
        }))
        .is_err());
        assert!(serde_json::from_value::<SymbolFieldSpec>(json!({})).is_err(), "SymbolFieldSpec requires table+field");
        assert!(serde_json::from_value::<SymbolFieldSpec>(json!({
            "table": "t", "field": "f", "bogus": 1
        }))
        .is_err());

        // ResolverSpec: `id` + `strategy` required; `call`/`from_tier` optional; unknown key rejected.
        assert!(serde_json::from_value::<ResolverSpec>(json!({ "strategy": "handler" })).is_err(), "missing id");
        assert!(serde_json::from_value::<ResolverSpec>(json!({ "id": "r" })).is_err(), "missing strategy");
        assert!(serde_json::from_value::<ResolverSpec>(json!({
            "id": "r", "strategy": "handler", "call": "x", "bogus": 1
        }))
        .is_err());
        let ok: ResolverSpec =
            serde_json::from_value(json!({ "id": "r", "strategy": "handler" })).unwrap();
        assert!(ok.call.is_none() && ok.from_tier.is_none());

        // Structs WITH a struct-level default: `{}` is valid (defaults applied) and unknown keys are rejected.
        let fs: FieldSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(fs.name, "", "name defaults to empty string via struct Default");
        assert!(serde_json::from_value::<FieldSpec>(json!({ "name": "x", "bogus": 1 })).is_err());

        let ls: LinkSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(ls.direction, Direction::Incoming, "direction defaults to Incoming");
        assert!(serde_json::from_value::<LinkSpec>(json!({
            "kind": "calls", "direction": "incoming", "bogus": 1
        }))
        .is_err());

        let al: AliasSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(al.namespace, "");
        assert!(al.qualifier.is_none());
        assert!(serde_json::from_value::<AliasSpec>(json!({
            "namespace": "n", "key": { "arg": 0 }, "bogus": 1
        }))
        .is_err());

        assert!(serde_json::from_value::<LinkAction>(json!({})).is_ok(), "LinkAction has struct default");
        assert!(serde_json::from_value::<LinkAction>(json!({ "kind": "calls", "bogus": 1 })).is_err());

        assert!(serde_json::from_value::<ProjectAction>(json!({})).is_ok(), "ProjectAction has struct default");
        assert!(serde_json::from_value::<ProjectAction>(json!({
            "kind": "maps_to", "along": "maps_to", "bogus": 1
        }))
        .is_err());
    }
}


