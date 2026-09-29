//! SQLite 适配器的持久化往返契约测试：建工程 → 落图（节点 / 边）→ 查询回读。
//!
//! 这是此前**完全零测试**的高风险 crate（全部持久化逻辑都在 `store.rs`）。这里只验证
//! 「写进去能读出来」的核心契约，不追求覆盖每个列 / 分支 —— 但足以在迁移脚本或
//! 读写逻辑被改坏时立刻报警。

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{
    EdgeKind, GraphDelta, IdentityKey, Language, NewEdge, NewNode, NewProject, NodeId, NodeKind, Phase,
};
use gt_domain::port::{EdgeDirection, GraphQuery, GraphSink, ProjectReader, ProjectWriter};

fn make_store() -> SqliteStore {
    SqliteStore::in_memory().expect("内存库应可打开")
}

fn seed_project(store: &SqliteStore) -> gt_domain::model::ProjectId {
    let p = store
        .create_project(NewProject {
            name: "test-proj".into(),
            root_path: "/tmp/test-proj".into(),
            description: None,
            config: None,
        })
        .expect("建工程应成功");
    p.id
}

#[test]
fn project_is_persisted_and_readable() {
    let store = make_store();
    let pid = seed_project(&store);
    let got = store.get_project(pid).expect("get_project 应成功");
    let p = got.expect("工程应存在");
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
        .expect("落图应成功");

    // 按 kind 查：只应命中 Table
    let tables = store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: Some(NodeKind::new("Table")),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query_nodes 应成功");
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "user");

    // 按 name 模糊查
    let by_name = store
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: None,
            name_contains: Some("createOrder".into()),
            limit: None,
            offset: None,
        })
        .expect("query_nodes 应成功");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "createOrder");

    // 精确取节点
    let got = store.get_node(NodeId(101)).expect("get_node 应成功");
    assert!(got.is_some());
    assert_eq!(got.unwrap().name, "user");

    // 边往返：Method(105) → Table(101)
    let out = store
        .edges_of(NodeId(105), EdgeDirection::Outgoing)
        .expect("edges_of 应成功");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].to_id, NodeId(101));
}
