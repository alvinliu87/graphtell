//! End-to-end self-check of the Node.js framework (TypeScript) route contracts.
//!
//! Covers the chain "decorator -> call site -> HttpContract + HandledBy", and pins one deliberate behaviour:
//! **a decorator with no path argument (`@Get()`) builds no contract** (if the path cannot be obtained, no contract
//! is built — better missing than guessed, otherwise the contract bridge gets polluted).
//!
//! Real sample (not in CI): the `node_a` checkout (`samples/**/node_a`)
//! Measured: all 17 route contracts connect to a handler (`http-contract-without-handler` matched 0 times).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_node_a_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-node-a-{}-{}",
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
        r#"import { Controller, Get, Post, Put, Delete, Patch } from '@nestjs/common';

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

  @Put(':id')
  updateOne() {
    return null;
  }

  @Delete(':id')
  removeOne() {
    return null;
  }

  @Patch(':id')
  patchOne() {
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
fn node_a_decorators_produce_contracts_linked_to_handlers() {
    let root = synthetic_node_a_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Node framework project should succeed");
    };
    let pairs = contract_targets(&b);

    // Only the decorators that carry a path build a contract; `@Get()` (listAll) is deliberately dropped because
    // no path can be obtained. That is the key invariant — better missing than guessed, or the contract bridge
    // gets polluted — so assert it explicitly rather than only via the count.
    assert!(
        !pairs.iter().any(|(_, t)| t == "listAll"),
        "a `@Get()` with no path argument must build no contract, got: {pairs:?}"
    );

    // Every REST verb the FKB declares (`@Get/@Post/@Put/@Delete/@Patch`, see fkb/js/nestjs.yaml) must produce its
    // own contract linked to its handler. The original test pinned only GET + POST, so a regression dropping
    // `@Put`/`@Delete`/`@Patch` from the FKB would have passed silently.
    assert_eq!(
        pairs.len(),
        5,
        "expected exactly 5 contracts (the five path-bearing decorators), got: {pairs:?}"
    );
    let want: &[( &str, &str )] = &[
        ("GET /:id", "findOne"),
        ("POST /bulk", "createMany"),
        ("PUT /:id", "updateOne"),
        ("DELETE /:id", "removeOne"),
        ("PATCH /:id", "patchOne"),
    ];
    for (c, t) in want {
        assert!(
            pairs.iter().any(|(cc, tt)| cc == c && tt == t),
            "expected {c} -> {t}, got: {pairs:?}"
        );
    }

    // Ownership: each contract is a backend contract (no frontend caller in this sample), so it must be tagged
    // `side = backend` — the same invariant `link_sample` pins for backend-only endpoints.
    let backend_count = nodes_of_kind(&b, "HttpContract")
        .iter()
        .filter(|n| n.properties.get("side").and_then(|v| v.as_str()) == Some("backend"))
        .count();
    assert_eq!(
        backend_count,
        pairs.len(),
        "every contract must be owned by the backend sub-project (side = backend), got: {:?}",
        nodes_of_kind(&b, "HttpContract")
            .iter()
            .map(|n| (n.name.clone(), n.properties.get("side").cloned()))
            .collect::<Vec<_>>()
    );
}
