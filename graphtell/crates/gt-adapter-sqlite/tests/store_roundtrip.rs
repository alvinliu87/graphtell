//! Persistence round-trip contract tests for the SQLite adapter: create project -> persist graph (nodes / edges) -> query back.
//!
//! These tests only verify the core contract "what is written can be read back", not every column / branch — but that is enough to
//! raise the alarm immediately when a migration script or the read/write logic is broken.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{
    EdgeKind, GraphDelta, IdentityKey, Language, NewEdge, NewNode, NewProject, NodeId, NodeKind, Phase,
};
use gt_domain::port::{EdgeDirection, GraphQuery, GraphSink, ProjectReader, ProjectWriter};

fn make_store() -> SqliteStore {
    SqliteStore::in_memory().expect("the in-memory database should open")
}

fn seed_project(store: &SqliteStore) -> gt_domain::model::ProjectId {
    let p = store
        .create_project(NewProject {
            name: "test-proj".into(),
            root_path: "/tmp/test-proj".into(),
            description: None,
            config: None,
        })
        .expect("creating the project should succeed");
    p.id
}

#[test]
fn project_is_persisted_and_readable() {
    let store = make_store();
    let pid = seed_project(&store);
    let got = store.get_project(pid).expect("get_project should succeed");
    let p = got.expect("the project should exist");
    assert_eq!(p.id, pid);
    assert_eq!(p.name, "test-proj");
    assert_eq!(p.root_path.to_string_lossy(), "/tmp/test-proj");
}

#[test]
fn node_and_edge_roundtrip() {
    let store = make_store();
    let pid = seed_project(&store);

    let table = NewNode {
        id: Some(NodeId(101)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new("Table"),
        name: "user".into(),
        fqn: None,
        identity: Some(IdentityKey::fqn("user")),
        file_id: None,
        span: gt_domain::model::Span { start_line: 10, end_line: 30, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase("Synthesize".to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };
    let method = NewNode {
        id: Some(NodeId(105)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new("Method"),
        name: "createOrder".into(),
        fqn: None,
        identity: Some(IdentityKey::fqn("createOrder")),
        file_id: None,
        span: gt_domain::model::Span { start_line: 40, end_line: 60, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase("Synthesize".to_string()),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    };

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![table, method],
            edges: vec![NewEdge::new(pid, EdgeKind::new(EdgeKind::CALLS), NodeId(105), NodeId(101))],
            ..Default::default()
        })
        .expect("persisting the graph should succeed");

    // Query by kind: only Table should match
    let tables = store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: Some(NodeKind::new("Table")),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query_nodes should succeed");
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "user");

    // Fuzzy query by name
    let by_name = store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: None,
            name_contains: Some("createOrder".into()),
            limit: None,
            offset: None,
        })
        .expect("query_nodes should succeed");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "createOrder");

    // Fetch a node exactly
    let got = store.get_node(NodeId(101)).expect("get_node should succeed");
    assert!(got.is_some());
    assert_eq!(got.unwrap().name, "user");

    // Edge round trip: Method(105) -> Table(101)
    let out = store
        .edges_of(NodeId(105), EdgeDirection::Outgoing)
        .expect("edges_of should succeed");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].to_id, NodeId(101));
}

/// `count_nodes` asks "does any of these parties have evidence" — reading the derived `sides` set, with the
/// scalar kept as a fallback so rows written before `sides` existed still count (no rebuild). Comparing one
/// scalar (`$.side = ?`) would make any node several parties had written invisible to a side-filtered
/// perspective: a contract bridge reads `side = bridge` and matches **neither** `side = frontend` nor
/// `side = backend`, so "the backend participates" would quietly drop every contract the front end touched.
#[test]
fn count_nodes_accepts_any_party_with_evidence() {
    let store = make_store();
    let pid = seed_project(&store);

    let node = |id: i64, kind: &str, name: &str, props: serde_json::Value| NewNode {
        id: Some(NodeId(id)),
        project_id: pid,
        sub_project_id: None,
        kind: NodeKind::new(kind),
        name: name.into(),
        fqn: None,
        identity: None,
        file_id: None,
        span: gt_domain::model::Span { start_line: 1, end_line: 2, start_byte: 0, end_byte: 0 },
        language: Language::new("php"),
        phase: Phase("Synthesize".to_string()),
        confidence: 1.0,
        properties: props,
    };
    let nodes = vec![
        // Written by both sides: the derived label is `bridge`, the parties live in the set.
        node(201, "HttpContract", "bridged", serde_json::json!({ "side": "bridge", "sides": ["backend", "frontend"] })),
        // One party each.
        node(202, "HttpContract", "backend-only", serde_json::json!({ "side": "backend", "sides": ["backend"] })),
        node(203, "HttpContract", "frontend-only", serde_json::json!({ "side": "frontend", "sides": ["frontend"] })),
        // No side evidence at all (e.g. a knowledge base that never declared `side`).
        node(204, "HttpContract", "anonymous", serde_json::json!({})),
        // A legacy row: scalar only, no `sides` set.
        node(205, "Cache", "token", serde_json::json!({ "side": "backend" })),
    ];
    store
        .apply(&GraphDelta { project_id: Some(pid), nodes, ..Default::default() })
        .expect("persisting the graph should succeed");

    let count = |kind: Option<&str>, sides: &[&str]| -> u64 {
        let sides: Vec<String> = sides.iter().map(|s| s.to_string()).collect();
        match kind {
            Some(k) => store.count_nodes(pid, Some(&NodeKind::new(k)), &sides),
            None => store.count_nodes(pid, None, &sides),
        }
        .expect("count_nodes should succeed")
    };

    let contracts = Some("HttpContract");
    assert_eq!(count(contracts, &["backend"]), 2, "bridge + backend-only");
    assert_eq!(count(contracts, &["frontend"]), 2, "bridge + frontend-only");
    assert_eq!(count(contracts, &["backend", "frontend"]), 3, "三方各行按需，无重复计数");
    assert_eq!(count(contracts, &["external"]), 0, "没有证据的一方不该命中");
    assert_eq!(count(contracts, &[]), 4, "空集合 = 不过滤");
    // Legacy rows (scalar only) keep working, which is what makes the filter safe without a rebuild.
    assert_eq!(count(Some("Cache"), &["backend"]), 1);
    assert_eq!(count(Some("Cache"), &["frontend"]), 0);
}
