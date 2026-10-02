//! End-to-end self-check of Flask (Python) route contracts.
//!
//! One goal is verifying that "the Nth framework of a language is nearly free": this case needs no new parser
//! capability, only `fkb/python/flask.yaml` plus the existing Python parser (a decorator is a call site).

use gt_domain::model::{Node, NodeKind, ProjectConfig};
use gt_domain::port::{EdgeDirection, GraphQuery, NodeFilter};

mod common;

fn synthetic_flask_root() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-flask-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");

    // pyproject.toml is both the sub-project marker (-> python) and the detector's manifest;
    // note that it deliberately **excludes** fastapi so that FastAPI's FKB does not match too.
    std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"demo\"\ndependencies = [\"flask>=2.3\"]\n")
        .expect("write pyproject");
    std::fs::write(dir.join("requirements.txt"), "flask>=2.3\n").expect("write requirements");

    std::fs::write(
        dir.join("app.py"),
        r#"from flask import Flask

app = Flask(__name__)


@app.route("/")
def index():
    return "ok"


@app.route("/orders", methods=["POST"])
def create_order():
    return "ok"


@app.get("/health")
def health():
    return "ok"
"#,
    )
    .expect("write app.py");

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

/// (contract node name -> the handler function name connected via HandledBy)
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
fn flask_routes_produce_http_contracts_linked_to_handlers() {
    let root = synthetic_flask_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Flask project should succeed");
    };
    let pairs = contract_targets(&b);
    assert!(
        pairs.len() >= 3,
        "all three routes should become HttpContracts, got: {pairs:?}"
    );

    // `@app.route("/")` — no methods declared: Flask responds to GET only by default,
    // consistent with the engine's GET fallback for ContractId
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /" && t == "index"),
        "expected GET / -> index, got: {pairs:?}"
    );
    // `@app.route("/orders", methods=["POST"])` — methods= is decoded, the method is the first item
    assert!(
        pairs.iter().any(|(c, t)| c == "POST /orders" && t == "create_order"),
        "expected POST /orders -> create_order, got: {pairs:?}"
    );
    // `@app.get("/health")` — a verb shortcut; the method is derived from the decorator name
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /health" && t == "health"),
        "expected GET /health -> health, got: {pairs:?}"
    );
}
