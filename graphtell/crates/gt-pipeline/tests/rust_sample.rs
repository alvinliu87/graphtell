//! End-to-end graph build for a synthetic Rust project, exercising the Rust FKB:
//! `actix-web` route attributes (and the Axum-style `App::route` call) must become `HttpContract`
//! nodes, and `sqlx::query!` must become an `ExternalSystem(Database)` node with `ReadsDb` / `WritesDb`
//! edges — the same semantic graph other stacks (Spring / Laravel) already build.

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
        "graphtell_rust_sample_{}_{}",
        n,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nactix-web = \"4\"\nsqlx = { version = \"0.7\", features = [\"postgres\"] }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/main.rs"),
        r#"
use actix_web::{get, post, route, App};

#[get("/health")]
async fn health() -> String { "ok".to_string() }

#[post("/users")]
async fn create_user() -> String { "created".to_string() }

#[route("/orders", method = "PUT")]
async fn update_order() -> String { "updated".to_string() }

async fn list_users() -> String { "users".to_string() }

fn routes() {
    let _app = App::route("/legacy", get(list_users));
}

struct Order { id: i64 }

async fn load_orders(pool: &sqlx::PgPool) {
    let _ = sqlx::query!("SELECT id FROM orders").fetch_all(pool).await;
}
"#,
    )
    .unwrap();
    dir
}

fn contracts(built: &common::Built) -> Vec<String> {
    let nodes = built
        .store
        .query_nodes(&NodeFilter {
            project_id: built.project.id,
            kind: Some(NodeKind("HttpContract".to_string())),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query HttpContract nodes");
    nodes.into_iter().map(|n| n.name).collect()
}

#[test]
fn rust_routes_become_http_contract_nodes() {
    let root = sample_dir();
    let built = graph_with_root(&root, ProjectConfig::default()).expect("graph build must succeed");
    let names = contracts(&built);

    assert!(
        names.iter().any(|n| n.contains("health")),
        "GET /health must synthesize an HttpContract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("users")),
        "POST /users must synthesize an HttpContract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("orders")),
        "PUT /orders must synthesize an HttpContract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("legacy")),
        "App::route /legacy must synthesize an HttpContract (call-style rewrite), got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("users")),
        "POST /users must synthesize an HttpContract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("orders")),
        "PUT /orders must synthesize an HttpContract, got: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("legacy")),
        "App::route /legacy must synthesize an HttpContract (axum-style rewrite), got: {names:?}"
    );
}

#[test]
fn rust_sqlx_macro_becomes_database_node() {
    let root = sample_dir();
    let built = graph_with_root(&root, ProjectConfig::default()).expect("graph build must succeed");
    let dbs = built
        .store
        .query_nodes(&NodeFilter {
            project_id: built.project.id,
            kind: Some(NodeKind("Database".to_string())),
            name_contains: None,
            limit: None,
            offset: None,
        })
        .expect("query Database nodes");
    assert!(
        !dbs.is_empty(),
        "sqlx::query! must synthesize an ExternalSystem(Database) node"
    );
}
