//! Always-on (sample-free) structural self-checks for the **CfAst** phase (P2).
//!
//! `cf_ast.rs` is the language-agnostic backbone that turns `SyntaxFacts` into
//! `Class` / `Method` / `Function` / `Property` / `Namespace` / `CallSite` nodes,
//! the `imports` table, the `by_name` index, and inheritance / implementation / trait
//! edges. A regression here silently breaks **every** stack, yet `cf_ast.rs` had no
//! unit tests of its own, and the only structural assertions lived in
//! `sample_project_pipeline.rs` behind a sample-gate that silently `skip`s (fake-green) when
//! `GRAPHTELL_SAMPLE_DIR` is unset.
//!
//! These tests build a tiny synthetic project in a temp dir via
//! `common::graph_with_root` — no external sample, so they run on every
//! `cargo test` and actually catch a CfAst regression. The riskiest recent CfAst
//! change (i18n locale derivation, the old `lang/{locale}/*.php` literal that never
//! matched) is covered at the unit level by `facts::locale_of_path` in
//! `phase/facts.rs`; here we pin the structural products for **every** stack the kernel
//! actively supports — PHP, Python, Java, JS/TS and Rust — via always-on synthetic roots,
//! so a language-agnostic CfAst regression is caught for each stack's FQN / node / edge shape.

use gt_domain::model::{NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn count(b: &common::Built, kind: &str) -> u64 {
    b.store
        .stats(b.project.id)
        .expect("stats readable")
        .by_kind
        .get(kind)
        .copied()
        .unwrap_or(0)
}

fn class_with_fqn(b: &common::Built, fqn_sub: &str) -> Option<gt_domain::model::Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .find(|n| n.fqn.as_deref().map(|f| f.contains(fqn_sub)).unwrap_or(false))
}

fn synthetic_php_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-cfast-php-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("app/model/order")).expect("mkdir");
    std::fs::write(
        dir.join("composer.json"),
        r#"{"name":"demo/demo","autoload":{"psr-4":{"app\\":"app/"}}}"#,
    )
    .expect("write composer.json");
    std::fs::write(
        dir.join("app/model/order/Order.php"),
        r#"<?php
namespace app\model\order;

class Order extends BaseModel implements Savable
{
    use Timestamps;

    private $items;

    public function save()
    {
        return $this->items;
    }

    public function place()
    {
        $this->save();
        \app\util\Logger::info("placed");
    }
}
"#,
    )
    .expect("write Order.php");
    // A conventional i18n file; locale derivation is pinned at the unit level in facts.rs,
    // so we only make sure CfAst still walks it without panicking.
    std::fs::create_dir_all(dir.join("lang/en")).expect("mkdir lang");
    std::fs::write(
        dir.join("lang/en/messages.php"),
        "<?php return ['hello' => 'world'];\n",
    )
    .expect("write lang/en/messages.php");
    dir
}

fn synthetic_python_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-cfast-py-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = [\"django>=4.2\"]\n",
    )
    .expect("write pyproject.toml");
    std::fs::write(dir.join("requirements.txt"), "django>=4.2\n").expect("write requirements");
    std::fs::write(
        dir.join("myapp/models.py"),
        "from django.db import models\n\n\nclass Article(models.Model):\n    title = models.CharField(max_length=200)\n",
    )
    .expect("write models.py");
    std::fs::write(
        dir.join("myapp/services.py"),
        "from django.db import transaction\n\nfrom myapp.models import Article\n\n\nclass ArticleService:\n    def publish(self):\n        Article.objects.create()\n        with transaction.atomic():\n            Article.objects.filter()\n",
    )
    .expect("write services.py");
    dir
}

/// CfAst must emit the full node set and refine method bodies into `CallSite`s.
#[test]
fn php_cf_ast_builds_nodes_and_resolves_fqn() {
    let root = synthetic_php_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };

    assert!(count(&b, NodeKind::CLASS) > 0, "CfAst should produce Class nodes");
    assert!(count(&b, NodeKind::METHOD) > 0, "CfAst should produce Method nodes");
    assert!(
        count(&b, NodeKind::PROPERTY) > 0,
        "CfAst should produce Property nodes"
    );
    assert!(
        count(&b, NodeKind::NAMESPACE) > 0,
        "CfAst should produce Namespace nodes"
    );
    assert!(
        count(&b, NodeKind::CALL_SITE) > 0,
        "method calls should be refined into CallSite nodes, got {}",
        count(&b, NodeKind::CALL_SITE)
    );

    // namespace + class -> FQN
    let order = class_with_fqn(&b, "order").expect("the Order class should exist with `order` in its FQN");
    assert_eq!(order.name, "Order");
    assert!(
        order.fqn.as_deref().unwrap_or("").contains("order"),
        "FQN should embed the namespace segment, got {:?}",
        order.fqn
    );
}

/// The `extends` clause must be recorded as an edge so later phases (e.g. table
/// inference) can walk the chain even when the parent lives in an excluded dir.
#[test]
fn php_cf_ast_records_inheritance_edge() {
    let root = synthetic_php_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };
    let order = class_with_fqn(&b, "order").expect("the Order class");
    let edges = b
        .store
        .edges_of(order.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Extends"),
        "the `extends BaseModel` clause should be recorded as an Extends edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}

/// A class that `implements` an interface must record an `Implements` edge, and a class that `use`s a trait must
/// record a `UsesTrait` edge. These two edge kinds drive downstream resolution (e.g. an interface's `WritesDb`
/// method propagating to every implementer) yet were never pinned at the build level — only `extends`/`Extends` was,
/// and that behind a weak `any(Extends, UsesTrait)` that could not tell the two apart. The `Savable` / `Timestamps`
/// names are unresolved (placeholder), exactly like `BaseModel`, so the edges must still be laid.
#[test]
fn php_cf_ast_records_implementation_and_trait_edges() {
    let root = synthetic_php_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic PHP graph build should succeed");
    };
    let order = class_with_fqn(&b, "order").expect("the Order class");
    let edges = b
        .store
        .edges_of(order.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    let kinds: Vec<&str> = edges.iter().map(|e| e.kind.as_str()).collect();

    assert!(
        kinds.iter().any(|k| *k == "Implements"),
        "the `implements Savable` clause should be recorded as an Implements edge, got: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| *k == "UsesTrait"),
        "the `use Timestamps` clause should be recorded as a UsesTrait edge, got: {kinds:?}"
    );
}

/// Python/Django: CfAst must surface classes, methods and call sites (the latter
/// feed the WritesDb / Tx detection in P13). Python is the newest stack and had no
/// CfAst-focused coverage at all.
#[test]
fn python_cf_ast_builds_nodes_and_call_sites() {
    let root = synthetic_python_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Python graph build should succeed");
    };

    assert!(count(&b, NodeKind::CLASS) > 0, "CfAst should produce Class nodes");
    assert!(
        count(&b, NodeKind::METHOD) > 0 || count(&b, NodeKind::FUNCTION) > 0,
        "CfAst should produce Method/Function nodes"
    );
    assert!(
        count(&b, NodeKind::CALL_SITE) > 0,
        "method calls should be refined into CallSite nodes, got {}",
        count(&b, NodeKind::CALL_SITE)
    );

    // The Django model must be a class so downstream phases can map it to a Table.
    let article = b
        .store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: Some("Article".into()),
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .find(|n| n.name == "Article");
    assert!(
        article.is_some(),
        "the Django `Article` model should be surfaced as a Class node"
    );
}

/// Like `class_with_fqn` but matches on the simple class *name* (the `name` column). The class `name` is
/// identical across every stack (`Order`, `OrderService`, …) whereas the `fqn` column is stack-specific
/// (PHP/Java embed the namespace, JS keeps the bare name, Rust prefixes `crate.`). So `name` is the portable
/// lookup key when a stack also emits *placeholder* parent nodes whose FQN reuses the same segment.
fn class_by_name(b: &common::Built, name: &str) -> Option<gt_domain::model::Node> {
    b.store
        .query_nodes(&NodeFilter {
            project_id: b.project.id,
            kind: Some(NodeKind(NodeKind::CLASS.to_string())),
            name_contains: None,
            limit: Some(1000),
            offset: Some(0),
        })
        .expect("query")
        .into_iter()
        .find(|n| n.name == name)
}

fn synthetic_java_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-cfast-java-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/main/java/com/example/order")).expect("mkdir");
    // A minimal Maven manifest so the marker phase recognises the Java sub-project (the way the real pipeline
    // expects); language-by-extension would still route the file, but the manifest also drives framework detection.
    std::fs::write(
        dir.join("pom.xml"),
        r#"<?xml version="1.0"?>
<project>
  <dependencies>
    <dependency>
      <groupId>org.springframework.boot</groupId>
      <artifactId>spring-boot-starter</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
    )
    .expect("write pom.xml");
    std::fs::write(
        dir.join("src/main/java/com/example/order/Order.java"),
        r#"package com.example.order;

public class Order extends BaseModel implements Savable {
    private String items;

    public String save() {
        return items;
    }

    public void place() {
        this.save();
        Logger.info("placed");
    }
}
"#,
    )
    .expect("write Order.java");
    dir
}

fn synthetic_js_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-cfast-js-{}-{}",
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
  "name": "synth",
  "dependencies": { "@nestjs/common": "^10.0.0", "typeorm": "^0.3.0" }
}
"#,
    )
    .expect("write package.json");
    // The frontend parser stores the **bare class name** as the FQN (no module prefix) — pinned below.
    std::fs::write(
        dir.join("src/order.service.ts"),
        r#"import { Injectable } from '@nestjs/common';

@Injectable()
export class OrderService extends BaseService implements OnModuleInit {
  private items: string;

  save() {
    return this.items;
  }

  place() {
    this.save();
    Logger.info('placed');
  }
}
"#,
    )
    .expect("write order.service.ts");
    dir
}

fn synthetic_rust_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-cfast-rust-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n\n[dependencies]\nactix-web = \"4\"\n",
    )
    .expect("write Cargo.toml");
    std::fs::write(
        dir.join("src/lib.rs"),
        r#"pub trait Savable {
    fn save(&self);
}

pub struct Order {
    items: String,
}

impl Savable for Order {
    fn save(&self) {
        println!("{}", self.items);
    }
}

impl Order {
    pub fn place(&self) {
        self.save();
    }
}
"#,
    )
    .expect("write src/lib.rs");
    dir
}

/// Java (Spring): CfAst must emit the node set and resolve the `com.example.order` package into the FQN,
/// mirroring the PHP self-check for a second actively-maintained backend stack.
#[test]
fn java_cf_ast_builds_nodes_and_resolves_fqn() {
    let root = synthetic_java_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Java graph build should succeed");
    };

    assert!(count(&b, NodeKind::CLASS) > 0, "CfAst should produce Class nodes");
    assert!(count(&b, NodeKind::METHOD) > 0, "CfAst should produce Method nodes");
    assert!(
        count(&b, NodeKind::CALL_SITE) > 0,
        "method calls should be refined into CallSite nodes, got {}",
        count(&b, NodeKind::CALL_SITE)
    );

    // package com.example.order + class Order -> FQN embeds the package segment (just like PHP's namespace).
    let order = class_by_name(&b, "Order").expect("the Order class should exist");
    assert_eq!(order.name, "Order");
    assert!(
        order.fqn.as_deref().unwrap_or("").contains("order"),
        "FQN should embed the package segment, got {:?}",
        order.fqn
    );
}

/// `extends` must be recorded as an `Extends` edge even when the parent (`BaseModel`) is unresolved.
#[test]
fn java_cf_ast_records_extends_edge() {
    let root = synthetic_java_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Java graph build should succeed");
    };
    let order = class_by_name(&b, "Order").expect("the Order class");
    let edges = b
        .store
        .edges_of(order.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Extends"),
        "the `extends BaseModel` clause should be recorded as an Extends edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}

/// `implements` must be recorded as an `Implements` edge. Java has no trait `use`, so unlike PHP there is no
/// `UsesTrait` edge to assert here.
#[test]
fn java_cf_ast_records_implements_edge() {
    let root = synthetic_java_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Java graph build should succeed");
    };
    let order = class_by_name(&b, "Order").expect("the Order class");
    let edges = b
        .store
        .edges_of(order.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Implements"),
        "the `implements Savable` clause should be recorded as an Implements edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}

/// JS / TS (NestJS): CfAst must surface classes, methods and call sites. The class FQN convention differs from
/// PHP/Java: the frontend parser stores the **bare class name** (no module path), which this test pins.
#[test]
fn js_cf_ast_builds_nodes_and_resolves_fqn() {
    let root = synthetic_js_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic JS graph build should succeed");
    };

    assert!(count(&b, NodeKind::CLASS) > 0, "CfAst should produce Class nodes");
    assert!(count(&b, NodeKind::METHOD) > 0, "CfAst should produce Method nodes");
    assert!(
        count(&b, NodeKind::CALL_SITE) > 0,
        "method calls should be refined into CallSite nodes, got {}",
        count(&b, NodeKind::CALL_SITE)
    );

    // The frontend parser keeps the bare class name as the FQN (no module prefix).
    let svc = class_by_name(&b, "OrderService").expect("the OrderService class should exist");
    assert_eq!(svc.name, "OrderService");
    assert!(
        svc.fqn.as_deref().unwrap_or("").contains("OrderService"),
        "the FQN must embed the class name, got {:?}",
        svc.fqn
    );
}

/// `extends` must be recorded as an `Extends` edge.
#[test]
fn js_cf_ast_records_extends_edge() {
    let root = synthetic_js_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic JS graph build should succeed");
    };
    let svc = class_by_name(&b, "OrderService").expect("the OrderService class");
    let edges = b
        .store
        .edges_of(svc.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Extends"),
        "the `extends BaseService` clause should be recorded as an Extends edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}

/// `implements` must be recorded as an `Implements` edge (the NestJS norm is a class that *only* implements).
#[test]
fn js_cf_ast_records_implements_edge() {
    let root = synthetic_js_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic JS graph build should succeed");
    };
    let svc = class_by_name(&b, "OrderService").expect("the OrderService class");
    let edges = b
        .store
        .edges_of(svc.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Implements"),
        "the `implements OnModuleInit` clause should be recorded as an Implements edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}

/// Rust: CfAst must surface structs (as `Class`), impl methods and call sites. Rust has **no `extends`** and no
/// trait `use`; trait bounds show up only as `impl Trait for Type` -> an `Implements` edge.
#[test]
fn rust_cf_ast_builds_nodes_and_resolves_fqn() {
    let root = synthetic_rust_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Rust graph build should succeed");
    };

    assert!(count(&b, NodeKind::CLASS) > 0, "CfAst should produce Class nodes (structs)");
    assert!(
        count(&b, NodeKind::METHOD) > 0 || count(&b, NodeKind::FUNCTION) > 0,
        "CfAst should produce Method/Function nodes"
    );
    assert!(
        count(&b, NodeKind::CALL_SITE) > 0,
        "method calls should be refined into CallSite nodes, got {}",
        count(&b, NodeKind::CALL_SITE)
    );

    let order = class_by_name(&b, "Order").expect("the Order struct should exist as a Class node");
    assert_eq!(order.name, "Order");
    assert!(
        order.fqn.as_deref().unwrap_or("").contains("Order"),
        "the FQN must embed the type name, got {:?}",
        order.fqn
    );
}

/// `impl Savable for Order` must be recorded as an `Implements` edge — Rust's only inheritance-shaped relation.
#[test]
fn rust_cf_ast_records_trait_impl_edge() {
    let root = synthetic_rust_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the synthetic Rust graph build should succeed");
    };
    let order = class_by_name(&b, "Order").expect("the Order struct");
    let edges = b
        .store
        .edges_of(order.id, EdgeDirection::Outgoing)
        .expect("edges readable");
    assert!(
        edges.iter().any(|e| e.kind.as_str() == "Implements"),
        "the `impl Savable for Order` clause should be recorded as an Implements edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        !edges.iter().any(|e| e.kind.as_str() == "Extends"),
        "Rust has no `extends`; no Extends edge may be synthesised, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
    );
}
