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
            let pattern = match file {
                Some(f) => {
                    let app_root = sub
                        .and_then(|s| app_root_by_sub.get(&s.get()).cloned())
                        .unwrap_or_else(|| "app".to_string());
                    crate::phase::prepare::expand(f, &app_root)
                }
                None => String::new(),
            };
            if !pattern.is_empty()
                && !path_matches(&pattern, &cfg.file)
                && !path_matches(&pattern, &sub_relative(ctx, sub, &cfg.file))
            {
                continue;
            }
            if !matches_config(&rule.selector, cfg, cfg_node(ctx, cfg), &ctx.ws) {
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

    use crate::context::PipelineContext;
    use gt_domain::model::{
        Language, Phase, Project, ProjectConfig, ProjectId, ProjectStatus, Rule, Selector, SubProject,
        SubProjectId,
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

    /// A minimal FKB `Rule`; only `id` / `phase` / `languages` matter for these tests, so `binding` is empty.
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
}
