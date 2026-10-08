//! End-to-end self-check that a **Django** (Python) project also surfaces environment variables as `ConfigKey`
//! nodes. This locks in the improvement made by moving the `os.environ` / `os.getenv` recognition out of
//! `fastapi.yaml` into a framework-independent `fkb/python/common.yaml` (`apply_without_detection`,
//! `language: python`). Before that move, a Django project — which does not detect FastAPI — would never have
//! produced a `ConfigKey` from env reads, because the rule lived behind FastAPI's detectors.

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_django_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-django-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("myapp")).expect("mkdir");

    // pyproject.toml is both the sub-project marker (-> language=python) and the detector's manifest;
    // it declares `django` so that `fkb/python/django.yaml` is recognised. It deliberately does NOT name
    // fastapi, which is exactly the scenario this test protects: a Django project is not a FastAPI project.
    std::fs::write(
        dir.join("pyproject.toml"),
        "[project]\nname = \"demo\"\ndependencies = [\"django>=4.2\"]\n",
    )
    .expect("write pyproject");
    std::fs::write(dir.join("requirements.txt"), "django>=4.2\n").expect("write requirements");

    // Django settings: env reads are the thing under test (both supported forms).
    std::fs::write(
        dir.join("settings.py"),
        r#"import os

SECRET_KEY = os.environ.get("SECRET_KEY", "dev")
DATABASES = {"default": {"HOST": os.getenv("DB_HOST", "localhost")}}
"#,
    )
    .expect("write settings.py");

    // A minimal Django model, to confirm Django itself is detected (a model -> Table),
    // so this is a genuine Django test rather than a bare-Python one.
    std::fs::write(
        dir.join("myapp/models.py"),
        r#"from django.db import models


class Article(models.Model):
    title = models.CharField(max_length=200)
"#,
    )
    .expect("write models.py");

    // A module that reads and writes through the Django ORM. This exercises fkb/python/django.yaml's
    // `db_verbs` + `variable_type` resolver (`django-orm-call`) wiring — the Django analogue of the Java JPA
    // ReadsDb/WritesDb pin in library_detection.rs, and never covered end-to-end here.
    std::fs::write(
        dir.join("myapp/views.py"),
        r#"from myapp.models import Article


def fetch_article(id):
    a = Article.objects.get(id=id)
    return Article.objects.create(title="new")
"#,
    )
    .expect("write views.py");

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

fn has_incoming_edge(b: &common::Built, kind: &str, edge: &str) -> bool {
    nodes_of_kind(b, kind).iter().any(|n| {
        b.store
            .edges_of(n.id, EdgeDirection::Incoming)
            .expect("edges")
            .iter()
            .any(|e| e.kind.as_str() == edge)
    })
}

/// Whether a `Function` node of the given name carries an outgoing edge of `edge` to some target.
fn fn_has_outgoing_edge(b: &common::Built, name: &str, edge: &str) -> bool {
    nodes_of_kind(b, "Function").iter().any(|n| {
        n.name == name
            && b
                .store
                .edges_of(n.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .any(|e| e.kind.as_str() == edge)
    })
}

/// `os.environ.get` / `os.getenv` in a Django project must produce `ConfigKey` + `ReadsConfig`,
/// even though Django does not detect FastAPI.
#[test]
fn django_env_config_produces_configkey() {
    let root = synthetic_django_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Django project should succeed");
    };

    // Django must be recognised (a model -> Table), otherwise this isn't a real Django test.
    let tables: Vec<String> = nodes_of_kind(&b, "Table").iter().map(|n| n.name.clone()).collect();
    assert!(
        !tables.is_empty(),
        "a Django model must be recognised and produce a Table node, got: {tables:?}"
    );

    // Both env-read forms become ConfigKey nodes.
    let cfg: Vec<String> = nodes_of_kind(&b, "ConfigKey").iter().map(|n| n.name.clone()).collect();
    assert!(
        cfg.iter().any(|n| n == "SECRET_KEY"),
        "os.environ.get must produce the SECRET_KEY config node, got: {cfg:?}"
    );
    assert!(
        cfg.iter().any(|n| n == "DB_HOST"),
        "os.getenv must produce the DB_HOST config node, got: {cfg:?}"
    );
    assert!(
        has_incoming_edge(&b, "ConfigKey", "ReadsConfig"),
        "the ConfigKey must have a ReadsConfig in-edge"
    );

    // Precision: only the *key* argument (arg 0, `require_literal`) becomes a ConfigKey — the default value
    // (arg 1) must not. `os.environ.get("SECRET_KEY", "dev")` would otherwise create a spurious `dev` node.
    assert!(
        !cfg.iter().any(|n| n == "dev" || n == "localhost"),
        "default values must not become ConfigKey nodes, got: {cfg:?}"
    );
}

/// Django ORM calls (`Article.objects.get` / `Article.objects.create`) must resolve — via django.yaml's
/// `variable_type` resolver and `db_verbs` — to `ReadsDb` / `WritesDb` edges on the `Article` table. This is
/// the Django analogue of the Java JPA ReadsDb/WritesDb pin and was previously untested end-to-end.
#[test]
fn django_orm_reads_and_writes_produce_db_edges() {
    let root = synthetic_django_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Django project should succeed");
    };

    // The model must be a Table, or the ORM wiring has nothing to resolve the receiver against.
    let tables: Vec<String> = nodes_of_kind(&b, "Table").iter().map(|n| n.name.clone()).collect();
    assert!(
        !tables.is_empty(),
        "the Article model must be a Table for the ORM resolver to land on, got: {tables:?}"
    );

    // The function reads through the ORM (`Article.objects.get`) -> a ReadsDb edge, and writes through it
    // (`Article.objects.create`) -> a WritesDb edge.
    assert!(
        fn_has_outgoing_edge(&b, "fetch_article", "ReadsDb"),
        "Article.objects.get must yield a ReadsDb edge on the fetch_article function"
    );
    assert!(
        fn_has_outgoing_edge(&b, "fetch_article", "WritesDb"),
        "Article.objects.create must yield a WritesDb edge on the fetch_article function"
    );
    // And both must resolve onto a Table (the ORM resolver lands the edge on the Article table), not just
    // "a DB edge exists somewhere".
    assert!(
        has_incoming_edge(&b, "Table", "ReadsDb"),
        "the ReadsDb edge must land on the Article Table"
    );
    assert!(
        has_incoming_edge(&b, "Table", "WritesDb"),
        "the WritesDb edge must land on the Article Table"
    );
}
