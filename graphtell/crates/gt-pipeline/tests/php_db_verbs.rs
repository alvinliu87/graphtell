//! End-to-end self-check of the **PHP (ThinkPHP / Laravel) read / write verb classification**.
//!
//! The Java side has `java_db_verbs.rs`, the JS/TS side has `node_db_verbs.rs`, and Python has
//! `django_features.rs` — each pins that `repo.save()` / `repo.find()` actually lays a `WritesDb` / `ReadsDb`
//! edge. PHP had **no** synthetic e2e for this: `fkb/php/{laravel,thinkphp}.yaml` both declare `db_verbs`, but
//! the only PHP tests touching the path were the FKB-ignorant unit tests in `php.rs` (`db_table_of` shape) and
//! the `db_schema` table-name collection. So a regression that stopped classifying `Db::name('x')->insert()` —
//! or that dropped a framework's `db_verbs` — would have left every PHP service without a single `WritesDb`
//! edge while the suite stayed green, exactly the gap `node_db_verbs.rs` was written to close on the JS side.
//!
//! The fixture deliberately uses the **facade** form (`Db::name('order')->insert()`) rather than a model
//! property call: that is the path where `gt-adapter-parser/src/php.rs::db_table_of` contributes the table
//! name, and `classify_facade_db_calls` (`resolve.rs:449`) is the only consumer that reads `call.db_table`
//! together with `call.method`. A model `$m->save()` would instead need variable-type resolution to reach the
//! table, a second path that `php_orm_features.rs` already exercises for the *relation* half.
//!
//! Synthetic project, no external sample.

mod common;

use std::path::PathBuf;

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

/// A minimal ThinkPHP project (detected by `topthink/framework` in `composer.json`) whose only PHP file is a
/// single service method performing the DB calls under test.
fn synthetic_tp_root(calls: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-dbverbs-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        calls.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/service")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{
  "name": "demo/tp",
  "require": { "topthink/framework": "^6.0" },
  "autoload": { "psr-4": { "app\\": "app/" } }
}
"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("app/service/OrderService.php"),
        format!(
            r#"<?php
namespace app\service;

class OrderService
{{
    public function handle()
    {{
{calls}
    }}
}}
"#
        ),
    )
    .expect("write OrderService.php");

    dir
}

fn nodes_of_kind<'a>(b: &'a common::Built, kind: &str) -> Vec<gt_domain::model::Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .collect()
}

/// Out-edge kinds of the `handle` method node (the owner of every call in the fixture).
fn handle_out_edge_kinds(b: &common::Built) -> Vec<String> {
    let m = nodes_of_kind(b, "Method")
        .into_iter()
        .find(|n| n.name == "handle")
        .unwrap_or_else(|| panic!("the `handle` method node must exist"));
    b.store
        .edges_of(m.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .map(|e| e.kind.as_str().to_string())
        .collect()
}

/// Whether `handle` has a `WritesDb` / `ReadsDb` edge reaching a `Table` named `table`.
fn handle_db_edge_to(b: &common::Built, edge: &str, table: &str) -> bool {
    let m = nodes_of_kind(b, "Method")
        .into_iter()
        .find(|n| n.name == "handle")
        .expect("the `handle` method node must exist");
    b.store
        .edges_of(m.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == edge)
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .any(|n| n.kind.as_str() == "Table" && n.name == table)
}

#[test]
fn thinkphp_facade_insert_is_a_write_to_its_table() {
    let root = synthetic_tp_root("        \\think\\facade\\Db::name('order')->insert($data);\n");
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic ThinkPHP project graph build should succeed");
    };

    // Precondition: the `Db::name('order')` call is what *creates* the `order` Table node (via `db_schema`);
    // if that ever breaks, the edge below would silently vanish, so state it explicitly.
    let tables: Vec<String> = nodes_of_kind(&b, "Table").iter().map(|n| n.name.clone()).collect();
    assert!(
        tables.iter().any(|t| t == "order"),
        "the `Db::name('order')` call must create the `order` Table node, got: {tables:?}"
    );

    assert!(
        handle_db_edge_to(&b, "WritesDb", "order"),
        "`Db::name('order')->insert($data)` must classify as a write to the order table: {:?}",
        handle_out_edge_kinds(&b)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn thinkphp_facade_find_is_a_read_to_its_table() {
    let root = synthetic_tp_root(
        "        \\think\\facade\\Db::name('order')->where('id', 1)->find();\n",
    );
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic ThinkPHP project graph build should succeed");
    };

    assert!(
        handle_db_edge_to(&b, "ReadsDb", "order"),
        "`Db::name('order')->where(...)->find()` must classify as a read of the order table: {:?}",
        handle_out_edge_kinds(&b)
    );

    // The same call must not be double-counted as a write.
    assert_eq!(
        handle_out_edge_kinds(&b)
            .iter()
            .filter(|k| *k == "WritesDb")
            .count(),
        0,
        "a read-only query must not also be a write"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// A chained facade call that never names a read / write verb (`where` only builds a query) must lay **no**
/// `WritesDb` / `ReadsDb` edge — otherwise one builder call would be turned into a spurious action edge.
#[test]
fn facade_builder_call_without_a_db_verb_is_not_classified() {
    let root = synthetic_tp_root("        \\think\\facade\\Db::name('order')->where('id', 1);\n");
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic ThinkPHP project graph build should succeed");
    };

    let edges = handle_out_edge_kinds(&b);
    assert!(
        !edges.iter().any(|k| k == "WritesDb" || k == "ReadsDb"),
        "a `Db::name('order')->where(...)` (no read/write verb) must not produce a DB action edge: {edges:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
