//! End-to-end graph build for a synthetic Rust project, exercising the **Diesel** FKB (`fkb/rust/diesel.yaml`).
//!
//! `rust_sample.rs` already pins the sqlx half (the *macro* path: `sqlx::query!` / `sqlx::execute!`). Diesel is the
//! other Rust DB-access FKB and it was never run end-to-end — yet it exercises a *different* parser path: ordinary
//! method-call `call_site`s (`users.load()`, `insert_into(users).values(...).execute()`) whose verb is matched by
//! **method name** in `gt-pipeline/src/engine.rs` (`alt_matches` bare-pattern → `method` branch), not by a macro
//! name. A regression that dropped Diesel's `Synthesize` rules (or broke the Rust parser's method-call call-site
//! emission) would have gone unnoticed.
//!
//! Mirrors `rust_sample::rust_sqlx_macro_becomes_database_node`: a `sqlx::query!`/`execute!` call synthesises an
//! `ExternalSystem(Database)` node and hangs `ReadsDb` / `WritesDb` edges off the *calling method*. Here the same
//! shape is produced by Diesel's `.load()` (read) and `.execute()` (write) verbs.

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
        "graphtell_rust_diesel_{}_{}",
        n,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\ndiesel = { version = \"2\", features = [\"postgres\"] }\n",
    )
    .unwrap();
    // The parser reads only source text — nothing is compiled, so the `table!` macro and `diesel::` items are
    // not resolved; what matters is that `users.load()` and `insert_into(users).execute()` parse into method-call
    // call sites whose method names (`load`, `execute`) drive the Diesel FKB verbs.
    fs::write(
        dir.join("src/main.rs"),
        r#"
use diesel::insert_into;

// A placeholder table value; real Diesel would derive `Queryable` etc., but the parser only needs a name in scope.
struct Users;

// Read: `users.load()` must hang a `ReadsDb` edge off the calling method.
fn load_users() {
    let users = Users;
    let _ = users.load();
}

// Write: `insert_into(users).values(...).execute()` must hang a `WritesDb` edge off the calling method. Note this
// is a *method chain* (`execute` is the final method), unlike sqlx's `execute!` macro — so the match goes through
// the method-name branch, not a macro-name branch.
fn write_users() {
    let users = Users;
    let _ = insert_into(users).values((1,)).execute();
}
"#,
    )
    .unwrap();
    dir
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

/// Diesel's `.load()` (read) and `.execute()` (write) verbs must synthesise an `ExternalSystem(Database)` node with
/// `ReadsDb` / `WritesDb` edges — the method-call analogue of the sqlx macro path that `rust_sample.rs` already pins.
#[test]
fn rust_diesel_method_calls_become_database_edges() {
    let root = sample_dir();
    let built = graph_with_root(&root, ProjectConfig::default()).expect("graph build must succeed");
    let dbs = nodes_of_kind(&built, "Database");
    assert!(
        !dbs.is_empty(),
        "Diesel `.load()` / `.execute()` must synthesise an ExternalSystem(Database) node"
    );
    let db = &dbs[0];

    let reads_db_in = built
        .store
        .edges_of(db.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == "ReadsDb");
    assert!(
        reads_db_in,
        "the Database node must carry an incoming ReadsDb edge (from `users.load()`)"
    );
    let writes_db_in = built
        .store
        .edges_of(db.id, EdgeDirection::Incoming)
        .expect("edges")
        .iter()
        .any(|e| e.kind.as_str() == "WritesDb");
    assert!(
        writes_db_in,
        "the Database node must carry an incoming WritesDb edge (from `insert_into(...).execute()`)"
    );

    // Pin that the edges land on the *calling method*, not just anywhere: `load_users` reads, `write_users` writes.
    assert!(
        function_writes_or_reads_db(&built, "load_users", "ReadsDb"),
        "load_users (`users.load()`) must READ the Database"
    );
    assert!(
        function_writes_or_reads_db(&built, "write_users", "WritesDb"),
        "write_users (`insert_into(...).execute()`) must WRITE the Database"
    );
}
