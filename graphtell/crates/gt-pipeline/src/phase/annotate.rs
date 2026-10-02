//! P4 Annotate-Pre / P6 Annotate-Post.
//!
//! * **Pre** selectors act on **source code** (call sites / config entries / inheritance)
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
    let app_root_by_sub: HashMap<i64, String> = ctx
        .sub_projects
        .iter()
        .map(|s| {
            (
                s.id.get(),
                ctx.ws
                    .get_fact(s.id, "app_root")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("app")
                    .to_string(),
            )
        })
        .collect();

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
        let app_root = sub.map(|s| app_root_by_sub.get(&s.get()).cloned().unwrap_or_default());
        for rule in &rules {
            // A config-file selector is expanded by app_root before matching in the Post phase
            if let Selector::ConfigEntry { file: Some(f), .. } = &rule.selector {
                let _ = (f, &app_root);
                continue;
            }
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

/// Run one phase's rules against call sites and config entries.
fn apply_source_rules(ctx: &mut PipelineContext, phase: &Phase) {
    let rules_by_sub = collect_rules(ctx, phase);
    let app_root_by_sub: HashMap<i64, String> = ctx
        .sub_projects
        .iter()
        .map(|s| {
            (
                s.id.get(),
                ctx.ws
                    .get_fact(s.id, "app_root")
                    .and_then(|v| v.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("app")
                    .to_string(),
            )
        })
        .collect();

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
                    f.replace("{app_root}", &app_root)
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
