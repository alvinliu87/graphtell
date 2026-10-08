//! End-to-end self-check of the generic `method_ref` resolver.
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
Route::get('/fallback', 'Login/missingMethod');            // class exists, method does NOT -> must fall back to the Class node
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
    handler_target_of(b, contract_name).map(|n| n.name)
}

/// The `HandledBy` target node of an `HttpContract` (by exact name), or `None` if there is no such edge.
/// Returning the whole node lets a test pin *which* node was resolved (name + kind), not just that one exists.
fn handler_target_of(b: &common::Built, contract_name: &str) -> Option<Node> {
    let contracts = nodes_of_kind(b, "HttpContract");
    let c = contracts.iter().find(|n| n.name == contract_name)?;
    b.store
        .edges_of(c.id, EdgeDirection::Outgoing)
        .expect("edges")
        .iter()
        .filter(|e| e.kind.as_str() == "HandledBy")
        .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
        .next()
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

    // 3) fully-qualified string (the shape a `X::class` produces) resolves by exact match, not template mangling.
    // Pin the *exact* target: it must be the real `Login` Class node (named by short name in this graph) — a
    // regression that mangled the FQN (e.g. dropped the namespace or invented a `::index` method, resolving to a
    // ghost) would have slipped through the previous `is_some()` check. (Base-vs-v1 disambiguation is already
    // pinned by case 1: only the base `Login` has a `login` method, so resolving `/login` to `login` proves the
    // app-segment inference picked the base class, not `v1\Login`.)
    let fqn_target = handler_target_of(&b, "GET /fqn")
        .expect("a fully-qualified string must resolve to a real node, not be mangled into a non-existent FQN");
    assert_eq!(
        fqn_target.kind.as_str(),
        "Class",
        "the FQN handler points at the Class node, not a synthetic Method"
    );
    assert_eq!(
        fqn_target.name, "Login",
        "the FQN must resolve to the real Login class, not a mangled/ghost node, got: {}",
        fqn_target.name
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

/// The resolver must **fall back to the controller Class** when the named method does not exist on the graph
/// (resolve.rs:880-889). Controllers frequently inherit actions from a base class, so requiring the exact
/// `Class::method` node to exist would break the whole route chain — the design choice is to land the
/// `HandledBy` edge on the Class node instead of inventing a ghost Method.
///
/// This is a *distinct* branch from `method_ref_never_synthesizes_ghost_nodes_on_miss`: there the class itself is
/// absent (so no edge at all), whereas here the class is present but the method is not (so an edge to the Class,
/// and still no ghost Method). The original suite never exercised this fall-back, so a regression that dropped it
/// — e.g. returning `None` whenever the method is missing — would have passed silently.
#[test]
fn method_ref_falls_back_to_class_when_method_absent() {
    let dir = synthetic_tp_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("graph build should succeed");
    };

    // `Login` (app\admin\controller\Login) exists with `login`/`logout`, but `/fallback` names `missingMethod`,
    // which is not on the graph. The edge must still be created — targeting the Class node.
    let target = handler_target_of(&b, "GET /fallback")
        .expect("a handler whose class exists must resolve, falling back to the Class node");
    assert_eq!(
        target.kind.as_str(),
        "Class",
        "when the method is absent the HandledBy edge must land on the Class node, not a ghost Method"
    );
    assert_eq!(
        target.name, "Login",
        "the fall-back must resolve to the real Login class (short name), not a ghost node, got: {}",
        target.name
    );

    // And crucially: no ghost `missingMethod` node is synthesized.
    let methods: Vec<String> = nodes_of_kind(&b, "Method").into_iter().map(|n| n.name).collect();
    assert!(
        !methods.iter().any(|m| m == "missingMethod"),
        "the missing method must not be invented as a ghost Method, got: {methods:?}"
    );
}
