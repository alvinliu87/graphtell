//! P3 Prepare: load FKB and authoritative symbol tables.
//!
//! * use FKB's `detectors` to identify which frameworks each sub-project uses
//! * use `root_rules` to parse `AppRoot` (e.g. from `composer.json`'s `autoload.psr-4`)
//! * use `loaders` to load authoritative sources: `schema` / `config_keys` / `i18n` / `facade_map`
//!   / `container_bindings` / `event_listeners` / `route_list` / `nginx`

use gt_domain::error::Result;
use gt_domain::model::{KnowledgeScope, Language, Phase, Rule};
use gt_domain::port::{
    FileSystem, KnowledgeProvider, ParserRegistry, TechStackRegistry,
};
use serde_json::{json, Value};
use tracing::info;

use crate::context::PipelineContext;

/// Run Prepare.
pub fn run(
    ctx: &mut PipelineContext,
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Result<()> {
    let phase = Phase(Phase::PREPARE.to_string());
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    // Union of frameworks recognized across all sub-projects — decides which framework rules qualify for the global rule set.
    let mut detected_frameworks: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sub in &subs {
        // Framework-level + project-level knowledge recognized separately: project-level loads only when that sub-project is recognized as the corresponding project,
        // its rules only enter `rules_by_sub` (not `ctx.frameworks`, not global), never leaking into other projects.
        let evidence = collect_code_evidence(ctx, sub);
        let frameworks = detect_frameworks(
            kb,
            fs,
            sub,
            &project_root,
            KnowledgeScope::Framework,
            &evidence,
            techstack,
        );
        info!("sub-project {} recognized framework: {:?}", sub.name, frameworks);
        detected_frameworks.extend(frameworks.iter().cloned());
        ctx.frameworks.insert(sub.id.get(), frameworks.clone());

        let projects = detect_frameworks(
            kb,
            fs,
            sub,
            &project_root,
            KnowledgeScope::Project,
            &evidence,
            techstack,
        );
        if !projects.is_empty() {
            info!("sub-project {} recognized project knowledge: {:?}", sub.name, projects);
        }

        // Route-handler resolution rules + consumer entry-method names: framework-level first, then project-level.
        // Both declared by FKB (how each framework writes handlers / what entry methods are called is framework knowledge).
        if let Some(mut spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.method_ref.clone())
        {
            // Derive `root_namespaces` / `app_segments` from the tech-stack adapter (PSR-4 autoload for PHP,
            // the equivalent manifest for other stacks; no controller-dir name assumed — the resolver learns
            // the controller location from the real class FQNs).
            if let Some(adapter) = techstack.adapter_for(&sub.language) {
                adapter.enrich_method_ref(&mut spec, &sub.root_path, &project_root);
            }
            if ctx.method_ref_spec_default.is_none() {
                ctx.method_ref_spec_default = Some(spec.clone());
            }
            ctx.method_ref_specs.insert(sub.id.get(), spec);
        }
        // `@method` magic-method forwarding target: also FKB knowledge (which property to forward to is a framework/project convention).
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.magic_delegation.clone())
            .filter(|s| !s.property.is_empty())
        {
            if ctx.magic_delegation_default.is_none() {
                ctx.magic_delegation_default = Some(spec.clone());
            }
            ctx.magic_delegation.insert(sub.id.get(), spec);
        }
        // Data-model read / write verbs: also FKB knowledge (what the framework's Model/Query API is called).
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.db_verbs.clone())
            .filter(|s| !s.write.is_empty() || !s.read.is_empty())
        {
            if ctx.db_verbs_default.is_none() {
                ctx.db_verbs_default = Some(spec.clone());
            }
            ctx.db_verbs.insert(sub.id.get(), spec);
        }
        // Signature-verification vocabulary: which calls compute a signature and which algorithms are weak is
        // language / library knowledge, collected the same way as the lists below — a stack that declares none
        // is not judged.
        //
        // `apply_without_detection` **must** be included: the unconditional language layer (`fkb/php/common.yaml`)
        // is where PHP's vocabulary lives, and it is never in `frameworks` because it has no detectors. Reading
        // only `frameworks` would silently disable P11 for every PHP project.
        if let Some(spec) = kb
            .all()
            .iter()
            .filter(|fk| {
                fk.language == sub.language
                    && (frameworks.contains(&fk.id)
                        || projects.contains(&fk.id)
                        || fk.apply_without_detection)
            })
            .find_map(|fk| fk.sign_check.clone())
        {
            if ctx.sign_check_default.is_none() {
                ctx.sign_check_default = Some(spec.clone());
            }
            ctx.sign_check.insert(sub.id.get(), spec);
        }
        // SQL-injection vocabulary (P9 Taint), collected the same way and under the same
        // `apply_without_detection` rule — see `sign_check` above for why that arm is not optional.
        if let Some(spec) = kb
            .all()
            .iter()
            .filter(|fk| {
                fk.language == sub.language
                    && (frameworks.contains(&fk.id)
                        || projects.contains(&fk.id)
                        || fk.apply_without_detection)
            })
            .find_map(|fk| fk.taint.clone())
        {
            if ctx.taint_default.is_none() {
                ctx.taint_default = Some(spec.clone());
            }
            ctx.taint.insert(sub.id.get(), spec);
        }
        // These three lists are collected only from knowledge that **actually applies** to this sub-project:
        // a recognised framework / project, or the unconditional language layer (`apply_without_detection`).
        //
        // Merging them from **every** FKB of the language with no detector consulted would let a library
        // contribute its entries to projects that never went near it — `GuzzleHttp\Client::request` landing
        // in every PHP project's outbound-call list, `Db::transaction` in Laravel's.
        //
        // The `apply_without_detection` arm is not optional: that layer holds the genuinely
        // framework-independent entries (`curl_exec`, `Http::get`, `transaction`, `commit` …), and it is
        // never in `frameworks` because it has no detectors. Dropping it would silently disable every rule
        // that reads these lists.
        //
        // Measured safe on the real samples (6 PHP projects, `tests/detector_coverage.rs`): zero cases where
        // the code used a library and its detectors did not fire.
        for fk in kb.all() {
            if fk.language != sub.language {
                continue;
            }
            let applies = frameworks.contains(&fk.id)
                || projects.contains(&fk.id)
                || fk.apply_without_detection;
            if !applies {
                continue;
            }
            for c in &fk.external_calls {
                if !ctx.external_calls.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    ctx.external_calls.push(c.clone());
                }
            }
            // Transaction markers are collected **per sub-project**, not globally: P13 needs "what does a
            // transaction look like in *this* stack", and a stack that declares none must not be judged
            // against another stack's markers (see `PipelineContext::tx_calls`).
            for c in &fk.tx_calls {
                let markers = ctx.tx_calls.entry(sub.id.get()).or_default();
                if !markers.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    markers.push(c.clone());
                }
            }
            if ctx.tx_calls_default.is_empty() {
                let cur = ctx.tx_calls.get(&sub.id.get()).cloned().unwrap_or_default();
                if !cur.is_empty() {
                    ctx.tx_calls_default = cur;
                }
            }
            // "middleware class → capability": names are framework/project conventions (`AuthTokenMiddleware`-style
            // naming the kernel shouldn't know), so also declared by FKB, here only dedup-merge.
            for mc in &fk.middleware_capabilities {
                if !ctx
                    .middleware_capabilities
                    .iter()
                    .any(|x| x.capability == mc.capability && x.matches == mc.matches)
                {
                    ctx.middleware_capabilities.push(mc.clone());
                }
            }
        }
        if let Some(methods) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find(|fk| !fk.entry_methods.is_empty())
            .map(|fk| fk.entry_methods.clone())
        {
            if ctx.entry_methods_default.is_empty() {
                ctx.entry_methods_default = methods.clone();
            }
            ctx.entry_methods.insert(sub.id.get(), methods);
        }

        // Framework + project knowledge both apply root_rules / loaders (project knowledge usually none, but the channel is generic).
        let mut facts = serde_json::Map::new();
        for id in frameworks.iter().chain(projects.iter()) {
            let Some(fk) = kb.by_id(id) else { continue };
            apply_root_rules(ctx, fk, sub, &mut facts, &phase, fs, parsers, techstack);
            // Table prefix(es). A `db_connections` list fact covers **every** configured connection
            // (read/write splitting, several databases): all non-empty prefixes are merged into
            // `table_prefixes` (a Vec, deduped), and `db_prefix` records the default connection's —
            // the single value the old `connections.mysql.prefix` rule used to hard-code, which silently
            // cost any project whose connection is not named `mysql` its whole column-level schema.
            if let Some(connections) = facts.get("db_connections").cloned() {
                let default_name = connections
                    .get("default")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let empty: Vec<Value> = Vec::new();
                let entries = connections
                    .get("value")
                    .and_then(|v| v.as_array())
                    .unwrap_or(&empty);
                let mut prefixes = ctx.ws.table_prefixes().to_vec();
                let mut first_non_empty: Option<String> = None;
                let mut default_prefix: Option<String> = None;
                for entry in entries {
                    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let prefix = entry.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
                    if prefix.is_empty() {
                        continue;
                    }
                    if !prefixes.iter().any(|p| p == prefix) {
                        prefixes.push(prefix.to_string());
                    }
                    if first_non_empty.is_none() {
                        first_non_empty = Some(prefix.to_string());
                    }
                    if default_name.as_deref() == Some(name) {
                        default_prefix = Some(prefix.to_string());
                    }
                }
                ctx.ws.set_table_prefixes(prefixes);
                if let Some(p) = default_prefix.or(first_non_empty) {
                    facts.insert(
                        "db_prefix".to_string(),
                        json!({
                            "value": p,
                            "confidence": 1.0,
                            "source": "db_connections (default connection)",
                            "fallback_used": false,
                        }),
                    );
                }
            }
            // Auto-detect table prefix: FKB's `db_prefix` root_rule reads it from project config,
            // merged with the prefix from the project's explicit config (dedup), for P3 loading and P5 normalization.
            if let Some(v) = facts
                .get("db_prefix")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
            {
                let mut prefixes = ctx.ws.table_prefixes().to_vec();
                if !prefixes.iter().any(|p| p == v) {
                    prefixes.push(v.to_string());
                }
                ctx.ws.set_table_prefixes(prefixes);
            }
        }
        // Authoritative symbol tables, under the same "does this knowledge apply" test as the lists above.
        // The `apply_without_detection` arm is what makes a **framework-independent** loader possible: an
        // unconditional FKB is never "detected", so without this arm its loaders never run and such a loader
        // would have to hide inside a framework file — costing every other framework the ability to read it.
        for fk in kb.all() {
            if fk.language != sub.language && fk.language.0 != "*" {
                continue;
            }
            let applies = frameworks.contains(&fk.id)
                || projects.contains(&fk.id)
                || fk.apply_without_detection;
            if !applies {
                continue;
            }
            run_loaders(ctx, fk, sub, &project_root, fs, parsers, &phase, techstack);
        }

        let facts_value = Value::Object(facts);
        for (k, v) in facts_value.as_object().cloned().unwrap_or_default() {
            ctx.ws.set_fact(sub.id, &k, v);
        }

        let mut rules: Vec<Rule> = frameworks
            .iter()
            .filter_map(|id| kb.by_id(id))
            .flat_map(|fk| fk.rules.iter().cloned())
            .collect();
        for fk in kb.all() {
            if fk.scope == KnowledgeScope::Framework
                && (fk.language == sub.language || fk.language.0 == "*")
                && !frameworks.contains(&fk.id)
                && fk.apply_without_detection
            {
                for r in &fk.rules {
                    if r.applies_to(&fk.language, &sub.language) {
                        rules.push(r.clone());
                    }
                }
            }
        }
        for id in &projects {
            if let Some(fk) = kb.by_id(id) {
                rules.extend(fk.rules.iter().cloned());
            }
        }
        ctx.rules_by_sub.insert(sub.id.get(), dedup_rules(rules));
    }

    let mut global: Vec<(Language, Rule)> = Vec::new();
    for fk in kb.all() {
        if fk.scope != KnowledgeScope::Framework {
            continue;
        }
        if !detected_frameworks.contains(&fk.id) && !fk.apply_without_detection {
            continue;
        }
        for r in &fk.rules {
            if !global.iter().any(|( _, g)| g.id == r.id) {
                global.push((fk.language.clone(), r.clone()));
            }
        }
    }
    ctx.rules_global = global;

    Ok(())
}

// The P3 prepare phase is split into focused submodules, one per concern, so this file stays a readable
// map of the load pipeline rather than one ~2.6k-line wall of functions.
mod framework_detection; // framework recognition from code evidence
mod approot; // AppRoot parsing from `root_rules`
mod loaders; // loader orchestration + i18n
mod builtin_loaders; // schema / config / route / middleware / nginx loaders
mod helpers; // text / SQL parsing helpers + path expansion

// Re-export every item so `run` (above) and the test module below keep resolving the helpers by
// bare name exactly as before the split.
pub(crate) use approot::*;
pub(crate) use builtin_loaders::*;
pub(crate) use framework_detection::*;
pub(crate) use helpers::*;
pub(crate) use loaders::*;

#[cfg(test)]
mod tests {
    use super::{collect_route_guards, guard_arg_name, guard_arg_text, load_schema, merge_schema_columns};
    use crate::workspace::{CallRecord, GraphWorkspace};
    use gt_domain::model::Phase;

    use super::{detect_frameworks, CodeEvidence};
    use crate::phase::facts::{expand_provided, lock_has, manifest_has};
    use super::{load_declared_middleware, PipelineContext};
    use super::{
        declared_mw_path_matches, match_route_call, norm_class, parse_create_tables,
        receiver_matches, short_callee, split_callee,
    };
    use super::{
        expand, load_config_keys, load_i18n, load_middleware_aliases, load_nginx, load_routes,
        resolve_guard_class,
    };
    use gt_adapter_fs::StdFileSystem;
    use gt_domain::port::{LanguageParser, ParserRegistry};
    use std::collections::HashMap;
    use serde_json::json;

    /// Dependency / lock layouts are ecosystem knowledge, so these tests need a real registry:
    /// composer.json / composer.lock are read by the PHP adapter, package-lock.json by the JS one.
    fn techstack() -> gt_domain::port::DefaultTechStackRegistry {
        gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()))
            .register(Box::new(gt_adapter_techstack::JsTechStackAdapter::new()))
    }
    use gt_domain::model::{
        ChainGuardSpec, ConfigEntryFact, ConsumerGuardSpec, ConsumerScope, Detector, FactValue,
        FileId, FrameworkKnowledge, GuardAttach, GuardAttachSpec, KnowledgeScope, Language,
        LoaderSource, LoaderSpec, NodeId, NormalizeStep, Project, ProjectId, ProjectStatus,
        RouteCallSpec, RouteGuardSpec, RouteMatchBy, SourceFile, Span, SubProject, SubProjectId,
        SyntaxFacts,
    };

    /// ThinkPHP 6's `route_guards` declaration (minimal set equivalent to `fkb/php/thinkphp.yaml`), for test reuse.
    fn tp6_spec() -> RouteGuardSpec {
        RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Route".into(),
                by: RouteMatchBy::Receiver,
                verb_methods: [
                    ("get".into(), "GET".into()),
                    ("post".into(), "POST".into()),
                    ("put".into(), "PUT".into()),
                    ("delete".into(), "DELETE".into()),
                    ("patch".into(), "PATCH".into()),
                    ("any".into(), "ANY".into()),
                    ("rule".into(), "ANY".into()),
                ]
                .into_iter()
                .collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: Some("group".into()),
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Chain(ChainGuardSpec {
                method: "middleware".into(),
                arg_index: 0,
                arg2_index: Some(1),
            })),
            alias_table: None,
            synthesize_unresolved: false,
        }
    }

    /// Build a call site: `Route::group('pc', fn)` / `Route::get('x','C@m')` / `->middleware(...)`.
    ///
    /// `byte` is this call's **start byte** in source: all call sites on the same chain share it
    /// (`member_call_expression` starts from the root source segment), this is the only basis for pairing.
    fn call(
        file: &str,
        receiver: &str,
        method: &str,
        args: Vec<FactValue>,
        start_line: u32,
        end_line: u32,
        byte: u32,
        end_byte: u32,
    ) -> CallRecord {
        CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "<file>".into(),
            owner_class: None,
            callee: format!("{}::{}", receiver, method),
            receiver: Some(receiver.into()),
            method: Some(method.into()),
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line,
                end_line,
                start_byte: byte,
                end_byte,
            },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        }
    }

    fn class(name: &str) -> FactValue {
        FactValue::ClassConst(name.into())
    }

    /// sample_project's real style: `Route::group('pc', fn){ ... })->middleware(A)->middleware(B, true)`,
    /// multiple routes inside the group. Middleware must land on **every** route.
    #[test]
    fn group_level_guard_covers_inner_routes() {
        let f = "app/api/route/pc.php";
        let calls = vec![
            // root: `Route::group(function(){...})` (no-prefix style, spans lines 1..3), byte=10
            call(f, "Route", "group", vec![], 1, 3, 10, 200),
            call(f, "Route", "get", vec![FactValue::String("a".into())], 2, 2, 40, 90),
            // the two `->middleware` on the chain: share start_byte=10 with the root, longer span
            call(
                f,
                "Route",
                "middleware",
                vec![class(r"app\http\middleware\AllowOriginMiddleware")],
                1,
                4,
                10,
                230,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(true),
                ],
                1,
                5,
                10,
                260,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "one chain must produce exactly one span");
        assert_eq!((scopes[0].start_line, scopes[0].end_line), (1, 3), "the span must be taken from the root");
        assert_eq!(scopes[0].guards.len(), 2, "both middleware must be collected");

        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 2);
        let names: Vec<&str> = got.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(
            names,
            vec![
                r"app\http\middleware\AllowOriginMiddleware",
                r"app\api\middleware\AuthTokenMiddleware",
            ]
        );
        assert_eq!(got[1].arg.as_deref(), Some("true"), "the second argument of `AuthToken::class, true` must be kept");
        // Routes outside the range shouldn't be guarded by it (this is exactly the "global vs group-level" boundary)
        assert!(ws.route_guards(f, 40).is_empty());
    }

    /// Route's own `->middleware(X::class, false)`: granularity down to a single route, the second arg only appears here.
    #[test]
    fn route_level_guard_keeps_force_flag() {
        let f = "app/api/route/user.php";
        let calls = vec![
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("info".into())],
                7,
                7,
                10,
                60,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(false),
                ],
                7,
                8,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1);
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 7);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].arg.as_deref(), Some("false"));
    }

    /// Laravel's **modifier-prefix** style: `Route::middleware('auth')->group(fn){...}`.
    /// The key is the root node must be `group` (only it wraps the route range), not the chain-head `middleware('auth')`.
    #[test]
    fn laravel_prefix_form_binds_to_group() {
        let f = "routes/web.php";
        let calls = vec![
            // chain head: `Route::middleware('auth')`, very short span (only this small segment)
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::String("auth".into())],
                1,
                1,
                10,
                40,
            ),
            // chain tail: `->group(fn){...}`, span covers the whole closure (lines 1~9)
            call(f, "Route", "group", vec![], 1, 9, 10, 400),
            // a route inside the group
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("profile".into())],
                3,
                3,
                90,
                130,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "the whole chain must produce exactly one span");
        assert_eq!(
            (scopes[0].start_line, scopes[0].end_line),
            (1, 9),
            "the span must be taken from the group (otherwise it cannot cover the routes inside it)"
        );
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 3);
        assert_eq!(got.len(), 1, "routes inside the group must be covered by this guard alias");
        assert_eq!(got[0].class, "auth");
    }

    /// Array-form multiple aliases (`->middleware(['auth', 'throttle:60'])`) must be collected separately.
    #[test]
    fn collects_array_form_guards() {
        let f = "routes/web.php";
        let calls = vec![
            call(f, "Route", "get", vec![FactValue::String("x".into())], 1, 1, 10, 60),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Array(vec![
                    ("0".into(), FactValue::String("auth".into())),
                    ("1".into(), FactValue::String("throttle:60".into())),
                ])],
                1,
                2,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes[0].guards.len(), 2, "every item in the array counts as one attach");
    }

    /// The same-name `->middleware()` on a non-Route receiver must not mis-match;
    /// non-scalar args (`->middleware($v)`) shouldn't produce pseudo-facts.
    #[test]
    fn ignores_foreign_and_dynamic_middleware() {
        let f = "app/Service.php";
        let calls = vec![
            call(f, "$router", "middleware", vec![class("app\\A")], 1, 2, 10, 30),
            call(f, "Route", "get", vec![FactValue::String("x".into())], 3, 3, 50, 80),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Unknown(Some("$mw".into()))],
                3,
                4,
                50,
                100,
            ),
        ];
        assert!(collect_route_guards(&tp6_spec(), &calls).is_empty());
        assert_eq!(guard_arg_text(&FactValue::Int(60)), Some("60".into()));
        assert_eq!(guard_arg_text(&FactValue::Null), None);
    }

    /// `declared_middleware`'s `paths` must honour the `root_rules`-resolved `app_root`, not a hard-coded
    /// `app/` — a ThinkPHP 5.x project whose psr-4 maps the app namespace to `application/` would otherwise
    /// never be scanned, silently losing every declarative middleware.
    #[test]
    fn declared_middleware_respects_app_root_fact_not_hardcoded_app() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        // Not `app/` — this project's app directory is `application/` (ThinkPHP 5.x convention).
        ctx.ws
            .set_fact(sub.id, "app_root", json!({ "value": "application" }));

        let dir = std::env::temp_dir().join(format!("gt_mw_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("application"));
        std::fs::write(
            dir.join("application").join("middleware.php"),
            "<?php\nreturn [\n    App\\Foo::class,\n    App\\Bar::class,\n];",
        )
        .unwrap();

        // `app/middleware.php` would not exist; only `{app_root}/middleware.php` (expanded to
        // `application/middleware.php`) should match.
        // The array syntax is declared, not assumed: PHP's `[ … ]` / `=>` / `//` / `::class`.
        let params = json!({
            "paths": ["{app_root}/middleware.php"],
            "scope": "global",
            "open": "[",
            "close": "]",
            "separator": "=>",
            "comment": "//",
            "class_suffix": "::class",
            "extensions": ["php"],
        });
        load_declared_middleware(&mut ctx, &sub, &dir, &params);

        let sym = ctx
            .ws
            .get_symbol("declared_middleware", "global_0")
            .expect("declared middleware should be recorded under the resolved app dir");
        let classes: Vec<String> = sym
            .get("classes")
            .and_then(|c| c.as_array())
            .unwrap_or(&Vec::new())
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            classes,
            vec!["App\\Foo".to_string(), "App\\Bar".to_string()],
            "must resolve middleware from the `{{app_root}}`-expanded path, not a hard-coded `app/`"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Express positional-arg style: `app.get('/x', mw1, mw2, handler)` — args after path are all middleware.
    #[test]
    fn express_positional_guards() {
        let f = "routes/index.js";
        let rc = RouteCallSpec {
            receiver: "app".into(),
            by: RouteMatchBy::Receiver,
            verb_methods: [("get".into(), "GET".into()), ("post".into(), "POST".into())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None, // Treat the last argument as a handler and drop it
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![rc],
            guard_attach: GuardAttach::One(GuardAttachSpec::Positional),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let calls = vec![
            call(f, "app", "get", vec![
                FactValue::String("/cart".into()),
                class("AuthMiddleware"),
                class("CartOwnerMiddleware"),
                FactValue::String("handler".into()),
            ], 1, 1, 10, 80),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        let names: Vec<&str> = scopes[0].guards.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(names, vec!["AuthMiddleware", "CartOwnerMiddleware"]);
        // Only `app.get` is collected; bare `router.use` without a path shouldn't be mistaken as a route guard
        let calls2 = vec![call(f, "router", "use", vec![class("X")], 2, 2, 90, 120)];
        assert!(collect_route_guards(&spec, &calls2).is_empty());
    }

    /// Decorator / annotation style: `@UseGuards(X)` and the decorated route `@Get` share owner_fqn, associate by owner.
    #[test]
    fn decorator_guards_group_by_owner() {
        let f = "user.controller.ts";
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["Get".into(), "Post".into()],
            guard_decorators: vec!["UseGuards".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("Get".into(), "GET".into()), ("Post".into(), "POST".into())]
                    .into_iter()
                    .collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, arg: FactValue, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args: vec![arg],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: f.into(),
            sub: None,
            language: Language::new(Language::PHP), // Placeholder only; does not affect decorator matching
        };
        let calls = vec![
            mk("Get", "UserController.profile", FactValue::String("/profile".into()), 10),
            mk("UseGuards", "UserController.profile", class("JwtAuthGuard"), 9),
            mk("Post", "UserController.update", FactValue::String("/update".into()), 20),
            mk("UseGuards", "UserController.update", class("AdminGuard"), 19),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 2, "each decorated route method produces one span");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(by_line.remove(&10).unwrap(), vec!["JwtAuthGuard".to_string()]);
        assert_eq!(by_line.remove(&20).unwrap(), vec!["AdminGuard".to_string()]);
    }

    /// A minimal declaration builder for "decorator route + consumer-style middleware" (avoid repetitive boilerplate).
    fn rc_callee(name: &str, verb: &str) -> RouteCallSpec {
        RouteCallSpec {
            receiver: name.into(),
            by: RouteMatchBy::Callee,
            verb_methods: [(name.to_ascii_lowercase(), verb.to_string())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None,
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        }
    }

    /// NestJS `MiddlewareConsumer`: `consumer.apply(AuthMiddleware).forRoutes({path, method})`'s **object-literal** arg
    /// must be parsed into (path, verb) and landed on the corresponding route by "path + verb".
    ///
    /// Reproduces the realworld project's `user.module.ts` style — without it `forRoutes` would only recognize
    /// string / `Unknown` names, object literals would be treated as nameless args, and the whole module would
    /// get 0 guard hits.
    #[test]
    fn nestjs_consumer_object_literal_routes() {
        let consumer_spec = ConsumerGuardSpec {
            receiver: "consumer".into(),
            apply_method: "apply".into(),
            for_routes_method: "forRoutes".into(),
            wildcards: vec!["*".into()],
            scope: ConsumerScope::Directory,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![
                rc_callee("@Get", "GET"),
                rc_callee("@Put", "PUT"),
                rc_callee("@Post", "POST"),
                rc_callee("@Delete", "DELETE"),
            ],
            guard_attach: GuardAttach::One(GuardAttachSpec::Consumer(consumer_spec)),
            alias_table: None,
            synthesize_unresolved: true,
        };
        let ctrl = "src/user/user.controller.ts";
        let module = "src/user/user.module.ts";
        let mk_route = |callee: &str, path: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserController".into(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: None,
            method: Some(callee.trim_start_matches('@').to_string()),
            args: vec![FactValue::String(path.into())],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: line,
                end_line: line,
                start_byte: line * 10,
                end_byte: line * 10 + 5,
            },
            file: ctrl.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        // The whole chain `consumer.apply(AuthMiddleware).forRoutes(...)` shares start_byte=100.
        let apply = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply".into(),
            receiver: Some("consumer".into()),
            method: Some("apply".into()),
            args: vec![FactValue::Unknown(Some("AuthMiddleware".into()))],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 17,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let for_routes = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply.forRoutes".into(),
            receiver: Some("consumer.apply(AuthMiddleware)".into()),
            method: Some("forRoutes".into()),
            args: vec![
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.GET".into()))),
                ]),
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.PUT".into()))),
                ]),
            ],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 18,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let calls = vec![
            mk_route("@Get", "user", 21),
            mk_route("@Put", "user", 26),
            mk_route("@Post", "users", 32),
            mk_route("@Delete", "users/:slug", 37),
            apply,
            for_routes,
        ];
        let scopes = collect_route_guards(&spec, &calls);
        // Should hit only 'user' GET and 'user' PUT (the verb dimension filters out POST / DELETE).
        assert_eq!(scopes.len(), 2, "only the two routes matched by an explicit path + verb should hit");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(
            by_line.remove(&21).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "GET 'user'"
        );
        assert_eq!(
            by_line.remove(&26).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "PUT 'user'"
        );
    }

    /// Django: `path('profile', views.profile)`'s route and view function are **in two files**,
    /// associating by owner would necessarily miss — rely on `link_via_handler_arg` to connect cross-file by handler arg.
    #[test]
    fn django_handler_link_guards() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec![],
            guard_decorators: vec!["login_required".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: true,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: Some(1),
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "path".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("path".into(), "ANY".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, args: Vec<FactValue>, file: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            // urls.py: route, handler is identifier `views.profile`
            mk("path", "urls", vec![FactValue::String("profile".into()), FactValue::Unknown(Some("views.profile".into()))], "urls.py", 5),
            // views.py: guard decorator hangs on the view function (owner is myapp.views.profile)
            mk("login_required", "myapp.views.profile", vec![], "views.py", 12),
            // Another view's guard shouldn't bleed onto profile
            mk("login_required", "myapp.views.other", vec![], "views.py", 20),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["login_required"]
        );
    }

    /// A broad include pattern must not treat **doc decorators** and **plain member calls** as guards (deny + shape constraint).
    #[test]
    fn decorator_guards_reject_docs_and_member_calls() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["@Get".into()],
            guard_decorators: vec!["@UseGuards".into()],
            // broad pattern (would hit ApiBearerAuth and generateJWT)
            guard_name_patterns: vec!["(auth|jwt)".into()],
            guard_exclude_patterns: vec!["^@?api".into(), "^_".into()],
            require_at_prefix: true,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "@Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("@get".into(), "GET".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, recv: Option<&str>, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: recv.map(|r| r.into()),
            method: None,
            args: vec![],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: "c.ts".into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            mk("@Get", "C.f", None, 10),
            mk("@UseGuards", "C.f", None, 9),   // Real guard (with @)
            mk("@ApiBearerAuth", "C.f", None, 8), // Doc decorator -> exclude
            mk("generateJWT", "C.f", Some("this.userService"), 11), // Member call -> exclude
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["@UseGuards"],
            "only real guards remain: doc decorators and member calls must not count"
        );
    }

    /// Inner overrides outer: group `AuthToken(true)`, route `AuthToken(false)` → the latter should win.
    #[test]
    fn inner_guard_overrides_outer() {
        let f = "app/api/route/pc.php";
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 1,
            end_line: 30,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("true".into()),
            }],
        }]);
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 5,
            end_line: 6,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("false".into()),
            }],
        }]);
        let got = ws.route_guards(f, 5);
        assert_eq!(got.len(), 1, "middleware with the same name counts once");
        assert_eq!(got[0].arg.as_deref(), Some("false"), "the inner argument overrides the outer one");
    }

    // ------------------------------------------------------------ code-evidence detectors

    /// A knowledge base of exactly the given entries, so detection needs no YAML directory.
    struct StaticKb(Vec<FrameworkKnowledge>);

    impl gt_domain::port::KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    /// A filesystem where nothing exists: the detectors under test never touch the disk.
    struct NoFs;

    impl gt_domain::port::FileSystem for NoFs {
        fn exists(&self, _: &std::path::Path) -> bool {
            false
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &std::path::Path) -> gt_domain::error::Result<String> {
            Err(gt_domain::error::DomainError::infra("no fs"))
        }
        fn len(&self, _: &std::path::Path) -> gt_domain::error::Result<u64> {
            Ok(0)
        }
    }

    fn fk(id: &str, detectors: Vec<Detector>) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.into(),
            display_name: id.into(),
            language: Language("php".into()),
            detectors,
            ..Default::default()
        }
    }

    fn evidence(imports: &[&str], callees: &[(&str, Option<&str>, Option<&str>)]) -> CodeEvidence {
        CodeEvidence {
            imports: imports.iter().map(|s| s.to_ascii_lowercase()).collect(),
            callees: callees
                .iter()
                .map(|(c, r, m)| {
                    (c.to_string(), (r.map(str::to_string), m.map(str::to_string)))
                })
                .collect(),
        }
    }

    #[test]
    fn import_exists_matches_fqn_exactly_and_by_prefix() {
        let ev = evidence(&["guzzlehttp\\client", "app\\services\\order"], &[]);
        assert!(ev.imports("GuzzleHttp\\Client"), "case-insensitive");
        assert!(ev.imports("\\GuzzleHttp\\Client"), "a leading separator is ignored");
        assert!(ev.imports("GuzzleHttp\\*"), "namespace prefix");
        assert!(!ev.imports("GuzzleHttp\\HandlerStack"), "symbols not imported under the prefix do not count");
        assert!(!ev.imports("GuzzleHttp"), "without a wildcard it is a full-FQN comparison, not a prefix match");
    }

    #[test]
    fn call_exists_uses_the_rule_callee_grammar() {
        let ev = evidence(
            &[],
            &[
                ("GuzzleHttp\\Client::request", Some("GuzzleHttp\\Client"), Some("request")),
                ("Client::get", Some("Client"), Some("get")),
            ],
        );
        // Fully-qualified inline call: what `import_exists` cannot see.
        assert!(ev.calls("GuzzleHttp\\Client::*"));
        assert!(ev.calls("GuzzleHttp\\Client::request|GuzzleHttp\\Client::get"));
        assert!(!ev.calls("GuzzleHttp\\Client::pool"));
        // Bare method names and `*` tail matching behave as they do in a rule selector.
        assert!(ev.calls("*:get"));
        assert!(!ev.calls("*:delete"));
    }

    #[test]
    fn detectors_activate_knowledge_without_any_manifest() {
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: std::path::PathBuf::from("/p"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        };
        let root = std::path::Path::new("/p");
        // No composer.json anywhere (NoFs says nothing exists), so only code evidence can fire.
        let ev = evidence(&["guzzlehttp\\client"], &[]);

        let kb = StaticKb(vec![
            fk(
                "guzzle",
                vec![Detector::ImportExists { symbol: "GuzzleHttp\\Client".into(), confidence: 0.95 }],
            ),
            fk(
                "unrelated",
                vec![Detector::CallExists { callee: "Redis::get".into(), confidence: 0.9 }],
            ),
        ]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &techstack());
        assert_eq!(hits, vec!["guzzle".to_string()], "only libraries actually used in the code are activated");

        // And the alias spelling — `use GuzzleHttp\Client as G;` — records the same FQN, so it too fires.
        let kb2 = StaticKb(vec![fk(
            "guzzle",
            vec![Detector::CallExists { callee: "GuzzleHttp\\Client::*".into(), confidence: 0.9 }],
        )]);
        let ev2 = evidence(&[], &[("GuzzleHttp\\Client::request", None, None)]);
        assert_eq!(
            detect_frameworks(&kb2, &NoFs, &sub, root, KnowledgeScope::Framework, &ev2, &techstack()),
            vec!["guzzle".to_string()]
        );
    }

    /// An in-memory filesystem, so lock-file parsing needs no temp files.
    struct MemFs(HashMap<std::path::PathBuf, String>);

    impl gt_domain::port::FileSystem for MemFs {
        fn exists(&self, p: &std::path::Path) -> bool {
            self.0.contains_key(p)
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, p: &std::path::Path) -> gt_domain::error::Result<String> {
            self.0.get(p).cloned().ok_or_else(|| gt_domain::error::DomainError::infra("missing"))
        }
        fn len(&self, p: &std::path::Path) -> gt_domain::error::Result<u64> {
            Ok(self.0.get(p).map(|s| s.len() as u64).unwrap_or(0))
        }
    }

    #[test]
    fn lock_dependency_sees_the_resolved_closure() {
        let lock = std::path::PathBuf::from("/p/composer.lock");
        let fs = MemFs(HashMap::from([(
            lock.clone(),
            r#"{"packages": [
                 {"name": "laravel/framework"},
                 {"name": "illuminate/database"},
                 {"name": "guzzlehttp/guzzle"}
               ], "packages-dev": [{"name": "phpunit/phpunit"}]}"#
                .into(),
        )]));
        // None of these are in the app's own composer.json — that is the whole point.
        let ts = techstack();
        let php = Language::new(Language::PHP);
        assert!(lock_has(&lock, "illuminate/database", &fs, &ts, &php));
        assert!(lock_has(&lock, "guzzlehttp/guzzle", &fs, &ts, &php));
        assert!(lock_has(&lock, "phpunit/phpunit", &fs, &ts, &php), "packages-dev counts too");
        assert!(!lock_has(&lock, "spatie/laravel-permission", &fs, &ts, &php));
        assert!(!lock_has(
            &std::path::PathBuf::from("/p/other.lock"),
            "illuminate/database",
            &fs,
            &ts,
            &php
        ));
    }

    #[test]
    fn lock_dependency_reads_npm_layouts_too() {
        // package-lock.json v2+: an object keyed by install path.
        let v2 = std::path::PathBuf::from("/p/package-lock.json");
        let fs = MemFs(HashMap::from([(
            v2.clone(),
            r#"{"packages": {"node_modules/express": {"version": "4.18.0"}, "": {"name": "app"}}}"#.into(),
        )]));
        let ts = techstack();
        let js = Language::new(Language::JAVASCRIPT);
        assert!(lock_has(&v2, "express", &fs, &ts, &js), "the key must match once the node_modules/ prefix is stripped");
        // A TypeScript sub-project must not lose the JS adapter (`serves`), or npm layouts would
        // silently degrade to the whole-file text probe.
        let ts_lang = Language::new(Language::TYPESCRIPT);
        assert!(lock_has(&v2, "express", &fs, &ts, &ts_lang), "typescript is served by the JS adapter too");

        // v1: an object keyed by package name under `dependencies`.
        let v1 = std::path::PathBuf::from("/q/package-lock.json");
        let fs1 = MemFs(HashMap::from([(
            v1.clone(),
            r#"{"dependencies": {"koa": {"version": "2.14.0"}}}"#.into(),
        )]));
        assert!(lock_has(&v1, "koa", &fs1, &ts, &js));
    }

    /// The dependency sections are read by the **ecosystem's** adapter; an ecosystem with none
    /// (Java's `pom.xml`, Python's `requirements.txt` …) must still match, via the whole-file text
    /// probe — degrading to silence would silently drop every framework of that stack.
    #[test]
    fn manifest_dependency_uses_the_adapter_then_falls_back_to_text() {
        let ts = techstack();
        let composer = std::path::PathBuf::from("/p/composer.json");
        let pom = std::path::PathBuf::from("/j/pom.xml");
        let fs = MemFs(HashMap::from([
            (
                composer.clone(),
                r#"{"require": {"topthink/framework": "^6"}, "replace": {"nothing/x": "*"}}"#.into(),
            ),
            (pom.clone(), "<dependency><artifactId>spring-boot</artifactId></dependency>".into()),
        ]));

        let php = Language::new(Language::PHP);
        assert!(manifest_has(&composer, "topthink/framework", &fs, &ts, &php));
        assert!(!manifest_has(&composer, "laravel/framework", &fs, &ts, &php));

        let java = Language::new(Language::JAVA);
        assert!(
            manifest_has(&pom, "spring-boot", &fs, &ts, &java),
            "an ecosystem with no adapter must fall back to text probing, not fail silently"
        );
    }

    /// `db_schema` keeps **no** built-in receiver / method list: which receivers name a table is stack
    /// knowledge, declared by FKB `params`. It used to default to PHP's `Db` / `Query` / `Model`, which
    /// silently applied PHP conventions to every stack that merely forgot to declare them.
    #[test]
    fn db_schema_collects_nothing_without_declared_receivers() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        // Exactly the shape the old PHP default would have matched.
        ctx.ws
            .calls
            .push(call("app/a.php", "Db", "name", vec![FactValue::String("eb_user".into())], 1, 1, 10, 30));

        load_schema(&mut ctx, &json!({}), &sub, std::path::Path::new("/t"), &Phase(Phase::PREPARE.to_string()));
        assert!(
            ctx.ws.get_symbol("schema", "eb_user").is_none(),
            "with no declared table_receivers / table_methods, no table name may be collected"
        );

        // Declared by FKB (as thinkphp.yaml / illuminate-database.yaml do) → collected.
        ctx.ws.calls.clear();
        ctx.ws.calls.push(call("app/a.php", "Db", "name", vec![FactValue::String("eb_user".into())], 1, 1, 10, 30));
        load_schema(
            &mut ctx,
            &json!({ "table_receivers": ["Db"], "table_methods": ["name"] }),
            &sub,
            std::path::Path::new("/t"),
            &Phase(Phase::PREPARE.to_string()),
        );
        assert!(
            ctx.ws.get_symbol("schema", "eb_user").is_some(),
            "table names must be collected once the FKB declares them"
        );
    }

    /// A built-in loader whose name is **not** a kernel-neutral id (a stack-specific one, e.g. a Java adapter's
    /// own loader) must be delegated to the tech-stack adapter matching the sub-project's language — `run_builtin`
    /// must not gate on a hard-coded PHP name. This locks the `other => run_adapter_loader` routing that replaced
    /// the explicit PHP-only arm (the id is now the language-agnostic `migration_schema`, whose
    /// format-specific parsing lives in each stack's own adapter).
    #[test]
    fn non_php_builtin_loader_routes_to_adapter() {
        use gt_domain::model::{
            FrameworkKnowledge, Language, Phase, Project, ProjectId, ProjectStatus, SubProject,
            SubProjectId,
        };
        use gt_domain::port::{AdapterFact, TechStackAdapter, TechStackRegistry};

        struct MockAdapter;
        impl TechStackAdapter for MockAdapter {
            fn language(&self) -> Language {
                Language::new(Language::JAVA)
            }
            fn load(
                &self,
                loader_id: &str,
                _params: &serde_json::Value,
                _sub: &gt_domain::model::SubProject,
                _project_root: &std::path::Path,
                _fs: &dyn gt_domain::port::FileSystem,
                _parsers: &dyn gt_domain::port::ParserRegistry,
                _table_prefixes: &[String],
            ) -> gt_domain::error::Result<Vec<AdapterFact>> {
                if loader_id == "mock_schema" {
                    Ok(vec![AdapterFact::Schema {
                        table: "mock_table".into(),
                        columns: vec!["id".into()],
                        source: "mock.php".into(),
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
        }

        struct MockRegistry;
        impl TechStackRegistry for MockRegistry {
            fn adapter_for(&self, language: &Language) -> Option<&dyn TechStackAdapter> {
                if language.0.as_str() == Language::JAVA {
                    static A: MockAdapter = MockAdapter;
                    Some(&A)
                } else {
                    None
                }
            }
        }

        struct EmptyParsers;
        impl gt_domain::port::ParserRegistry for EmptyParsers {
            fn parser_for(&self, _: &Language) -> Option<&dyn gt_domain::port::LanguageParser> {
                None
            }
            fn language_for_extension(&self, _: &str) -> Option<Language> {
                None
            }
            fn supported_languages(&self) -> Vec<Language> {
                Vec::new()
            }
        }

        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::JAVA),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: serde_json::Value::Object(Default::default()),
        };
        let fk = FrameworkKnowledge {
            id: "x".into(),
            display_name: "x".into(),
            language: Language::new(Language::JAVA),
            ..Default::default()
        };
        let registry = MockRegistry;
        let parsers = EmptyParsers;
        let fs = NoFs;

        super::run_builtin(
            &mut ctx,
            "mock_schema",
            &json!({}),
            &sub,
            std::path::Path::new("/t"),
            &fs,
            &parsers,
            &Phase(Phase::PREPARE.to_string()),
            &[],
            &fk,
            &registry,
        );

        assert!(
            ctx.ws.get_symbol("schema", "mock_table").is_some(),
            "non-PHP stack-specific builtin loader must reach the adapter and land its facts"
        );
    }

    /// End-to-end pin for the language-agnostic `migration_schema` id: FKB declares it (see
    /// `fkb/php/illuminate-database.yaml`), the kernel delegates it to the tech-stack adapter matched by
    /// `sub.language`, and the PHP adapter's migration parser turns `database/migrations/*.php` into `schema`
    /// symbol-table rows. Guards the language-agnostic `migration_schema` spelling — no PHP-only alias remains.
    #[test]
    fn migration_schema_loader_writes_schema_table() {
        use gt_domain::model::{
            FrameworkKnowledge, Language, Phase, Project, ProjectId, ProjectStatus, SubProject,
            SubProjectId,
        };

        struct NoParsers;
        impl gt_domain::port::ParserRegistry for NoParsers {
            fn parser_for(&self, _: &Language) -> Option<&dyn gt_domain::port::LanguageParser> {
                None
            }
            fn language_for_extension(&self, _: &str) -> Option<Language> {
                None
            }
            fn supported_languages(&self) -> Vec<Language> {
                Vec::new()
            }
        }

        let dir = std::env::temp_dir().join(format!("gt_mig_schema_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("database/migrations")).unwrap();
        std::fs::write(
            dir.join("database/migrations/2019_01_01_create_users.php"),
            "<?php\nSchema::create('users', function (Blueprint $table) {\n    $table->id();\n    $table->string('email');\n});",
        )
        .unwrap();

        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: dir.clone(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["laravel".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        let fk = FrameworkKnowledge {
            id: "x".into(),
            display_name: "x".into(),
            language: Language::new(Language::PHP),
            ..Default::default()
        };
        let registry = techstack();
        let parsers = NoParsers;
        let fs = NoFs;

        super::run_builtin(
            &mut ctx,
            "migration_schema",
            &json!({ "paths": ["database/migrations"], "extensions": ["php"] }),
            &sub,
            &dir,
            &fs,
            &parsers,
            &Phase(Phase::PREPARE.to_string()),
            &[],
            &fk,
            &registry,
        );

        assert!(
            ctx.ws.get_symbol("schema", "users").is_some(),
            "the FKB-declared `migration_schema` must be parsed through the PHP adapter and land in the schema symbol table"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `exact_table_receivers` matches the receiver's **last namespace segment**, so `Db` / `think\facade\Db`
    /// count while `DbHelper` does not — the substring form (`table_receivers`) would swallow the latter.
    #[test]
    fn db_schema_exact_receivers_match_the_last_segment_only() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        // This test is about PHP's own spelling (`think\facade\Db`), so it must state PHP's notation
        // explicitly: `NamespacePolicy::default()` is deliberately **empty** ("no language wired up"), not
        // silently PHP. Relying on the old PHP default would have made this test pass for the wrong reason.
        use gt_domain::model::NamespacePolicy;
        use gt_domain::port::ParserRegistry;
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let php = parsers
            .parser_for(&Language::new(Language::PHP))
            .expect("php parser is registered");
        ctx.lang_policy_default = NamespacePolicy::from_parser(php);
        let params = json!({ "exact_table_receivers": ["Db"], "table_methods": ["name"] });
        let run = |ctx: &mut PipelineContext, receiver: &str, table: &str| {
            ctx.ws.calls.clear();
            ctx.ws
                .calls
                .push(call("app/a.php", receiver, "name", vec![FactValue::String(table.into())], 1, 1, 10, 30));
            load_schema(ctx, &params, &sub, std::path::Path::new("/t"), &Phase(Phase::PREPARE.to_string()));
            ctx.ws.get_symbol("schema", table).is_some()
        };

        assert!(run(&mut ctx, "Db", "t_users"), "a bare `Db` must match");
        assert!(run(&mut ctx, "think\\facade\\Db", "t_ns"), "a namespaced `Db` must match (same last segment)");
        assert!(run(&mut ctx, "DB", "t_upper"), "case-insensitive");
        assert!(!run(&mut ctx, "DbHelper", "t_junk"), "`DbHelper` must not be treated as a query builder");
    }

    /// The array syntax (`[ … ]` / `=>` / `//` / `::class`) used to be hard-coded, so only PHP-shaped
    /// declarations could ever be read. It is declared by FKB now: the same loader must read a
    /// completely different syntax when FKB says so.
    #[test]
    fn declared_middleware_reads_whatever_array_syntax_fkb_declares() {
        let dir = std::env::temp_dir().join(format!("gt_mw_syntax_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();

        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: dir.clone(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        let ctx_of = || PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });

        // ① PHP array literal.
        std::fs::write(
            dir.join("app/middleware.php"),
            "<?php\nreturn [\n    App\\Foo::class,\n    App\\Bar::class, // trailing comment\n];",
        )
        .unwrap();
        let mut ctx = ctx_of();
        load_declared_middleware(
            &mut ctx,
            &sub,
            &dir,
            &json!({
                "paths": ["app/middleware.php"],
                "open": "[", "close": "]", "separator": "=>", "comment": "//",
                "class_suffix": "::class", "extensions": ["php"],
            }),
        );
        assert_eq!(
            ctx.ws
                .get_symbol("declared_middleware", "global_0")
                .and_then(|v| v.get("classes"))
                .and_then(|c| c.get(0))
                .and_then(|c| c.as_str()),
            Some("App\\Foo"),
            "a PHP array literal must be parsed as declared"
        );

        // ② A different stack's syntax: `{}` blocks, `:` pairs, `#` comments, quoted values.
        std::fs::write(
            dir.join("app/middleware.conf"),
            "{\n  \"a\": \"Acme\\Foo\",\n  \"b\": \"Acme\\Bar\" # trailing\n}",
        )
        .unwrap();
        let mut ctx2 = ctx_of();
        load_declared_middleware(
            &mut ctx2,
            &sub,
            &dir,
            &json!({
                "paths": ["app/middleware.conf"],
                "open": "{", "close": "}", "separator": ":", "comment": "#",
                "extensions": ["conf"],
            }),
        );
        let classes: Vec<String> = ctx2
            .ws
            .get_symbol("declared_middleware", "global_0")
            .and_then(|v| v.get("classes").cloned())
            .and_then(|c| serde_json::from_value::<Vec<String>>(c).ok())
            .unwrap_or_default();
        assert_eq!(
            classes,
            vec!["Acme\\Foo".to_string(), "Acme\\Bar".to_string()],
            "non-PHP syntax must be parsed as declared too"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provides_switches_on_component_knowledge_and_ranks_below_direct_hits() {
        // A framework bundles components the app never declared; recognising the bundle recognises them.
        let mut laravel = fk("laravel", vec![]);
        laravel.provides = vec!["illuminate-database".into()];
        let mut db = fk("illuminate-database", vec![]);
        db.provides = vec!["illuminate-support".into()]; // transitive: a component may bundle further ones
        let kb = StaticKb(vec![laravel, db, fk("illuminate-support", vec![])]);

        let hits = expand_provided(vec![("laravel".to_string(), 0.95)], &kb);
        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["laravel", "illuminate-database", "illuminate-support"]);

        // Directly detected knowledge keeps its place ahead of provided knowledge, whatever the confidence.
        let hits2 = expand_provided(
            vec![("laravel".to_string(), 0.95), ("guzzle".to_string(), 0.5)],
            &kb,
        );
        let ids2: Vec<&str> = hits2.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids2,
            vec!["laravel", "guzzle", "illuminate-database", "illuminate-support"],
            "derived knowledge is ranked after direct hits"
        );
        assert!(
            hits2.iter().find(|(id, _)| id == "illuminate-database").unwrap().1 < 0.95,
            "knowledge pulled in via `provides` has a lower confidence than its provider"
        );
    }

    #[test]
    fn provides_is_cycle_safe() {
        let mut a = fk("a", vec![]);
        a.provides = vec!["b".into()];
        let mut b = fk("b", vec![]);
        b.provides = vec!["a".into(), "b".into()]; // mutual, and self-referential
        let kb = StaticKb(vec![a, b]);
        let hits = expand_provided(vec![("a".to_string(), 0.9)], &kb);
        assert_eq!(hits.len(), 2, "a cycle must not cause duplicates or infinite expansion");
    }

    #[test]
    fn detector_confidence_is_declared_not_defaulted() {
        use gt_domain::model::Detector;
        assert_eq!(
            Detector::ImportExists { symbol: "x".into(), confidence: 0.5 }.confidence(),
            0.5
        );
        assert_eq!(
            Detector::CallExists { callee: "x".into(), confidence: 0.7 }.confidence(),
            0.7
        );
    }

    // ------------------------------------------------------- the SQL DDL parser
    //
    // Hand-rolled, and the only source of `Column` nodes for schema files: a mis-split field list silently
    // drops a column (no Column node, so no PII / annotation can ever land on it), while a table-level
    // constraint line taken for a column invents one.

    #[test]
    fn ddl_parses_every_create_table_with_its_columns() {
        let sql = "CREATE TABLE IF NOT EXISTS `eb_user` (\n  `id` int(11) NOT NULL,\n  `name` varchar(32) NOT NULL,\n  PRIMARY KEY (`id`)\n);\nCREATE TABLE \"eb_order\" (\n  id int,\n  price decimal(10,2) NOT NULL,\n  KEY `idx_price` (`price`)\n);";
        let tables = parse_create_tables(sql);

        assert_eq!(
            tables.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
            vec!["eb_user".to_string(), "eb_order".to_string()],
            "two tables must be parsed with quotes stripped: {tables:?}"
        );
        assert_eq!(
            tables[0].1,
            vec!["id".to_string(), "name".to_string()],
            "`PRIMARY KEY` is not a column: {:?}",
            tables[0].1
        );
        assert_eq!(
            tables[1].1,
            vec!["id".to_string(), "price".to_string()],
            "`KEY idx_price` is not a column: {:?}",
            tables[1].1
        );
    }

    /// A comma inside parentheses (`decimal(10,2)`) or inside a string literal (`DEFAULT 'x,y'`) belongs to
    /// the field definition — splitting on it would truncate the column list.
    #[test]
    fn ddl_does_not_split_on_commas_inside_parens_or_strings() {
        let tables = parse_create_tables(
            "CREATE TABLE t (a decimal(10,2) NOT NULL, b varchar(8) DEFAULT 'x,y', c int)",
        );

        assert_eq!(
            tables[0].1,
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "commas inside parentheses or strings must not split fields: {:?}",
            tables[0].1
        );
    }

    /// Comment lines are not columns, and a plain field still is.
    #[test]
    fn ddl_skips_comment_lines() {
        let tables = parse_create_tables(
            "CREATE TABLE t (\n  -- a comment\n  id int,\n  # another\n  name varchar(8)\n)",
        );

        assert_eq!(
            tables[0].1,
            vec!["id".to_string(), "name".to_string()],
            "a comment line must not become a column: {:?}",
            tables[0].1
        );
    }

    /// A column whose **name merely starts with** a constraint keyword is still a column — `unique_id`,
    /// `check_time`, `index_sort`, `foreign_id` are ordinary names, and dropping them costs the column its
    /// `Column` node (hence any PII / annotation on it).
    #[test]
    fn ddl_keeps_columns_named_after_a_constraint_keyword() {
        let tables = parse_create_tables(
            "CREATE TABLE t (\n  unique_id varchar(32) NOT NULL,\n  check_time int,\n  index_sort int,\n  foreign_id int,\n  PRIMARY KEY (`unique_id`)\n)",
        );

        assert_eq!(
            tables[0].1,
            vec![
                "unique_id".to_string(),
                "check_time".to_string(),
                "index_sort".to_string(),
                "foreign_id".to_string(),
            ],
            "a legal column name starting with a constraint keyword must be kept: {:?}",
            tables[0].1
        );
    }

    // ------------------------------------------------------- name normalisation helpers

    /// Alias tables / route guards go through several rounds of JSON and text escaping, so
    /// `Illuminate\\Session\\X` (doubled separators) must fold to one per level — otherwise a declared
    /// middleware never matches (measured on laravel10: the whole table came out empty).
    #[test]
    fn norm_class_collapses_repeated_separators() {
        assert_eq!(
            norm_class(r"Illuminate\\Session\\Middleware\\X"),
            r"Illuminate\Session\Middleware\X"
        );
        assert_eq!(
            norm_class(r"\App\Http\Middleware\Auth"),
            r"App\Http\Middleware\Auth"
        );
        assert_eq!(norm_class("Plain"), "Plain");
    }

    #[test]
    fn callee_helpers_split_at_the_last_separator() {
        assert_eq!(
            split_callee("Http::get"),
            (Some("Http".to_string()), Some("get".to_string()))
        );
        assert_eq!(
            split_callee("$db->query"),
            (Some("$db".to_string()), Some("query".to_string()))
        );
        assert_eq!(split_callee("curl_exec"), (None, None), "with no receiver both components are None");

        assert_eq!(short_callee(r"App\Http\Middleware\Auth"), "Auth");
        assert_eq!(short_callee("org.springframework.Boot"), "Boot");
        assert_eq!(short_callee("plain"), "plain");
    }

    /// A declared middleware's `path` matches as a **suffix** of the file's path, with `*` for one segment.
    #[test]
    fn declared_middleware_path_matches_as_a_suffix() {
        assert!(declared_mw_path_matches("app/Http/Middleware", "app/Http/Middleware"));
        assert!(
            declared_mw_path_matches("Http/Middleware", "app/Http/Middleware"),
            "must match by path suffix"
        );
        assert!(
            declared_mw_path_matches("app/*/Middleware", "app/Http/Middleware"),
            "`*` wildcards exactly one segment"
        );
        assert!(
            !declared_mw_path_matches("app/Http/Middleware", "Http/Middleware"),
            "a path shorter than the pattern must not match"
        );
        assert!(
            !declared_mw_path_matches("app/Http", "app/Http/Middleware"),
            "the pattern must be a suffix of the path"
        );
    }

    // ------------------------------------------------------- the two remaining untested pure leaves
    //
    // `expand` is the `{app_root}` placeholder substitution every loader `paths`/`path` declaration relies on;
    // if it regresses, route / nginx paths silently resolve against the wrong directory.
    #[test]
    fn expand_substitutes_app_root_placeholder() {
        assert_eq!(expand("{app_root}/routes", "application"), "application/routes");
        assert_eq!(
            expand("app/routes", "application"),
            "app/routes",
            "with no placeholder it is kept verbatim"
        );
        assert_eq!(expand("{app_root}", ""), "", "an empty app_root must still be substituted correctly");
    }

    /// `resolve_guard_class` turns a mount arg alias (`auth`) back into a class, passes a class literal
    /// through untouched, and never fabricates when the alias is unknown — `load_routes` builds the
    /// `route_list` guards from exactly this, so a regression silently mis-links (or invents) middleware.
    #[test]
    fn resolve_guard_class_resolves_alias_and_passthrough() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        // alias table "mw": auth -> App\Auth
        ctx.ws
            .symbols
            .entry("mw".into())
            .or_default()
            .insert("auth".into(), json!({ "class": r"App\Auth" }));

        let sep = &['\\'];
        // alias with no separator -> resolved from the table
        assert_eq!(
            resolve_guard_class(&ctx, "auth", Some("mw"), sep),
            Some(r"App\Auth".into()),
            "an alias must be resolved back to its class via the table"
        );
        // already a class name (contains a separator) -> returned as-is, trimmed
        assert_eq!(
            resolve_guard_class(&ctx, r"\App\Http\Auth", Some("mw"), sep),
            Some(r"App\Http\Auth".into()),
            "a name containing a namespace separator is returned verbatim and trimmed"
        );
        // empty -> None
        assert_eq!(resolve_guard_class(&ctx, "  ", Some("mw"), sep), None, "an empty string returns None");
        // unknown alias, no separator -> None (never fabricate)
        assert_eq!(
            resolve_guard_class(&ctx, "unknown", Some("mw"), sep),
            None,
            "an unknown alias is never invented"
        );
    }

    // ------------------------------------------------------- the untested built-in loaders
    //
    // `load_schema` / `load_declared_middleware` are covered; these five reach disk / the call graph and had
    // no direct test, so a regression in any of them silently produces an empty symbol table.

    /// A `config()` / `Config::get` call with a static key becomes a `config_keys` symbol; keys with a space
    /// are dropped (they cannot be a real config key and would pollute the table).
    #[test]
    fn load_config_keys_records_accessor_calls_as_config_symbols() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let f = "config/sys.php";
        ctx.ws.calls = vec![
            call(
                f,
                "Config",
                "get",
                vec![FactValue::String("sys.site_name".into())],
                3,
                3,
                10,
                40,
            ),
            call(
                f,
                "Config",
                "get",
                vec![FactValue::String("has space".into())],
                4,
                4,
                50,
                60,
            ),
        ];
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: json!({}),
        };
        load_config_keys(
            &mut ctx,
            &json!({ "suffixes": ["::get"], "storage": "Database", "mutable": "EnvFixed" }),
            &sub,
        );
        let key = ctx
            .ws
            .get_symbol("config_keys", "sys.site_name")
            .expect("must write the config_keys symbol");
        assert_eq!(key["storage"], json!("Database"));
        assert_eq!(key["mutable"], json!("EnvFixed"));
        assert_eq!(key["value_type"], json!("string"));
        assert_eq!(key["file"], json!(f));
        assert!(
            ctx.ws.get_symbol("config_keys", "has space").is_none(),
            "a key containing a space must not be written"
        );
    }

    /// nginx `server_name` / `root` / `location` are scanned into a `nginx` symbol keyed by file path.
    #[test]
    fn load_nginx_parses_server_name_root_and_locations() {
        let dir = std::env::temp_dir().join(format!("gt_nginx_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(
            dir.join("site.conf"),
            "server_name example.com;\nroot /var/www/html;\nlocation /api { proxy_pass x; }\n",
        )
        .unwrap();
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "web".into(),
            root_path: dir.clone(),
            language: Language("conf".into()),
            role: "web".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: json!({}),
        };
        load_nginx(&mut ctx, &sub, &dir, &StdFileSystem::new());
        let sym = ctx
            .ws
            .get_symbol("nginx", "site.conf")
            .expect("must write the nginx symbol");
        assert_eq!(sym["server_name"], json!("example.com"));
        assert_eq!(sym["root"], json!("/var/www/html"));
        assert_eq!(sym["locations"].as_array().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `middleware_aliases` scans a `key => Class::class` file (syntax declared by FKB, not hard-coded) into
    /// the alias table; the `{app_root}` placeholder expands from the `root_rules`-resolved app directory.
    #[test]
    fn load_middleware_aliases_reads_key_class_pairs() {
        let dir = std::env::temp_dir().join(format!("gt_mwalias_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("application"));
        std::fs::write(
            dir.join("application/mw.php"),
            "<?php return [\n  'auth' => App\\Auth::class,\n  'throttle' => App\\Throttle::class,\n];\n",
        )
        .unwrap();
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "backend".into(),
            root_path: dir.clone(),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: json!({}),
        };
        // Not `app/` — this project's app directory is `application/` (ThinkPHP 5.x convention).
        ctx.ws
            .set_fact(sub.id, "app_root", json!({ "value": "application" }));
        load_middleware_aliases(
            &mut ctx,
            &sub,
            &dir,
            &json!({
                "paths": ["{app_root}/mw.php"],
                "end": "];",
                "separator": "=>",
                "extensions": ["php"],
                "class_suffix": "::class",
            }),
        );
        let table = ctx
            .ws
            .symbols
            .get("middleware_aliases")
            .expect("must write into the alias table");
        assert_eq!(
            table
                .get("auth")
                .and_then(|v| v.get("class"))
                .and_then(|c| c.as_str()),
            Some("App\\Auth"),
            "the `auth` alias resolves back to App\\Auth"
        );
        assert_eq!(
            table
                .get("throttle")
                .and_then(|v| v.get("class"))
                .and_then(|c| c.as_str()),
            Some("App\\Throttle")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_routes` turns the call graph into `route_list` entries, resolving each mounted guard's class
    /// (here a class literal, whose namespace separators must survive intact).
    #[test]
    fn load_routes_builds_route_list_with_resolved_guards() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let f = "app/api/route/user.php";
        // `Route::get('info')` and `->middleware(AuthTokenMiddleware::class, false)` share byte range 10.
        ctx.ws.calls = vec![
            call(f, "Route", "get", vec![FactValue::String("info".into())], 7, 7, 10, 60),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(false),
                ],
                7,
                8,
                10,
                90,
            ),
        ];
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: json!({}),
        };
        let steps: Vec<NormalizeStep> = Vec::new();
        load_routes(&mut ctx, &tp6_spec(), &sub, &steps);
        // Bare route (no group prefix) -> key is `METHOD path` with no leading slash; the slash only
        // appears once a route-group prefix is inserted by `route_group_prefix`.
        let route = ctx
            .ws
            .get_symbol("route_list", "GET info")
            .expect("must write the route_list symbol");
        assert_eq!(route["handler"], json!(""));
        let guards = route["guards"].as_array().expect("a guard must be present");
        assert_eq!(guards.len(), 1, "exactly one route guard must be parsed");
        assert_eq!(
            guards[0]["class"],
            json!(r"app\api\middleware\AuthTokenMiddleware"),
            "the guard class is kept verbatim (namespace included)"
        );
        assert_eq!(guards[0]["arg"], json!("false"));
    }

    /// i18n files under `lang/<locale>/*` are parsed into locale-keyed `i18n` symbols (`{stem}.{key_path}`).
    struct I18nParser;
    impl LanguageParser for I18nParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> gt_domain::error::Result<SyntaxFacts> {
            Ok(SyntaxFacts {
                config_entries: vec![ConfigEntryFact {
                    key_path: "hello".into(),
                    value: FactValue::String("Hi".into()),
                    span: Span {
                        start_line: 0,
                        end_line: 0,
                        start_byte: 0,
                        end_byte: 0,
                    },
                }],
                ..Default::default()
            })
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    struct I18nRegistry {
        parser: I18nParser,
    }
    impl ParserRegistry for I18nRegistry {
        fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
            Some(&self.parser)
        }
        fn language_for_extension(&self, ext: &str) -> Option<Language> {
            if ext == "php" {
                Some(Language::new("php"))
            } else {
                None
            }
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    #[test]
    fn load_i18n_builds_locale_keyed_symbols() {
        let dir = std::env::temp_dir().join(format!("gt_i18n_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("lang/en"));
        let p = "lang/en/messages.php";
        std::fs::write(dir.join(p), "<?php return ['hello' => 'Hi'];").unwrap();
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.files = vec![SourceFile {
            id: FileId::new(1),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            path: p.into(),
            language: Language("php".into()),
            size_bytes: 1,
            content_hash: "x".into(),
        }];
        let loader = LoaderSpec {
            id: "i18n".into(),
            table: "i18n".into(),
            from: LoaderSource::File {
                path: "".into(),
                key_path: None,
            },
            confidence: 1.0,
        };
        load_i18n(
            &mut ctx,
            &loader,
            "lang/*/*",
            &dir,
            &StdFileSystem::new(),
            &I18nRegistry { parser: I18nParser },
            Some(r"(en|zh)"),
            &gt_domain::port::DefaultTechStackRegistry::new(),
        );
        let sym = ctx
            .ws
            .get_symbol("i18n", "messages.hello")
            .expect("must write the i18n symbol");
        assert_eq!(sym["texts"]["en"], json!("Hi"), "the `en` locale text must be written");
        assert_eq!(sym["file"], json!(p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------- receiver_matches / guard_arg_name

    /// `receiver_matches` has two modes (`contains` vs `receiver_ends_with`) and is case-insensitive.
    /// The `ends_with` mode additionally strips leading separators — `\app\Route` must still match.
    #[test]
    fn receiver_matches_contains_and_ends_with_modes() {
        let mut rc = tp6_spec().route_calls[0].clone();
        // contains mode (default): the pattern must appear anywhere in the receiver.
        assert!(receiver_matches(&rc, "Route"));
        assert!(receiver_matches(&rc, r"\think\facade\Route"));
        assert!(receiver_matches(&rc, "route"), "case-insensitive");
        assert!(!receiver_matches(&rc, "app"), "a receiver that does not contain `route` must not match");

        // ends_with mode: only the tail counts, leading separators are stripped.
        rc.receiver_ends_with = true;
        assert!(receiver_matches(&rc, "Route"));
        assert!(receiver_matches(&rc, r"\app\Route"));
        assert!(!receiver_matches(&rc, "RouteFacade"), "a receiver not ending in `route` must not match");
    }

    /// Only **literals** may become a guard name; a dynamic PHP arg (`->middleware($v)`) would be
    /// fabrication. Identifiers (JS / Python function references) are accepted **only** when FKB
    /// declares `accept_identifier`.
    #[test]
    fn guard_arg_name_accepts_literals_and_gated_identifiers() {
        // Literals: PHP middleware is always `X::class`.
        assert_eq!(
            guard_arg_name(&FactValue::ClassConst("A\\B".into()), false),
            Some("A\\B".to_string())
        );
        assert_eq!(
            guard_arg_name(&FactValue::String("  auth  ".into()), false),
            Some("auth".to_string()),
            "the literal must be trimmed"
        );
        assert_eq!(
            guard_arg_name(&FactValue::String("   ".into()), false),
            None,
            "a blank literal is not a guard name"
        );
        // Dynamic PHP arg must be rejected unless FKB opts in.
        assert_eq!(
            guard_arg_name(&FactValue::Unknown(Some("middleware($v)".into())), false),
            None
        );
        // Identifier: accepted only with `accept_identifier`, and the call parens are stripped.
        assert_eq!(
            guard_arg_name(&FactValue::Unknown(Some("authGuard(...)".into())), true),
            Some("authGuard".to_string())
        );
        assert_eq!(
            guard_arg_name(&FactValue::Unknown(Some("   ".into())), true),
            None,
            "an identifier that is empty after trimming does not count"
        );
        // Other fact kinds are never a guard name.
        assert_eq!(guard_arg_name(&FactValue::Bool(true), true), None);
    }

    // ------------------------------------------------------- match_route_call

    /// The heart of route recognition, previously only reached through `collect_route_guards`:
    /// verb comes from the FKB verb table, path from `path_arg`, handler from `handler_arg`.
    #[test]
    fn match_route_call_extracts_verb_path_and_handler() {
        let rc = tp6_spec().route_calls[0].clone();
        let f = "app/api/route/pc.php";

        // verb + path, no handler arg supplied -> handler is None
        assert_eq!(
            match_route_call(
                &call(f, "Route", "get", vec![FactValue::String("info".into())], 1, 1, 0, 10),
                &rc
            ),
            Some(("GET".to_string(), "info".to_string(), None))
        );
        // handler read from arg 1
        assert_eq!(
            match_route_call(
                &call(
                    f,
                    "Route",
                    "post",
                    vec![
                        FactValue::String("save".into()),
                        FactValue::String("C@m".into())
                    ],
                    2,
                    2,
                    0,
                    10
                ),
                &rc
            ),
            Some(("POST".to_string(), "save".to_string(), Some("C@m".to_string())))
        );
        // A receiver that does not match the spec is not a route.
        assert_eq!(
            match_route_call(&call(f, "Db", "get", vec![FactValue::String("x".into())], 3, 3, 0, 10), &rc),
            None
        );
        // An empty path is never a route.
        assert_eq!(
            match_route_call(&call(f, "Route", "get", vec![FactValue::String("".into())], 4, 4, 0, 10), &rc),
            None
        );
    }

    // ------------------------------------------------------- merge_schema_columns

    /// Schema columns arrive from several loaders (migrations, SQL probes, …) and are merged into one
    /// symbol per table: columns must be appended **without duplicates**, and each distinct source
    /// recorded once — otherwise the same column is listed twice and provenance is lost.
    #[test]
    fn merge_schema_columns_dedupes_columns_and_tracks_sources() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: std::path::PathBuf::from("/t"),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        merge_schema_columns(&mut ctx, "user", vec!["id".into(), "name".into()], "migration");
        merge_schema_columns(&mut ctx, "user", vec!["name".into(), "email".into()], "migration");

        let sym = ctx.ws.get_symbol("schema", "user").expect("the schema symbol must be written");
        let cols: Vec<&str> = sym["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(cols, vec!["id", "name", "email"], "columns must be merged and de-duplicated");
        assert_eq!(sym["sources"], json!(["migration"]), "the same source is recorded only once");

        // A different source is appended.
        merge_schema_columns(&mut ctx, "user", vec!["phone".into()], "sql_probe");
        let sym = ctx.ws.get_symbol("schema", "user").expect("schema symbol");
        assert_eq!(sym["sources"], json!(["migration", "sql_probe"]));

        // Empty table name / empty column list are no-ops (guarded at the top).
        merge_schema_columns(&mut ctx, "", vec!["x".into()], "migration");
        merge_schema_columns(&mut ctx, "empty", vec![], "migration");
        assert!(ctx.ws.get_symbol("schema", "").is_none(), "an empty table name skips the whole entry");
        assert!(ctx.ws.get_symbol("schema", "empty").is_none(), "an empty column list skips the whole entry");
    }

    // ------------------------------------------------------- run (integration, minimal ports)

    /// `run` is the phase entry and had **no** direct coverage: its sub-functions are tested, but the
    /// wiring "FKB detector -> `ctx.frameworks` per sub-project" (which gates every later framework
    /// rule) was never exercised end to end.
    #[test]
    fn run_detects_framework_and_records_it_per_sub_project() {
        // Local port stubs: the knowledge base holds exactly one framework, and no parser is needed
        // because this framework declares no loaders.
        struct OneKb(FrameworkKnowledge);
        impl gt_domain::port::KnowledgeProvider for OneKb {
            fn all(&self) -> Vec<&FrameworkKnowledge> {
                vec![&self.0]
            }
            fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
                if self.0.id == id { Some(&self.0) } else { None }
            }
        }
        struct NullParsers;
        impl gt_domain::port::ParserRegistry for NullParsers {
            fn parser_for(&self, _: &Language) -> Option<&dyn LanguageParser> {
                None
            }
            fn supported_languages(&self) -> Vec<Language> {
                vec![]
            }
        }

        let dir = std::env::temp_dir().join(format!("gt_prepare_run_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("composer.json"), "{}").unwrap();

        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.sub_projects = vec![SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "app".into(),
            root_path: dir.clone(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }];
        let kb = OneKb(FrameworkKnowledge {
            id: "myfw".into(),
            language: Language::new(Language::PHP),
            scope: KnowledgeScope::Framework,
            detectors: vec![Detector::FileExists {
                path: "composer.json".into(),
                confidence: 0.9,
            }],
            // No loaders / rules: this test pins the detection wiring only.
            ..Default::default()
        });

        super::run(&mut ctx, &kb, &StdFileSystem::new(), &NullParsers, &techstack()).expect("run ok");

        let got = ctx.frameworks.get(&1).cloned().unwrap_or_default();
        assert_eq!(
            got,
            vec!["myfw".to_string()],
            "recognised frameworks must be recorded into ctx.frameworks: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
