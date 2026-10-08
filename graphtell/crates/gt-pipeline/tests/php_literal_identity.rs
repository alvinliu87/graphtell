//! End-to-end: what `crates/gt-adapter-parser/src/php/value.rs` can evaluate **statically** is exactly what
//! FKB may use as a node's identity (`require_literal`).
//!
//! `value.rs` has thorough unit tests (`eval_expr_maps_statically_known_literals`,
//! `eval_expr_keeps_arrays_and_rejects_interpolation`, …) that already go through `PhpParser::parse`, so the
//! *value* each expression folds into is pinned. What was never pinned is the **consequence** through a build:
//! `fkb/universal/common.yaml`'s cache rules take their identity from `{ arg: 0, require_literal: true }` with a
//! `value_fallback` of the literal `Cache`. So:
//! * a literal key names the node (`order-status`) — that is the whole point of evaluating literals;
//! * a variable key (`$key`) must fall back to `Cache` instead of becoming a node named `$key`;
//! * an **interpolated** string (`"user_$id"`) must fall back too: `eval_encapsed` degrades it to `Unknown`, and
//!   a raw `user_$id` would be a fabricated identity (one node per call site, none of them real).
//!
//! Synthetic PHP project, no external sample.

mod common;

use std::path::PathBuf;

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

fn synthetic_php_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-php-literal-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/service")).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{"name":"demo/demo","require":{"topthink/framework":"^6.0"},"autoload":{"psr-4":{"app\\":"app/"}}}"#,
    )
    .expect("write composer.json");
    std::fs::write(
        dir.join("app/service/Svc.php"),
        r#"<?php
namespace app\service;

class Svc
{
    public function readStatus()
    {
        return Cache::get('order-status');
    }

    public function writeAny($key, $v)
    {
        Cache::set($key, $v);
    }

    public function readUser($id)
    {
        return Cache::get("user_$id");
    }
}
"#,
    )
    .expect("write Svc.php");
    dir
}

fn cache_nodes(root: &std::path::Path) -> Vec<Node> {
    let b = common::graph_with_root(root, ProjectConfig::default()).expect("graphing");
    let mut nodes = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Cache".to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query");
    // Keep a stable order for the assertions below.
    nodes.sort_by(|a, c| a.name.cmp(&c.name));
    nodes
}

fn has_incoming(b: &common::Built, name: &str, edge: &str) -> bool {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind("Cache".to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .filter(|n| n.name == name)
        .any(|n| {
            b.store
                .edges_of(n.id, EdgeDirection::Incoming)
                .unwrap_or_default()
                .iter()
                .any(|e| e.kind.as_str() == edge)
        })
}

/// A literal argument becomes the node's identity; a variable one must not — it falls back to the FKB's
/// `value_fallback` (`Cache`). Both still carry their read / write edge, so the fallback is visible rather
/// than dropped.
#[test]
fn literal_key_names_the_node_and_variable_key_falls_back() {
    let root = synthetic_php_root();
    let nodes = cache_nodes(&root);
    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();

    assert!(
        names.contains(&"order-status"),
        "`Cache::get('order-status')` must name the node by its literal key, got: {names:?}"
    );
    assert!(
        names.contains(&"Cache"),
        "a non-literal key must fall back to the `value_fallback` node, got: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with('$')),
        "a variable must never become an identity (`require_literal`), got: {names:?}"
    );

    // The `key` field is the same `{ arg: 0, require_literal: true }` source as the identity, so it is
    // present only when the argument really was a literal — which is what makes the fallback node the
    // non-literal one (and not some other Cache node).
    let literal = nodes.iter().find(|n| n.name == "order-status").expect("checked above");
    assert_eq!(
        literal.properties.get("key").and_then(|v| v.as_str()),
        Some("order-status"),
        "a literal argument must also fill the `key` field: {:?}",
        literal.properties
    );
    let fallback = nodes.iter().find(|n| n.name == "Cache").expect("checked above");
    assert_eq!(
        fallback.properties.get("key").and_then(|v| v.as_str()),
        None,
        "a non-literal argument must leave the `key` field unset: {:?}",
        fallback.properties
    );

    let b = common::graph_with_root(&root, ProjectConfig::default()).expect("graphing");
    assert!(
        has_incoming(&b, "order-status", "ReadsCache"),
        "the literal-key node must carry the ReadsCache edge"
    );
    assert!(
        has_incoming(&b, "Cache", "WritesCache"),
        "the fallback node must still carry the WritesCache edge"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// An **interpolated** key (`"user_$id"`) is statically unknowable — `eval_encapsed` degrades it to
/// `Unknown` — so it must not name a node. Without this, every call site would invent its own identity.
#[test]
fn interpolated_key_is_not_an_identity() {
    let root = synthetic_php_root();
    let nodes = cache_nodes(&root);
    let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
    assert!(
        !names.iter().any(|n| n.starts_with("user_")),
        "an interpolated key must not become a node identity, got: {names:?}"
    );
    assert!(
        names.contains(&"Cache"),
        "it must land on the `value_fallback` node instead, got: {names:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
