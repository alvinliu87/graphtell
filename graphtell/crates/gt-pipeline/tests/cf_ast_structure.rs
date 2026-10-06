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
//! `phase/facts.rs`; here we pin the structural products for the two stacks we
//! actively maintain (PHP and Python/Django).

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

class Order extends BaseModel
{
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
        edges
            .iter()
            .any(|e| e.kind.as_str() == "Extends" || e.kind.as_str() == "UsesTrait"),
        "the `extends BaseModel` clause should be recorded as an Extends edge, got: {:?}",
        edges
            .iter()
            .map(|e| e.kind.to_string())
            .collect::<Vec<_>>()
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
