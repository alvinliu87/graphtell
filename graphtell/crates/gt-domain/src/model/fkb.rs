//! Domain model of the Framework Knowledge Base (FKB).
//!
//! FKB is the serialisable form of "preset framework knowledge": ThinkPHP, Uni-app, Laravel, CRMEB…
//! one YAML per framework, describing
//! * how to **recognise** the framework ([`Detector`])
//! * how to **resolve its root** ([`RootRule`], e.g. `autoload.psr-4` in `composer.json`)
//! * which **authoritative symbol tables** P3 should load ([`LoaderSpec`])
//! * the **rules** to run in each phase ([`Rule`] = selector + binding)
//!
//! Everything is data-driven and the kernel knows no concrete framework — this is where the **open-closed
//! principle** and **dependency inversion** land: supporting a new framework only means adding one YAML file.

use serde::{Deserialize, Serialize};

use crate::model::kinds::Language;

// The FKB data model is split into focused submodules, one per concern, so this file stays a readable
// map of the knowledge shape rather than one 3k-line wall of structs.
mod detector; // framework recognition + root/loader resolution
mod guards; // routing / middleware / taint / security specs
mod resolution; // symbol resolution (links, projections, transforms)
mod rules; // rule selector + action language

// Re-export every public FKB type so external crates keep resolving `gt_domain::model::fkb::Rule`
// (etc.) exactly as before, and so each submodule's `use super::*` can see its siblings' types.
pub use detector::*;
pub use guards::*;
pub use resolution::*;
pub use rules::*;

/// One framework's knowledge.
///
/// `deny_unknown_fields`: a key the model does not have is a key the kernel cannot read. Silently
/// ignoring it (the serde default) is how a field like the retired `exclude_globs` / `format` can sit
/// in FKB for months looking effective while doing nothing — so an unknown key is a load error instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrameworkKnowledge {
    pub id: String,
    pub display_name: String,
    pub language: Language,
    /// Applicable-version hint, display only.
    pub version_hint: Option<String>,
    /// Recognition signals.
    pub detectors: Vec<Detector>,
    /// Component knowledge this one **brings with it**: when this knowledge is recognised, the listed ids
    /// count as recognised too (transitively, so a component may itself provide further ones).
    ///
    /// This exists because a framework is a bundle of libraries plus conventions, and it is the **bundle**
    /// the manifest names: `laravel/framework` pulls in `illuminate/database`, `illuminate/cache` … none of
    /// which appear in the app's own `composer.json`, so no detector of theirs can fire. Declaring the
    /// bundle here is what lets each library keep its own file — detected on its own terms, by manifest, by
    /// lock file or by code — while still being switched on as a group the moment the framework is.
    ///
    /// Directly detected knowledge always ranks **above** provided knowledge, so a framework's own
    /// declaration of a single-valued field still wins over a component's.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Rules for resolving the framework root / key paths.
    pub root_rules: Vec<RootRule>,
    /// P3 authoritative symbol-table loaders.
    pub loaders: Vec<LoaderSpec>,
    /// Rules grouped by phase.
    pub rules: Vec<Rule>,

    /// **Business-specific annotation kinds** introduced by this FKB (appended to the kernel-standard
    /// [`AnnotationKind`]).
    ///
    /// The standard kinds (pii / data.criticality / config.storage / auth.public / i18n.missing_locale …) are
    /// produced by kernel recognisers and are not listed here; only "project-specific business semantics" goes
    /// here (e.g. `entrypoint.login`).
    /// They are registered at load time into [`crate::model::kinds::register_annotation_kinds`], isomorphic to
    /// `semantic_kinds` (nodes) — adding a business annotation should not cost a kernel change.
    #[serde(default)]
    pub annotation_kinds: Vec<String>,
    /// The **party** this knowledge describes: `frontend` / `backend` (a closed set, validated at load time).
    ///
    /// It is the default `side` for every [`SynthesizeAction`] in this file that does not state one explicitly
    /// (see `loader.rs`), so one line here replaces tens of per-rule literals — and, more importantly, so a rule
    /// cannot *forget* it: a file declaring no side at all leaves its Cache / Event / Queue nodes with no
    /// evidence of a party, and they vanish from every side-filtered perspective.
    ///
    /// A per-rule `fields: [ { name: side, ... } ]` still wins when present (an escape hatch for knowledge that
    /// genuinely crosses parties). Cross-language knowledge (`language: "*"`, e.g. `universal/common.yaml`) should
    /// **not** declare one: the same file is loaded for every party, so a single value would be a lie.
    #[serde(default)]
    pub side: Option<String>,
    /// P7 dynamic-resolution declarations: which calls are container resolutions / event triggers / facade calls.
    pub resolvers: Vec<ResolverSpec>,
    /// Directories / files to keep **out of the P0 scan**, resolved from the project's own
    /// configuration rather than from hard-coded directory names (see [`ExcludeRule`]).
    #[serde(default)]
    pub exclude_rules: Vec<ExcludeRule>,
    /// Scope of the knowledge base: framework-level (default) vs project-level.
    ///
    /// * `Framework`: generic framework knowledge (e.g. `thinkphp` / `laravel`), loaded by any project using that
    ///   framework;
    /// * `Project`: project-specific knowledge (e.g. `crmeb`), loaded **only when the project is recognised as
    ///   that project**, so project conventions (e.g. CRMEB's crontab routes) do not bleed into other projects on
    ///   the same framework.
    pub scope: KnowledgeScope,
    /// Resolution rules for **string-literal callable references** (route handlers, queue string-jobs,
    /// `invokeAction` …): how to turn a string argument naming `class::method` / a function back into a graph
    /// node. Generic — not route-only; see [`MethodRefSpec`].
    ///
    /// This is **framework knowledge**, not kernel knowledge — see [`MethodRefSpec`].
    #[serde(default)]
    pub method_ref: Option<MethodRefSpec>,
    /// Magic-method delegation: a class declares `@method getList(...)` and forwards it to some property via
    /// `__call`.
    ///
    /// "annotation declaration + `__call` forwarding" is common in the PHP ecosystem (CRMEB's `BaseServices`
    /// forwards 20-odd `get*` / `count*` / `delete*` to `$this->dao`), but **who it forwards to** is a project
    /// convention the kernel must not guess — FKB just names the property, and everything else (annotation
    /// parsing, where the type comes from, inheritance walk-back) is a generic capability.
    #[serde(default)]
    pub magic_delegation: Option<MagicDelegationSpec>,
    /// Data-model CRUD verb -> read / write classification (used together with `MapsTo`).
    ///
    /// "A model maps to a table" is **static structure**; `$model->save()` / `$model->find()` are the
    /// **actions**. Which method names count as reads and which as writes is a framework API convention (ThinkPHP's
    /// `save` / `find`, Laravel's `create`…); once FKB declares them, P7 can label the
    /// `entry -> model -> table` edge as a real `WritesDb` / `ReadsDb` instead of propagating a vague `MapsTo` all
    /// the way through.
    #[serde(default)]
    pub db_verbs: Option<DbVerbsSpec>,
    /// Signature-verification vocabulary (`sign_check`) — see [`SignCheckSpec`].
    #[serde(default)]
    pub sign_check: Option<SignCheckSpec>,
    /// SQL-injection vocabulary (`taint`) — see [`TaintSpec`].
    #[serde(default)]
    pub taint: Option<TaintSpec>,
    /// A callee list for **external system calls** (HTTP / SMS / email / RPC): `curl_exec`, `Http::get` …
    /// declared by FKB for the "external call inside a loop" judgement (one network round trip costs far more
    /// than one query, so putting it in a loop kills an endpoint even more reliably than N+1).
    #[serde(default)]
    pub external_calls: Vec<String>,
    /// A **"middleware class -> capability" mapping**: what capability a given middleware carries (auth /
    /// rate limiting …).
    ///
    /// This list **belongs to framework knowledge** (what counts as an auth middleware, and what name the project
    /// gave it), so it lives in FKB rather than the kernel — the kernel knows no middleware name. The judgement is
    /// anchored on the middleware's **own confirmed identity** (its class name), not on "does this endpoint look
    /// like it needs a login".
    #[serde(default)]
    pub middleware_capabilities: Vec<MiddlewareCapability>,
    /// **Route-guard recognition rules**: how to tell from the call graph "which middleware guards which route".
    ///
    /// This is **framework knowledge, not kernel knowledge** — `Route::get()->middleware(X)` is the ThinkPHP /
    /// Laravel chained form, `app.get(path, mw, handler)` is the Express positional-argument form, and
    /// `@UseGuards(X)` / `@login_required` / `@PreAuthorize` are the NestJS / Python / Spring decorator /
    /// annotation forms. The kernel knows none of them; FKB declares all of them, the generic extractor collects
    /// them from the graph per the declaration and writes them into the `route_list` symbol table (with keys of
    /// the same shape as the `HttpContract.name` synthesised in P5), so P14 can promote the guard class to a
    /// `Middleware` and attach a `PassesThrough` edge.
    ///
    /// Supporting middleware for a new language / framework = add one `route_guards` block, **no Rust change**.
    #[serde(default)]
    pub route_guards: Option<RouteGuardSpec>,
    /// **Transaction-boundary markers**: `transaction` / `startTrans` / `beginTransaction` …
    /// for the "several writes to the DB in one method but no transaction recognised" judgement (a partial
    /// success leaves dirty data).
    #[serde(default)]
    pub tx_calls: Vec<String>,
    /// Candidates for the "consumer entry method name": when connecting to a class, which of its methods to
    /// connect to first.
    ///
    /// Conventions differ per framework: Laravel / queue Jobs use `handle`, Symfony uses `__invoke`,
    /// ThinkPHP / CRMEB Jobs use `doJob`, TP5 behaviour classes use `run`.
    /// Declared by FKB so a new framework does not have to change the kernel for one method name.
    /// When undeclared it falls back to the kernel's built-in **cross-framework common entry names** default set.
    #[serde(default)]
    pub entry_methods: Vec<String>,
    /// **First-class semantic node kinds** introduced by this FKB (appended to [`NodeKind::SYNTHESIZED`]).
    ///
    /// FKB declares it itself: `semantic_kinds: [Store, Page]`, registered at load time into
    /// [`crate::model::kinds::register_semantic_kinds`], and the folded view then renders them as semantic nodes.
    #[serde(default)]
    pub semantic_kinds: Vec<String>,
    /// **First-class semantic edge kinds** introduced by this FKB (appended to the built-in [`EdgeKind::SEMANTIC`]
    /// list).
    ///
    /// Isomorphic to `semantic_kinds` (nodes): adding a semantic edge should not cost a kernel change.
    /// Example: a framework invents a `SendsWebhook` edge; after declaring
    /// `semantic_edge_kinds: [SendsWebhook]` it is counted and drawn as a semantic edge just like `ReadsDb`,
    /// with no change to `kinds.rs`.
    #[serde(default)]
    pub semantic_edge_kinds: Vec<String>,
    /// **Bridge edge kinds** introduced by this FKB (appended to the built-in [`EdgeKind::BRIDGE`] list).
    #[serde(default)]
    pub bridge_edge_kinds: Vec<String>,
    /// Whether to apply this knowledge's rules even when the framework was not recognised (default **false**).
    ///
    /// Framework-level rules carry strong framework assumptions (`Db::name` is a table name, the second argument
    /// of `Route::get` is a handler…). Applying them unconditionally to a project that **uses the same language
    /// but a different framework** means interpreting framework B's code with framework A's knowledge, producing
    /// a graph that **looks reasonable but is not trustworthy** — measured: applying ThinkPHP rules to a Laravel
    /// project conjures hundreds of `Table` / `HttpContract` nodes out of nothing.
    ///
    /// So by default they take effect only when the detector matches; this is switched on explicitly only for
    /// knowledge that really is "a generic fallback for that language" (carrying no concrete framework
    /// assumptions).
    #[serde(default)]
    pub apply_without_detection: bool,
}

/// Scope of a knowledge base.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeScope {
    /// Framework knowledge: loaded along with framework recognition, applies to every project using that framework.
    #[default]
    Framework,
    /// Project knowledge: loaded along with project recognition, applies only to projects recognised as that project (its detectors matched).
    Project,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The tests exercise the serialisation model directly, so they need the `serde_json` / `kinds` / `graph`
    // types that the (now-split) model types used to pull in via this module's pre-split imports.
    use serde_json::Value;
    use crate::model::graph::MergeStrategy;
    use crate::model::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};

    /// `RootSource::Manifest` is language-agnostic; the PHP-specific legacy tag `manifest_php` was
    /// **removed** (no backward compatibility). Unknown tags must fail **loudly** at deserialization —
    /// the FKB loader silently skips unparseable files, so a still-accepted legacy tag would instead
    /// surface as silently lost knowledge (`app_root` / `db_prefix` / `method_ref` all gone).
    #[test]
    fn root_source_manifest_deserializes_and_legacy_php_tag_is_rejected() {
        let as_new: RootSource = serde_json::from_value(json!({
            "kind": "manifest",
            "manifest": "config/database.php",
            "pointer": "connections.mysql.prefix"
        }))
        .expect("language-agnostic manifest tag must deserialize");
        assert!(matches!(as_new, RootSource::Manifest { .. }));

        let legacy = serde_json::from_value::<RootSource>(json!({
            "kind": "manifest_php",
            "manifest": "config/database.php",
            "pointer": "connections.mysql.prefix"
        }));
        assert!(
            legacy.is_err(),
            "legacy manifest_php tag must be rejected, not silently accepted"
        );
    }

    #[test]
    fn root_source_other_variants_deserialize() {
        let json_src: RootSource = serde_json::from_value(json!({
            "kind": "manifest_json",
            "manifest": "composer.json",
            "pointer": "autoload.psr-4",
            "pick": "shallowest_dir"
        }))
        .expect("manifest_json must deserialize");
        assert!(matches!(json_src, RootSource::ManifestJson { .. }));

        let dir_src: RootSource = serde_json::from_value(json!({
            "kind": "directory_exists",
            "path": "app"
        }))
        .expect("directory_exists must deserialize");
        assert!(matches!(dir_src, RootSource::DirectoryExists { .. }));
    }

    /// `RootRule` defaults `confidence` (0.9) and `fallbacks` (empty) when omitted — a regression that turns these
    /// into required fields would silently break every FKB root rule.
    #[test]
    fn root_rule_defaults_confidence_and_fallbacks() {
        let rule: RootRule = serde_json::from_value(json!({
            "id": "r1",
            "key": "app_root",
            "source": { "kind": "directory_exists", "path": "app" }
        }))
        .expect("RootRule without confidence/fallbacks must deserialize");
        assert_eq!(rule.confidence, 0.9);
        assert!(rule.fallbacks.is_empty());
    }

    // ===== Below: the gaps the original 3 tests left open — language scoping, template placeholders,
    // the guard-attach normaliser, detector confidence, the resolution ladder and the declared defaults. =====

    fn rule(languages: Option<Vec<&str>>) -> Rule {
        Rule {
            id: "r".into(),
            phase: Phase("AnnotatePre".into()),
            selector: Selector::Node {
                node_kind: None,
                r#where: Vec::new(),
            },
            binding: Vec::new(),
            confidence: 1.0,
            languages: languages.map(|l| l.into_iter().map(Language::new).collect()),
        }
    }

    /// With no per-rule `languages`, the rule inherits the language of the FKB that declared it — and a
    /// language-agnostic FKB (`*`) applies everywhere.
    #[test]
    fn rule_applies_to_inherits_the_fkb_language() {
        let inherited = rule(None);
        assert!(inherited.applies_to(&Language::new("php"), &Language::new("php")));
        assert!(!inherited.applies_to(&Language::new("php"), &Language::new("java")));
        // A cross-language FKB declares `*`.
        assert!(inherited.applies_to(&Language::new("*"), &Language::new("java")));
    }

    /// A declared `languages` list is authoritative: it overrides the FKB's language entirely (in both
    /// directions), and the `*` sentinel means "every language".
    #[test]
    fn rule_applies_to_lets_the_rule_override_the_fkb_language() {
        let php_only = rule(Some(vec!["php"]));
        assert!(php_only.applies_to(&Language::new("php"), &Language::new("php")));
        // The FKB says java, but the rule restricts itself to php — a java sub-project must be skipped.
        assert!(!php_only.applies_to(&Language::new("java"), &Language::new("java")));
        // ...and the reverse: the sub-project is php even though the FKB is not.
        assert!(php_only.applies_to(&Language::new("java"), &Language::new("php")));

        let any = rule(Some(vec!["*"]));
        assert!(any.applies_to(&Language::new("php"), &Language::new("java")));
        assert!(any.applies_to(&Language::new("php"), &Language::new("python")));
    }

    /// Glob templates reference `{placeholder}` names; they are collected in order of appearance, trimmed and
    /// de-duplicated, and a malformed template must not panic.
    #[test]
    fn template_placeholders_in_order_deduped_and_trimmed() {
        assert_eq!(
            template_placeholders("src/{module}/{layer}.ts"),
            vec!["module".to_string(), "layer".to_string()]
        );
        // Repeats collapse.
        assert_eq!(template_placeholders("src/{module}/{module}.ts"), vec!["module".to_string()]);
        // Surrounding spaces are trimmed, empty names skipped.
        assert_eq!(template_placeholders("{ a }/{}"), vec!["a".to_string()]);
        // No placeholders at all.
        assert!(template_placeholders("src/main.ts").is_empty());
        assert!(template_placeholders("").is_empty());
        // Unterminated `{` stops the scan instead of panicking.
        assert!(template_placeholders("src/{module").is_empty());
        assert!(template_placeholders("src/{").is_empty());
    }

    #[test]
    fn guard_attach_specs_normalises_one_and_many() {
        assert_eq!(GuardAttach::default().specs().len(), 1, "the default is a single positional spec");
        assert!(matches!(
            GuardAttach::default(),
            GuardAttach::One(GuardAttachSpec::Positional)
        ));
        assert_eq!(GuardAttach::Many(Vec::new()).specs().len(), 0);
        let two = GuardAttach::Many(vec![
            GuardAttachSpec::Positional,
            GuardAttachSpec::Consumer(ConsumerGuardSpec::default()),
        ]);
        assert_eq!(two.specs().len(), 2);
    }

    /// `guard_attach` is untagged, so one framework can declare a single model or several without breaking
    /// existing FKBs.
    #[test]
    fn guard_attach_deserializes_a_single_value_or_a_list() {
        let one: GuardAttach = serde_json::from_value(json!({ "kind": "positional" }))
            .expect("a single model must deserialize");
        assert!(matches!(one, GuardAttach::One(_)));
        assert_eq!(one.specs().len(), 1);

        let many: GuardAttach = serde_json::from_value(json!([
            { "kind": "positional" },
            { "kind": "consumer" }
        ]))
        .expect("a list of models must deserialize");
        assert!(matches!(many, GuardAttach::Many(_)));
        assert_eq!(many.specs().len(), 2);
    }

    /// Every variant carries its own confidence, defaulting to 0.9 — a new variant silently inheriting the
    /// last match arm is exactly what the accessor exists to prevent.
    #[test]
    fn detector_confidence_is_declared_per_variant() {
        assert_eq!(
            Detector::ManifestDependency { manifest: "composer.json".into(), dependency: "x/y".into(), confidence: 0.4 }
                .confidence(),
            0.4
        );
        assert_eq!(
            Detector::FileExists { path: "artisan".into(), confidence: 0.5 }.confidence(),
            0.5
        );
        assert_eq!(
            Detector::ImportExists { symbol: "GuzzleHttp".into(), confidence: 0.6 }.confidence(),
            0.6
        );
        assert_eq!(
            Detector::LockDependency { lock: "composer.lock".into(), dependency: "x/y".into(), confidence: 0.7 }
                .confidence(),
            0.7
        );
        assert_eq!(
            Detector::CallExists { callee: "Db::query".into(), confidence: 0.8 }.confidence(),
            0.8
        );

        // Omitted in YAML -> the 0.9 default.
        let from_yaml: Detector = serde_json::from_value(json!({ "kind": "file_exists", "path": "artisan" })).unwrap();
        assert!((from_yaml.confidence() - 0.9).abs() < 1e-6);
        let explicit: Detector = serde_json::from_value(json!({ "kind": "call_exists", "callee": "Db::query", "confidence": 0.5 })).unwrap();
        assert!((explicit.confidence() - 0.5).abs() < 1e-6);
    }

    /// The ladder is **not** monotonic in the level number — L6 (intersection with the finite universe of
    /// 203 tables) is trusted more than L5 (constant propagation) — but Exact is the ceiling and Unknown the floor.
    #[test]
    fn resolve_tier_base_confidence_ladder() {
        assert_eq!(ResolveTier::Exact.base_confidence(), 1.0);
        assert_eq!(ResolveTier::Registry.base_confidence(), 0.95);
        assert_eq!(ResolveTier::Alias.base_confidence(), 0.85);
        assert_eq!(ResolveTier::Convention.base_confidence(), 0.8);
        assert_eq!(ResolveTier::ConstProp.base_confidence(), 0.6);
        assert_eq!(ResolveTier::Intersection.base_confidence(), 0.7);
        assert_eq!(ResolveTier::Unknown.base_confidence(), 0.3);

        assert!(
            ResolveTier::Intersection.base_confidence() > ResolveTier::ConstProp.base_confidence(),
            "L6 is more trustworthy than L5 (intersection over a finite universe vs constant propagation)"
        );
        for t in [
            ResolveTier::Unknown,
            ResolveTier::ConstProp,
            ResolveTier::Intersection,
            ResolveTier::Convention,
            ResolveTier::Alias,
            ResolveTier::Registry,
        ] {
            assert!(t.base_confidence() < ResolveTier::Exact.base_confidence());
            assert!(t.base_confidence() > ResolveTier::Unknown.base_confidence() || t == ResolveTier::Unknown);
        }
    }

    #[test]
    fn resolution_unknown_and_resolved() {
        let u = Resolution::unknown("no candidate");
        assert_eq!(u.tier, ResolveTier::Unknown);
        assert!(u.candidates.is_empty(), "unresolved = no candidates");
        assert!((u.confidence - ResolveTier::Unknown.base_confidence()).abs() < 1e-6);
        assert_eq!(u.evidence, "no candidate");

        let r = Resolution::resolved(ResolveTier::Alias, crate::model::ids::NodeId(7), "facade_map");
        assert_eq!(r.tier, ResolveTier::Alias);
        assert_eq!(r.candidates, vec![crate::model::ids::NodeId(7)]);
        assert!((r.confidence - 0.85).abs() < 1e-6, "confidence takes that layer's base value");
        assert_eq!(r.evidence, "facade_map");
    }

    /// The NestJS convention is the default, so an FKB only names the exceptions.
    #[test]
    fn consumer_guard_spec_defaults_match_the_nestjs_convention() {
        let d = ConsumerGuardSpec::default();
        assert_eq!(d.receiver, "consumer");
        assert_eq!(d.apply_method, "apply");
        assert_eq!(d.for_routes_method, "forRoutes");
        assert_eq!(d.wildcards, vec!["*".to_string()]);
        assert_eq!(d.scope, ConsumerScope::Directory);
        assert_eq!(ConsumerScope::default(), ConsumerScope::Directory);

        // An empty declaration gets the same defaults.
        let from_yaml: ConsumerGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(from_yaml.receiver, "consumer");
        assert_eq!(from_yaml.for_routes_method, "forRoutes");
        assert_eq!(from_yaml.scope, ConsumerScope::Directory);

        // Unknown keys are rejected rather than ignored.
        assert!(serde_json::from_value::<ConsumerGuardSpec>(json!({ "nope": 1 })).is_err());
    }

    #[test]
    fn annotate_defaults_are_the_expected_neutral_values() {
        let a = AnnotateAction::default();
        assert_eq!(a.channel.0, AnnotationChannel::FKB_MARK);
        assert!(matches!(a.target, AnnotateTarget::Matched));
        assert!(matches!(a.merge, MergeStrategy::MaxByKind));
        assert!(a.annotations.is_empty());
        assert!(a.phase.is_none(), "no declared phase = run in the phase the rule belongs to");
        assert!(a.confidence_scale.is_none());
        assert!(a.scope.is_none());
        assert!(a.r#where.is_empty());

        assert!(matches!(AnnotateTarget::default(), AnnotateTarget::Matched));

        let s = AnnotationSpec::default();
        assert!((s.confidence - 1.0).abs() < 1e-6, "an undeclared confidence means no decay");
        assert!(s.kind.is_empty());
        assert!(s.subkind.is_none());
        assert!(s.channel.is_none());
        assert!(s.severity.is_none());
        assert!(s.evidence.is_none());
    }

    #[test]
    fn synthesize_and_expand_defaults_are_empty_and_unknown() {
        let s = SynthesizeAction::default();
        assert_eq!(s.node.as_str(), NodeKind::UNKNOWN, "the node kind must be declared by the FKB");
        assert!((s.confidence - 0.9).abs() < 1e-6);
        assert!(s.subtype.is_none());
        assert!(s.fields.is_empty());
        assert!(s.link.is_none());
        assert!(s.alias.is_none());
        assert!(s.expand.is_none());
        assert!(s.modifiers.is_empty());

        let e = ExpandSpec::default();
        assert!(e.variants.is_empty(), "no variants = no expansion (a single node is synthesised)");
        assert!(e.only.is_none());
        assert!(e.except.is_none());
    }

    #[test]
    fn magic_delegation_and_method_ref_defaults() {
        let m = MagicDelegationSpec::default();
        assert!(m.property.is_empty(), "the forwarded target property must be declared by the FKB");
        assert!((m.confidence - 0.7).abs() < 1e-6, "a declared annotation scores below an exact hit");

        let r = MethodRefSpec::default();
        assert_eq!(r.method_separators, vec!["/".to_string()], "ThinkPHP's `Login/appleLogin` convention");
        assert_eq!(r.controller_layer_depth, 1);
        assert!(r.hierarchy_separators.is_empty());
        assert!(r.root_namespaces.is_empty());
        assert!(r.app_segments.is_empty());
        assert!(r.app_anchor_dir.is_none());
        assert!(r.app_fallback.is_empty());
    }

    // ===== Dead-field defense (`deny_unknown_fields`): an unknown key is a key the kernel cannot read,
    // so it must fail loudly at load time instead of silently vanishing. These are the regression nets
    // for the retired `exclude_globs` / `format` / rule-level `where` incidents. =====

    /// `FrameworkKnowledge` is the top-level container: a key it does not have is knowledge the kernel
    /// will never consult. The retired `exclude_globs` (now `exclude_rules`) must not be accepted under
    /// its old name — otherwise the exclusion would silently stop working.
    #[test]
    fn framework_knowledge_rejects_unknown_keys_and_defaults_scope() {
        let fk: FrameworkKnowledge = serde_json::from_value(json!({
            "id": "tp",
            "display_name": "ThinkPHP",
            "language": "php"
        }))
        .expect("minimal FKB must deserialize with struct defaults");
        assert_eq!(fk.id, "tp");
        assert_eq!(fk.scope, KnowledgeScope::Framework, "scope defaults to framework");
        assert!(!fk.apply_without_detection, "apply_without_detection defaults to false");
        assert!(fk.provides.is_empty());
        assert!(fk.side.is_none());
        assert!(fk.detectors.is_empty());
        assert!(fk.rules.is_empty());

        // Explicit scope / side / provides round-trip.
        let fk2: FrameworkKnowledge = serde_json::from_value(json!({
            "id": "crmeb",
            "display_name": "CRMEB",
            "language": "php",
            "scope": "project",
            "side": "backend",
            "provides": ["thinkphp"]
        }))
        .expect("explicit scope/side/provides must deserialize");
        assert_eq!(fk2.scope, KnowledgeScope::Project);
        assert_eq!(fk2.side.as_deref(), Some("backend"));
        assert_eq!(fk2.provides, vec!["thinkphp".to_string()]);

        // A retired / misspelled key must be rejected, not ignored.
        assert!(
            serde_json::from_value::<FrameworkKnowledge>(json!({
                "id": "tp",
                "display_name": "ThinkPHP",
                "language": "php",
                "exclude_globs": ["vendor/**"]
            }))
            .is_err(),
            "retired exclude_globs must be rejected by deny_unknown_fields"
        );
        assert!(
            serde_json::from_value::<FrameworkKnowledge>(json!({
                "id": "tp",
                "display_name": "ThinkPHP",
                "language": "php",
                "format": "php"
            }))
            .is_err(),
            "never-read `format` key must be rejected"
        );
    }

    /// The rule layer must never accept a `where:` written at the rule level (it belongs inside the
    /// `selector`). A JavaScript FKB once had `where:` here and silently mislabelled every HTTP contract
    /// as `frontend.called` — pinned now.
    #[test]
    fn rule_rejects_unknown_fields_at_rule_level() {
        assert!(
            serde_json::from_value::<Rule>(json!({
                "id": "r",
                "phase": "AnnotatePre",
                "selector": { "kind": "node" },
                "where": [{ "has_property": "x" }]
            }))
            .is_err(),
            "rule-level `where` must be rejected (it belongs in the selector)"
        );

        // A well-formed rule with selector + binding + per-rule languages round-trips.
        // `Action` is externally tagged (no `kind` tag), so each binding is `{ "Annotate": { ... } }`.
        let r: Rule = serde_json::from_value(json!({
            "id": "r",
            "phase": "AnnotatePre",
            "selector": {
                "kind": "node",
                "node_kind": "Class",
                "where": [{ "has_property": "dao" }]
            },
            "binding": [
                { "Annotate": { "channel": "fkb_mark", "annotations": [] } }
            ],
            "languages": ["php"]
        }))
        .expect("well-formed rule must deserialize");
        assert!(matches!(r.selector, Selector::Node { .. }));
        assert_eq!(r.confidence, 0.9, "a Rule's default confidence is 0.9");
        assert!(r.applies_to(&Language::new("java"), &Language::new("php")));
        assert!(!r.applies_to(&Language::new("java"), &Language::new("java")));
    }

    /// `Selector` is a tagged enum: an unknown `kind` must be rejected, and a `node` selector carrying
    /// `where` predicates must round-trip.
    #[test]
    fn selector_rejects_unknown_kind_and_node_roundtrips() {
        assert!(
            serde_json::from_value::<Selector>(json!({ "kind": "bogus" })).is_err(),
            "unknown selector kind must be rejected"
        );
        let s: Selector = serde_json::from_value(json!({
            "kind": "node",
            "node_kind": "Class",
            "where": [
                { "has_property": "dao" },
                { "name_matches": "Service" },
                { "fan_in_gte": 5 }
            ]
        }))
        .expect("node selector must deserialize");
        match s {
            Selector::Node { node_kind, r#where } => {
                assert_eq!(node_kind.as_ref().map(|k| k.as_str()), Some("Class"));
                assert_eq!(r#where.len(), 3);
            }
            _ => panic!("expected a Node selector"),
        }
    }

    /// `Predicate` is the leaf condition: a misspelled variant field must be rejected rather than produce a
    /// silently-empty condition that matches nothing (or everything).
    #[test]
    fn predicate_deny_unknown_fields_and_roundtrips() {
        assert!(
            serde_json::from_value::<Predicate>(json!({ "has_property": "x", "bogus": 1 })).is_err(),
            "extra field on a predicate must be rejected"
        );
        assert!(
            serde_json::from_value::<Predicate>(json!({ "kind": "has_property", "0": "x" })).is_err(),
            "tuple variant must not be wrapped under `kind`"
        );

        let p: Predicate = serde_json::from_value(json!({ "has_property": "dao" })).unwrap();
        assert!(matches!(p, Predicate::HasProperty(ref s) if s == "dao"));
        let a: Predicate = serde_json::from_value(json!({ "arg_starts_with": { "arg": 0, "prefix": "crontab" } })).unwrap();
        assert!(matches!(a, Predicate::ArgStartsWith { arg: 0, prefix } if prefix == "crontab"));
    }

    /// The `exclude_rules` redesign replaced the dead `exclude_globs`: an FKB still using `exclude_globs`
    /// must error, while a valid `ExcludeRule` (with `source` + `fallbacks`) round-trips.
    #[test]
    fn exclude_rule_rejects_dead_exclude_globs_and_roundtrips() {
        assert!(
            serde_json::from_value::<ExcludeRule>(json!({
                "id": "e",
                "glob": "{app_root}/runtime/**",
                "exclude_globs": ["vendor/**"]
            }))
            .is_err(),
            "retired exclude_globs must be rejected on ExcludeRule"
        );
        let e: ExcludeRule = serde_json::from_value(json!({
            "id": "runtime",
            "glob": "{app_root}/runtime/**",
            "source": { "kind": "directory_exists", "path": "app" },
            "fallbacks": ["vendor", "runtime"]
        }))
        .expect("valid ExcludeRule must deserialize");
        assert_eq!(e.id, "runtime");
        assert!(e.source.is_some());
        assert_eq!(e.fallbacks, vec!["vendor".to_string(), "runtime".to_string()]);
        // Minimal: only id + glob are required.
        let min: ExcludeRule = serde_json::from_value(json!({ "id": "x", "glob": "a/**" })).unwrap();
        assert!(min.source.is_none());
        assert!(min.fallbacks.is_empty());
    }

    /// Every detector variant must deserialise under its snake_case tag and default its confidence to 0.9
    /// when omitted.
    #[test]
    fn detector_variants_deserialize_with_default_confidence() {
        let md: Detector = serde_json::from_value(json!({
            "kind": "manifest_dependency", "manifest": "composer.json", "dependency": "topthink/framework"
        })).unwrap();
        assert!(matches!(md, Detector::ManifestDependency { .. }));
        assert!((md.confidence() - 0.9).abs() < 1e-6);

        let cases: Vec<Value> = vec![
            json!({ "kind": "file_exists", "path": "artisan" }),
            json!({ "kind": "import_exists", "symbol": "think\\facade\\Db" }),
            json!({ "kind": "lock_dependency", "lock": "composer.lock", "dependency": "x/y" }),
            json!({ "kind": "call_exists", "callee": "Db::query" }),
        ];
        for c in cases {
            let d: Detector = serde_json::from_value(c).expect("detector variant must deserialize");
            assert!((d.confidence() - 0.9).abs() < 1e-6, "variant default conf");
        }
    }

    /// `RouteCallSpec` defaults: `path_arg = 0`, `handler_arg = None`, `by = receiver`; verb-method keys are
    /// lower-cased and values upper-cased on the way in.
    #[test]
    fn route_call_spec_defaults_and_verb_normalisation() {
        let d: RouteCallSpec = serde_json::from_value(json!({ "receiver": "Route" })).unwrap();
        assert_eq!(d.path_arg, 0);
        assert!(d.handler_arg.is_none());
        assert_eq!(d.by, RouteMatchBy::Receiver);
        assert!(!d.receiver_ends_with);
        assert!(!d.accept_identifier);

        let v: RouteCallSpec = serde_json::from_value(json!({
            "receiver": "Route",
            "by": "callee",
            "verb_methods": { "get": "get", "any": "any" },
            "path_arg": 1,
            "handler_arg": 2
        }))
        .unwrap();
        assert_eq!(v.by, RouteMatchBy::Callee);
        assert_eq!(v.path_arg, 1);
        assert_eq!(v.handler_arg, Some(2));
        // `verb_methods` round-trips verbatim; key/value normalisation (lowercase key, uppercase verb)
        // happens at lookup time, not on deserialization.
        assert_eq!(v.verb_methods.get("get").map(String::as_str), Some("get"));
        assert_eq!(v.verb_methods.get("any").map(String::as_str), Some("any"));
    }

    /// The small classification enums all round-trip their snake_case tags; this guards against a rename
    /// silently changing the on-disk FKB spelling.
    #[test]
    fn small_enums_roundtrip_snake_case() {
        assert_eq!(serde_json::from_value::<KnowledgeScope>(json!("project")).unwrap(), KnowledgeScope::Project);
        assert_eq!(serde_json::from_value::<KnowledgeScope>(json!("framework")).unwrap(), KnowledgeScope::Framework);

        assert_eq!(serde_json::from_value::<RouteMatchBy>(json!("callee")).unwrap(), RouteMatchBy::Callee);
        assert_eq!(serde_json::from_value::<RouteMatchBy>(json!("receiver")).unwrap(), RouteMatchBy::Receiver);

        assert_eq!(serde_json::from_value::<ConsumerScope>(json!("all")).unwrap(), ConsumerScope::All);
        assert_eq!(serde_json::from_value::<ConsumerScope>(json!("explicit_only")).unwrap(), ConsumerScope::ExplicitOnly);
        assert_eq!(serde_json::from_value::<ConsumerScope>(json!("directory")).unwrap(), ConsumerScope::Directory);

        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("container")).unwrap(), ResolveStrategy::Container);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("event")).unwrap(), ResolveStrategy::Event);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("event_listen")).unwrap(), ResolveStrategy::EventListen);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("facade")).unwrap(), ResolveStrategy::Facade);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("accessor")).unwrap(), ResolveStrategy::Accessor);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("variable_type")).unwrap(), ResolveStrategy::VariableType);
        assert_eq!(serde_json::from_value::<ResolveStrategy>(json!("handler")).unwrap(), ResolveStrategy::Handler);

        assert_eq!(serde_json::from_value::<PickStrategy>(json!("shallowest_dir")).unwrap(), PickStrategy::ShallowestDir);
        assert_eq!(serde_json::from_value::<PickStrategy>(json!("first_dir")).unwrap(), PickStrategy::FirstDir);
        assert_eq!(serde_json::from_value::<PickStrategy>(json!("by_namespace_key")).unwrap(), PickStrategy::ByNamespaceKey);

        assert_eq!(serde_json::from_value::<EntryFieldFrom>(json!("key")).unwrap(), EntryFieldFrom::Key);
    }

    /// `LoaderSpec` + `LoaderSource` round-trip; the source is a tagged enum so each `kind` must parse.
    #[test]
    fn loader_spec_and_source_roundtrip() {
        for src in [
            json!({ "kind": "builtin", "name": "php_db_schema" }),
            json!({ "kind": "file", "path": "config/app.php", "key_path": "aliases" }),
            json!({ "kind": "glob", "pattern": "lang/*/*.php", "locale_regex": "lang/([a-z]+)/" }),
            json!({ "kind": "inline", "rows": [ { "k": "v" } ] }),
        ] {
            let ls: LoaderSpec = serde_json::from_value(json!({
                "id": "l", "table": "schema", "from": src
            }))
            .unwrap_or_else(|e| panic!("loader spec must deserialize: {e}"));
            assert_eq!(ls.table, "schema");
            assert!((ls.confidence - 0.9).abs() < 1e-6, "the LoaderSpec default confidence");
        }
    }

    /// `RootSource::ManifestEntries` (the multi-connection form) carries its `fields` and round-trips;
    /// `EntryField` requires `name` and defaults `pointer` / `from`.
    #[test]
    fn root_source_manifest_entries_roundtrips() {
        let rs: RootSource = serde_json::from_value(json!({
            "kind": "manifest_entries",
            "manifest": "config/database.php",
            "root": "connections",
            "fields": [
                { "name": "driver", "pointer": "{key}.driver" },
                { "name": "name", "from": "key" }
            ],
            "default_from": "default"
        }))
        .expect("ManifestEntries must deserialize");
        match rs {
            RootSource::ManifestEntries { root, fields, default_from, .. } => {
                assert_eq!(root, "connections");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].name, "driver");
                assert_eq!(fields[0].pointer.as_deref(), Some("{key}.driver"));
                assert_eq!(fields[1].from, Some(EntryFieldFrom::Key));
                assert_eq!(default_from.as_deref(), Some("default"));
            }
            _ => panic!("expected ManifestEntries"),
        }
    }

    // ===== Second pass: the tests above cover the hot types and the incidents already suffered. Everything
    // below is the rest of the surface an FKB author writes against, and it carries exactly the same risk:
    // a lost rename, a flipped default, or a key the kernel never reads. Each test pins the deserialisation
    // contract of one group of types (defaults + which fields are mandatory + dead-field rejection). =====

    macro_rules! assert_fields_none {
        ($v:expr, [$($f:ident),* $(,)?]) => {{ $(assert!($v.$f.is_none(), "{} should default to None", stringify!($f));)* }};
    }

    /// `ValueSource` is the most-used type in FKB (identities, fields and links are all built from it) and
    /// **every** field of it must be optional: an absent key means "this source yields nothing", which is what
    /// lets `value_fallback` take over instead of failing the whole rule.
    ///
    /// `self` goes through `#[serde(rename)]` because `self` is a Rust keyword — dropping that rename silently
    /// disables every `{ self: true }` in FKB.
    #[test]
    fn value_source_is_all_optional_and_keeps_the_self_rename() {
        let empty: ValueSource = serde_json::from_value(json!({})).expect("an empty ValueSource must deserialize");
        assert_fields_none!(empty, [
            arg, element, field, property, self_value, path_segment, method_name, entry_value,
            array_values, array_length, key_path, file_stem, locale, owner_class, owner_member,
            receiver_class, entity, require_class, require_literal, literal, source, transform,
            normalize, resolve, default, path, join, expand_method, expand_entry,
        ]);

        let selfy: ValueSource = serde_json::from_value(json!({ "self": true })).unwrap();
        assert_eq!(selfy.self_value, Some(true), "`self` must map to self_value");
        assert!(
            serde_json::from_value::<ValueSource>(json!({ "self_value": true })).is_err(),
            "the Rust field name is not part of the YAML spelling"
        );
        assert!(serde_json::from_value::<ValueSource>(json!({ "owner_class": true, "ownerClass": true })).is_err());
    }

    /// Sources compose: a nested `source`, a multi-segment `path`+`join`, a `transform` chain and a default
    /// are what turn "argument 0 of a call" into a semantic identity.
    #[test]
    fn value_source_nested_transform_and_multi_segment_path_roundtrip() {
        let nested: ValueSource = serde_json::from_value(json!({ "source": { "arg": 1 }, "field": "url" })).unwrap();
        assert_eq!(nested.source.as_ref().and_then(|s| s.arg), Some(1));
        assert_eq!(nested.field.as_deref(), Some("url"));

        let joined: ValueSource = serde_json::from_value(json!({
            "path": [ { "file_stem": true }, { "key_path": true } ],
            "join": "."
        }))
        .unwrap();
        assert_eq!(joined.path.as_ref().map(Vec::len), Some(2));
        assert_eq!(joined.join.as_deref(), Some("."));

        let t: ValueSource =
            serde_json::from_value(json!({ "arg": 0, "transform": { "snake_plural": true, "strip_namespace": true } }))
                .unwrap();
        let tx = t.transform.as_ref().expect("transform must deserialize");
        assert_eq!(tx.snake_plural, Some(true));
        assert_eq!(tx.strip_namespace, Some(true));
        assert_eq!(tx.snake, None, "an undeclared conversion stays off rather than defaulting to everything on");

        let n: ValueSource = serde_json::from_value(json!({
            "owner_class": true,
            "normalize": ["lower", "leading_slash"],
            "resolve": "class_const",
            "default": "unknown"
        }))
        .unwrap();
        assert_eq!(n.owner_class, Some(true));
        assert_eq!(n.normalize.as_ref().map(Vec::len), Some(2));
        assert_eq!(n.resolve, Some(ResolveAs::ClassConst));
        assert_eq!(n.default.as_deref(), Some("unknown"));
    }

    /// The normalisation ladder is **shape-sensitive**: unit variants are bare strings, `strip_prefix` a list
    /// and `replace` an object. A misspelled step must fail loudly — a silently-ignored normalisation leaves
    /// two identities that should have merged as two separate nodes.
    #[test]
    fn normalize_step_covers_unit_list_and_struct_shapes() {
        let cases: Vec<(Value, NormalizeStep)> = vec![
            (json!("lower"), NormalizeStep::Lower),
            (json!("upper"), NormalizeStep::Upper),
            (json!("leading_slash"), NormalizeStep::LeadingSlash),
            (json!("singularize"), NormalizeStep::Singularize),
            (json!("snake_plural"), NormalizeStep::SnakePlural),
            (json!("strip_namespace"), NormalizeStep::StripNamespace),
            (json!("short_name"), NormalizeStep::ShortName),
            (json!("param_wildcard"), NormalizeStep::ParamWildcard),
            (json!("strip_query"), NormalizeStep::StripQuery),
            (json!("trim"), NormalizeStep::Trim),
            (json!({ "strip_prefix": ["app\\"] }), NormalizeStep::StripPrefix(vec!["app\\".to_string()])),
            (
                json!({ "replace": { "from": "-", "to": "_" } }),
                NormalizeStep::Replace { from: "-".into(), to: "_".into() },
            ),
        ];
        for (v, expected) in cases {
            let got: NormalizeStep =
                serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{v} must deserialize: {e}"));
            assert_eq!(got, expected);
        }
        assert!(serde_json::from_value::<NormalizeStep>(json!("snakeplural")).is_err());
        assert!(serde_json::from_value::<NormalizeStep>(json!({ "replace": { "from": "-", "too": "_" } })).is_err());
    }

    /// How a value is resolved (`class_const` / `method_ref` / `by_alias` / `as_is`) and which way an edge
    /// points. `Direction` defaults to **incoming** — omitting the key must not silently flip Caller->New node.
    #[test]
    fn resolve_as_and_direction_roundtrip_with_the_incoming_default() {
        assert_eq!(
            serde_json::from_value::<ResolveAs>(json!("class_const")).unwrap(),
            ResolveAs::ClassConst
        );
        assert_eq!(serde_json::from_value::<ResolveAs>(json!("method_ref")).unwrap(), ResolveAs::MethodRef);
        assert_eq!(serde_json::from_value::<ResolveAs>(json!("by_alias")).unwrap(), ResolveAs::ByAlias);
        assert_eq!(serde_json::from_value::<ResolveAs>(json!("as_is")).unwrap(), ResolveAs::AsIs);

        assert_eq!(
            Direction::default(),
            Direction::Incoming,
            "no declared direction = the edge starts at the matched caller"
        );
        assert_eq!(serde_json::from_value::<Direction>(json!("outgoing")).unwrap(), Direction::Outgoing);
        assert_eq!(serde_json::from_value::<Direction>(json!("to_target")).unwrap(), Direction::ToTarget);
        let d: LinkSpec = serde_json::from_value(json!({ "kind": "HandledBy", "to": { "arg": 1 } })).unwrap();
        assert_eq!(d.direction, Direction::Incoming, "LinkSpec inherits the same default");
    }

    /// Field / link / alias / expand specs: which halves are mandatory, which default away.
    #[test]
    fn field_link_alias_and_expand_specs_defaults_and_strictness() {
        let f: FieldSpec = serde_json::from_value(json!({ "name": "text", "value": { "locale": true } })).unwrap();
        assert_eq!(f.name, "text");
        assert!(f.value.is_some());
        assert!(f.accumulate.is_none(), "a normal field is not an accumulating merge");
        assert!(f.from_symbol_table.is_none());
        let bare: FieldSpec = serde_json::from_value(json!({})).unwrap();
        assert!(bare.name.is_empty() && bare.value.is_none(), "every FieldSpec field is optional");

        // Accumulate is a pair (`{key, value}`) — either half missing is a declaration mistake.
        let acc: AccumulateSpec =
            serde_json::from_value(json!({ "key": { "locale": true }, "value": { "self": true } })).unwrap();
        assert_eq!(acc.key.locale, Some(true));
        assert!(serde_json::from_value::<AccumulateSpec>(json!({ "key": { "locale": true } })).is_err());
        // Symbol-table enrichment needs to know which table and which column.
        let sym: SymbolFieldSpec = serde_json::from_value(json!({ "table": "schema", "field": "table_name" })).unwrap();
        assert_eq!(sym.table, "schema");
        assert!(sym.of.is_none(), "omitting `of` = act on the current value");
        assert!(serde_json::from_value::<SymbolFieldSpec>(json!({ "table": "schema" })).is_err());

        let l: LinkSpec = serde_json::from_value(json!({})).unwrap();
        assert_eq!(l.kind, EdgeKind::default(), "an undeclared kind falls back to Unknown, not a random kind");
        assert!(l.to.is_none() && l.to_method.is_none() && l.to_fallback.is_none() && l.confidence.is_none());
        let l2: LinkSpec = serde_json::from_value(json!({
            "kind": "HandledBy",
            "to": { "arg": 1 },
            "to_method": { "expand_entry": true },
            "to_fallback": { "owner_class": true },
            "direction": "to_target",
            "resolve": "method_ref",
            "confidence": 0.8
        }))
        .unwrap();
        assert_eq!(l2.to_method.is_some(), true);
        assert_eq!(l2.direction, Direction::ToTarget);
        assert_eq!(l2.resolve, Some(ResolveAs::MethodRef));

        let a: AliasSpec = serde_json::from_value(json!({})).unwrap();
        assert!(a.namespace.is_empty() && a.qualifier.is_none());
        assert_eq!(a.key.arg, None, "key is itself a ValueSource, all-None by default");
        let a2: AliasSpec =
            serde_json::from_value(json!({ "namespace": "event", "key": { "arg": 0 }, "qualifier": { "method_name": true } }))
                .unwrap();
        assert_eq!(a2.namespace, "event");
        assert!(a2.qualifier.is_some());

        let v: ExpandVariant = serde_json::from_value(json!({})).unwrap();
        assert!(v.name.is_empty() && v.method.is_none() && v.path_suffix.is_none() && v.entry.is_none());
        let v2: ExpandVariant = serde_json::from_value(json!({
            "name": "read", "method": "GET", "path_suffix": "/:id", "entry": "read"
        }))
        .unwrap();
        assert_eq!(v2.name, "read");
        assert_eq!(v2.method.as_deref(), Some("GET"));
        assert_eq!(v2.path_suffix.as_deref(), Some("/:id"));
        assert_eq!(v2.entry.as_deref(), Some("read"));
    }

    /// `identity` is what makes several rules merge onto one node, so the old habit of writing every key as
    /// `Option` applies: only `kind` has to be there, `value_fallback` / `normalize` are opt-in.
    #[test]
    fn identity_spec_defaults_and_takes_an_explicit_fallback() {
        let i: IdentitySpec = serde_json::from_value(json!({ "kind": "fqn", "value": { "arg": 0 } })).unwrap();
        assert!(i.kind.eq_ignore_ascii_case(SynthesizedKind::FQN), "kind is an open string, matched case-insensitively");
        assert!(i.value.is_some());
        assert!(i.method.is_none() && i.path.is_none());
        assert!(i.normalize.is_empty(), "no declared normalize = used verbatim");
        assert!(i.value_fallback.is_none());

        let full: IdentitySpec = serde_json::from_value(json!({
            "kind": "ContractId",
            "method": { "method_name": true },
            "path": { "arg": 0, "normalize": ["leading_slash"] },
            "normalize": ["param_wildcard"],
            "value_fallback": { "owner_class": true }
        }))
        .unwrap();
        assert!(full.kind.eq_ignore_ascii_case(SynthesizedKind::CONTRACT_ID));
        assert!(full.method.is_some() && full.path.is_some());
        assert_eq!(full.normalize, vec![NormalizeStep::ParamWildcard]);
        assert!(full.value_fallback.is_some());

        assert!(serde_json::from_value::<IdentitySpec>(json!({ "kind": "fqn", "valuee": { "arg": 0 } })).is_err());
    }

    /// Every sub-kind source must sit **under `subkind:`**. Two CRMEB / ThinkPHP declarations once sat directly
    /// on the annotation spec (`computed:` / `from_symbol_table:` at the wrong level) and were silently
    /// ignored; keeping the four shapes here pins both the correct nesting and the rejection.
    #[test]
    fn subkind_sources_must_be_nested_under_subkind() {
        for nested in [
            json!({ "literal": "entrypoint.login" }),
            json!({ "from_symbol_table": { "table": "schema", "field": "table_name" } }),
            json!({ "from_fan_in": { "thresholds": { "high": 20, "medium": 5 } } }),
            json!({ "computed": "missing_locales" }),
        ] {
            let s: SubkindSource =
                serde_json::from_value(nested.clone()).unwrap_or_else(|e| panic!("{nested} must deserialize: {e}"));
            assert!(matches!(
                s,
                SubkindSource::Literal(_)
                    | SubkindSource::FromSymbolTable { .. }
                    | SubkindSource::FromFanIn { .. }
                    | SubkindSource::Computed(_)
            ));
        }

        let spec: AnnotationSpec =
            serde_json::from_value(json!({ "kind": "i18n.missing_locale", "subkind": { "computed": "missing_locales" } }))
                .unwrap();
        assert!(matches!(spec.subkind, Some(SubkindSource::Computed(ref c)) if c == "missing_locales"));
        assert!(serde_json::from_value::<AnnotationSpec>(json!({ "kind": "k", "computed": "missing_locales" })).is_err());
        assert!(serde_json::from_value::<AnnotationSpec>(json!({ "kind": "k", "from_symbol_table": { "table": "schema", "field": "t" } })).is_err());

        // A fan-in grading needs both boundaries; the labels are opt-in.
        let th: FanInThresholds = serde_json::from_value(json!({ "high": 20, "medium": 5, "high_label": "hot" })).unwrap();
        assert_eq!((th.high, th.medium), (20, 5));
        assert_eq!(th.high_label.as_deref(), Some("hot"));
        assert!(th.low_label.is_none() && th.medium_label.is_none());
        assert!(serde_json::from_value::<FanInThresholds>(json!({ "high": 20 })).is_err());
        assert!(serde_json::from_value::<SubkindSource>(json!({ "from_fan_in": {} })).is_err());
    }

    /// `Action` is **externally** tagged with `PascalCase` keys (`Annotate` / `Synthesize` / `Link` /
    /// `Project`) — the one enum whose tag differs from the rest of the file's snake_case.
    #[test]
    fn action_variants_are_pascal_case_tagged() {
        for v in [
            json!({ "Annotate": {} }),
            json!({ "Synthesize": {} }),
            json!({ "Link": {} }),
            json!({ "Project": {} }),
        ] {
            let a: Action = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{v} must deserialize: {e}"));
            assert!(matches!(
                a,
                Action::Annotate(_) | Action::Synthesize(_) | Action::Link(_) | Action::Project(_)
            ));
        }
        assert!(
            serde_json::from_value::<Action>(json!({ "annotate": {} })).is_err(),
            "the tag is PascalCase"
        );

        let s: Action = serde_json::from_value(json!({
            "Synthesize": { "node": "Table", "identity": { "kind": "named", "value": { "arg": 0 } }, "confidence": 0.95 }
        }))
        .unwrap();
        match s {
            Action::Synthesize(a) => {
                assert_eq!(a.node.as_str(), "Table");
                assert!(a.identity.kind.eq_ignore_ascii_case(SynthesizedKind::NAMED));
                assert!((a.confidence - 0.95).abs() < 1e-6);
            }
            _ => panic!("expected a Synthesize action"),
        }

        // An annotate target may be derived instead of "whatever matched".
        let t: AnnotateTarget =
            serde_json::from_value(json!({ "from_field": { "source": { "arg": 0 }, "resolve": "class_const" } })).unwrap();
        assert!(matches!(t, AnnotateTarget::FromField { resolve: Some(ResolveAs::ClassConst), .. }));
        let r: AnnotateTarget = serde_json::from_value(json!({ "synthesized_ref": "node-1" })).unwrap();
        assert!(matches!(r, AnnotateTarget::SynthesizedRef(ref id) if id == "node-1"));
    }

    /// `Link` / `Project` are the two edge-only actions. `Project` walks a hop chain, so its `from` / `to`
    /// chains are **lists** that default to empty (= "the landing point is the endpoint itself").
    #[test]
    fn link_and_project_actions_defaults_and_roundtrip() {
        let l: LinkAction = serde_json::from_value(json!({})).unwrap();
        assert_eq!(l.kind, EdgeKind::default());
        assert!(l.from.is_none() && l.to.is_none() && l.resolve.is_none() && l.confidence.is_none());
        let l2: LinkAction = serde_json::from_value(json!({
            "kind": "HandledBy", "from": { "self": true }, "to": { "arg": 1 },
            "resolve": "method_ref", "confidence": 0.8
        }))
        .unwrap();
        assert_eq!(l2.kind.as_str(), "HandledBy");
        assert_eq!(l2.from.as_ref().and_then(|s| s.self_value), Some(true));
        assert_eq!(l2.resolve, Some(ResolveAs::MethodRef));

        let p: ProjectAction = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.kind, EdgeKind::default());
        assert_eq!(p.along, EdgeKind::default());
        assert!(
            p.from.is_empty() && p.to.is_empty(),
            "an empty chain = the landing point is the edge's endpoint itself"
        );
        let p2: ProjectAction = serde_json::from_value(json!({
            "kind": "HasForeignKey", "along": "MapsTo", "from": ["Extends"], "to": ["MapsTo"], "confidence": 0.7
        }))
        .unwrap();
        assert_eq!(p2.along.as_str(), "MapsTo");
        assert_eq!(p2.from, vec!["Extends".to_string()]);
        assert_eq!(p2.to, vec!["MapsTo".to_string()]);
        assert!((p2.confidence.unwrap() - 0.7).abs() < 1e-6);
        assert!(serde_json::from_value::<ProjectAction>(json!({ "alongs": "MapsTo" })).is_err());
    }

    /// Every selector variant must keep a parseable spelling **and** reject keys that belong to another
    /// variant (that is what `deny_unknown_fields` buys: a `where` nested in the wrong place is a load
    /// error instead of a silently-ignored condition). The positive rows double as an inventory: adding a
    /// variant to `Selector` means adding a row here **and** a matcher in `engine.rs`.
    #[test]
    fn selector_variants_roundtrip_and_reject_foreign_keys() {
        // ── Positive: one row per variant, every field asserted (including `config_entry.where`, the one
        // field this test used to skip even though `matches_config` reads it).
        let call: Selector = serde_json::from_value(json!({
            "kind": "call", "callee": "Db::name", "where": [{ "arg_count": 1 }]
        }))
        .expect("call selector must deserialize");
        match call {
            Selector::Call { callee, r#where } => {
                assert_eq!(callee.as_deref(), Some("Db::name"));
                assert_eq!(r#where.len(), 1);
            }
            _ => panic!("expected a Call selector"),
        }

        let inherit: Selector = serde_json::from_value(json!({
            "kind": "inheritance", "base": "Model", "with_property": "table"
        }))
        .expect("inheritance selector must deserialize");
        match inherit {
            Selector::Inheritance { base, with_property } => {
                assert_eq!(base.as_deref(), Some("Model"));
                assert_eq!(with_property.as_deref(), Some("table"));
            }
            _ => panic!("expected an Inheritance selector"),
        }

        let cfg: Selector = serde_json::from_value(json!({
            "kind": "config_entry",
            "file": "config/database.php",
            "key_path": "connections.*",
            "where": [{ "name_matches": "mysql" }]
        }))
        .expect("config_entry selector must deserialize");
        match cfg {
            Selector::ConfigEntry { file, key_path, r#where } => {
                assert_eq!(file.as_deref(), Some("config/database.php"));
                assert_eq!(key_path.as_deref(), Some("connections.*"));
                assert_eq!(r#where.len(), 1, "`config_entry` carries `where` predicates too");
            }
            _ => panic!("expected a ConfigEntry selector"),
        }

        // Every field is optional: a bare `kind` must deserialize to the all-default variant.
        let bare: Selector =
            serde_json::from_value(json!({ "kind": "node" })).expect("a bare node selector must deserialize");
        match bare {
            Selector::Node { node_kind, r#where } => {
                assert!(node_kind.is_none());
                assert!(r#where.is_empty());
            }
            _ => panic!("expected a Node selector"),
        }

        // ── Negative: the `kind` tag is mandatory, and a key belonging to another variant must be rejected.
        assert!(
            serde_json::from_value::<Selector>(json!({ "node_kind": "Class" })).is_err(),
            "a selector without `kind` must be rejected"
        );
        assert!(
            serde_json::from_value::<Selector>(json!({ "kind": "call", "node_kind": "Class" })).is_err(),
            "`node_kind` belongs to the node selector, not the call selector"
        );
        assert!(
            serde_json::from_value::<Selector>(json!({ "kind": "inheritance", "where": [] })).is_err(),
            "`where` exists on call / config_entry / node only — not on inheritance"
        );
        assert!(
            serde_json::from_value::<Selector>(json!({ "kind": "config_entry", "callee": "Db::name" })).is_err(),
            "`callee` belongs to the call selector"
        );
        assert!(
            serde_json::from_value::<Selector>(json!({ "kind": "node", "base": "Model" })).is_err(),
            "`base` belongs to the inheritance selector"
        );

        // A misspelled field name must not degrade into an all-default selector that matches everything.
        for (kind, typo) in [
            ("config_entry", json!({ "keypath": "connections.*" })),
            ("inheritance", json!({ "with_properties": ["table"] })),
            ("call", json!({ "callees": "Db::name" })),
            ("node", json!({ "node_kinds": ["Class"] })),
        ] {
            let mut obj = json!({ "kind": kind });
            obj.as_object_mut()
                .unwrap()
                .extend(typo.as_object().unwrap().clone().into_iter());
            assert!(
                serde_json::from_value::<Selector>(obj).is_err(),
                "a misspelled field on the `{kind}` selector must be rejected"
            );
        }

        // The two variants removed for having no matcher must stay rejected (rather than being silently
        // accepted and never firing).
        for dead in ["declaration", "dynamic"] {
            assert!(
                serde_json::from_value::<Selector>(json!({ "kind": dead })).is_err(),
                "`{dead}` was removed — it must not parse"
            );
        }
    }

    /// The **write** direction is part of the contract too: the `kind` tag must stay snake_case and the
    /// `r#where` field must be emitted as `where`, otherwise a re-serialised rule is no longer readable.
    #[test]
    fn selector_serialises_its_kind_tag_and_where_field() {
        let cases: Vec<(Selector, &str)> = vec![
            (
                Selector::Call { callee: Some("Db::name".into()), r#where: vec![] },
                "call",
            ),
            (
                Selector::Inheritance { base: Some("Model".into()), with_property: None },
                "inheritance",
            ),
            (
                Selector::ConfigEntry { file: None, key_path: None, r#where: vec![] },
                "config_entry",
            ),
            (Selector::Node { node_kind: None, r#where: vec![] }, "node"),
        ];
        for (sel, kind) in cases {
            let written = serde_json::to_value(&sel).expect("selector must serialize");
            assert_eq!(written["kind"], kind, "the `kind` tag must stay snake_case");
            let back: Selector = serde_json::from_value(written).expect("a written selector must be readable");
            assert_eq!(
                std::mem::discriminant(&back),
                std::mem::discriminant(&sel),
                "`{kind}` must round-trip as the same variant"
            );
        }

        let with_pred = serde_json::to_value(&Selector::Node {
            node_kind: None,
            r#where: vec![Predicate::HasProperty("dao".into())],
        })
        .expect("node selector must serialize");
        assert!(with_pred.get("where").is_some(), "`r#where` must be written as `where`");
        assert!(with_pred.get("r#where").is_none());
    }

    /// Every predicate variant must keep a parseable YAML spelling. The list doubles as an inventory: adding
    /// a variant to `Predicate` means adding one row here.
    #[test]
    fn every_predicate_variant_has_a_yaml_spelling() {
        let cases: Vec<Value> = vec![
            json!({ "has_property": "dao" }),
            json!({ "in_symbol_table": { "table": "schema", "key_of": { "method_name": true } } }),
            json!({ "columns_match": { "table": "schema", "names": ["id", "name"] } }),
            json!({ "has_annotation": { "kind": "pii" } }),
            json!({ "none_of_capability": ["Authentication"] }),
            json!({ "has_missing": true }),
            json!({ "fan_in_gte": 5 }),
            json!({ "arg_count": 2 }),
            json!({ "name_matches": "Service" }),
            json!({ "property_is": { "name": "side", "value": "frontend" } }),
            json!({ "arg_starts_with": { "arg": 0, "prefix": "crontab/" } }),
            json!({ "entry_arity_gte": 1 }),
            json!({ "fqn_matches": "controller" }),
            json!({ "name_not_in": ["__construct", "initialize"] }),
            json!({ "not_claimed_by": "HandledBy" }),
            json!({ "has_incoming": "CallsHttp" }),
        ];
        for c in cases {
            let p: Predicate = serde_json::from_value(c.clone()).unwrap_or_else(|e| panic!("{c} must deserialize: {e}"));
            assert!(format!("{p:?}").contains(&format!("{p:?}")[..1]));
        }
        // `key_of` carries no default — an `in_symbol_table` without it is a writing mistake.
        assert!(serde_json::from_value::<Predicate>(json!({ "in_symbol_table": { "table": "schema" } })).is_err());
    }

    /// The taint / sign / CRUD vocabularies: a stack that declares nothing is simply not judged, so an empty
    /// struct must stay empty rather than pick up a default list.
    #[test]
    fn taint_sign_and_db_verbs_vocabularies_roundtrip() {
        let t: TaintSpec = serde_json::from_value(json!({
            "raw_sql_sinks": [ { "method": "query", "receiver": "Db" }, { "method": "whereRaw" } ],
            "where_interp_sinks": ["where"],
            "request_sources": ["$_GET", "input("]
        }))
        .unwrap();
        assert_eq!(t.raw_sql_sinks[0].receiver.as_deref(), Some("Db"));
        assert!(t.raw_sql_sinks[1].receiver.is_none(), "no declared receiver = any receiver counts");
        assert_eq!(t.where_interp_sinks, vec!["where".to_string()]);
        assert!(
            serde_json::from_value::<TaintSink>(json!({ "receiver": "Db" })).is_err(),
            "a sink's method is required"
        );
        assert!(TaintSpec::default().raw_sql_sinks.is_empty());

        let s: SignCheckSpec = serde_json::from_value(json!({
            "hash_calls": ["md5"], "weak_algos": ["sha1"], "name_contains": "sign",
            "name_excludes": ["signin"], "value_hints": ["sign"], "value_hints_require_compare": ["key="]
        }))
        .unwrap();
        assert_eq!(s.hash_calls, vec!["md5".to_string()]);
        assert_eq!(s.name_contains.as_deref(), Some("sign"));
        assert!(SignCheckSpec::default().name_contains.is_none(), "undeclared = do not guess by name");

        let d: DbVerbsSpec = serde_json::from_value(json!({ "write": ["save"], "read": ["find"] })).unwrap();
        assert_eq!(d.write, vec!["save".to_string()]);
        assert_eq!(d.read, vec!["find".to_string()]);
        assert!(DbVerbsSpec::default().write.is_empty() && DbVerbsSpec::default().read.is_empty());
        assert!(serde_json::from_value::<DbVerbsSpec>(json!({ "writes": ["save"] })).is_err());
    }

    /// A capability declaration is a **pair** — matching alone says what to look at but not what it provides.
    #[test]
    fn middleware_capability_requires_both_halves() {
        let m: MiddlewareCapability =
            serde_json::from_value(json!({ "matches": "AuthToken", "capability": "Authentication" })).unwrap();
        assert_eq!(m.matches, "AuthToken");
        assert_eq!(m.capability, "Authentication");
        assert!(serde_json::from_value::<MiddlewareCapability>(json!({ "matches": "AuthToken" })).is_err());
        assert!(serde_json::from_value::<MiddlewareCapability>(json!({ "capability": "Authentication" })).is_err());
        assert!(
            serde_json::from_value::<MiddlewareCapability>(json!({ "matches": "AuthToken", "capabilities": "Authentication" }))
                .is_err()
        );
    }

    /// `route_guards` collects one framework's several attachment forms; `specs()` is what turns them into the
    /// union the kernel walks. `synthesize_unresolved` defaults to **false** because fabricating a middleware
    /// node for a class that is not in the graph is guessing.
    #[test]
    fn route_guard_spec_collects_several_attachment_forms() {
        let spec: RouteGuardSpec = serde_json::from_value(json!({
            "route_calls": [ { "receiver": "Route", "verb_methods": { "get": "GET" } } ],
            "guard_attach": [
                { "kind": "chain", "method": "middleware", "arg_index": 0 },
                { "kind": "positional" },
                { "kind": "decorator", "guard_decorators": ["@UseGuards"] },
                { "kind": "consumer" }
            ],
            "alias_table": "middleware_aliases",
            "synthesize_unresolved": true
        }))
        .unwrap();
        assert_eq!(spec.route_calls.len(), 1);
        assert_eq!(spec.guard_attach.specs().len(), 4, "the different attach forms are unioned");
        assert_eq!(spec.alias_table.as_deref(), Some("middleware_aliases"));
        assert!(spec.synthesize_unresolved);

        let empty: RouteGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert!(empty.route_calls.is_empty());
        assert!(matches!(empty.guard_attach, GuardAttach::One(GuardAttachSpec::Positional)));
        assert!(!empty.synthesize_unresolved, "by default the node of an unknown guard is not guessed");
        assert!(empty.alias_table.is_none());
        assert!(serde_json::from_value::<RouteGuardSpec>(json!({ "guard_attachs": [] })).is_err());
    }

    /// The two structured guard specs: `chain`'s argument index defaults to **0**, and `decorator`'s two
    /// boolean defaults are **true** (`name_from_args` / `include_class_level`) — flipping either silently
    /// loses `@UseGuards(X)` or every class-level guard.
    #[test]
    fn chain_and_decorator_guard_defaults() {
        let c: ChainGuardSpec = serde_json::from_value(json!({})).unwrap();
        assert!(c.method.is_empty(), "the attach method name must be declared by the FKB");
        assert_eq!(c.arg_index, 0);
        assert!(c.arg2_index.is_none());
        let c2: ChainGuardSpec =
            serde_json::from_value(json!({ "method": "middleware", "arg_index": 1, "arg2_index": 2 })).unwrap();
        assert_eq!(c2.method, "middleware");
        assert_eq!((c2.arg_index, c2.arg2_index), (1, Some(2)));

        let d = DecoratorGuardSpec::default();
        assert!(d.route_decorators.is_empty() && d.guard_decorators.is_empty());
        assert!(d.guard_name_patterns.is_empty() && d.guard_exclude_patterns.is_empty());
        assert!(d.name_from_args, "by default the guard class is taken from the arguments (`@UseGuards(X)`)");
        assert!(d.include_class_level, "class-level guards are recognised by default too");
        assert!(!d.require_at_prefix, "by default the callee need not carry an `@` prefix");
        assert!(!d.require_no_receiver);
        assert!(d.link_via_handler_arg.is_none());

        let d2: DecoratorGuardSpec = serde_json::from_value(json!({
            "name_from_args": false,
            "include_class_level": false,
            "require_at_prefix": true,
            "link_via_handler_arg": 1,
            "guard_name_patterns": ["(login|auth)"],
            "guard_exclude_patterns": ["ApiBearerAuth"]
        }))
        .unwrap();
        assert!(!d2.name_from_args, "in Spring's `@PreAuthorize` the guard is the annotation itself");
        assert!(!d2.include_class_level);
        assert!(d2.require_at_prefix);
        assert_eq!(d2.link_via_handler_arg, Some(1));
        assert_eq!(d2.guard_name_patterns, vec!["(login|auth)".to_string()]);
        assert_eq!(d2.guard_exclude_patterns, vec!["ApiBearerAuth".to_string()]);
    }

    /// A resolver names **which calls are dynamic** and **how**; the starting tier is strategy-specific, so
    /// omitting `from_tier` must leave it unset rather than default to Exact.
    #[test]
    fn resolver_spec_defaults_and_rejects_unknown_keys() {
        let r: ResolverSpec = serde_json::from_value(json!({ "id": "container-make", "strategy": "container" })).unwrap();
        assert_eq!(r.id, "container-make");
        assert_eq!(r.strategy, ResolveStrategy::Container);
        assert!(r.call.is_none());
        assert!(r.from_tier.is_none(), "the starting tier is decided by the strategy, it must not default");

        let r2: ResolverSpec = serde_json::from_value(json!({
            "id": "facade", "call": "Cache::*", "strategy": "facade", "from_tier": "alias"
        }))
        .unwrap();
        assert_eq!(r2.call.as_deref(), Some("Cache::*"));
        assert_eq!(r2.from_tier, Some(ResolveTier::Alias));

        assert!(
            serde_json::from_value::<ResolverSpec>(json!({ "id": "x" })).is_err(),
            "a missing strategy must error"
        );
        assert!(
            serde_json::from_value::<ResolverSpec>(json!({ "id": "x", "strategy": "container", "stratergy": "event" }))
                .is_err(),
            "a misspelled key must not be silently ignored"
        );
    }
}
