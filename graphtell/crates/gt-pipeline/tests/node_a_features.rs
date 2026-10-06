//! End-to-end self-check of NestJS (Node / TypeScript) route contracts.
//!
//! Covers the chain "decorator -> call site -> HttpContract + HandledBy", and pins one deliberate behaviour:
//! **a decorator with no path argument (`@Get()`) builds no contract** (if the path cannot be obtained, no contract
//! is built — better missing than guessed, otherwise the contract bridge gets polluted).
//!
//! Real sample (not in CI): `samples/node-projects/nestjs/nestjs-realworld-example-app`
//! Measured: all 17 route contracts connect to a handler (`http-contract-without-handler` matched 0 times).

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

/// (contract node name -> the handler method name connected via HandledBy)
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
        panic!("the graph build of the synthetic NestJS project should succeed");
    };
    let pairs = contract_targets(&b);

    // Only the two with a path argument build a contract; `@Get()` is deliberately dropped because no path can be obtained
    assert_eq!(
        pairs.len(),
        2,
        "expected only 2 contracts (a `@Get()` with no path builds none), got: {pairs:?}"
    );
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /:id" && t == "findOne"),
        "expected GET /:id -> findOne, got: {pairs:?}"
    );
    assert!(
        pairs.iter().any(|(c, t)| c == "POST /bulk" && t == "createMany"),
        "expected POST /bulk -> createMany, got: {pairs:?}"
    );
}
