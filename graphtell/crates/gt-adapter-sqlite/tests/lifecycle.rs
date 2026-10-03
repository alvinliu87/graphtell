//! Project lifecycle contract tests: create -> update -> list -> status -> replace files -> delete (with cascade).
//!
//! Mirrors the gt-pipeline/tests style: a synthetic in-memory store, no external fixtures. These guard the
//! `ProjectReader` / `ProjectWriter` surface that the existing 3 tests never touched — a `delete_project` that
//! fails to cascade, or a `file_path` that can't resolve a stored source file, would both fail silently.

use gt_adapter_sqlite::SqliteStore;
use gt_domain::model::{
    NewProject, NewSourceFile, ProjectPatch, ProjectStatus, Span,
};
use gt_domain::port::{GraphQuery, GraphSink, ProjectReader, ProjectWriter};

fn store() -> SqliteStore {
    SqliteStore::in_memory().expect("the in-memory database should open")
}

fn new_proj(name: &str) -> NewProject {
    NewProject {
        name: name.into(),
        root_path: format!("/tmp/{name}").into(),
        description: None,
        config: None,
    }
}

#[test]
fn create_update_and_list_project() {
    let s = store();
    let pid = s.create_project(new_proj("p1")).expect("create").id;
    assert_eq!(s.list_projects().expect("list").len(), 1);

    let updated = s
        .update_project(
            pid,
            ProjectPatch {
                name: Some("p1-renamed".into()),
                ..Default::default()
            },
        )
        .expect("update");
    assert_eq!(updated.name, "p1-renamed");
    let got = s.get_project(pid).expect("get").expect("exists");
    assert_eq!(got.name, "p1-renamed");
}

#[test]
fn set_project_status_persists() {
    let s = store();
    let pid = s.create_project(new_proj("p2")).expect("create").id;
    s.set_project_status(pid, ProjectStatus::Ready).expect("status");
    assert_eq!(
        s.get_project(pid).expect("get").expect("exists").status,
        ProjectStatus::Ready
    );
}

#[test]
fn delete_project_cascades_graph_and_disappears_from_list() {
    let s = store();
    let pid = s.create_project(new_proj("p3")).expect("create").id;
    s.apply(&gt_domain::model::GraphDelta {
        project_id: Some(pid),
        nodes: vec![gt_domain::model::NewNode {
            id: Some(gt_domain::model::NodeId(1)),
            project_id: pid,
            sub_project_id: None,
            kind: gt_domain::model::NodeKind::new("Table"),
            name: "t".into(),
            fqn: None,
            identity: None,
            file_id: None,
            span: Span { start_line: 1, end_line: 2, start_byte: 0, end_byte: 0 },
            language: gt_domain::model::Language::new("php"),
            phase: gt_domain::model::Phase("Synthesize".to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        }],
        ..Default::default()
    })
    .expect("apply");

    s.delete_project(pid).expect("delete");
    assert!(
        s.get_project(pid).expect("get").is_none(),
        "project gone after delete"
    );
    assert!(
        s.list_projects().expect("list").is_empty(),
        "project removed from list"
    );
    let nodes = s
        .query_nodes(&gt_domain::port::NodeFilter {
            project_id: pid,
            kind: None,
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query");
    assert!(nodes.is_empty(), "graph must be cascade-deleted");
}

/// Source files are written by `replace_files` and resolved back by `file_path` / `list_files` — a break here
/// means jump-to-source links in the UI silently 404.
#[test]
fn replace_files_then_resolve_path() {
    let s = store();
    let pid = s.create_project(new_proj("p4")).expect("create").id;
    let files = s
        .replace_files(
            pid,
            vec![NewSourceFile {
                project_id: pid,
                sub_project_id: None,
                path: "app/Order.php".into(),
                language: gt_domain::model::Language::new("php"),
                size_bytes: 10,
                content_hash: "abc".into(),
            }],
        )
        .expect("replace_files");
    assert_eq!(files.len(), 1);
    let fid = files[0].id;

    assert_eq!(
        s.file_path(fid).expect("file_path"),
        Some("app/Order.php".to_string())
    );
    let listed = s.list_files(pid, None).expect("list_files");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].path, "app/Order.php");
}
