//! End-to-end graph build for a synthetic Rust project, exercising the Rust FKB:
//! `actix-web` route attributes (and the Axum-style `App::route` call) must become `HttpContract`
//! nodes, and `sqlx::query!` must become an `ExternalSystem(Database)` node with `ReadsDb` / `WritesDb`
//! edges — the same semantic graph other stacks (Spring / Laravel) already build.

mod common;

use std::fs;
use std::path::PathBuf;

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

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

// A write: the `execute!` macro (not `query!`) must hang a `WritesDb` edge off the calling method — the read
// branch is already covered by `load_orders`, this pins the *write* half of sqlx.yaml that the original sample
// never exercised.
async fn write_order(pool: &sqlx::PgPool) {
    let _ = sqlx::execute!("INSERT INTO orders (id) VALUES (1)", pool).await;
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

fn nodes_of_kind(built: &common::Built, kind: &str) -> Vec<Node> {
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
}

/// Whether a contract (matched by path fragment) has an outgoing `HandledBy` edge. For module-level Rust functions
/// the target resolves to the module (`crate`) node rather than the function name, so we pin the *edge* (the FKB lays
/// `HandledBy`), not the target's name.
fn contract_has_handled_by(built: &common::Built, path_fragment: &str) -> bool {
    let c = nodes_of_kind(built, "HttpContract")
        .into_iter()
        .find(|n| n.name.contains(path_fragment))
        .unwrap_or_else(|| {
            panic!(
                "no HttpContract containing {path_fragment}, got: {:?}",
                contracts(built)
            )
        });
    built
        .store
        .edges_of(c.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == "HandledBy")
}

/// Whether a `Function` node of the given name has an outgoing `edge` (ReadsDb / WritesDb) into a `Database` node.
fn function_writes_or_reads_db(built: &common::Built, fn_name: &str, edge: &str) -> bool {
    nodes_of_kind(built, "Function")
        .iter()
        .find(|n| n.name == fn_name)
        .is_some_and(|f| {
            built
                .store
                .edges_of(f.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .any(|e| {
                    e.kind.as_str() == edge
                        && built
                            .store
                            .get_node(e.to_id)
                            .ok()
                            .flatten()
                            .is_some_and(|t| t.kind.as_str() == "Database")
                })
        })
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

    // The FKB lays a `HandledBy` edge from each contract to its handler (the route attribute / call-style rewrite
    // records the decorated function as owner). The original test only checked the contract names, so a regression
    // that dropped the edge would have passed.
    assert!(
        contract_has_handled_by(&built, "health"),
        "GET /health must HandledBy its handler"
    );
    assert!(
        contract_has_handled_by(&built, "users"),
        "POST /users must HandledBy its handler"
    );
    assert!(
        contract_has_handled_by(&built, "orders"),
        "PUT /orders must HandledBy its handler"
    );
    assert!(
        contract_has_handled_by(&built, "legacy"),
        "App::route /legacy must HandledBy its handler (call-style rewrite)"
    );
}

#[test]
fn rust_sqlx_macro_becomes_database_node() {
    let root = sample_dir();
    let built = graph_with_root(&root, ProjectConfig::default()).expect("graph build must succeed");
    let dbs = nodes_of_kind(&built, "Database");
    assert!(
        !dbs.is_empty(),
        "sqlx::query! must synthesize an ExternalSystem(Database) node"
    );
    let db = &dbs[0];

    // `query!` (read) hangs a `ReadsDb` edge off the calling method; `execute!` (write) hangs a `WritesDb` edge.
    // The original test only asserted the node existed — the edges (the whole point of the Database mediator) were
    // never pinned, so a regression that built the node but dropped the edge would have passed.
    let reads_db_in = built
        .store
        .edges_of(db.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == "ReadsDb");
    assert!(reads_db_in, "the Database node must carry an incoming ReadsDb edge (from sqlx::query!)");
    let writes_db_in = built
        .store
        .edges_of(db.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == "WritesDb");
    assert!(writes_db_in, "the Database node must carry an incoming WritesDb edge (from sqlx::execute!)");

    // Pin that the edges land on the *calling method*, not just anywhere: `load_orders` reads, `write_order` writes.
    assert!(
        function_writes_or_reads_db(&built, "load_orders", "ReadsDb"),
        "load_orders (sqlx::query!) must READ the Database"
    );
    assert!(
        function_writes_or_reads_db(&built, "write_order", "WritesDb"),
        "write_order (sqlx::execute!) must WRITE the Database"
    );
}
