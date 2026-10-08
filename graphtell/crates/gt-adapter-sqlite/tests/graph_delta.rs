//! `GraphDelta` variant contract tests beyond the basic node/edge round-trip.
//!
//! The existing `store_roundtrip.rs` only exercised `nodes` / `edges`. These guard the subtler, failure-prone
//! branches of `apply`:
//! * `reset_project` — re-running the pipeline must clear the previous graph (otherwise ghosts accumulate);
//! * `kind_patches` — middleware promotion must UPDATE the single node's kind, never INSERT a second node
//!   (a regression duplicates a node, splitting fan-in and offering two jump-to-source locations);
//! * `symbols` — the schema/table symbol table that P5/P6/P7 read (the source of `Column` facts);
//! * `annotations` — semantic tags attached to nodes;
//! * `property_patches` — late-phase property back-fill.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{
    AnnotationChannel, Diagnostic, GraphDelta, IdentityKey, Language, MergeStrategy, NewAnnotation,
    NewNode, NodeId, NodeKind, Phase, Severity, SourceLocation, Span, SymbolEntry,
};
use gt_domain::port::{DiagnosticSink, GraphQuery, GraphSink, ProjectWriter, SymbolTableReader};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("open")
}

fn proj(s: &SqliteStore) -> gt_domain::model::ProjectId {
    s.create_project(gt_domain::model::NewProject {
        name: "delta".into(),
        root_path: "/tmp/delta".into(),
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
fn reset_project_clears_previous_graph() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 1, "Table")],
        ..Default::default()
    })
    .expect("apply");

    // Re-run with reset and no nodes.
    s.apply(&GraphDelta {
        project_id: Some(pid),
        reset_project: true,
        ..Default::default()
    })
    .expect("apply reset");
    let remaining = s
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: None,
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query");
    assert!(remaining.is_empty(), "reset must wipe the graph");
}

#[test]
fn kind_patch_promotes_without_duplicating() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 301, "Class")],
        ..Default::default()
    })
    .expect("apply");

    s.apply(&GraphDelta {
        project_id: Some(pid),
        kind_patches: vec![(NodeId(301), NodeKind::new("Middleware"))],
        ..Default::default()
    })
    .expect("apply kind patch");

    let promoted = s
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: Some(NodeKind::new("Middleware")),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query");
    assert_eq!(promoted.len(), 1, "exactly one promoted node");

    let legacy = s
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: Some(NodeKind::new("Class")),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query");
    assert!(legacy.is_empty(), "old kind must be gone (no duplicate)");

    let total = s
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: None,
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query");
    assert_eq!(total.len(), 1, "still a single node");
}

#[test]
fn property_patch_merges_into_node() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 302, "Method")],
        ..Default::default()
    })
    .expect("apply");

    s.apply(&GraphDelta {
        project_id: Some(pid),
        property_patches: vec![(NodeId(302), serde_json::json!({ "hot": true }))],
        ..Default::default()
    })
    .expect("apply patch");
    let got = s.get_node(NodeId(302)).expect("get").expect("exists");
    assert_eq!(got.properties, serde_json::json!({ "hot": true }));
}

#[test]
fn symbols_roundtrip_through_symbol_table() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        symbols: vec![
            SymbolEntry {
                project_id: pid,
                table: "schema".into(),
                key: "user".into(),
                value: serde_json::json!({ "columns": ["id", "email"] }),
            },
            SymbolEntry {
                project_id: pid,
                table: "schema".into(),
                key: "order".into(),
                value: serde_json::json!({ "columns": ["id"] }),
            },
        ],
        ..Default::default()
    })
    .expect("apply symbols");

    let one = s
        .get_symbol(pid, "schema", "user")
        .expect("get_symbol")
        .expect("user exists");
    assert_eq!(one, serde_json::json!({ "columns": ["id", "email"] }));

    let all = s.list_symbols(pid, "schema").expect("list_symbols");
    assert_eq!(all.len(), 2);
}

#[test]
fn annotations_attach_to_node() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![node(pid, 303, "Method")],
        ..Default::default()
    })
    .expect("apply");

    s.apply(&GraphDelta {
        project_id: Some(pid),
        annotations: vec![NewAnnotation {
            node_id: NodeId(303),
            channel: AnnotationChannel::new("FkbMark"),
            kind: "pii".into(),
            subkind: Some("phone".into()),
            confidence: 1.0,
            evidence: serde_json::json!({ "hook": "where" }),
            phase: Phase("Annotate".to_string()),
            merge: MergeStrategy::default(),
        }],
        ..Default::default()
    })
    .expect("apply annotations");

    let anns = s.annotations_of(NodeId(303)).expect("annotations_of");
    assert_eq!(anns.len(), 1);
    assert_eq!(anns[0].kind, "pii");
}

/// A synthetic node is referenced from many co-occurrence locations; `location_patches` appends them into
/// `properties.locations` on an **existing** node (a `Null` properties is skipped), caps the array at 50, and a
/// patch for a missing node is a silent no-op. Only covered in `src/store.rs`; this pins it on the public
/// `GraphSink::apply` surface the pipeline actually calls.
#[test]
fn location_patches_append_co_occurrence_locations_and_cap() {
    let s = store();
    let pid = proj(&s);
    let mut n = node(pid, 401, "Table");
    n.properties = serde_json::json!({}); // location_patches only attaches onto an object properties
    s.apply(&GraphDelta {
        project_id: Some(pid),
        nodes: vec![n],
        ..Default::default()
    })
    .expect("apply node");

    let loc = |i: u32| SourceLocation {
        file: format!("f{i}.php"),
        line: i,
        symbol: None,
        note: None,
        snippet: None,
    };

    s.apply(&GraphDelta {
        project_id: Some(pid),
        location_patches: vec![(NodeId(401), loc(1))],
        ..Default::default()
    })
    .expect("apply first location patch");
    let props = s.get_node(NodeId(401)).expect("get").expect("exists").properties;
    assert_eq!(
        props["locations"].as_array().unwrap().len(),
        1,
        "exactly one co-occurrence location appended: {props}"
    );

    // A patch for a node that does not exist must not error and must not materialise a phantom node.
    s.apply(&GraphDelta {
        project_id: Some(pid),
        location_patches: vec![(NodeId(999), loc(999))],
        ..Default::default()
    })
    .expect("apply missing-patch must not error");
    assert!(
        s.get_node(NodeId(999)).expect("get").is_none(),
        "a location patch for a non-existent node must not create anything"
    );

    // Boundary: 50 distinct locations is the cap (`if arr.len() < 50`).
    let mut many = GraphDelta::new(pid);
    for i in 1..=50 {
        many.location_patches.push((NodeId(401), loc(i)));
    }
    s.apply(&many).expect("apply many locations");
    let props = s.get_node(NodeId(401)).expect("get").expect("exists").properties;
    assert_eq!(
        props["locations"].as_array().unwrap().len(),
        50,
        "co-occurrence locations must be capped at 50: {props}"
    );
}

/// A `GraphDelta` can carry `diagnostics` inline; `apply` must persist them so they later surface through the
/// same `list_diagnostics` the diagnostics page reads (a separate path from `push_diagnostics`). Only covered in
/// `src/store.rs`; this pins it on the public `apply` surface.
#[test]
fn apply_carries_diagnostics_inline() {
    let s = store();
    let pid = proj(&s);
    s.apply(&GraphDelta {
        project_id: Some(pid),
        diagnostics: vec![Diagnostic {
            project_id: pid,
            sub_project_id: None,
            phase: Phase("Check".to_string()),
            code: "rule:hot-table".into(),
            severity: Severity::Warning,
            message: "a hot table".into(),
            location: None,
            payload: serde_json::Value::Null,
        }],
        ..Default::default()
    })
    .expect("apply diagnostics");

    let listed = s.list_diagnostics(pid, 100).expect("list");
    assert_eq!(listed.len(), 1, "diagnostics carried inline by apply must be persisted");
    assert_eq!(listed[0].code, "rule:hot-table");
    assert_eq!(listed[0].severity, Severity::Warning);
}
