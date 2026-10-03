//! End-to-end self-check of the generic `method_ref` resolver (formerly `handler_pattern`).
//!
//! Covers the three behaviours agreed for the "string -> method/function" resolver:
//!   1. a string-literal handler (`'admin.Login/login'`) resolves to the real controller method via `HandledBy`;
//!   2. a fully-qualified string (the shape a `X::class` resolves to) is resolved by exact match, not mangled;
//!   3. a typo'd / non-existent handler produces **no** edge and **no** ghost node.
//!
//! Uses a synthetic ThinkPHP project (no external sample needed).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_tp_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-method-ref-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/admin/controller/v1")).expect("mkdir");

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
        dir.join("app/admin/controller/Login.php"),
        r#"<?php
namespace app\admin\controller;

class Login
{
    public function login() { return 'ok'; }
    public function logout() { return 'ok'; }
}
"#,
    )
    .expect("write Login.php");

    std::fs::write(
        dir.join("app/admin/controller/v1/Login.php"),
        r#"<?php
namespace app\admin\controller\v1;

class Login
{
    public function dashboard() { return 'ok'; }
}
"#,
    )
    .expect("write v1/Login.php");

    // route file lives under app/admin/route/ -> `route` directory anchor => app segment "admin"
    // (the app segment is the directory *immediately before* a `route/` directory, per `MethodRefSpec.app_anchor_dir`)
    std::fs::create_dir_all(dir.join("app/admin/route")).expect("mkdir route dir");
    std::fs::write(
        dir.join("app/admin/route/routes.php"),
        r#"<?php
use think\facade\Route;

Route::get('/login', 'Login/login');                       // app segment inferred from path, controller = Login
Route::get('/dash', 'v1.Login/dashboard');                 // hierarchy separator '.' -> '\'
Route::get('/fqn', 'app\admin\controller\Login');          // fully-qualified string (class_const shape)
Route::get('/ghost', 'nope.Nope/ghost');                   // typo: no matching class anywhere
"#,
    )
    .expect("write route.php");

    dir
}

fn nodes_of_kind(b: &common::Built, kind: &str) -> Vec<Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(kind.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
}

/// Name of the `HandledBy` target of an `HttpContract` (by exact name), or `None` if there is no such edge.
fn handler_method_of(b: &common::Built, contract_name: &str) -> Option<String> {
    let contracts = nodes_of_kind(b, "HttpContract");
    let c = contracts.iter().find(|n| n.name == contract_name)?;
    b.store
        .edges_of(c.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .next()
        .map(|n| n.name)
}

#[test]
fn method_ref_resolves_string_handler_to_real_method() {
    let dir = synthetic_tp_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graph build should succeed");
    };

    // 1) controller-only handler, app segment inferred from the route file path -> method `login`
    assert_eq!(
        handler_method_of(&b, "GET /login").as_deref(),
        Some("login"),
        "string handler 'Login/login' must resolve to the real `login` method"
    );

    // 2) hierarchy separator '.' -> '\' : 'v1.Login/dashboard' -> `dashboard`
    assert_eq!(
        handler_method_of(&b, "GET /dash").as_deref(),
        Some("dashboard"),
        "hierarchy separator must map v1.Login -> v1\\Login"
    );

    // 3) fully-qualified string (the shape a `X::class` produces) resolves by exact match, not template mangling
    assert!(
        handler_method_of(&b, "GET /fqn").is_some(),
        "a fully-qualified string must resolve to a real node, not be mangled into a non-existent FQN"
    );
}

#[test]
fn method_ref_never_synthesizes_ghost_nodes_on_miss() {
    let dir = synthetic_tp_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graph build should succeed");
    };

    // The typo'd handler 'nope.Nope/ghost' must produce no HandledBy edge ...
    assert_eq!(
        handler_method_of(&b, "GET /ghost"),
        None,
        "an unresolvable handler string must not create a HandledBy edge"
    );

    // ... and must not synthesize a ghost Class / Method node out of the string.
    let classes: Vec<String> = nodes_of_kind(&b, "Class").into_iter().map(|n| n.name).collect();
    let methods: Vec<String> = nodes_of_kind(&b, "Method").into_iter().map(|n| n.name).collect();
    assert!(
        !classes.iter().any(|c| c == "Nope"),
        "no ghost Class `Nope` should be synthesized, got: {classes:?}"
    );
    assert!(
        !methods.iter().any(|m| m == "ghost"),
        "no ghost Method `ghost` should be synthesized, got: {methods:?}"
    );
}
