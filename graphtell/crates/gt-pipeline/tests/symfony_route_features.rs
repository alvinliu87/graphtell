//! End-to-end self-check of Symfony routes (PHP 8 attributes `#[Route]` / `#[Get]` …).
//!
//! Covers "a route attribute on a controller method -> an `HttpContract` (method + path) + a `HandledBy` edge to
//! the controller method". Uses a synthetic project (no external sample needed).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// A synthetic Symfony project: a `composer.json` (with the symfony dependency) plus one controller carrying route attributes.
fn synthetic_symfony_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-symfony-routes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/Controller")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{
  "name": "demo/app",
  "require": { "symfony/framework-bundle": "^6.4" }
}
"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("src/Controller/UserController.php"),
        r#"<?php
namespace App\Controller;

use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Annotation\Route;

class UserController
{
    #[Route('/api/users', methods: ['GET'])]
    public function listUsers(): Response
    {
        return new Response();
    }

    #[Route('/api/users/{id}', methods: ['GET', 'POST'])]
    public function show(int $id): Response
    {
        return new Response();
    }

    #[Get('/api/ping')]
    public function ping(): Response
    {
        return new Response();
    }

    #[Post('/api/echo')]
    public function echo(): Response
    {
        return new Response();
    }
}
"#,
    )
    .expect("write controller");

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

#[test]
fn symfony_route_attributes_become_http_contracts() {
    let dir = synthetic_symfony_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };



    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(contracts.len(), 5, "expected exactly 5 contracts, got: {names:?}");
    for want in [
        "GET /api/users",
        "GET /api/users/{id}",
        "POST /api/users/{id}",
        "GET /api/ping",
        "POST /api/echo",
    ] {
        assert!(
            names.iter().any(|n| *n == want),
            "missing contract {want:?}, got: {names:?}"
        );
    }

    // HandledBy: every contract connects to the corresponding controller method
    let handler_of = |name: &str| -> String {
        let c = contracts.iter().find(|n| n.name == name).expect("contract");
        let hs: Vec<Node> = b
            .store
            .edges_of(c.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .filter(|e| e.kind.as_str() == "HandledBy")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .collect();
        assert!(!hs.is_empty(), "{name} should have a HandledBy target");
        hs[0].name.clone()
    };

    assert_eq!(handler_of("GET /api/users"), "listUsers");
    assert_eq!(handler_of("GET /api/users/{id}"), "show");
    assert_eq!(handler_of("POST /api/users/{id}"), "show");
    assert_eq!(handler_of("GET /api/ping"), "ping");
    assert_eq!(handler_of("POST /api/echo"), "echo");
}

/// A second synthetic Symfony controller exercising the features that the basic controller above does not pin:
///  - a `#[Route]` with **no `methods:`** must degrade to the wildcard `ANY` contract (php.rs degrades an empty
///    method list to `ANY`), and
///  - the remaining HTTP-method shortcut attributes `#[Put]` / `#[Delete]` / `#[Patch]` (the parser recognises all
///    eight of Route/Get/Post/Put/Delete/Patch/Options/Head, but the basic controller only exercised Get/Post).
fn synthetic_symfony_root_extra() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-symfony-routes-extra-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/Controller")).expect("mkdir");

    std::fs::write(
        dir.join("composer.json"),
        r#"{
  "name": "demo/app",
  "require": { "symfony/framework-bundle": "^6.4" }
}
"#,
    )
    .expect("write composer.json");

    std::fs::write(
        dir.join("src/Controller/ExtraController.php"),
        r#"<?php
namespace App\Controller;

use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Annotation\Route;

class ExtraController
{
    // No `methods:` -> must degrade to the wildcard `ANY` contract.
    #[Route('/api/default')]
    public function wildcard(): Response
    {
        return new Response();
    }

    #[Put('/api/users/{id}')]
    public function replace(int $id): Response
    {
        return new Response();
    }

    #[Delete('/api/users/{id}')]
    public function remove(int $id): Response
    {
        return new Response();
    }

    #[Patch('/api/users/{id}')]
    public function patch(int $id): Response
    {
        return new Response();
    }

    #[Options('/api/users/{id}')]
    public function options(int $id): Response
    {
        return new Response();
    }

    #[Head('/api/users/{id}')]
    public function head(int $id): Response
    {
        return new Response();
    }
}
"#,
    )
    .expect("write controller");

    dir
}

#[test]
fn symfony_route_default_method_is_any_and_shortcuts_covered() {
    let dir = synthetic_symfony_root_extra();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build should succeed");
    };

    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    for want in [
        "ANY /api/default",
        "PUT /api/users/{id}",
        "DELETE /api/users/{id}",
        "PATCH /api/users/{id}",
        "OPTIONS /api/users/{id}",
        "HEAD /api/users/{id}",
    ] {
        assert!(
            names.iter().any(|n| *n == want),
            "missing contract {want:?}, got: {names:?}"
        );
    }

    // HandledBy: every contract connects back to its controller method.
    let handler_of = |name: &str| -> String {
        let c = contracts.iter().find(|n| n.name == name).expect("contract");
        let hs: Vec<Node> = b
            .store
            .edges_of(c.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .filter(|e| e.kind.as_str() == "HandledBy")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .collect();
        assert!(!hs.is_empty(), "{name} should have a HandledBy target");
        hs[0].name.clone()
    };

    assert_eq!(handler_of("ANY /api/default"), "wildcard");
    assert_eq!(handler_of("PUT /api/users/{id}"), "replace");
    assert_eq!(handler_of("DELETE /api/users/{id}"), "remove");
    assert_eq!(handler_of("PATCH /api/users/{id}"), "patch");
    assert_eq!(handler_of("OPTIONS /api/users/{id}"), "options");
    assert_eq!(handler_of("HEAD /api/users/{id}"), "head");
}
