//! P8 Propagate: propagate the "method → semantic node" action edges built at synthesis up the `Calls` call chain.
//!
//! # Motivation
//!
//! FKB rules only hit at **literal call sites** (e.g. `Queue::push`). If a high-level method ultimately calls that point through several wrapper layers,
//! only the innermost method gets connected to the semantic node; all outer callers are lost — exactly the root cause of framework wrappers like
//! `sample_project\utils\Queue::push → QueueThink::push` being swallowed.
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
        // `CallsHttp` is a *front-end function → external contract* edge, not an "action the method performs on
        // a resource" (the propagation's purpose: swallow-proofing framework wrappers that ultimately reach a
        // Queue / DB / Config the FKB recognises). A transitive caller does not itself *issue* the HTTP request —
        // it only calls the function that does — so replicating `caller → HttpContract` up the call chain is wrong
        // and double-counts the endpoint. Skip it; the direct issuer keeps its edge.
        // See `crates/gt-app/tests/frontend_folded_view.rs`.
        if s.kind == EdgeKind::CALLS_HTTP {
            continue;
        }
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
        "P8 propagation done: {} sources, {} propagation edges ({} root causes in total)",
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
    let mut best: HashMap<i64, f32> = HashMap::new();
    let mut stack = vec![(src, 1.0f32)];
    best.insert(src, 1.0);
    while let Some((cur, cur_conf)) = stack.pop() {
        let Some(next) = callers.get(&cur) else {
            continue;
        };
        for (c, econf) in next {
            let path_conf = cur_conf * *econf;
            // Skip if already visited and the current path isn't better; otherwise record the best product and keep going up.
            if let Some(prev) = best.get(c) {
                if *prev >= path_conf {
                    continue;
                }
            }
            best.insert(*c, path_conf);
            // Only continue traversing up when the caller is a method / function; other nodes (class / file) don't enter the propagation chain.
            let is_site = ctx
                .ws
                .node(NodeId(*c))
                .map(|n| is_action_site(n.kind.as_str()))
                .unwrap_or(false);
            if is_site {
                stack.push((*c, path_conf));
            }
        }
    }
    // Materialize from `best` so each caller appears exactly once with its highest accumulated confidence:
    // a caller reachable via multiple call paths keeps the strongest path, not the first one pushed during traversal.
    let mut out = Vec::new();
    for (c, conf) in best {
        if c == src {
            continue;
        }
        let is_site = ctx
            .ws
            .node(NodeId(c))
            .map(|n| is_action_site(n.kind.as_str()))
            .unwrap_or(false);
        if is_site {
            out.push((c, conf));
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
/// Note: numeric decay does not use a fixed coefficient — a deterministic call chain (edge confidence 1.0) stays un-decayed on product,
/// only inferred / dynamic calls (edge confidence <1.0) decay naturally. Here we only set the `indirect` flag.
const DECAYED_KINDS: &[&str] = &[EdgeKind::READS_CONFIG, EdgeKind::READS_CACHE];

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
            "a deterministic chain must not decay, expected 0.85, got {}",
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
            "an uncertain chain must decay (0.5^2), expected 0.2125, got {}",
            edge.confidence
        );
    }

    /// Seed helper: `(source --kind--> target)` staged for propagation.
    fn seed(ctx: &mut PipelineContext, source: NodeId, target: NodeId, kind: &str, confidence: f32) {
        ctx.propagation_seeds.push(PropSeed {
            source,
            target,
            kind: kind.to_string(),
            confidence,
            sub: None,
            phase: Phase("Synthesize".into()),
        });
    }

    // ------------------------------------------------------- the `MapsTo` suppression rule
    //
    // A caller that really reads / writes the table must not also carry "merely maps to it": once a
    // propagated `ReadsDb` / `WritesDb` exists for the same (from, to), the propagated `MapsTo` is dropped.
    // Downstream (views, rules) reads the difference as "the action really happens here" vs "just a mapping".

    #[test]
    fn maps_to_is_dropped_where_the_same_pair_really_reads_the_table() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let table = add_node(&mut ctx, "Table", "user");

        calls(&mut ctx, mid, leaf);
        seed(&mut ctx, leaf, table, "MapsTo", 1.0);
        seed(&mut ctx, leaf, table, "ReadsDb", 1.0);

        super::run(&mut ctx);

        assert!(has_edge(&ctx, mid, table, EdgeKind::READS_DB), "an edge that really reads a table must be kept");
        assert!(
            !has_edge(&ctx, mid, table, EdgeKind::MAPS_TO),
            "when the same (from,to) really reads the table, a propagated MapsTo must be suppressed"
        );
    }

    /// Control for the one above: the suppression must not degenerate into "never propagate MapsTo".
    #[test]
    fn maps_to_survives_when_the_pair_has_no_read_or_write() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let table = add_node(&mut ctx, "Table", "user");

        calls(&mut ctx, mid, leaf);
        seed(&mut ctx, leaf, table, "MapsTo", 1.0);

        super::run(&mut ctx);

        assert!(
            has_edge(&ctx, mid, table, EdgeKind::MAPS_TO),
            "with no read/write, MapsTo must still be propagated upwards"
        );
    }

    // ------------------------------------------------------- who may join the chain

    /// Only methods / functions are action emitters: a class node sitting in the call chain must not end up
    /// carrying a semantic edge — hanging one on a class is exactly what this phase avoids.
    #[test]
    fn only_methods_and_functions_join_the_propagation_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let cls = add_node(&mut ctx, "Class", "app\\Wrapper");
        let func = add_node(&mut ctx, "Function", "app\\helper");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, cls, leaf);
        calls(&mut ctx, func, leaf);
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        assert!(
            has_edge(&ctx, func, queue, EdgeKind::PUBLISHES_TO),
            "Function is an action node, so it must take part in propagation"
        );
        assert!(
            !has_edge(&ctx, cls, queue, EdgeKind::PUBLISHES_TO),
            "Class is not an action node, so no propagated semantic edge may be attached to it"
        );
    }

    // ------------------------------------------------------- one edge, every root cause

    /// Two different emitters reaching the same caller collapse into **one** edge that records both root
    /// causes, so downstream can still tell why the edge is there.
    #[test]
    fn one_propagated_edge_carries_every_root_cause() {
        let mut ctx = test_ctx();
        let leaf_a = add_method(&mut ctx, "app\\A::run");
        let leaf_b = add_method(&mut ctx, "app\\B::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, top, leaf_a);
        calls(&mut ctx, top, leaf_b);
        seed(&mut ctx, leaf_a, queue, "PublishesTo", 0.85);
        seed(&mut ctx, leaf_b, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        let edges: Vec<_> = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| {
                e.from_id == top && e.to_id == queue && e.kind.as_str() == EdgeKind::PUBLISHES_TO
            })
            .collect();
        assert_eq!(edges.len(), 1, "there must be only one edge per (from,to,kind): {edges:?}");

        let props = &edges[0].properties;
        assert_eq!(props.get("via").and_then(|v| v.as_str()), Some("propagate"));
        let sources = props
            .get("seed_sources")
            .and_then(|v| v.as_array())
            .expect("seed_sources must be an array");
        assert_eq!(sources.len(), 2, "both root causes must be recorded: {sources:?}");
    }

    // ------------------------------------------------------- the `indirect` flag

    /// Both environment-read kinds are flagged `indirect` (`ReadsCache` is covered by the decay tests
    /// above, this pins `ReadsConfig` too), while an action that really happens is not.
    #[test]
    fn both_environment_read_kinds_are_flagged_indirect() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let config = add_node(&mut ctx, "ConfigKey", "app.debug");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls_conf(&mut ctx, mid, leaf, 1.0);
        seed(&mut ctx, leaf, config, "ReadsConfig", 0.9);
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.9);

        super::run(&mut ctx);

        let flag = |to: NodeId, kind: &str| {
            ctx.ws
                .edges()
                .iter()
                .find(|e| e.from_id == mid && e.to_id == to && e.kind.as_str() == kind)
                .and_then(|e| e.properties.get("indirect").cloned())
        };
        assert_eq!(
            flag(config, "ReadsConfig"),
            Some(serde_json::json!(true)),
            "ReadsConfig must be marked indirect too"
        );
        assert_eq!(
            flag(queue, "PublishesTo"),
            None,
            "an action that really happens must not be marked indirect"
        );
    }

    // ------------------------------------------------------- pure leaves (lock the contract directly)

    /// `is_action_site` is the gate that decides who may join the chain; pin it so a future change can't
    /// silently start hanging semantic edges on classes / tables.
    #[test]
    fn is_action_site_accepts_only_methods_and_functions() {
        assert!(is_action_site("Method"));
        assert!(is_action_site("Function"));
        assert!(!is_action_site("Class"));
        assert!(!is_action_site("Table"));
        assert!(!is_action_site("Queue"));
        assert!(!is_action_site("File"));
        assert!(!is_action_site(""));
    }

    /// `propagated` is the single place that decides (a) confidence = seed × path product and (b) which kinds
    /// become `indirect`. A regression here would change every propagated edge's weight or indirect flag.
    #[test]
    fn propagated_computes_confidence_and_indirect_flag() {
        // confidence = base × path_conf
        assert_eq!(propagated("PublishesTo", 0.9, 0.5), (0.45, false));
        assert_eq!(propagated("ReadsDb", 0.8, 1.0), (0.8, false));
        // environment reads are flagged indirect
        assert_eq!(propagated("ReadsCache", 0.9, 1.0), (0.9, true));
        assert_eq!(propagated("ReadsConfig", 0.9, 0.5), (0.45, true));
        // actions that really happen are never indirect
        assert_eq!(propagated("WritesDb", 1.0, 1.0).1, false);
        assert_eq!(propagated("PublishesTo", 1.0, 1.0).1, false);
    }

    // ------------------------------------------------------- the multi-path (highest-confidence) rule

    /// A caller reached from the seed via two call paths must keep the strongest (highest confidence product)
    /// path, not the first one discovered during traversal — this is the contract documented on `transitive_callers`.
    #[test]
    fn transitive_callers_keeps_highest_confidence_path() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid1 = add_method(&mut ctx, "app\\Mid1::run");
        let mid2 = add_method(&mut ctx, "app\\Mid2::run");
        let top = add_method(&mut ctx, "app\\Top::run");

        // top reaches leaf via two paths: leaf→mid1→top (1.0·1.0) and leaf→mid2→top (0.5·0.5).
        let callers: std::collections::HashMap<i64, Vec<(i64, f32)>> =
            std::collections::HashMap::from([
                (leaf.get(), vec![(mid1.get(), 1.0), (mid2.get(), 0.5)]),
                (mid1.get(), vec![(top.get(), 1.0)]),
                (mid2.get(), vec![(top.get(), 0.5)]),
            ]);

        let reach = transitive_callers(leaf.get(), &callers, &ctx);
        let top_conf = reach
            .iter()
            .find(|(c, _)| *c == top.get())
            .map(|(_, conf)| *conf);
        assert_eq!(top_conf, Some(1.0), "top must keep the strongest path 1.0, not the weaker 0.25");
        let count = reach.iter().filter(|(c, _)| *c == top.get()).count();
        assert_eq!(count, 1, "the same caller must not appear twice");
    }

    // ------------------------------------------------------- early-return / no-op guards

    /// With no seeds, `run` must do nothing (no panic, no edges) — otherwise a stale workspace could get
    /// bogus propagation edges on a re-run.
    #[test]
    fn run_is_noop_when_there_are_no_seeds() {
        let mut ctx = test_ctx();
        let a = add_method(&mut ctx, "app\\A::run");
        let b = add_method(&mut ctx, "app\\B::run");
        calls(&mut ctx, a, b);
        super::run(&mut ctx);
        let propagated = ctx
            .ws
            .edges()
            .iter()
            .any(|e| e.properties.get("via") == Some(&serde_json::json!("propagate")));
        assert!(!propagated, "with no seed, no propagation edge may be added (existing Calls edges stay)");
    }

    /// A seed exists, but nothing calls its source: propagation only targets *callers*, so the source itself
    /// must never receive an edge, and nothing else appears.
    #[test]
    fn run_does_not_emit_edge_for_a_seed_with_no_callers() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);
        super::run(&mut ctx);
        assert!(
            !has_edge(&ctx, leaf, queue, EdgeKind::PUBLISHES_TO),
            "the seed's own source must not get an edge (propagation goes to callers only)"
        );
        assert_eq!(ctx.ws.edges().len(), 0, "with no caller, no edge may be added");
    }

    // ------------------------------------------------------- the `MapsTo` suppression also covers WritesDb

    /// The suppression rule (line: `READS_DB || WRITES_DB`) must drop a propagated `MapsTo` when the same
    /// (from, to) really *writes* the table too — not just when it reads. Without this, a writer would also
    /// carry a misleading "merely maps to it" edge.
    #[test]
    fn writes_db_also_suppresses_maps_to_for_the_same_pair() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let table = add_node(&mut ctx, "Table", "order");

        calls(&mut ctx, mid, leaf);
        seed(&mut ctx, leaf, table, "MapsTo", 1.0);
        seed(&mut ctx, leaf, table, "WritesDb", 1.0);

        super::run(&mut ctx);

        assert!(has_edge(&ctx, mid, table, EdgeKind::WRITES_DB), "an edge that really writes a table must be kept");
        assert!(
            !has_edge(&ctx, mid, table, EdgeKind::MAPS_TO),
            "when the same (from,to) really writes the table, a propagated MapsTo must be suppressed"
        );
    }

    // ------------------------------------------------------- root-cause bookkeeping

    /// When several emitters reach the same caller, the single collapsed edge records every root cause, and
    /// `seed_source` is the minimum root-cause id (the `seed_sources` array is ascending) so downstream can
    /// tell "why this edge" deterministically.
    #[test]
    fn seed_source_records_minimum_root_cause_id() {
        let mut ctx = test_ctx();
        let leaf_a = add_method(&mut ctx, "app\\A::run");
        let leaf_b = add_method(&mut ctx, "app\\B::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, top, leaf_a);
        calls(&mut ctx, top, leaf_b);
        // `leaf_a` is added first, so it gets the smaller node id and is the minimum root cause.
        seed(&mut ctx, leaf_a, queue, "PublishesTo", 0.85);
        seed(&mut ctx, leaf_b, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        let edge = ctx
            .ws
            .edges()
            .iter()
            .find(|e| {
                e.from_id == top
                    && e.to_id == queue
                    && e.kind.as_str() == EdgeKind::PUBLISHES_TO
            })
            .expect("there must be one propagation edge");
        let sources = edge
            .properties
            .get("seed_sources")
            .and_then(|v| v.as_array())
            .expect("seed_sources must be an array");
        assert_eq!(sources.len(), 2, "both root causes must be recorded");
        assert!(sources[0].as_i64() < sources[1].as_i64(), "seed_sources must be sorted ascending");
        assert_eq!(
            edge.properties.get("seed_source").and_then(|v| v.as_i64()),
            sources[0].as_i64(),
            "seed_source must be the smallest root-cause id"
        );
    }

    // ------------------------------------------------------- only `Calls` edges form the chain

    /// The reverse caller index is built from `Calls` edges only: a pre-existing **semantic** edge
    /// (here `MapsTo`) pointing at the seed's source must not turn its holder into a caller.
    #[test]
    fn non_call_edges_do_not_form_the_propagation_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        // `mid` is linked to `leaf` by a semantic edge, not by a call.
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind(EdgeKind::MAPS_TO.to_string()),
            from_id: mid,
            to_id: leaf,
            phase: Phase("Synthesize".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        assert!(
            !has_edge(&ctx, mid, queue, EdgeKind::PUBLISHES_TO),
            "only Calls edges form a call chain; a semantic edge must not make mid a caller"
        );
    }

    // ------------------------------------------------------- seeds are consumed (re-run safety)

    /// `run` **takes** the seeds (`std::mem::take`), so a second run must not duplicate propagation.
    /// Without this, re-running the phase (or any later replay) would pile up duplicate semantic edges.
    #[test]
    fn run_consumes_seeds_so_a_second_run_adds_nothing() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, mid, leaf);
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);
        let after_first = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| e.kind.as_str() == EdgeKind::PUBLISHES_TO)
            .count();

        super::run(&mut ctx);
        let after_second = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| e.kind.as_str() == EdgeKind::PUBLISHES_TO)
            .count();

        assert_eq!(after_first, 1, "the first run must produce one propagation edge");
        assert_eq!(after_second, after_first, "the seed has been consumed, so a second run must not propagate again");
        assert!(ctx.propagation_seeds.is_empty(), "the seed must be consumed, not kept");
    }

    // ------------------------------------------------------- the chain stops at a non-action node

    /// A non-action node sitting **in the middle** of the chain (`top → Class → leaf`) must stop the
    /// walk: `top` is reachable only *through* the class, so it must not inherit the semantic edge.
    /// The existing test only covers a class as a *direct* caller, which is filtered at output time;
    /// this one pins the traversal-side guard (`if is_site { stack.push(..) }`).
    #[test]
    fn a_non_action_node_in_the_middle_breaks_the_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let cls = add_node(&mut ctx, "Class", "app\\Wrapper");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        // top → cls → leaf
        calls(&mut ctx, cls, leaf);
        calls(&mut ctx, top, cls);
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        assert!(
            !has_edge(&ctx, top, queue, EdgeKind::PUBLISHES_TO),
            "a non-action node cuts the call chain, so top must not get a propagation edge"
        );
        assert!(
            !has_edge(&ctx, cls, queue, EdgeKind::PUBLISHES_TO),
            "Class itself must not get a semantic edge either"
        );
    }

    /// Control for the one above: a `Function` in the same middle position **is** an action site, so the
    /// chain must continue past it — the guard must not degenerate into "stop at anything".
    #[test]
    fn a_function_in_the_middle_continues_the_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let func = add_node(&mut ctx, "Function", "app\\helper");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        // top → func → leaf
        calls(&mut ctx, func, leaf);
        calls(&mut ctx, top, func);
        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);

        super::run(&mut ctx);

        assert!(
            has_edge(&ctx, func, queue, EdgeKind::PUBLISHES_TO),
            "Function is an action node, so it must receive the propagation edge"
        );
        assert!(
            has_edge(&ctx, top, queue, EdgeKind::PUBLISHES_TO),
            "Function does not cut the chain, so it must keep walking up to top"
        );
    }

    // ------------------------------------------------------- collapsed-edge confidence is deterministic

    /// Several seeds of **differing** confidence collapsing onto the same `(from, to, kind)` produce one
    /// edge whose confidence is deterministic: sources are iterated in ascending id order and `edge_meta`
    /// uses `or_insert`, so the lowest-id source supplies it — never a hash-order-dependent value.
    #[test]
    fn collapsed_edge_confidence_is_deterministic_across_seeds() {
        let mut ctx = test_ctx();
        let leaf_a = add_method(&mut ctx, "app\\A::run"); // added first -> smaller node id
        let leaf_b = add_method(&mut ctx, "app\\B::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, top, leaf_a);
        calls(&mut ctx, top, leaf_b);
        seed(&mut ctx, leaf_a, queue, "PublishesTo", 0.9);
        seed(&mut ctx, leaf_b, queue, "PublishesTo", 0.4);

        super::run(&mut ctx);

        let edges: Vec<_> = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| {
                e.from_id == top && e.to_id == queue && e.kind.as_str() == EdgeKind::PUBLISHES_TO
            })
            .collect();
        assert_eq!(edges.len(), 1, "seeds with different confidences must still merge into one edge");
        // `calls` helper gives each Calls edge 0.7; the lowest-id source (`leaf_a`, 0.9) supplies the seed.
        let expected = 0.9f32 * 0.7f32;
        assert!(
            (edges[0].confidence - expected).abs() < 1e-3,
            "confidence must be taken deterministically from the seed with the smallest id, expected {expected}, got {}",
            edges[0].confidence
        );
    }

    // ------------------------------------------------------- defensive / negative coverage
    //
    // The happy-path and structural tests above pin what *should* happen. These pin the branches that
    // must degrade gracefully or stay silent — a regression in any of them is silent (wrong edges, or a
    // panic on a stale / partially-built workspace).

    /// A `Calls` edge whose *source* node no longer exists in the graph must be skipped when building the
    /// caller index (the `ctx.ws.node(e.from_id)` guard), never panic. Real callers must still propagate.
    #[test]
    fn dangling_calls_edge_with_missing_source_node_is_ignored() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, top, mid);
        calls(&mut ctx, mid, leaf);
        // Dangling edge: caller id 9999 does not exist in the workspace.
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: NodeId(9999),
            to_id: leaf,
            phase: Phase("Resolve".into()),
            confidence: 0.7,
            properties: serde_json::Value::Null,
        });

        seed(&mut ctx, leaf, queue, "PublishesTo", 0.85);
        super::run(&mut ctx);

        // Real chain still propagates; the dangling edge contributed nothing and no panic occurred.
        assert!(has_edge(&ctx, mid, queue, EdgeKind::PUBLISHES_TO));
        assert!(has_edge(&ctx, top, queue, EdgeKind::PUBLISHES_TO));
    }

    /// The seed's source self-calls (`A Calls A`): the source must never receive a propagated edge to
    /// itself. `transitive_callers` materializes every caller *except* the source (`c == src` skip) and
    /// the best-path guard refuses to revisit the source, so the self-edge is dropped.
    #[test]
    fn seed_source_calling_itself_does_not_produce_a_self_edge() {
        let mut ctx = test_ctx();
        let a = add_method(&mut ctx, "app\\A::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        calls(&mut ctx, a, a); // self-referential call
        seed(&mut ctx, a, queue, "PublishesTo", 0.85);
        super::run(&mut ctx);

        assert!(
            !has_edge(&ctx, a, queue, EdgeKind::PUBLISHES_TO),
            "the seed source must not emit a propagated edge to itself even when it self-calls"
        );
    }

    /// Only the environment-read kinds (`ReadsConfig` / `ReadsCache`) are flagged `indirect`. A real action
    /// (`ReadsDb`) must NOT be flagged, otherwise downstream would down-weight an action that genuinely
    /// happened. This pins the *boundary* of `DECAYED_KINDS` — the sibling test
    /// `both_environment_read_kinds_are_flagged_indirect` asserts the positive side; this asserts the
    /// negative, so adding a real action to `DECAYED_KINDS` would fail here.
    #[test]
    fn action_kind_is_not_flagged_indirect() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let table = add_node(&mut ctx, "Table", "user");

        calls(&mut ctx, top, leaf);
        seed(&mut ctx, leaf, table, "ReadsDb", 0.85);
        super::run(&mut ctx);

        let edge = ctx
            .ws
            .edges()
            .iter()
            .find(|e| {
                e.from_id == top
                    && e.to_id == table
                    && e.kind.as_str() == EdgeKind::READS_DB
            })
            .expect("a real action edge must be propagated");
        assert!(
            edge.properties.get("indirect").is_none(),
            "a real action (ReadsDb) must not be flagged indirect: {:?}",
            edge.properties
        );
    }
}

