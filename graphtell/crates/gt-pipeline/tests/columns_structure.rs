//! Always-on (sample-free) structural self-checks for the **Columns** phase (P6, `phase/columns.rs`).
//!
//! `columns.rs` materialises `Table --HasColumn--> Column` from the authoritative schema symbol
//! table (loaded from SQL install scripts / migrations in P3). The `Column`/`HasColumn` feature is
//! already exercised by synthetic integration tests in `php_orm_features.rs` (PHP DDL + Laravel
//! migration, which also pins the plural-name fallback `users` -> `user`) and `django_features.rs`
//! (Django field declarations). Those cover the happy path and the cross-stack shape.
//!
//! The third `Column`/`HasColumn` *source* — the TypeORM `@Column()` decorator path (JS/TS), which
//! materialises columns from ORM field declarations rather than a schema table — is pinned by the
//! always-on synthetic test `node_real_samples.rs::synthetic_nestjs_and_express_graph` (it asserts
//! `UserEntity.username` -> `Column` and the `UserEntity --HasColumn--> Column` edge). It is not
//! sample-gated, so it runs on every `cargo test` too.
//!
//! What they do **not** pin are the two invariants the phase's own code comments call out as
//! dangerous, so a regression there would silently merge or invent columns:
//!
//! 1. **Table-scoped column identity** (`columns.rs` lines ~67-70): the same column name in two
//!    tables (`user.id` vs `order.id`) must stay two distinct `Column` nodes, each linked only to
//!    its own table — otherwise two tables' impact surfaces get stitched together.
//! 2. **No schema => no columns** (`columns.rs` lines ~14): a `Table` with only an ORM declaration
//!    and no DDL must get *zero* `HasColumn` edges — better missing than guessed.
//!
//! These tests build a tiny synthetic PHP/Laravel project (two DDL tables sharing a column name plus
//! one model with no DDL) via `common::graph_with_root` — no external sample, so they run on every
//! `cargo test` and actually catch a Columns regression.

use std::collections::HashMap;

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(10_000),
            offset: Some(0),
        })
        .expect("query")
}

fn table_named(b: &common::Built, name: &str) -> Node {
    nodes_of_kind(b, "Table")
        .into_iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("expected a Table node named `{name}`"))
}

/// The `Column` node names a table points at via `HasColumn` (resolved from the edge targets).
fn has_column_targets(b: &common::Built, table: &Node) -> Vec<String> {
    let by_id: HashMap<_, String> = nodes_of_kind(b, "Column")
        .into_iter()
        .map(|n| (n.id, n.name.clone()))
        .collect();
    b.store
        .edges_of(table.id, EdgeDirection::Outgoing)
        .expect("edges readable")
        .into_iter()
        .filter(|e| e.kind.as_str() == "HasColumn")
        .filter_map(|e| by_id.get(&e.to_id).cloned())
        .collect()
}

fn synthetic_php_schema_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-columns-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("write composer.json");

    // Two DDL tables that *share* the column name `id` — this is the case the table-scoped identity
    // invariant protects against. Both use the plural DDL name (Laravel convention); the table nodes
    // come out singular (`user` / `order`).
    std::fs::write(
        dir.join("install.sql"),
        r#"CREATE TABLE `users` (
  `id` int(11) NOT NULL AUTO_INCREMENT,
  `email` varchar(120) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;

CREATE TABLE `orders` (
  `id` int(11) NOT NULL AUTO_INCREMENT,
  `total` decimal(10,2) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
"#,
    )
    .expect("write install.sql");

    // `User` / `Order` get a Table node (from `extends Model`) + schema columns (from DDL).
    // `Session` is declared as a model but has NO DDL -> must get no columns (don't guess).
    std::fs::write(
        dir.join("app/Models/User.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class User extends Model
{
}
"#,
    )
    .expect("write User.php");
    std::fs::write(
        dir.join("app/Models/Order.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Order extends Model
{
}
"#,
    )
    .expect("write Order.php");
    std::fs::write(
        dir.join("app/Models/Session.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Session extends Model
{
}
"#,
    )
    .expect("write Session.php");
    dir
}

/// Like [`synthetic_php_schema_root`] but the DDL table name is already **singular** (`CREATE TABLE user`,
/// not `users`). Its only job is to pin that the real DDL loader keeps the raw name as the schema key, so the
/// columns phase resolves by *exact* key instead of only through the plural fallback.
fn synthetic_php_schema_singular_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-columns-singular-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/Models")).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{ "require": { "laravel/framework": "^11.0" } }"#,
    )
    .expect("write composer.json");
    std::fs::write(
        dir.join("install.sql"),
        r#"CREATE TABLE `user` (
  `id` int(11) NOT NULL AUTO_INCREMENT,
  `email` varchar(120) NOT NULL,
  PRIMARY KEY (`id`)
) ENGINE=InnoDB;
"#,
    )
    .expect("write install.sql");
    std::fs::write(
        dir.join("app/Models/User.php"),
        r#"<?php
namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class User extends Model
{
}
"#,
    )
    .expect("write User.php");
    dir
}

/// Two tables sharing a column name must yield two distinct `Column` nodes, each wired only to its
/// own table — `user.id` and `order.id` must not merge.
#[test]
fn two_tables_sharing_a_column_keep_distinct_identity() {
    let root = synthetic_php_schema_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };

    let cols: Vec<String> = nodes_of_kind(&b, "Column")
        .iter()
        .map(|n| n.name.clone())
        .collect();

    // Both tables' `id` columns exist, and are kept separate by table scope.
    assert!(
        cols.iter().any(|c| c == "user.id"),
        "user.id column should exist, got: {cols:?}"
    );
    assert!(
        cols.iter().any(|c| c == "order.id"),
        "order.id column should exist, got: {cols:?}"
    );
    assert!(
        cols.iter().any(|c| c == "user.email"),
        "user.email column should exist, got: {cols:?}"
    );
    assert!(
        cols.iter().any(|c| c == "order.total"),
        "order.total column should exist, got: {cols:?}"
    );

    // The `user` table must point at *its* columns only — never at `order.id` (no cross-table merge).
    let user = table_named(&b, "user");
    let user_cols = has_column_targets(&b, &user);
    assert!(
        user_cols.iter().any(|c| c == "user.id") && user_cols.iter().any(|c| c == "user.email"),
        "user table should link user.id + user.email, got: {user_cols:?}"
    );
    assert!(
        !user_cols.iter().any(|c| c == "order.id"),
        "user table must NOT link order.id (table-scoped identity violated): {user_cols:?}"
    );

    let order = table_named(&b, "order");
    let order_cols = has_column_targets(&b, &order);
    assert!(
        order_cols.iter().any(|c| c == "order.id") && order_cols.iter().any(|c| c == "order.total"),
        "order table should link order.id + order.total, got: {order_cols:?}"
    );
    assert!(
        !order_cols.iter().any(|c| c == "user.id"),
        "order table must NOT link user.id (table-scoped identity violated): {order_cols:?}"
    );
}

/// A model with no DDL entry gets no `HasColumn` edges — columns are never guessed.
#[test]
fn table_without_ddl_gets_no_columns() {
    let root = synthetic_php_schema_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };

    let session = table_named(&b, "session");
    let session_cols = has_column_targets(&b, &session);
    assert!(
        session_cols.is_empty(),
        "a Table with no DDL must get zero HasColumn edges (no guessed columns), got: {session_cols:?}"
    );

    // Sanity: the DDL-backed tables did get columns, so this test isn't vacuously passing.
    assert!(
        !has_column_targets(&b, &table_named(&b, "user")).is_empty(),
        "sanity: the user table (with DDL) should have columns"
    );
}

/// A DDL table whose name is already singular (`CREATE TABLE user`) must resolve by **exact** key, not only
/// through the plural fallback. Every other sample in this file writes plural DDL (`users`/`orders`), which
/// only exercises `or_else(plural)` — so a regression that dropped the exact-match lookup would pass all of
/// them while silently breaking every project whose DDL is written singular. This pins the real DDL loader
/// keeps the raw name as the schema key.
#[test]
fn table_with_singular_ddl_resolves_exact_key() {
    let root = synthetic_php_schema_singular_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };
    let user = table_named(&b, "user");
    let cols = has_column_targets(&b, &user);
    assert!(
        cols.iter().any(|c| c == "user.id"),
        "user.id must resolve by exact key, got: {cols:?}"
    );
    assert!(
        cols.iter().any(|c| c == "user.email"),
        "user.email must resolve by exact key, got: {cols:?}"
    );
    assert_eq!(cols.len(), 2, "exactly the two DDL columns, got: {cols:?}");
}

/// A `Column` node must record its provenance (`column` name, `category`, `sources: ["schema"]`) in `properties`.
/// The kernel cannot hard-code which semantic kinds exist, so this metadata is what the `ColumnsMatch` predicate
/// (and the UI) rely on to know "this column came from the authoritative DDL". The phase unit test pins it for an
/// in-memory workspace; this pins it **survives the full DDL → P3 schema-symbol → P6 materialise → store** round trip.
#[test]
fn column_node_records_schema_provenance_through_full_pipeline() {
    let root = synthetic_php_schema_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };

    let email = nodes_of_kind(&b, "Column")
        .into_iter()
        .find(|n| n.name == "user.email")
        .unwrap_or_else(|| panic!("user.email column node should exist"));

    let p = &email.properties;
    assert_eq!(
        p.get("column").and_then(|v| v.as_str()),
        Some("email"),
        "the column name must be recorded"
    );
    assert_eq!(
        p.get("category").and_then(|v| v.as_str()),
        Some("Column"),
        "category must be Column"
    );
    let sources = p
        .get("sources")
        .and_then(|v| v.as_array())
        .expect("sources must be an array");
    assert!(
        sources.iter().any(|s| s.as_str() == Some("schema")),
        "the source must be the authoritative schema (DDL), got: {sources:?}"
    );
}
