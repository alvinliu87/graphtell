//! `GraphQuery` batch / aggregate contract tests.
//!
//! The existing `store_roundtrip.rs` only used `query_nodes` / `get_node` / `edges_of` / `count_nodes`. The batch
//! and aggregate reads (used by the rule engine and the view layer, where per-node round trips measured as a third
//! of the object-view time) had no tests — a broken `nodes_summary` / `edges_outgoing` silently starves the canvas
//! or the rule engine of data. These guard the remaining `GraphQuery` surface.

use std::collections::HashMap;

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{
    AnnotationChannel, EdgeKind, GraphDelta, IdentityKey, Language, MergeStrategy, NewAnnotation,
    NewEdge, NewNode, NodeId, NodeKind, Phase, Span,
};
use gt_domain::port::{GraphQuery, GraphSink, ProjectWriter};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn proj(s: &SqliteStore) -> gt_domain::model::ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: "gq".into(),
        root_path: "/tmp/gq".into(),
        description: None,
        config: None,
    })
    .expect("create")
    .id
}

fn node(pid: gt_domain::model::ProjectId, id: i64, kind: &str) -> NewNode {
    NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: format!("n{id}"),
        fqn: None,
        identity: Some(IdentityKey::fqn(&format!("n{id}"))),
        file_id: None,
        span: Span { start_line: 1, end_line: 2, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase("Synthesize".to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    }
}

#[test]
fn get_nodes_batch_and_summary() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table"), node(pid, 2, "Method")],
        ..Default::default()
    })
    .expect("apply");

    let fetched = s.get_nodes(&[NodeId(1), NodeId(2)]).expect("get_nodes");
    assert_eq!(fetched.len(), 2);
    assert!(fetched.contains_key(&1));
    assert!(fetched.contains_key(&2));

    let summary = s.nodes_summary(pid).expect("nodes_summary");
    assert_eq!(summary.len(), 2);
    assert_eq!(summary.get(&1).unwrap().kind, "Table");
}

#[test]
fn edge_kinds_and_node_kinds_are_reported() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table"), node(pid, 2, "Method")],
        edges: vec![NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(2), NodeId(1))],
        ..Default::default()
    })
    .expect("apply");

    let kinds = s.node_kinds(pid).expect("node_kinds");
    assert!(kinds.contains(&"Table".to_string()));
    assert!(kinds.contains(&"Method".to_string()));

    let ek = s.edge_kinds(pid).expect("edge_kinds");
    assert!(ek.contains(&EdgeKind::CALLS.to_string()), "edge kind missing: {ek:?}");
}

#[test]
fn batch_edge_queries_and_find_edge() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table"), node(pid, 2, "Method")],
        edges: vec![NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(2), NodeId(1))],
        ..Default::default()
    })
    .expect("apply");

    let out: HashMap<i64, Vec<gt_domain::model::Edge>> =
        s.edges_outgoing(&[NodeId(2)]).expect("edges_outgoing");
    assert_eq!(out.get(&2).unwrap().len(), 1);
    assert_eq!(out.get(&2).unwrap()[0].to_id, NodeId(1));

    let inc = s.edges_incoming(&[NodeId(1)]).expect("edges_incoming");
    assert_eq!(inc.get(&1).unwrap()[0].from_id, NodeId(2));

    // Resolve an edge primary key via find_edge.
    let eid = out.get(&2).unwrap()[0].id;
    let found = s.find_edge(eid).expect("find_edge").expect("edge exists");
    assert_eq!(found.to_id, NodeId(1));
}

#[test]
fn annotation_kinds_and_annotations_of_project() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Method")],
        annotations: vec![NewAnnotation {
            node_id: NodeId(1),
            channel: AnnotationChannel::new("FkbMark"),
            kind: "pii".into(),
            subkind: Some("phone".into()),
            confidence: 1.0,
            evidence: serde_json::json!({}),
            phase: Phase("Annotate".to_string()),
            merge: MergeStrategy::default(),
        }],
        ..Default::default()
    })
    .expect("apply");

    let ak = s.annotation_kinds(pid).expect("annotation_kinds");
    assert!(
        ak.contains(&("FkbMark".to_string(), "pii".to_string())),
        "annotation kind missing: {ak:?}"
    );

    let all = s.annotations_of_project(pid).expect("annotations_of_project");
    assert_eq!(all.get(&1).unwrap().len(), 1);
}

#[test]
fn stats_counts_nodes_and_edges() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table"), node(pid, 2, "Method")],
        edges: vec![NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(2), NodeId(1))],
        ..Default::default()
    })
    .expect("apply");

    let stats = s.stats(pid).expect("stats");
    assert_eq!(stats.nodes, 2);
    assert_eq!(stats.edges, 1);
}

#[test]
fn chain_adjacency_returns_consistent_maps() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table"), node(pid, 2, "Method")],
        edges: vec![NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(2), NodeId(1))],
        ..Default::default()
    })
    .expect("apply");

    let (out, inc, _sem) = s.chain_adjacency(pid).expect("chain_adjacency");
    // CALLS is a chain edge; it must show in both directions.
    assert_eq!(out.get(&2).cloned(), Some(vec![1]));
    assert_eq!(inc.get(&1).cloned(), Some(vec![2]));
}
