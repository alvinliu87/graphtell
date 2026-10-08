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

/// `count_nodes` asks "is this node owned by the requested party": a node is owned by exactly one sub-project,
/// so `side` is one scalar and the comparison is equality (`$.side = ?`). It used to be set semantics over a
/// derived `sides` set, because a contract bridge merged several parties onto one node; that node shape no
/// longer exists, so there is nothing for a set to express.
#[test]
fn count_nodes_filters_on_the_owning_party() {
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
        // One party each: the same endpoint declared by two sub-projects is two nodes, not one shared one.
        node(202, "HttpContract", "backend-only", serde_json::json!({ "side": "backend" })),
        node(203, "HttpContract", "frontend-only", serde_json::json!({ "side": "frontend" })),
        // No side evidence at all (e.g. a knowledge base that never declared `side`).
        node(204, "HttpContract", "anonymous", serde_json::json!({})),
        node(205, "Cache", "token", serde_json::json!({ "side": "backend" })),
    ];
    store
        .apply(&GraphDelta { project_id: Some(pid), nodes, ..Default::default() })
        .expect("persisting the graph should succeed");

    let count = |kind: Option<&str>, side: Option<&str>| -> u64 {
        match kind {
            Some(k) => store.count_nodes(pid, Some(&NodeKind::new(k)), side),
            None => store.count_nodes(pid, None, side),
        }
        .expect("count_nodes should succeed")
    };

    let contracts = Some("HttpContract");
    assert_eq!(count(contracts, Some("backend")), 1, "only the backend's own contract");
    assert_eq!(count(contracts, Some("frontend")), 1, "only the frontend's own contract");
    assert_eq!(count(contracts, Some("external")), 0, "a side with no evidence must not match");
    assert_eq!(count(contracts, None), 3, "no side = no filtering");
    assert_eq!(count(Some("Cache"), Some("backend")), 1);
    assert_eq!(count(Some("Cache"), Some("frontend")), 0);
}

/// The core "what is written can be read back" contract for the *semantic* node columns — `fqn`, `identity`,
/// `phase`, `language`, `confidence`, `properties`. `node_and_edge_roundtrip` writes `identity` but never asserts
/// it returns, and leaves the rest at their defaults; a migration that drops one of these columns (or a broken read
/// mapping) would pass every other assertion here. This pins each column end-to-end.
#[test]
fn node_semantic_fields_roundtrip() {
    let store = make_store();
    let pid = seed_project(&store);

    store
        .apply(&GraphDelta {
            project_id: Some(pid),
            nodes: vec![NewNode {
                id: Some(NodeId(301)),
                project_id: pid,
                sub_project_id: None,
                kind: NodeKind::new("Method"),
                name: "placeOrder".into(),
                fqn: Some("App\\Service\\Cart::placeOrder".into()),
                identity: Some(IdentityKey::fqn("App\\Service\\Cart::placeOrder")),
                file_id: None,
                span: gt_domain::model::Span { start_line: 10, end_line: 20, start_byte: 0, end_byte: 0 },
                language: Language::new("php"),
                phase: Phase("Annotate".to_string()),
                confidence: 0.83,
                properties: serde_json::json!({ "hot": true, "owner": "checkout" }),
            }],
            ..Default::default()
        })
        .expect("persisting the node should succeed");

    let got = store.get_node(NodeId(301)).expect("get_node should succeed").expect("node exists");
    assert_eq!(got.fqn.as_deref(), Some("App\\Service\\Cart::placeOrder"), "fqn must round-trip");
    assert_eq!(
        got.identity,
        Some(IdentityKey::fqn("App\\Service\\Cart::placeOrder")),
        "identity (used for node de-duplication / merge) must round-trip"
    );
    assert_eq!(got.phase, Phase("Annotate".to_string()), "phase must round-trip");
    assert_eq!(got.language, Language::new("php"), "language must round-trip");
    assert!(
        (got.confidence - 0.83).abs() < 1e-6,
        "confidence must round-trip, got {}",
        got.confidence
    );
    assert_eq!(
        got.properties,
        serde_json::json!({ "hot": true, "owner": "checkout" }),
        "arbitrary JSON properties must round-trip verbatim"
    );
}
