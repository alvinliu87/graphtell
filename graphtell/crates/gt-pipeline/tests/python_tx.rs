//! End-to-end self-check that P13 Tx (**multi-table write without a transaction boundary**) fires on a
//! Python (Django) sample — proving the kernel's language-agnostic Tx rule serves a third stack, not
//! just PHP and Java. The transaction markers come from `fkb/python/django.yaml`'s `tx_calls:` (FKB
//! vocabulary); the `WritesDb` edges come from that file's `db_verbs:` plus the Python analyzer's
//! `Model.objects.create(...)` / `self.save()` resolution (via the shared `variable_type` resolver and
//! the `models.Model` -> `Table` `MapsTo` edge).
//!
//! `place_bad` writes two tables (order + order_item) with no transaction boundary -> must be flagged.
//! `place_in_tx` wraps the same two writes in `with transaction.atomic():` -> must NOT be flagged.

use gt_domain::model::{Annotation, Node, NodeKind, ProjectConfig};
use gt_domain::port::{GraphQuery, NodeFilter};

mod common;

fn synthetic_django_tx_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-python-tx-{}-{}",
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

    std::fs::write(
        dir.join("myapp/models.py"),
        r#"from django.db import models


class Order(models.Model):
    pass


class OrderItem(models.Model):
    pass
"#,
    )
    .expect("write models.py");

    std::fs::write(
        dir.join("myapp/services.py"),
        r#"from django.db import transaction

from myapp.models import Order, OrderItem


class OrderService:
    # No transaction boundary: writes two tables (order + order_item) with nothing opening a transaction.
    def place_bad(self):
        Order.objects.create()
        OrderItem.objects.create()

    # Has a transaction boundary (`with transaction.atomic():`) around the same two writes.
    def place_in_tx(self):
        with transaction.atomic():
            Order.objects.create()
            OrderItem.objects.create()
"#,
    )
    .expect("write services.py");

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

fn annotations_of_kind(b: &common::Built, kind: &str) -> Vec<Annotation> {
    let all = b
        .store
        .annotations_of_project(b.project.id)
        .expect("annotations");
    all.values()
        .flatten()
        .filter(|a| a.kind == kind)
        .cloned()
        .collect()
}

#[test]
fn python_multi_write_without_tx_is_flagged() {
    let dir = synthetic_django_tx_root();
    let Some(b) = common::graph_with_root(&dir, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Django project should succeed");
    };

    // Sanity: the two tables the service writes to must exist (model -> Table via `MapsTo`).
    // The `model-table` rule emits the singular snake_case class short name (`Order` -> `order`).
    let tables = nodes_of_kind(&b, "Table");
    assert!(
        tables
            .iter()
            .any(|t| t.name.eq_ignore_ascii_case("order"))
            && tables
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case("order_item")),
        "expected order + order_item tables to be synthesised, got: {:?}",
        tables.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let hits = annotations_of_kind(&b, "multi-write-without-tx");
    assert!(
        !hits.is_empty(),
        "P13 应在 Django 样本上触发 multi-write-without-tx（place_bad 写两张表且无事务边界），实际无任何命中"
    );
    assert!(
        hits.iter().any(|a| a
            .evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("place_bad"))
            .unwrap_or(false)),
        "被标注的方法应为 place_bad（无事务边界），实际 owner_fqns：{:?}",
        hits.iter()
            .map(|a| a.evidence.get("owner_fqn"))
            .collect::<Vec<_>>()
    );
    assert!(
        !hits.iter().any(|a| a
            .evidence
            .get("owner_fqn")
            .and_then(|v| v.as_str())
            .map(|s| s.contains("place_in_tx"))
            .unwrap_or(false)),
        "place_in_tx 已被 transaction.atomic 包裹，不应被标注"
    );
    eprintln!("Django multi-write-without-tx hits = {}", hits.len());
}
