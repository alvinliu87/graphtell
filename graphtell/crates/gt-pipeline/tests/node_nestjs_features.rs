//! NestJS（Node / TypeScript）路由契约的端到端自检。
//!
//! 覆盖「装饰器 → 调用点 → HttpContract + HandledBy」这条链，
//! 并锁定一个刻意的行为：**无路径实参的装饰器（`@Get()`）不建契约**
//! （取不到路径就不建，宁可缺不可猜，否则会污染契约桥）。
//!
//! 真实样本（不在 CI）：`samples/node-projects/nestjs/nestjs-realworld-example-app`
//! 实测 17 条路由契约全部连上 handler（`http-contract-without-handler` 0 命中）。

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_nestjs_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-nestjs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");

    std::fs::write(
        dir.join("package.json"),
        r#"{
  "name": "cats",
  "dependencies": { "@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0" }
}
"#,
    )
    .expect("write package.json");

    std::fs::write(
        dir.join("src/cats.controller.ts"),
        r#"import { Controller, Get, Post } from '@nestjs/common';

@Controller('cats')
export class CatsController {
  @Get(':id')
  findOne() {
    return null;
  }

  @Post('bulk')
  createMany() {
    return null;
  }

  @Get()
  listAll() {
    return null;
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

/// （契约节点名 → HandledBy 连到的处理方法名）
fn contract_targets(b: &common::Built) -> Vec<(String, String)> {
    nodes_of_kind(b, "HttpContract")
        .iter()
        .map(|n| {
            let target: String = b
                .store
                .edges_of(n.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .filter(|e| e.kind.as_str() == "HandledBy")
                .filter_map(|e| b.store.get_node(e.to_id).ok().flatten())
                .map(|t| t.name.clone())
                .next()
                .unwrap_or_default();
            (n.name.clone(), target)
        })
        .collect()
}

#[test]
fn nestjs_decorators_produce_contracts_linked_to_handlers() {
    let root = synthetic_nestjs_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("合成 NestJS 工程建图应成功");
    };
    let pairs = contract_targets(&b);

    // 只有带路径实参的两条建契约；`@Get()` 因取不到路径被刻意丢弃
    assert_eq!(
        pairs.len(),
        2,
        "应只建 2 条契约（`@Get()` 无路径不建），实际：{pairs:?}"
    );
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /:id" && t == "findOne"),
        "应产出 GET /:id → findOne，实际：{pairs:?}"
    );
    assert!(
        pairs.iter().any(|(c, t)| c == "POST /bulk" && t == "createMany"),
        "应产出 POST /bulk → createMany，实际：{pairs:?}"
    );
}
