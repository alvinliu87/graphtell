//! Symfony 路由（PHP 8 属性 `#[Route]` / `#[Get]` …）端到端自检。
//!
//! 覆盖「控制器方法上的路由属性 → `HttpContract`（method + path）+ `HandledBy` 连到
//! 控制器方法」。用合成工程（无需外部样本）。

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

/// 合成 Symfony 工程：`composer.json`（含 symfony 依赖）+ 一个带路由属性的控制器。
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
        panic!("建图应成功");
    };



    // 期望契约：每个 (方法, 路径) 一条
    //   GET /api/users
    //   GET /api/users/{id}   POST /api/users/{id}   （Route 带 methods 数组 → 拆两条）
    //   GET /api/ping         （#[Get] 快捷属性）
    //   POST /api/echo        （#[Post] 快捷属性）
    let contracts: Vec<Node> = nodes_of_kind(&b, "HttpContract");
    let names: Vec<&str> = contracts.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(contracts.len(), 5, "应恰好 5 条契约，实际：{names:?}");
    for want in [
        "GET /api/users",
        "GET /api/users/{id}",
        "POST /api/users/{id}",
        "GET /api/ping",
        "POST /api/echo",
    ] {
        assert!(
            names.iter().any(|n| *n == want),
            "缺少契约 {want:?}，实际：{names:?}"
        );
    }

    // HandledBy：每条契约连到对应控制器方法
    let handler_of = |name: &str| -> String {
        let c = contracts.iter().find(|n| n.name == name).expect("契约");
        let hs: Vec<Node> = b
            .store
            .edges_of(c.id, EdgeDirection::Outgoing)
            .expect("edges")
            .iter()
            .filter(|e| e.kind.as_str() == "HandledBy")
            .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
            .collect();
        assert!(!hs.is_empty(), "{name} 应有 HandledBy 目标");
        hs[0].name.clone()
    };

    assert_eq!(handler_of("GET /api/users"), "listUsers");
    assert_eq!(handler_of("GET /api/users/{id}"), "show");
    assert_eq!(handler_of("POST /api/users/{id}"), "show");
    assert_eq!(handler_of("GET /api/ping"), "ping");
    assert_eq!(handler_of("POST /api/echo"), "echo");
}
