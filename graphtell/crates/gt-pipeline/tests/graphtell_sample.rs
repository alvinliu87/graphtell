//! End-to-end graph build for a *synthetic GraphTell-shaped* Rust project, exercising the bootstrap FKB:
//! `fkb/projects/graphtell.yaml` must turn every `impl <gt-domain port> for <adapter>` into a `DomainPort`
//! hub node with `ImplementsPort` edges, while the generic `fkb/rust/*` FKBs build the framework-level
//! semantic nodes GraphTell itself uses — `rusqlite` DB access and `axum` HTTP routes.
//!
//! This mirrors how GraphTell analyses *itself* (its real crates depend on `gt-domain`, call `rusqlite`, and
//! serve `axum` routes), so the same assertions hold for the actual workspace.

mod common;

use std::fs;
use std::path::PathBuf;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

use common::graph_with_root;

fn sample_dir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "graphtell_bootstrap_sample_{}_{}",
        n,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    // A project that *is* GraphTell-shaped: depends on gt-domain (ports+adapters), rusqlite (DB), axum (serve).
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\ngt-domain = \"0.1\"\nrusqlite = \"0.40\"\naxum = \"0.8\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/main.rs"),
        r#"
use rusqlite::Connection;
use axum::routing::get;
use axum::Router;

// --- ports + adapters architecture (the GraphTell signature) ---
struct MyParser;
impl gt_domain::port::LanguageParser for MyParser {}

struct MyStore;
impl gt_domain::port::RuleConfigStore for MyStore {}

// --- DB access through rusqlite (what GraphTell's gt-adapter-sqlite does) ---
fn db_access(conn: &Connection) {
    let _ = conn.query_row("SELECT id FROM projects", [], |r| r.get(0));
}

// --- HTTP routes through axum (what GraphTell's gt-adapter-http does) ---
async fn health() -> &'static str { "ok" }

fn routes() -> Router {
    Router::new().route("/api/health", get(health))
}
"#,
    )
    .unwrap();
    dir
}

fn names_of(built: &common::Built, kind: &str) -> Vec<String> {
    built
        .store
        .query_nodes(&NodeFilter {
            project_id: built.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query nodes")
        .into_iter()
        .map(|n| n.name)
        .collect()
}

#[test]
fn graphtell_bootstrap_builds_framework_and_architecture_nodes() {
    let root = sample_dir();
    let built = graph_with_root(&root, ProjectConfig::default()).expect("graph build must succeed");

    // 1) Generic rusqlite FKB: DB access becomes an ExternalSystem(Database) node.
    let dbs = names_of(&built, "Database");
    assert!(
        !dbs.is_empty(),
        "rusqlite::query_row must synthesize a Database node (got: {dbs:?})"
    );

    // 2) Generic axum FKB: the route becomes an HttpContract node.
    let contracts = names_of(&built, "HttpContract");
    assert!(
        contracts.iter().any(|n| n.contains("health")),
        "axum Router::route /api/health must synthesize an HttpContract (got: {contracts:?})"
    );

    // 3) Project-level graphtell.yaml: every `impl <port> for <adapter>` becomes a DomainPort hub.
    let ports = names_of(&built, "DomainPort");
    assert!(
        !ports.is_empty(),
        "graphtell.yaml must synthesize a DomainPort node from the impl blocks (got: {ports:?})"
    );
    // And the adapter structs connect to it via ImplementsPort edges.
    let port_id = built
        .store
        .query_nodes(&NodeFilter {
            project_id: built.project.id,
            kind: Some(NodeKind("DomainPort".to_string())),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query DomainPort")
        .into_iter()
        .next()
        .expect("DomainPort node must exist")
        .id;
    let incoming = built
        .store
        .edges_incoming(&[port_id])
        .expect("query incoming edges");
    let has_impl = incoming
        .values()
        .flat_map(|es| es.iter())
        .any(|e| e.kind.0 == "ImplementsPort");
    assert!(
        has_impl,
        "DomainPort must have ImplementsPort edges from the adapter structs"
    );
}
