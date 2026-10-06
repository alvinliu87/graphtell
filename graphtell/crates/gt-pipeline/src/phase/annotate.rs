//! P4 Annotate-Pre / P5 Synthesize / P6 Annotate-Post.
//!
//! * **Pre / Synthesize** selectors act on **source code** (call sites / config entries / inheritance;
//!   Synthesize additionally sees inheritance)
//! * **Post** selectors act on **graph nodes** — a capability unique to P6: aggregates like fan_in or `texts`
//!   coverage are only accurate by P6.

use std::collections::HashMap;

use gt_domain::model::{Phase, Rule, Selector, SubProjectId};

use crate::context::PipelineContext;
use crate::engine::{exec_binding, matches_call, matches_config, matches_node, path_matches};
use crate::eval::MatchCtx;

/// P4: run rules against source-code matches (including Synthesize / Link nested inside the rule).
pub fn run_pre(ctx: &mut PipelineContext) {
    apply_source_rules(ctx, &Phase(Phase::ANNOTATE_PRE.to_string()));
}

/// P5: the synthesis phase.
pub fn run_synthesize(ctx: &mut PipelineContext) {
    apply_source_rules(ctx, &Phase(Phase::SYNTHESIZE.to_string()));
    apply_inherit_rules(ctx, &Phase(Phase::SYNTHESIZE.to_string()));
}

/// P6: run rules against graph nodes.
pub fn run_post(ctx: &mut PipelineContext) {
    let phase = Phase(Phase::ANNOTATE_POST.to_string());
    let rules_by_sub = collect_rules(ctx, &phase);

    // Group by node_kind to avoid walking the whole graph
    let mut by_kind: HashMap<String, Vec<Rule>> = HashMap::new();
    let mut wild: Vec<Rule> = Vec::new();
    for (_sub, rules) in rules_by_sub.iter() {
        for r in rules {
            match &r.selector {
                Selector::Node { node_kind, .. } => match node_kind {
                    Some(k) => {
                        let slot = by_kind.entry(k.to_string()).or_default();
                        if !slot.iter().any(|x: &Rule| x.id == r.id) {
                            slot.push(r.clone());
                        }
                    }
                    None => {
                        if !wild.iter().any(|x: &Rule| x.id == r.id) {
                            wild.push(r.clone());
                        }
                    }
                },
                _ => {}
            }
        }
    }

    let mut targets: Vec<(gt_domain::model::NodeId, Option<SubProjectId>, Vec<Rule>)> = Vec::new();
    for (kind, rules) in by_kind {
        for id in ctx.ws.nodes_of_kind(&kind) {
            targets.push((id, sub_of(ctx, id), rules.clone()));
        }
    }
    if !wild.is_empty() {
        for id in ctx.ws.node_ids() {
            targets.push((id, sub_of(ctx, id), wild.clone()));
        }
    }

    for (id, sub, rules) in targets {
        for rule in &rules {
            if !matches_node(&rule.selector, id, &ctx.ws) {
                continue;
            }
            let mut last: Option<gt_domain::model::NodeId> = None;
            exec_binding(ctx, rule, MatchCtx::Node(id), id, id, sub, &phase, &mut last);
        }
    }

    // After the rules run, settle the authoritative schema's columns into `Column` nodes (`Table --HasColumn--> Column`).
    // Last, because it is "read the graph + supplement the graph": it takes no part in rule matching and should not be matched by a rule again.
    crate::phase::columns::materialize(ctx);
}

fn sub_of(ctx: &PipelineContext, id: gt_domain::model::NodeId) -> Option<SubProjectId> {
    ctx.ws.node(id).and_then(|n| n.sub_project_id)
}

/// Convert an in-project relative path into a "relative to the sub-project" path.
///
/// FKB writes `app/event.php` relative to the **sub-project root**, while file paths are relative to the
/// **project root** (e.g. `crmeb/app/event.php`).
fn sub_relative(ctx: &PipelineContext, sub: Option<SubProjectId>, file: &str) -> String {
    let Some(sub) = sub else { return file.to_string() };
    let Some(s) = ctx.sub_projects.iter().find(|x| x.id == sub) else {
        return file.to_string();
    };
    let prefix = s
        .root_path
        .strip_prefix(&ctx.project.root_path)
        .unwrap_or(&s.root_path)
        .to_string_lossy()
        .replace('\\', "/");
    let prefix = format!("{}/", prefix.trim_matches('/'));
    file.strip_prefix(&prefix).unwrap_or(file).to_string()
}

/// `sub-project id -> app-root directory`, resolved once per phase so `{app_root}` expansion inside
/// hot loops does not re-read the fact. Shares `prepare::app_root_of`, including its `app` fallback.
fn app_root_map(ctx: &PipelineContext) -> HashMap<i64, String> {
    ctx.sub_projects
        .iter()
        .map(|s| (s.id.get(), crate::phase::prepare::app_root_of(ctx, s)))
        .collect()
}

/// Run one phase's rules against call sites and config entries.
fn apply_source_rules(ctx: &mut PipelineContext, phase: &Phase) {
    let rules_by_sub = collect_rules(ctx, phase);
    let app_root_by_sub = app_root_map(ctx);

    let calls = std::mem::take(&mut ctx.ws.calls);
    for call in &calls {
        let sub = call.sub;
        let rules = rules_for(&rules_by_sub, sub);
        for rule in rules {
            if !matches_call(&rule.selector, call, &ctx.ws) {
                continue;
            }
            let mut last: Option<gt_domain::model::NodeId> = None;
            exec_binding(ctx, rule, MatchCtx::Call(call), call.node, call.owner, sub, phase, &mut last);
        }
    }
    ctx.ws.calls = calls;

    let configs = std::mem::take(&mut ctx.ws.configs);
    for cfg in &configs {
        let sub = cfg.sub;
        let rules = rules_for(&rules_by_sub, sub);
        for rule in rules {
            let Selector::ConfigEntry { file, .. } = &rule.selector else {
                continue;
            };
            let app_root = sub
                .and_then(|s| app_root_by_sub.get(&s.get()).cloned())
                .unwrap_or_else(|| "app".to_string());
            let pattern = match file {
                Some(f) => crate::phase::prepare::expand(f, &app_root),
                None => String::new(),
            };
            if !pattern.is_empty()
                && !path_matches(&pattern, &cfg.file)
                && !path_matches(&pattern, &sub_relative(ctx, sub, &cfg.file))
            {
                continue;
            }
            if !matches_config(&rule.selector, cfg, cfg_node(ctx, cfg), &ctx.ws, &app_root) {
                continue;
            }
            let mut last: Option<gt_domain::model::NodeId> = None;
            exec_binding(ctx, rule, MatchCtx::Config(cfg), cfg_node(ctx, cfg), cfg_node(ctx, cfg), sub, phase, &mut last);
        }
    }
    ctx.ws.configs = configs;
}

/// A config entry has no node, so the file node it belongs to is borrowed as the annotation's landing point.
fn cfg_node(ctx: &PipelineContext, cfg: &crate::workspace::ConfigRecord) -> gt_domain::model::NodeId {
    ctx.ws.file_node(&cfg.file).unwrap_or_else(|| gt_domain::model::NodeId(1))
}

fn apply_inherit_rules(ctx: &mut PipelineContext, phase: &Phase) {
    let rules_by_sub = collect_rules(ctx, phase);
    let inherits = std::mem::take(&mut ctx.ws.inherits);
    for inh in &inherits {
        let rules = rules_for(&rules_by_sub, inh.sub);
        for rule in rules {
            if !crate::engine::matches_inherit(&rule.selector, inh, &ctx.ws) {
                continue;
            }
            let mut last: Option<gt_domain::model::NodeId> = None;
            exec_binding(ctx, rule, MatchCtx::Inherit(inh), inh.child, inh.child, inh.sub, phase, &mut last);
        }
    }
    ctx.ws.inherits = inherits;
}

fn collect_rules(ctx: &PipelineContext, phase: &Phase) -> HashMap<Option<i64>, Vec<Rule>> {
    let mut out: HashMap<Option<i64>, Vec<Rule>> = HashMap::new();
    for sub in &ctx.sub_projects {
        let mut rules = ctx
            .rules_by_sub
            .get(&sub.id.get())
            .cloned()
            .unwrap_or_default();
        rules.extend(
            ctx.rules_global
                .iter()
                .filter(|(l, _)| *l == sub.language)
                .map(|(_, r)| r.clone()),
        );
        let filtered: Vec<Rule> = dedup(rules)
            .into_iter()
            .filter(|r| r.phase == *phase)
            .collect();
        out.insert(Some(sub.id.get()), filtered);
    }
    let global: Vec<Rule> = ctx
        .rules_global
        .iter()
        .map(|(_, r)| r)
        .filter(|r| r.phase == *phase)
        .cloned()
        .collect();
    out.insert(None, global);
    out
}

fn dedup(rules: Vec<Rule>) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for r in rules {
        if !out.iter().any(|x| x.id == r.id) {
            out.push(r);
        }
    }
    out
}

/// Borrow the rule slice: avoids cloning the whole rule set on every match (that was the previous performance bottleneck).
fn rules_for<'a>(map: &'a HashMap<Option<i64>, Vec<Rule>>, sub: Option<SubProjectId>) -> &'a [Rule] {
    match map.get(&sub.map(|s| s.get())) {
        Some(v) => v.as_slice(),
        None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use serde_json::json;

    use crate::context::PipelineContext;
    use crate::workspace::{CallRecord, ConfigRecord, InheritRecord};
    use gt_domain::model::fkb::AccumulateSpec;
    use gt_domain::model::{
        Action, AnnotateAction, AnnotateTarget, AnnotationSpec, Direction, EdgeKind,
        ExpandSpec, ExpandVariant, FactValue, FanInThresholds, FieldSpec, IdentitySpec, Language,
        LinkAction, LinkSpec, MergeStrategy, NewEdge, NewNode, NodeId, NodeKind, Phase, Predicate,
        Project, ProjectAction, ProjectConfig, ProjectId, ProjectStatus, ResolveAs, Rule, Selector,
        Span, SubProject, SubProjectId, SubkindSource, SynthesizeAction, SynthesizedKind, ValueSource,
    };

    const SUB: i64 = 1;

    fn project() -> Project {
        Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: PathBuf::from("/p"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Ready,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn sub(root: &str, language: &str) -> SubProject {
        SubProject {
            id: SubProjectId::new(SUB),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: PathBuf::from(root),
            language: Language::new(language),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: vec![],
            facts: serde_json::Value::Null,
        }
    }

    /// A minimal FKB `Rule`; only `id` / `phase` matter here, so `binding` is empty.
    ///
    /// `languages` is *not* consulted by this module — the FKB loader already subset rules by language
    /// (`gt-adapter-fkb/src/loader.rs`), so nothing downstream re-checks it.
    fn rule(id: &str, phase: &str, languages: Option<&str>) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Call { callee: None, r#where: vec![] },
            binding: vec![],
            confidence: 1.0,
            languages: languages.map(|l| vec![Language::new(l)]),
        }
    }

    fn ids(rules: &[Rule]) -> Vec<String> {
        rules.iter().map(|r| r.id.clone()).collect()
    }

    // ---- dedup: by `id`, first occurrence wins ----

    #[test]
    fn dedup_keeps_first_occurrence_by_id() {
        let out = dedup(vec![
            rule("a", "AnnotatePre", None),
            rule("b", "AnnotatePre", None),
            rule("a", "AnnotatePre", None),
        ]);
        assert_eq!(ids(&out), vec!["a".to_string(), "b".to_string()]);
    }

    // ---- sub_relative: project-relative path -> sub-project-relative ----

    #[test]
    fn sub_relative_returns_file_when_no_sub() {
        let ctx = PipelineContext::new(project());
        assert_eq!(
            sub_relative(&ctx, None, "crmeb/app/event.php"),
            "crmeb/app/event.php"
        );
    }

    #[test]
    fn sub_relative_strips_sub_project_root_prefix() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // `file` is project-relative (`crmeb/app/event.php`); the sub root relative to the project is `crmeb/app`.
        assert_eq!(
            sub_relative(&ctx, Some(SubProjectId::new(SUB)), "crmeb/app/event.php"),
            "event.php"
        );
    }

    #[test]
    fn sub_relative_returns_file_when_sub_id_is_unknown() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // A record whose sub id is not a registered sub-project: there is no sub root to strip against,
        // so the path is returned unchanged rather than panicking or mis-stripping.
        assert_eq!(
            sub_relative(&ctx, Some(SubProjectId::new(999)), "crmeb/app/event.php"),
            "crmeb/app/event.php"
        );
    }

    // ---- collect_rules: global rules are scoped by language; everything filtered by phase ----

    #[test]
    fn collect_rules_scopes_global_rules_by_language() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        ctx.rules_by_sub
            .insert(SUB, vec![rule("own", "AnnotatePre", None)]);
        ctx.rules_global.push((Language::new("php"), rule("g_php", "AnnotatePre", None)));
        ctx.rules_global.push((Language::new("java"), rule("g_java", "AnnotatePre", None)));

        let map = collect_rules(&ctx, &Phase::new("AnnotatePre"));

        // The PHP sub sees its own rule + the PHP global rule, never the Java one.
        assert_eq!(
            ids(map.get(&Some(SUB)).unwrap()),
            vec!["own".to_string(), "g_php".to_string()]
        );
        // The `None` (cross-project) entry carries every global rule of this phase, regardless of language.
        assert_eq!(
            ids(map.get(&None).unwrap()),
            vec!["g_php".to_string(), "g_java".to_string()]
        );
    }

    #[test]
    fn collect_rules_filters_by_phase() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                rule("own_pre", "AnnotatePre", None),
                rule("own_post", "AnnotatePost", None),
            ],
        );
        ctx.rules_global.push((Language::new("php"), rule("g_pre", "AnnotatePre", None)));

        let pre = collect_rules(&ctx, &Phase::new("AnnotatePre"));
        assert_eq!(
            ids(pre.get(&Some(SUB)).unwrap()),
            vec!["own_pre".to_string(), "g_pre".to_string()]
        );
        assert_eq!(ids(pre.get(&None).unwrap()), vec!["g_pre".to_string()]);

        let post = collect_rules(&ctx, &Phase::new("AnnotatePost"));
        assert_eq!(ids(post.get(&Some(SUB)).unwrap()), vec!["own_post".to_string()]);
        assert!(post.get(&None).unwrap().is_empty());
    }

    // ---- rules_for: a pure lookup — unknown sub -> empty, `None` sub -> global entry ----

    #[test]
    fn rules_for_unknown_sub_is_empty_and_none_is_global() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        ctx.rules_by_sub
            .insert(SUB, vec![rule("own", "AnnotatePre", None)]);
        ctx.rules_global.push((Language::new("php"), rule("g_php", "AnnotatePre", None)));

        let map = collect_rules(&ctx, &Phase::new("AnnotatePre"));
        // A sub id not present in the map yields an empty slice; the `None` entry is a distinct key.
        assert!(rules_for(&map, Some(SubProjectId::new(999))).is_empty());
        assert_eq!(ids(rules_for(&map, None)), vec!["g_php".to_string()]);
    }

    // ---------------------------------------------------------------- shared fixtures

    fn span() -> Span {
        Span { start_line: 1, end_line: 1, start_byte: 0, end_byte: 0 }
    }

    fn sub_n(root: &str, language: &str, id: i64) -> SubProject {
        let mut s = sub(root, language);
        s.id = SubProjectId::new(id);
        s
    }

    /// Add a graph node belonging to sub `sub`.
    fn node(ctx: &mut PipelineContext, kind: &str, sub: i64) -> NodeId {
        node_with_props(ctx, kind, sub, serde_json::Value::Null)
    }

    /// Same, carrying properties (so `where: property_is` has something to read).
    fn node_with_props(ctx: &mut PipelineContext, kind: &str, sub: i64, props: serde_json::Value) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: Some(SubProjectId::new(sub)),
            kind: NodeKind::from(kind),
            name: kind.to_string(),
            fqn: Some(kind.to_string()),
            identity: None,
            file_id: None,
            span: span(),
            language: Language::new("php"),
            phase: Phase::new(""),
            confidence: 1.0,
            properties: props,
        })
    }

    /// An `Annotate` action tagging `kind` onto the matched node.
    fn annotate(kind: &str) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::Matched,
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// Same, but the annotation lands on the node this rule synthesised (`@last` / `@self`).
    fn annotate_last(kind: &str) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::SynthesizedRef("@last".into()),
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// Same, but gated on a phase: the action only runs when the rule's phase matches.
    fn annotate_phase(phase: &str, kind: &str) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::Matched,
            phase: Some(Phase::new(phase)),
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// `target: { from_field: ... }` — the annotation lands on the nodes named by a value source
    /// (`fkb/php/thinkphp.yaml` uses this to stamp `listener` on the classes an event entry lists).
    fn annotate_from_field(kind: &str) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::FromField {
                source: ValueSource { array_values: Some(true), ..ValueSource::default() },
                resolve: Some(ResolveAs::ClassConst),
            },
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// Same, with an explicit merge strategy (the default `MaxByKind` keeps only the best per kind).
    fn annotate_merge(kind: &str, merge: MergeStrategy, confidence: f32) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::Matched,
            merge,
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                confidence,
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// Same, with an explicit spec confidence and an action-level decay factor.
    fn annotate_conf(kind: &str, confidence: f32, scale: Option<f32>) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::Matched,
            confidence_scale: scale,
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                confidence,
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// A rule assembled from parts, so tests can pick selector / binding / confidence freely.
    fn rule_of(id: &str, phase: &str, selector: Selector, binding: Vec<Action>, confidence: f32) -> Rule {
        Rule { id: id.to_string(), phase: Phase::new(phase), selector, binding, confidence, languages: None }
    }

    /// A call-site selector.
    fn call_sel(callee: &str) -> Selector {
        Selector::Call { callee: Some(callee.to_string()), r#where: vec![] }
    }

    /// A graph-node selector.
    fn node_sel(kind: Option<&str>) -> Selector {
        Selector::Node { node_kind: kind.map(NodeKind::from), r#where: vec![] }
    }

    /// A config-entry selector.
    fn config_sel(file: Option<&str>) -> Selector {
        Selector::ConfigEntry { file: file.map(|f| f.to_string()), key_path: None, r#where: vec![] }
    }

    /// A `Synthesize` whose identity source yields nothing — it cannot build a node.
    fn synth_without_identity() -> Action {
        Action::Synthesize(SynthesizeAction {
            node: NodeKind::from("Table"),
            ..SynthesizeAction::default()
        })
    }

    /// An `Annotate` action carrying a subkind (`subkind: { literal: ... }` etc.).
    fn annotate_subkind(kind: &str, subkind: SubkindSource) -> Action {
        Action::Annotate(AnnotateAction {
            target: AnnotateTarget::Matched,
            annotations: vec![AnnotationSpec {
                kind: kind.to_string(),
                subkind: Some(subkind),
                ..AnnotationSpec::default()
            }],
            ..AnnotateAction::default()
        })
    }

    /// A `Link` action: an edge between two nodes found by name.
    fn link_action(kind: &str, from: ValueSource, to: ValueSource) -> Action {
        Action::Link(LinkAction {
            kind: EdgeKind::from(kind),
            from: Some(from),
            to: Some(to),
            resolve: None,
            confidence: None,
        })
    }

    /// A `Project` action: walk the matched node's `along` out-edges and rebuild them as `kind`.
    fn project_action(kind: &str, along: &str, to: Vec<String>) -> Action {
        Action::Project(ProjectAction {
            kind: EdgeKind::from(kind),
            along: EdgeKind::from(along),
            from: vec![],
            to,
            confidence: None,
        })
    }

    /// An i18n-shaped `Synthesize`: the node's `texts` field accumulates one entry per locale
    /// (`fkb/php/thinkphp.yaml` `accumulate: { key: { locale: true }, value: { entry_value: true } }`).
    fn synth_i18n_action() -> Action {
        Action::Synthesize(SynthesizeAction {
            node: NodeKind::from("I18nKey"),
            identity: IdentitySpec {
                kind: SynthesizedKind(SynthesizedKind::NAMED.to_string()),
                value: Some(ValueSource { key_path: Some(true), ..ValueSource::default() }),
                ..IdentitySpec::default()
            },
            fields: vec![FieldSpec {
                name: "texts".into(),
                accumulate: Some(AccumulateSpec {
                    key: ValueSource { locale: Some(true), ..ValueSource::default() },
                    value: ValueSource { entry_value: Some(true), ..ValueSource::default() },
                }),
                ..FieldSpec::default()
            }],
            ..SynthesizeAction::default()
        })
    }

    /// A `Route::resource`-shaped `Synthesize`: one call site expands into N contracts, each variant
    /// injecting its method into `{ expand_method: true }` (the variant table itself comes from FKB).
    fn synth_expand_action() -> Action {
        Action::Synthesize(SynthesizeAction {
            node: NodeKind::from("HttpContract"),
            identity: IdentitySpec {
                kind: SynthesizedKind(SynthesizedKind::CONTRACT_ID.to_string()),
                method: Some(ValueSource { expand_method: Some(true), ..ValueSource::default() }),
                path: Some(ValueSource { arg: Some(0), ..ValueSource::default() }),
                ..IdentitySpec::default()
            },
            expand: Some(ExpandSpec {
                variants: vec![
                    ExpandVariant {
                        name: "index".into(),
                        method: Some("GET".into()),
                        path_suffix: None,
                        entry: None,
                    },
                    ExpandVariant {
                        name: "store".into(),
                        method: Some("POST".into()),
                        path_suffix: None,
                        entry: None,
                    },
                ],
                only: None,
                except: None,
            }),
            ..SynthesizeAction::default()
        })
    }

    /// Add a node with an explicit name / FQN (so `from_field` can resolve a class reference to it).
    fn node_fqn(ctx: &mut PipelineContext, kind: &str, name: &str, fqn: &str, sub: i64) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: Some(SubProjectId::new(sub)),
            kind: NodeKind::from(kind),
            name: name.to_string(),
            fqn: Some(fqn.to_string()),
            identity: None,
            file_id: None,
            span: span(),
            language: Language::new("php"),
            phase: Phase::new(""),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn node_rule(id: &str, phase: &str, kind: Option<&str>, ann: &str) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Node {
                node_kind: kind.map(NodeKind::from),
                r#where: vec![],
            },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    fn call_rule(id: &str, phase: &str, callee: &str, ann: &str) -> Rule {
        call_rule_where(id, phase, callee, vec![], ann)
    }

    /// A call rule carrying `where` predicates.
    fn call_rule_where(id: &str, phase: &str, callee: &str, r#where: Vec<Predicate>, ann: &str) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Call { callee: Some(callee.to_string()), r#where },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    /// A node rule carrying `where` predicates.
    fn node_rule_where(
        id: &str,
        phase: &str,
        kind: Option<&str>,
        r#where: Vec<Predicate>,
        ann: &str,
    ) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Node { node_kind: kind.map(NodeKind::from), r#where },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    /// Register a call site on `at`, split into receiver / method as the parser would.
    fn push_call(ctx: &mut PipelineContext, at: NodeId, callee: &str, args: Vec<FactValue>) {
        let (receiver, method) = match callee.split_once("::") {
            Some((r, m)) => (Some(r.to_string()), Some(m.to_string())),
            None => (None, Some(callee.to_string())),
        };
        ctx.ws.calls.push(CallRecord {
            node: at,
            owner: at,
            owner_fqn: "App\\X".into(),
            owner_class: None,
            callee: callee.into(),
            receiver,
            method,
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: span(),
            file: "crmeb/app/x.php".into(),
            sub: Some(SubProjectId::new(SUB)),
            language: Language::new("php"),
        });
    }

    fn config_rule(id: &str, phase: &str, file: Option<&str>, ann: &str) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::ConfigEntry {
                file: file.map(|f| f.to_string()),
                key_path: None,
                r#where: vec![],
            },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    fn inherit_rule(id: &str, phase: &str, base: &str, ann: &str) -> Rule {
        inherit_rule_with(id, phase, base, None, ann)
    }

    /// Same, plus `with_property`: the rule only fires when the child carries that class property.
    fn inherit_rule_with(id: &str, phase: &str, base: &str, with_property: Option<&str>, ann: &str) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Inheritance {
                base: Some(base.to_string()),
                with_property: with_property.map(|p| p.to_string()),
            },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    /// An inheritance rule with `base: None`: the selector does not name a base class, so it admits every record.
    fn inherit_rule_any(id: &str, phase: &str, ann: &str) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Inheritance { base: None, with_property: None },
            binding: vec![annotate(ann)],
            confidence: 1.0,
            languages: None,
        }
    }

    /// The `Synthesize` half of the real FKB shape (`fkb/php/illuminate-database.yaml` `model-table`):
    /// `class X extends Model` becomes a `Table` node plus a `MapsTo` edge.
    fn synth_table_action() -> Action {
        Action::Synthesize(SynthesizeAction {
            node: NodeKind::from("Table"),
            identity: IdentitySpec {
                kind: SynthesizedKind(SynthesizedKind::FQN.to_string()),
                // `{ self: true }` resolves to the inheriting class's FQN in an inheritance match.
                value: Some(ValueSource { self_value: Some(true), ..ValueSource::default() }),
                ..IdentitySpec::default()
            },
            link: Some(LinkSpec {
                kind: EdgeKind::from("MapsTo"),
                direction: Direction::Incoming,
                ..LinkSpec::default()
            }),
            ..SynthesizeAction::default()
        })
    }

    fn inherit_rule_synth(id: &str, phase: &str, base: &str) -> Rule {
        inherit_rule_binding(id, phase, base, vec![synth_table_action()])
    }

    /// The full FKB shape: synthesise, then annotate **the node just synthesised**
    /// (`target: { synthesized_ref: "@self" }`, see `fkb/php/thinkphp.yaml` `event-registry`).
    fn inherit_rule_synth_then_annotate(id: &str, phase: &str, base: &str, ann: &str) -> Rule {
        inherit_rule_binding(id, phase, base, vec![synth_table_action(), annotate_last(ann)])
    }

    /// A rule whose binding is only `Annotate` at `@last`, with no `Synthesize` of its own.
    fn inherit_rule_annotate_last(id: &str, phase: &str, base: &str, ann: &str) -> Rule {
        inherit_rule_binding(id, phase, base, vec![annotate_last(ann)])
    }

    fn inherit_rule_binding(id: &str, phase: &str, base: &str, binding: Vec<Action>) -> Rule {
        Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: Selector::Inheritance {
                base: Some(base.to_string()),
                with_property: None,
            },
            binding,
            confidence: 1.0,
            languages: None,
        }
    }

    /// `child_fqn extends base`, owned by sub `sub` (`None` = the cross-project bucket).
    fn inherit(ctx: &mut PipelineContext, child: NodeId, child_fqn: &str, base: &str, sub: Option<i64>) {
        ctx.ws.inherits.push(InheritRecord {
            child,
            child_fqn: child_fqn.into(),
            base: base.into(),
            kind: EdgeKind::from("Extends"),
            sub: sub.map(SubProjectId::new),
            file: "crmeb/app/x.php".into(),
            span: span(),
        });
    }

    // ---------------------------------------------------------------- run_post: node-kind grouping + wild fallback

    #[test]
    fn run_post_node_kind_rule_annotates_only_matching_kind() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        let c = node(&mut ctx, "Class", SUB);
        ctx.rules_by_sub
            .insert(SUB, vec![node_rule("n1", "AnnotatePost", Some("Method"), "hit")]);
        run_post(&mut ctx);
        assert!(ctx.ws.has_annotation(m, "hit"));
        assert!(!ctx.ws.has_annotation(c, "hit"));
    }

    #[test]
    fn run_post_wild_node_rule_hits_every_node() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let a = node(&mut ctx, "Method", SUB);
        let b = node(&mut ctx, "Class", SUB);
        // A `node_kind: None` rule lives in the global (`None`) bucket and must fire on every node.
        ctx.rules_global
            .push((Language::new("php"), node_rule("w", "AnnotatePost", None, "wild")));
        run_post(&mut ctx);
        assert!(ctx.ws.has_annotation(a, "wild"));
        assert!(ctx.ws.has_annotation(b, "wild"));
    }

    #[test]
    fn run_post_dedups_same_rule_id_across_subs() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub_n("/p/a", "php", 1));
        ctx.sub_projects.push(sub_n("/p/b", "php", 2));
        let n = node(&mut ctx, "Method", 2);
        // Same id in two subs' rule lists must be de-duplicated in the by_kind slot,
        // so the target node is annotated exactly once.
        ctx.rules_by_sub
            .insert(1, vec![node_rule("dup", "AnnotatePost", Some("Method"), "x")]);
        ctx.rules_by_sub
            .insert(2, vec![node_rule("dup", "AnnotatePost", Some("Method"), "x")]);
        run_post(&mut ctx);
        assert_eq!(ctx.ws.annotations_of(n).len(), 1);
    }

    // ---------------------------------------------------------------- apply_source_rules: call + config

    #[test]
    fn run_pre_call_rule_annotates_matched_call_site() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        ctx.ws.calls.push(CallRecord {
            node: m,
            owner: m,
            owner_fqn: "App\\X".into(),
            owner_class: None,
            callee: "Db::name".into(),
            // `Db::name` is parsed by `matches_call` as receiver `Db` / method `name`, so the record
            // must carry both for the selector to match.
            receiver: Some("Db".into()),
            method: Some("name".into()),
            args: vec![],
            db_table: None,
            in_loop: false,
            entity: None,
            span: span(),
            file: "crmeb/app/x.php".into(),
            sub: Some(SubProjectId::new(SUB)),
            language: Language::new("php"),
        });
        ctx.rules_by_sub
            .insert(SUB, vec![call_rule("c1", "AnnotatePre", "Db::name", "called")]);
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(m, "called"));
    }

    #[test]
    fn run_pre_config_rule_matches_project_relative_file() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // Real FKB config selectors are written app-relative (e.g. `app/event.php`); the config entry's
        // `file` is project-relative (`crmeb/app/event.php`). `path_matches` ends-with matching reconciles them.
        ctx.ws.configs.push(ConfigRecord {
            file: "crmeb/app/event.php".into(),
            key_path: "listen.x".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: Some(SubProjectId::new(SUB)),
            locale: None,
            file_stem: None,
        });
        ctx.rules_by_sub
            .insert(SUB, vec![config_rule("cfg1", "AnnotatePre", Some("app/event.php"), "cfg")]);
        run_pre(&mut ctx);
        // No file node exists, so `cfg_node` falls back to `NodeId(1)` as the landing point.
        assert!(ctx.ws.has_annotation(NodeId(1), "cfg"));
    }

    #[test]
    fn run_pre_config_rule_matches_sub_relative_file() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // Config entry stored already sub-relative (no project prefix); the rule file matches it directly.
        ctx.ws.configs.push(ConfigRecord {
            file: "event.php".into(),
            key_path: "listen.x".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: Some(SubProjectId::new(SUB)),
            locale: None,
            file_stem: None,
        });
        ctx.rules_by_sub
            .insert(SUB, vec![config_rule("cfg2", "AnnotatePre", Some("event.php"), "cfg")]);
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(NodeId(1), "cfg"));
    }

    #[test]
    fn run_pre_config_rule_matches_via_app_root_expansion() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // A non-default app root: the rule uses the `{app_root}` placeholder, which `matches_config` must
        // expand (against this fact) to reconcile with the project-relative config path. Before the fix,
        // `matches_config` re-checked the *unexpanded* `{app_root}/event.php` and the rule never fired.
        ctx.ws
            .set_fact(SubProjectId::new(SUB), "app_root", json!({ "value": "crmeb/app" }));
        ctx.ws.configs.push(ConfigRecord {
            file: "crmeb/app/event.php".into(),
            key_path: "listen.x".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: Some(SubProjectId::new(SUB)),
            locale: None,
            file_stem: None,
        });
        ctx.rules_by_sub.insert(
            SUB,
            vec![config_rule("cfg1", "AnnotatePre", Some("{app_root}/event.php"), "cfg")],
        );
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(NodeId(1), "cfg"));
    }

    #[test]
    fn cfg_node_falls_back_to_node_one_when_file_node_absent() {
        let ctx = PipelineContext::new(project());
        let cfg = ConfigRecord {
            file: "nope.php".into(),
            key_path: "k".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: None,
            locale: None,
            file_stem: None,
        };
        assert_eq!(cfg_node(&ctx, &cfg), NodeId(1));
    }

    #[test]
    fn cfg_node_returns_the_file_node_when_present() {
        let mut ctx = PipelineContext::new(project());
        let f = ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind::from("File"),
            name: "event.php".into(),
            fqn: None,
            identity: None,
            file_id: None,
            span: span(),
            language: Language::new("php"),
            phase: Phase::new(""),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
        ctx.ws.record_file_node("nope.php", f);
        let cfg = ConfigRecord {
            file: "nope.php".into(),
            key_path: "k".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: None,
            locale: None,
            file_stem: None,
        };
        // Happy path: when a file node exists for the config's path, the annotation lands there rather
        // than falling back to `NodeId(1)`.
        assert_eq!(cfg_node(&ctx, &cfg), f);
    }

    #[test]
    fn run_pre_config_rule_without_file_filter_matches_any_config() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // A config entry whose file is nowhere the FKB predicted.
        ctx.ws.configs.push(ConfigRecord {
            file: "anything/at/all.php".into(),
            key_path: "k".into(),
            value: FactValue::String("v".into()),
            span: span(),
            sub: Some(SubProjectId::new(SUB)),
            locale: None,
            file_stem: None,
        });
        // `file: None` → the path filter is skipped (`!pattern.is_empty()` is false), so the rule matches
        // every config entry of the phase regardless of where the file lives.
        ctx.rules_by_sub
            .insert(SUB, vec![config_rule("cfg0", "AnnotatePre", None, "cfg")]);
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(NodeId(1), "cfg"));
    }

    // ---------------------------------------------------------------- apply_inherit_rules

    #[test]
    fn run_synthesize_inherit_rule_annotates_child_transitively() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        // `X extends Y extends Model`: the record's immediate base is `Y`, the rule targets `Model`.
        ctx.ws.record_supertype("X", "Y");
        ctx.ws.record_supertype("Y", "Model");
        ctx.ws.inherits.push(InheritRecord {
            child,
            child_fqn: "X".into(),
            base: "Y".into(),
            kind: EdgeKind::from("Extends"),
            sub: Some(SubProjectId::new(SUB)),
            file: "crmeb/app/x.php".into(),
            span: span(),
        });
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule("inh1", "Synthesize", "Model", "inh")]);
        run_synthesize(&mut ctx);
        assert!(ctx.ws.has_annotation(child, "inh"));
    }

    #[test]
    fn inherit_rule_matches_the_immediate_base_without_any_name_chain() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        // `X extends Model`: the record's own base already matches, so no `supertypes` entry is needed.
        // Guards the `recv_matches` short-circuit — the chain walk must not be the only path.
        inherit(&mut ctx, child, "X", "Model", Some(SUB));
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule("inh0", "Synthesize", "Model", "inh")]);
        run_synthesize(&mut ctx);
        assert!(ctx.ws.has_annotation(child, "inh"));
    }

    #[test]
    fn inherit_rule_skips_classes_outside_the_base_chain() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        // A perfectly real chain (`X extends Collection extends ArrayAccess`), just not the one asked for.
        // The negative nail: without it, "any non-empty supertypes entry matches" would pass silently.
        inherit(&mut ctx, child, "X", "Collection", Some(SUB));
        ctx.ws.record_supertype("X", "Collection");
        ctx.ws.record_supertype("Collection", "ArrayAccess");
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule("inh1", "Synthesize", "Model", "inh")]);
        run_synthesize(&mut ctx);
        assert!(!ctx.ws.has_annotation(child, "inh"));
    }

    #[test]
    fn inherit_base_matches_namespace_segments_not_substrings() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        // FKB writes the short name (`Model`), the chain holds fully-qualified names.
        inherit(&mut ctx, child, "App\\Models\\Product", "App\\Models\\BaseModel", Some(SUB));
        ctx.ws
            .record_supertype("App\\Models\\Product", "App\\Models\\BaseModel");
        ctx.ws
            .record_supertype("App\\Models\\BaseModel", "Illuminate\\Database\\Eloquent\\Model");
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                inherit_rule("short", "Synthesize", "Model", "short"),
                inherit_rule("tail", "Synthesize", "Eloquent\\Model", "tail"),
                // `Mod` is a prefix of `Model`, not a namespace segment: no separator boundary, so no match.
                inherit_rule("prefix", "Synthesize", "Mod", "prefix"),
            ],
        );
        run_synthesize(&mut ctx);
        assert!(ctx.ws.has_annotation(child, "short"));
        assert!(ctx.ws.has_annotation(child, "tail"));
        assert!(!ctx.ws.has_annotation(child, "prefix"));
    }

    #[test]
    fn inherit_chain_with_a_cycle_terminates_without_matching() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        // A broken hierarchy (`A extends B extends A`): `has_supertype` must terminate (visited set + step cap)
        // and answer "no", instead of spinning until the build hangs.
        inherit(&mut ctx, child, "A", "B", Some(SUB));
        ctx.ws.record_supertype("A", "B");
        ctx.ws.record_supertype("B", "A");
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule("cyc", "Synthesize", "Model", "inh")]);
        run_synthesize(&mut ctx);
        assert!(!ctx.ws.has_annotation(child, "inh"));
    }

    #[test]
    fn inherit_rule_with_property_requires_the_property_on_the_child() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let with = node(&mut ctx, "Class", SUB);
        let without = node(&mut ctx, "Class", SUB);
        // `protected $table = 'users'` on the model: recorded as a class property default.
        ctx.ws.record_property(with, "table", FactValue::String("users".into()));
        inherit(&mut ctx, with, "User", "Model", Some(SUB));
        inherit(&mut ctx, without, "Role", "Model", Some(SUB));
        ctx.rules_by_sub.insert(
            SUB,
            vec![inherit_rule_with("prop", "Synthesize", "Model", Some("table"), "inh")],
        );
        run_synthesize(&mut ctx);
        assert!(ctx.ws.has_annotation(with, "inh"));
        assert!(!ctx.ws.has_annotation(without, "inh"));
    }

    #[test]
    fn inherit_rules_are_scoped_to_the_record_sub_project() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let orphan = node(&mut ctx, "Class", SUB);
        let cross = node(&mut ctx, "Class", SUB);
        // A record whose sub is not a registered sub-project has no rule bucket: silent skip, not a crash
        // and not a fall back to some other sub's rules.
        inherit(&mut ctx, orphan, "X", "Model", Some(999));
        // A record with no sub lands in the `None` bucket, fed by `rules_global`.
        inherit(&mut ctx, cross, "Y", "Model", None);
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule("sub", "Synthesize", "Model", "sub")]);
        ctx.rules_global
            .push((Language::new("php"), inherit_rule("glob", "Synthesize", "Model", "glob")));
        run_synthesize(&mut ctx);
        assert!(!ctx.ws.has_annotation(orphan, "sub"));
        assert!(ctx.ws.has_annotation(cross, "glob"));
    }

    #[test]
    fn inherit_rule_synthesize_binding_builds_node_and_edge() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        inherit(&mut ctx, child, "X", "Model", Some(SUB));
        // The production binding shape: not `Annotate` but `Synthesize` (Table + MapsTo), which is what
        // every real FKB inheritance rule actually declares.
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule_synth("model-table", "Synthesize", "Model")]);
        run_synthesize(&mut ctx);

        let tables = ctx.ws.nodes_of_kind("Table");
        assert_eq!(tables.len(), 1, "exactly one Table node should be synthesised");
        let table = tables[0];
        assert_eq!(
            ctx.ws.node(table).map(|n| n.name.clone()),
            Some("X".into()),
            "the table is named after the inheriting class FQN"
        );
        assert!(
            ctx.ws
                .out_edges_of(child)
                .iter()
                .any(|(k, to)| k == "MapsTo" && *to == table.get()),
            "the child class should point at the synthesised Table"
        );
    }

    // ---------------------------------------------------------------- selector `where` predicates

    #[test]
    fn node_rule_where_predicate_gates_the_match() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let hit = node_with_props(&mut ctx, "Method", SUB, json!({ "side": "frontend" }));
        let miss = node_with_props(&mut ctx, "Method", SUB, json!({ "side": "backend" }));
        // The selector matched both nodes (same kind); only the predicate separates them. Every other
        // test in this file passes `where: []`, so nothing guarded `eval_predicate` being applied at all.
        ctx.rules_by_sub.insert(
            SUB,
            vec![node_rule_where(
                "w1",
                "AnnotatePost",
                Some("Method"),
                vec![Predicate::PropertyIs { name: "side".into(), value: "frontend".into() }],
                "hit",
            )],
        );
        run_post(&mut ctx);
        assert!(ctx.ws.has_annotation(hit, "hit"));
        assert!(!ctx.ws.has_annotation(miss, "hit"));
    }

    #[test]
    fn call_rule_where_predicate_reads_the_call_context() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let prefixed = node(&mut ctx, "Method", SUB);
        let plain = node(&mut ctx, "Method", SUB);
        // Same callee, different first argument: the predicate needs the call match-context, not just the node.
        push_call(&mut ctx, prefixed, "Db::name", vec![FactValue::String("eb_orders".into())]);
        push_call(&mut ctx, plain, "Db::name", vec![FactValue::String("orders".into())]);
        ctx.rules_by_sub.insert(
            SUB,
            vec![call_rule_where(
                "c1",
                "AnnotatePre",
                "Db::name",
                vec![Predicate::ArgStartsWith { arg: 0, prefix: "eb_".into() }],
                "prefixed",
            )],
        );
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(prefixed, "prefixed"));
        assert!(!ctx.ws.has_annotation(plain, "prefixed"));
    }

    // ---------------------------------------------------------------- AnnotateTarget / phase gate / idempotency

    #[test]
    fn annotate_target_synthesized_ref_lands_on_the_new_node() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        inherit(&mut ctx, child, "X", "Model", Some(SUB));
        // `Synthesize` then `Annotate { target: { synthesized_ref: "@self" } }` -- the shape
        // `fkb/php/thinkphp.yaml` uses to stamp an alias onto the node it just built.
        ctx.rules_by_sub.insert(
            SUB,
            vec![inherit_rule_synth_then_annotate("model-alias", "Synthesize", "Model", "aliased")],
        );
        run_synthesize(&mut ctx);

        let tables = ctx.ws.nodes_of_kind("Table");
        assert_eq!(tables.len(), 1, "the Table node should still be synthesised");
        assert!(
            ctx.ws.has_annotation(tables[0], "aliased"),
            "the annotation must land on the synthesised node"
        );
        assert!(
            !ctx.ws.has_annotation(child, "aliased"),
            "not on the matched class -- that is what `target: matched` would do"
        );
    }

    #[test]
    fn last_synth_does_not_leak_into_another_rule() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        inherit(&mut ctx, child, "X", "Model", Some(SUB));
        // `last_synth` is created per (record, rule) -- see the three `let mut last = None` in this file --
        // so a rule that only annotates `@last` cannot borrow the node another rule synthesised.
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                inherit_rule_synth("model-table", "Synthesize", "Model"),
                inherit_rule_annotate_last("orphan-alias", "Synthesize", "Model", "aliased"),
            ],
        );
        run_synthesize(&mut ctx);
        assert_eq!(ctx.ws.nodes_of_kind("Table").len(), 1, "the first rule still synthesises");
        assert_eq!(ctx.ws.annotation_count(), 0, "the second rule sees no `@last` and stays silent");
    }

    #[test]
    fn annotate_action_phase_gate_skips_mismatched_actions() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        push_call(&mut ctx, m, "Db::name", vec![]);
        // One rule per phase (P4 matches call sites, P6 matches graph nodes), each carrying both a
        // `Pre` and a `Post` action -- so the four outcomes are distinguishable by kind.
        let rule = |id: &str, phase: &str, sel: Selector, pre: &str, post: &str| Rule {
            id: id.to_string(),
            phase: Phase::new(phase),
            selector: sel,
            binding: vec![annotate_phase("Pre", pre), annotate_phase("Post", post)],
            confidence: 1.0,
            languages: None,
        };
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                rule(
                    "r-pre",
                    "AnnotatePre",
                    Selector::Call { callee: Some("Db::name".into()), r#where: vec![] },
                    "pre-ok",
                    "pre-leak",
                ),
                rule(
                    "r-post",
                    "AnnotatePost",
                    Selector::Node { node_kind: Some(NodeKind::from("Method")), r#where: vec![] },
                    "post-leak",
                    "post-ok",
                ),
            ],
        );
        run_pre(&mut ctx);
        run_post(&mut ctx);

        // An action whose declared phase differs from the rule's phase never runs: the rule is collected
        // for exactly one phase (`collect_rules` filters by equality), so the other action is unreachable.
        assert!(ctx.ws.has_annotation(m, "pre-ok"));
        assert!(ctx.ws.has_annotation(m, "post-ok"));
        assert!(!ctx.ws.has_annotation(m, "pre-leak"));
        assert!(!ctx.ws.has_annotation(m, "post-leak"));
    }

    #[test]
    fn running_the_same_phase_twice_does_not_duplicate_annotations() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        push_call(&mut ctx, m, "Db::name", vec![]);
        ctx.rules_by_sub
            .insert(SUB, vec![call_rule("c1", "AnnotatePre", "Db::name", "called")]);
        // `apply_source_rules` restores `ctx.ws.calls` after `mem::take`, so a second pass re-evaluates
        // the same record; the default `MaxByKind` merge must collapse it into one annotation.
        run_pre(&mut ctx);
        run_pre(&mut ctx);
        assert_eq!(ctx.ws.annotations_of(m).len(), 1);
    }

    // ---------------------------------------------------------------- inheritance selector without a base

    #[test]
    fn inherit_selector_without_a_base_matches_every_record() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let a = node(&mut ctx, "Class", SUB);
        let b = node(&mut ctx, "Class", SUB);
        inherit(&mut ctx, a, "A", "Whatever", Some(SUB));
        inherit(&mut ctx, b, "B", "Illuminate\\Database\\Eloquent\\Model", Some(SUB));
        // `base:` omitted means "any supertype" -- the name chain is not consulted at all.
        ctx.rules_by_sub
            .insert(SUB, vec![inherit_rule_any("any", "Synthesize", "any")]);
        run_synthesize(&mut ctx);
        assert!(ctx.ws.has_annotation(a, "any"));
        assert!(ctx.ws.has_annotation(b, "any"));
    }

    // ---------------------------------------------------------------- AnnotateTarget::FromField / merge / confidence

    #[test]
    fn annotate_target_from_field_resolves_the_named_nodes() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let listener = node_fqn(&mut ctx, "Class", "SmsListener", "App\\Listener\\SmsListener", SUB);
        let bystander = node(&mut ctx, "Class", SUB);
        ctx.ws.configs.push(ConfigRecord {
            file: "crmeb/app/event.php".into(),
            key_path: "listen.sms_send".into(),
            // The entry lists two classes; only one exists in the graph.
            value: FactValue::Array(vec![
                ("0".into(), FactValue::ClassConst("App\\Listener\\SmsListener".into())),
                ("1".into(), FactValue::ClassConst("App\\Listener\\Ghost".into())),
            ]),
            span: span(),
            sub: Some(SubProjectId::new(SUB)),
            locale: None,
            file_stem: None,
        });
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of(
                "cfg-listener",
                "AnnotatePre",
                config_sel(Some("app/event.php")),
                vec![annotate_from_field("listener")],
                1.0,
            )],
        );
        run_pre(&mut ctx);
        assert!(ctx.ws.has_annotation(listener, "listener"));
        assert!(!ctx.ws.has_annotation(bystander, "listener"));
        // The unresolvable class is dropped silently; it must not fall back to "the matched node".
        assert_eq!(ctx.ws.annotation_count(), 1);
    }

    #[test]
    fn coexist_merge_keeps_every_entry_of_the_same_kind() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        push_call(&mut ctx, m, "Db::name", vec![]);
        // Two rules tag the same kind on the same node; `Coexist` is what lets "both sanitised and
        // tainted" survive, where the default `MaxByKind` would keep only the stronger one.
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                rule_of(
                    "one",
                    "AnnotatePre",
                    call_sel("Db::name"),
                    vec![annotate_merge("raw", MergeStrategy::Coexist, 0.9)],
                    1.0,
                ),
                rule_of(
                    "two",
                    "AnnotatePre",
                    call_sel("Db::name"),
                    vec![annotate_merge("raw", MergeStrategy::Coexist, 0.4)],
                    1.0,
                ),
            ],
        );
        run_pre(&mut ctx);
        assert_eq!(ctx.ws.annotations_of(m).len(), 2);
    }

    #[test]
    fn annotation_confidence_multiplies_spec_rule_and_scale() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        push_call(&mut ctx, m, "Db::name", vec![]);
        // spec 0.8 x rule 0.5 x scale 0.5 = 0.2
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of(
                "c1",
                "AnnotatePre",
                call_sel("Db::name"),
                vec![annotate_conf("hit", 0.8, Some(0.5))],
                0.5,
            )],
        );
        run_pre(&mut ctx);
        let anns = ctx.ws.annotations_of(m);
        assert_eq!(anns.len(), 1);
        assert!((anns[0].confidence - 0.2).abs() < 1e-6, "confidence was {}", anns[0].confidence);
    }

    // ---------------------------------------------------------------- diagnostics

    #[test]
    fn unresolvable_annotate_target_is_diagnosed_not_panicking() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        // `target: { synthesized_ref: "@last" }` with no Synthesize before it: nothing to annotate.
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of("orphan", "AnnotatePost", node_sel(Some("Method")), vec![annotate_last("x")], 1.0)],
        );
        run_post(&mut ctx);
        assert!(!ctx.ws.has_annotation(m, "x"));
        assert!(
            ctx.ws
                .diagnostics
                .iter()
                .any(|d| d.code == "AnnotateTargetMissing" && d.phase.as_str() == "AnnotatePost"),
            "diagnostics: {:?}",
            ctx.ws.diagnostics.iter().map(|d| d.code.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn synthesise_without_an_identity_is_diagnosed() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let child = node(&mut ctx, "Class", SUB);
        inherit(&mut ctx, child, "X", "Model", Some(SUB));
        // A Synthesize with no identity source at all: no node may be invented for it.
        ctx.rules_by_sub.insert(
            SUB,
            vec![inherit_rule_binding(
                "bad",
                "Synthesize",
                "Model",
                vec![synth_without_identity()],
            )],
        );
        run_synthesize(&mut ctx);
        assert!(ctx.ws.nodes_of_kind("Table").is_empty());
        assert!(
            ctx.ws.diagnostics.iter().any(|d| d.code == "IdentityUnresolved"),
            "diagnostics: {:?}",
            ctx.ws.diagnostics.iter().map(|d| d.code.clone()).collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------- sub_of / app_root_map

    #[test]
    fn sub_of_reads_the_node_sub_and_none_for_unknown_nodes() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let n = node(&mut ctx, "Method", SUB);
        assert_eq!(sub_of(&ctx, n).map(|s| s.get()), Some(SUB));
        // A node id that is not in the graph (P6 also looks up freshly synthesised ids): no sub, no panic.
        assert_eq!(sub_of(&ctx, NodeId(9999)).map(|s| s.get()), None);
    }

    #[test]
    fn app_root_map_reads_the_fact_and_falls_back_to_app() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // No fact yet -> the documented default (the app directory is a project decision, never hard-coded
        // anywhere else, so this fallback is the only place `app` may appear).
        assert_eq!(app_root_map(&ctx).get(&SUB).map(|s| s.as_str()), Some("app"));
        ctx.ws
            .set_fact(SubProjectId::new(SUB), "app_root", json!({ "value": "crmeb/app" }));
        assert_eq!(app_root_map(&ctx).get(&SUB).map(|s| s.as_str()), Some("crmeb/app"));
        // A non-string fact is ignored rather than crashing.
        ctx.ws.set_fact(SubProjectId::new(SUB), "app_root", json!({ "value": 1 }));
        assert_eq!(app_root_map(&ctx).get(&SUB).map(|s| s.as_str()), Some("app"));
    }

    // ---------------------------------------------------------------- Synthesize: fields / expand

    #[test]
    fn synthesize_accumulate_field_merges_one_entry_per_record() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        // Two locale files, same key: they must merge into **one** node whose `texts` holds both locales.
        // English comes first — it is the source language and leads the i18n baseline.
        for (locale, text) in [("en", "hello-en"), ("zh", "hello-zh")] {
            ctx.ws.configs.push(ConfigRecord {
                file: format!("crmeb/app/lang/{}/messages.php", locale),
                key_path: "greeting".into(),
                value: FactValue::String(text.into()),
                span: span(),
                sub: Some(SubProjectId::new(SUB)),
                locale: Some(locale.into()),
                file_stem: None,
            });
        }
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of("i18n", "Synthesize", config_sel(Some("messages.php")), vec![synth_i18n_action()], 1.0)],
        );
        run_synthesize(&mut ctx);

        let keys = ctx.ws.nodes_of_kind("I18nKey");
        assert_eq!(keys.len(), 1, "both locales must merge into one node");
        let texts = ctx.ws.node(keys[0]).and_then(|n| n.properties.get("texts").cloned());
        assert_eq!(texts, Some(json!({ "en": "hello-en", "zh": "hello-zh" })), "texts: {:?}", texts);
    }

    #[test]
    fn synthesize_expand_builds_one_node_per_variant() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node(&mut ctx, "Method", SUB);
        // `Route::resource('cms', Ctrl::class)` — one call site, N contracts (the table comes from FKB).
        push_call(
            &mut ctx,
            m,
            "Route::resource",
            vec![FactValue::String("cms".into()), FactValue::String("App\\Ctrl".into())],
        );
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of("resource", "Synthesize", call_sel("Route::resource"), vec![synth_expand_action()], 1.0)],
        );
        run_synthesize(&mut ctx);

        let mut names: Vec<String> = ctx
            .ws
            .nodes_of_kind("HttpContract")
            .into_iter()
            .filter_map(|id| ctx.ws.node(id).map(|n| n.name.clone()))
            .collect();
        names.sort();
        assert_eq!(names, vec!["GET cms".to_string(), "POST cms".to_string()]);
    }

    // ---------------------------------------------------------------- subkind

    #[test]
    fn subkind_literal_and_from_symbol_table_are_resolved() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let m = node_fqn(&mut ctx, "Method", "run", "App\\Job\\Run", SUB);
        ctx.ws.put_symbol(ProjectId(1), "meta", "App\\Job\\Run", json!({ "owner": "team-a" }));
        ctx.rules_by_sub.insert(
            SUB,
            vec![
                rule_of(
                    "lit",
                    "AnnotatePost",
                    node_sel(Some("Method")),
                    vec![annotate_subkind("pii", SubkindSource::Literal("phone".into()))],
                    1.0,
                ),
                rule_of(
                    "sym",
                    "AnnotatePost",
                    node_sel(Some("Method")),
                    vec![annotate_subkind(
                        "owner",
                        SubkindSource::FromSymbolTable {
                            table: "meta".into(),
                            field: "owner".into(),
                            of: None,
                        },
                    )],
                    1.0,
                ),
            ],
        );
        run_post(&mut ctx);

        let anns = ctx.ws.annotations_of(m);
        let subkind_of = |kind: &str| {
            anns.iter().find(|a| a.kind == kind).and_then(|a| a.subkind.clone())
        };
        assert_eq!(subkind_of("pii"), Some("phone".into()));
        // `of` omitted -> keyed by the node's own FQN.
        assert_eq!(subkind_of("owner"), Some("team-a".into()));
    }

    #[test]
    fn subkind_from_fan_in_uses_the_threshold_labels() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let hot = node(&mut ctx, "Table", SUB);
        let cold = node(&mut ctx, "Table", SUB);
        // fan_in comes from incoming edges: 2 -> the `medium` label, 0 -> the low one.
        // Two distinct callers: `add_edge` de-duplicates by (kind, from, to), so repeating one
        // caller would stay at fan_in 1.
        for _ in 0..2 {
            let caller = node(&mut ctx, "Method", SUB);
            ctx.ws.add_edge(NewEdge {
                project_id: ProjectId(1),
                kind: EdgeKind::from("ReadsDb"),
                from_id: caller,
                to_id: hot,
                phase: Phase::new(""),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            });
        }
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of(
                "grade",
                "AnnotatePost",
                node_sel(Some("Table")),
                vec![annotate_subkind(
                    "hotness",
                    SubkindSource::FromFanIn {
                        thresholds: FanInThresholds {
                            high: 5,
                            medium: 2,
                            low_label: Some("cold".into()),
                            medium_label: Some("warm".into()),
                            high_label: Some("hot".into()),
                        },
                    },
                )],
                1.0,
            )],
        );
        run_post(&mut ctx);
        let subkind_of = |id: NodeId| {
            ctx.ws
                .annotations_of(id)
                .into_iter()
                .find(|a| a.kind == "hotness")
                .and_then(|a| a.subkind.clone())
        };
        assert_eq!(subkind_of(hot), Some("warm".into()));
        assert_eq!(subkind_of(cold), Some("cold".into()));
    }

    // ---------------------------------------------------------------- Link / Project actions

    #[test]
    fn link_action_builds_an_edge_between_named_nodes() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let a = node_fqn(&mut ctx, "Method", "run", "App\\A", SUB);
        let b = node_fqn(&mut ctx, "Method", "exec", "App\\B", SUB);
        // `{ self: true }` on a node match yields the node's FQN; the other end is a literal name.
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of(
                "l1",
                "AnnotatePost",
                node_sel(Some("Method")),
                vec![link_action(
                    "Calls",
                    ValueSource { self_value: Some(true), ..ValueSource::default() },
                    ValueSource { literal: Some("App\\B".into()), ..ValueSource::default() },
                )],
                1.0,
            )],
        );
        run_post(&mut ctx);
        assert!(
            ctx.ws.out_edges_of(a).iter().any(|(k, to)| k == "Calls" && *to == b.get()),
            "out edges of a: {:?}",
            ctx.ws.out_edges_of(a)
        );
    }

    #[test]
    fn project_action_rebuilds_edges_onto_another_layer() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        let entity = node(&mut ctx, "Class", SUB);
        let field = node(&mut ctx, "Field", SUB);
        let table = node(&mut ctx, "Table", SUB);
        // entity --HasColumn--> field --MapsTo--> table
        for (from, to, kind) in [(entity, field, "HasColumn"), (field, table, "MapsTo")] {
            ctx.ws.add_edge(NewEdge {
                project_id: ProjectId(1),
                kind: EdgeKind::from(kind),
                from_id: from,
                to_id: to,
                phase: Phase::new(""),
                confidence: 1.0,
                properties: serde_json::Value::Null,
            });
        }
        // Walk every `HasColumn` out-edge, follow `MapsTo` from its end, and rebuild it as `ReadsDb`.
        ctx.rules_by_sub.insert(
            SUB,
            vec![rule_of(
                "p1",
                "AnnotatePost",
                node_sel(Some("Class")),
                vec![project_action("ReadsDb", "HasColumn", vec!["MapsTo".into()])],
                1.0,
            )],
        );
        run_post(&mut ctx);
        assert!(
            ctx.ws
                .out_edges_of(entity)
                .iter()
                .any(|(k, to)| k == "ReadsDb" && *to == table.get()),
            "out edges of entity: {:?}",
            ctx.ws.out_edges_of(entity)
        );
    }

    // ---------------------------------------------------------------- collect_rules: sub rule wins over same-id global

    #[test]
    fn collect_rules_sub_rule_wins_over_same_id_global() {
        let mut ctx = PipelineContext::new(project());
        ctx.sub_projects.push(sub("/p/crmeb/app", "php"));
        ctx.rules_by_sub
            .insert(SUB, vec![node_rule("dup", "AnnotatePre", Some("Method"), "sub")]);
        ctx.rules_global
            .push((Language::new("php"), node_rule("dup", "AnnotatePre", Some("Method"), "glob")));
        let got = collect_rules(&ctx, &Phase::new("AnnotatePre"))
            .get(&Some(SUB))
            .unwrap()
            .to_vec();
        // `dedup` keeps the first occurrence (the sub-project rule), dropping the global one.
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "dup");
        match &got[0].binding[0] {
            Action::Annotate(a) => assert_eq!(a.annotations[0].kind, "sub"),
            _ => panic!("expected an Annotate action"),
        }
    }
}
