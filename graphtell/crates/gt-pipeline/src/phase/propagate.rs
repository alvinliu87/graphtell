//! P8 Propagate: propagate the "method → semantic node" action edges built at synthesis up the `Calls` call chain.
//!
//! # Motivation
//!
//! FKB rules only hit at **literal call sites** (e.g. `Queue::push`). If a high-level method ultimately calls that point through several wrapper layers,
//! only the innermost method gets connected to the semantic node; all outer callers are lost — exactly the root cause of framework wrappers like
//! `crmeb\utils\Queue::push → QueueThink::push` being swallowed.
//!
//! This violates a principle: **a feature that ultimately calls something the FKB recognizes should be resolved correctly, no matter how deep.**
//!
//! # Design (generic, bound to no framework)
//!
//! * Depends only on the already-built `Calls` edges (P7 has resolved method→method calls, including aliases / short names / receiver types).
//! * Only moves the fact "action-emitter method → semantic node": when `M --kind--> S` exists and `C` calls `M`,
//!   replicate `C --kind--> S` and recurse along the call chain to a fixed point (cycle-safe).
//! * Only go up along "method / function" nodes; class nodes (e.g. `MapsTo` sources) don't participate, avoiding mis-hanging semantic edges on classes.
//! * Independent of concrete edge kinds: `PublishesTo` / `ReadsDb` / `ReadsCache` / `ReadsConfig` etc. are all treated equally.

use std::collections::{BTreeSet, HashMap, HashSet};

use gt_domain::model::{EdgeKind, NewEdge, NodeId, Phase};
use serde_json::json;

use crate::context::{PipelineContext, PropSeed};

/// Run propagation. Must run after P7 (depends on its `Calls` edges; seeds are collected at P5, staged in ctx).
pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase(Phase::PROPAGATE.to_string());

    // Reverse call index: called method → all its "method / function" callers, along with that `Calls` edge's
    // resolution confidence (static call 1.0, variable-type inference lower). All decay during propagation is decided by these confidences.
    let mut callers: HashMap<i64, Vec<(i64, f32)>> = HashMap::new();
    for e in ctx.ws.edges() {
        if e.kind.as_str() != EdgeKind::CALLS {
            continue;
        }
        let Some(from) = ctx.ws.node(e.from_id) else {
            continue;
        };
        if !is_action_site(from.kind.as_str()) {
            continue;
        }
        callers
            .entry(e.to_id.get())
            .or_default()
            .push((e.from_id.get(), e.confidence));
    }

    let seeds = std::mem::take(&mut ctx.propagation_seeds);
    if seeds.is_empty() {
        return;
    }

    // Group by source: the reachable-caller set for the same source only needs computing once.
    let mut by_source: HashMap<i64, Vec<PropSeed>> = HashMap::new();
    for s in seeds {
        by_source.entry(s.source.get()).or_default().push(s);
    }

    let mut sources: Vec<i64> = by_source.keys().copied().collect();
    sources.sort_unstable();

    // `(kind, from, to)` → all root causes (direct edge starts, ascending); plus confidence / indirect flag.
    let mut edge_seeds: HashMap<(EdgeKind, i64, i64), BTreeSet<i64>> = HashMap::new();
    let mut edge_meta: HashMap<(EdgeKind, i64, i64), (f32, bool)> = HashMap::new();
    for src in &sources {
        let src_seeds = &by_source[src];
        let reach = transitive_callers(*src, &callers, ctx);
        for s in src_seeds {
            for (c, path_conf) in &reach {
                // Propagation confidence = seed confidence × product of path resolution confidences along the call chain.
                let (confidence, indirect) = propagated(&s.kind, s.confidence, *path_conf);
                // `from` is the caller reached going up the call chain, `to` is the semantic target;
                // `src` is the method node that actually performs the action (the root cause).
                let key = (EdgeKind(s.kind.clone()), *c, s.target.get());
                edge_seeds.entry(key.clone()).or_default().insert(*src);
                edge_meta.entry(key.clone()).or_insert((confidence, indirect));
            }
        }
    }
    let mut keys: Vec<(EdgeKind, i64, i64)> = edge_seeds.keys().cloned().collect();
    keys.sort_by(|a, b| (a.0.as_str(), a.1, a.2).cmp(&(b.0.as_str(), b.1, b.2)));

    let action_pairs: HashSet<(i64, i64)> = keys
        .iter()
        .filter(|(k, _, _)| k.as_str() == EdgeKind::READS_DB || k.as_str() == EdgeKind::WRITES_DB)
        .map(|(_, f, t)| (*f, *t))
        .collect();
    keys.retain(|(k, f, t)| k.as_str() != EdgeKind::MAPS_TO || !action_pairs.contains(&(*f, *t)));

    let mut added = 0usize;
    for key in keys {
        let seeds: Vec<i64> = edge_seeds[&key].iter().copied().collect();
        let (confidence, indirect) = edge_meta[&key];
        let mut props = json!({
            "via": "propagate",
            "seed_source": seeds[0],
            "seed_sources": seeds,
        });
        if indirect {
            props["indirect"] = json!(true);
        }
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: key.0,
            from_id: NodeId(key.1),
            to_id: NodeId(key.2),
            phase: phase.clone(),
            confidence,
            properties: props,
        });
        added += 1;
    }
    tracing::info!(
        "P8 传播完成：{} 个 source，{} 条传播边（共 {} 个根因）",
        by_source.len(),
        added,
        edge_seeds.values().map(|s| s.len()).sum::<usize>()
    );
}

/// From `src`, walk up the `callers` index to collect all reachable "method / function" callers (excluding src itself),
/// and return each caller's accumulated **resolution-confidence product** along the path (product of every `Calls` edge's confidence on the path).
///
/// A deterministic call chain (edge confidence 1.0) stays 1.0 on product → **no decay**; inferred / dynamic calls (<1.0) decay naturally.
/// `best` guarantees cycle safety and keeps only the highest product for the same node.
fn transitive_callers(
    src: i64,
    callers: &HashMap<i64, Vec<(i64, f32)>>,
    ctx: &PipelineContext,
) -> Vec<(i64, f32)> {
    let mut out: Vec<(i64, f32)> = Vec::new();
    let mut best: HashMap<i64, f32> = HashMap::new();
    let mut stack = vec![(src, 1.0f32)];
    best.insert(src, 1.0);
    while let Some((cur, cur_conf)) = stack.pop() {
        let Some(next) = callers.get(&cur) else {
            continue;
        };
        for (c, econf) in next {
            let path_conf = cur_conf * *econf;
            // Skip if already visited and the current path isn't better; otherwise update the best and keep going up.
            if let Some(prev) = best.get(c) {
                if *prev >= path_conf {
                    continue;
                }
            }
            best.insert(*c, path_conf);
            // Only include and continue up when the caller is a method / function; other nodes (class / file) don't enter the propagation chain.
            let is_site = ctx
                .ws
                .node(NodeId(*c))
                .map(|n| is_action_site(n.kind.as_str()))
                .unwrap_or(false);
            if is_site {
                out.push((*c, path_conf));
                stack.push((*c, path_conf));
            }
        }
    }
    out
}

/// Whether a node is an "action emitter": only methods / functions can be propagated as the source of a semantic action.
fn is_action_site(kind: &str) -> bool {
    kind == NODE_KIND_METHOD || kind == NODE_KIND_FUNCTION
}

/// Semantic-edge kinds marked "indirect" after propagation (environment reads).
///
/// Unlike "actions that really happen" (`ReadsDb` / `WritesDb` / `PublishesTo`: when the caller calls it,
/// the action really happened, propagation isn't distortion), `ReadsCache` / `ReadsConfig` are **environment reads**,
/// whose meaning weakens after propagation from "read here" to "somewhere upstream was read, this entry may be affected". Downstream uses this to
/// down-weight such edges as "indirect", avoiding a shared helper marking every entry it passes through as having read that config / cache.
///
/// Note: numeric decay **no longer** uses a fixed coefficient — a deterministic call chain (edge confidence 1.0) stays un-decayed on product,
/// only inferred / dynamic calls (edge confidence <1.0) decay naturally. Here we only set the `indirect` flag.
const DECAYED_KINDS: &[&str] = &[EdgeKind::READS_CONFIG, "ReadsCache"];

/// Compute a propagated edge's confidence and "is-indirect" flag.
///
/// * `base`: the seed's (innermost action emitter's) confidence.
/// * `path_conf`: the product of each `Calls` edge's resolution confidence along the call chain (given by `transitive_callers`).
///   Deterministic chain ≈ 1.0 → no decay; inferred / dynamic chain < 1.0 → natural decay.
/// * `indirect`: only flagged for environment-read kinds, so downstream can tell direct from indirect.
fn propagated(kind: &str, base: f32, path_conf: f32) -> (f32, bool) {
    let indirect = DECAYED_KINDS.contains(&kind);
    (base * path_conf, indirect)
}

const NODE_KIND_METHOD: &str = "Method";
const NODE_KIND_FUNCTION: &str = "Function";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PropSeed;
    use gt_domain::model::{
        EdgeKind, Language, NewEdge, NewNode, NodeId, NodeKind, Phase, Project, ProjectConfig,
        ProjectId, ProjectStatus, Span,
    };
    use std::path::PathBuf;

    fn test_ctx() -> PipelineContext {
        let project = Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: PathBuf::from("/t"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        };
        PipelineContext::new(project)
    }

    fn add_method(ctx: &mut PipelineContext, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(NODE_KIND_METHOD.to_string()),
            name: fqn.into(),
            fqn: Some(fqn.into()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("CfAst".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn add_node(ctx: &mut PipelineContext, kind: &str, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(kind.to_string()),
            name: fqn.into(),
            fqn: Some(fqn.into()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("CfAst".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn calls(ctx: &mut PipelineContext, from: NodeId, to: NodeId) {
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: from,
            to_id: to,
            phase: Phase("Resolve".into()),
            confidence: 0.7,
            properties: serde_json::Value::Null,
        });
    }

    fn has_edge(ctx: &PipelineContext, from: NodeId, to: NodeId, kind: &str) -> bool {
        ctx.ws.edges().iter().any(|e| {
            e.from_id == from && e.to_id == to && e.kind.as_str() == kind
        })
    }

    #[test]
    fn propagates_action_edge_up_call_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        // call chain: top → mid → leaf
        calls(&mut ctx, top, mid);
        calls(&mut ctx, mid, leaf);

        // seed: leaf as the action emitter, posting to the queue.
        ctx.propagation_seeds.push(PropSeed {
            source: leaf,
            target: queue,
            kind: "PublishesTo".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        super::run(&mut ctx);

        // All callers should be recognized as emitters "posting to the queue", no matter how deep.
        assert!(has_edge(&ctx, mid, queue, EdgeKind::PUBLISHES_TO));
        assert!(has_edge(&ctx, top, queue, EdgeKind::PUBLISHES_TO));
        // The seed source itself has no base edge in this test (base edges are built separately at synthesis); propagation only targets callers.
    }

    #[test]
    fn propagation_is_cycle_safe_and_deduped() {
        let mut ctx = test_ctx();
        let a = add_method(&mut ctx, "app\\A::run");
        let b = add_method(&mut ctx, "app\\B::run");
        let c = add_method(&mut ctx, "app\\C::run");
        let q = add_node(&mut ctx, "Queue", "Queue:default");

        // cycle: a ↔ b, and c → a (c reaches a's caller b via two paths).
        calls(&mut ctx, b, a);
        calls(&mut ctx, a, b);
        calls(&mut ctx, c, a);

        ctx.propagation_seeds.push(PropSeed {
            source: a,
            target: q,
            kind: "PublishesTo".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        // No infinite loop; and b appears only once.
        super::run(&mut ctx);
        let b_count = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| e.from_id == b && e.to_id == q && e.kind.as_str() == EdgeKind::PUBLISHES_TO)
            .count();
        assert_eq!(b_count, 1);
        assert!(has_edge(&ctx, c, q, EdgeKind::PUBLISHES_TO));
    }

    fn calls_conf(ctx: &mut PipelineContext, from: NodeId, to: NodeId, conf: f32) {
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: from,
            to_id: to,
            phase: Phase("Resolve".into()),
            confidence: conf,
            properties: serde_json::Value::Null,
        });
    }

    #[test]
    fn static_chain_propagates_without_decay() {
        // Deterministic call chain (Calls edge confidence 1.0): seed 0.85 should pass to the top unchanged, no decay.
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let cache = add_node(&mut ctx, "ExternalSystem", "Cache:default");

        calls_conf(&mut ctx, top, mid, 1.0);
        calls_conf(&mut ctx, mid, leaf, 1.0);

        ctx.propagation_seeds.push(PropSeed {
            source: leaf,
            target: cache,
            kind: "ReadsCache".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        super::run(&mut ctx);
        let edge = ctx
            .ws
            .edges()
            .iter()
            .find(|e| e.from_id == top && e.to_id == cache && e.kind.as_str() == "ReadsCache")
            .unwrap();
        assert!(
            (edge.confidence - 0.85).abs() < 1e-3,
            "确定性链不应衰减，期望 0.85，实际 {}",
            edge.confidence
        );
        // Environment reads are still flagged indirect, so downstream distinguishes direct from indirect.
        assert_eq!(edge.properties.get("indirect"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn uncertain_chain_propagates_with_decay() {
        // Inferred / dynamic call chain (each Calls edge confidence 0.5): two segments product 0.25, seed 0.85 → 0.2125.
        // shows decay accumulates with hop count (each uncertain call decays once).
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let cache = add_node(&mut ctx, "ExternalSystem", "Cache:default");

        calls_conf(&mut ctx, top, mid, 0.5);
        calls_conf(&mut ctx, mid, leaf, 0.5);

        ctx.propagation_seeds.push(PropSeed {
            source: leaf,
            target: cache,
            kind: "ReadsCache".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        super::run(&mut ctx);
        let edge = ctx
            .ws
            .edges()
            .iter()
            .find(|e| e.from_id == top && e.to_id == cache && e.kind.as_str() == "ReadsCache")
            .unwrap();
        assert!(
            (edge.confidence - 0.2125).abs() < 1e-3,
            "不确定性链应衰减（0.5^2），期望 0.2125，实际 {}",
            edge.confidence
        );
    }
}

