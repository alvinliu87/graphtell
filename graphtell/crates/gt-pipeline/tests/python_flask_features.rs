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
        r#"from flask import Flask, Blueprint

app = Flask(__name__)

# Blueprint: Flask sub-applications register routes on a `Blueprint` object (receiver `bp`), a separate receiver
# alternative declared in flask.yaml's `route_calls` — the original sample only ever used `app`.
bp = Blueprint("items", __name__)


@app.route("/")
def index():
    return "ok"


@app.route("/orders", methods=["POST"])
def create_order():
    return "ok"


@app.get("/health")
def health():
    return "ok"


@bp.route("/items")
def list_items():
    return "ok"


# Route guard: `@login_required` is a bare decorator on the same view function as the route. Its name matches
# flask.yaml's `guard_name_patterns`, so it must be materialised as a security `Middleware` node (the FKB sets
# `synthesize_unresolved: true`), wired to the contract via `PassesThrough`. The original sample had no guards.
@app.route("/admin")
@login_required
def admin_dashboard():
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

/// Flask blueprints register routes on a `Blueprint` object (`@bp.route(...)`), a distinct receiver alternative
/// (`bp` / `blueprint` / `api`) in flask.yaml's `route_calls`. The original sample only used `@app.route`, so a
/// regression that dropped the non-`app` receivers would have passed silently.
#[test]
fn flask_blueprint_route_is_recognized() {
    let root = synthetic_flask_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Flask project should succeed");
    };
    let pairs = contract_targets(&b);
    assert!(
        pairs.iter().any(|(c, t)| c == "GET /items" && t == "list_items"),
        "the blueprint route `bp.route(\"/items\")` must become GET /items -> list_items, got: {pairs:?}"
    );
}

/// A route guard (`@login_required`) is materialised into a security `Middleware` node and wired to its contract
/// via `PassesThrough` (flask.yaml sets `synthesize_unresolved: true`). This pins both the guard correlation and
/// the fact that the guard lands on **exactly one** route (not every route in the module).
#[test]
fn flask_route_guard_builds_middleware() {
    let root = synthetic_flask_root();
    let Some(b) = common::graph_with_root(&root, ProjectConfig::default()) else {
        panic!("the graph build of the synthetic Flask project should succeed");
    };
    let middleware: Vec<String> = nodes_of_kind(&b, "Middleware")
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert!(
        middleware.iter().any(|n| n == "login_required"),
        "the route guard @login_required must be materialised as a Middleware node, got: {middleware:?}"
    );

    // Exactly the guarded contract passes through the middleware — a guard on one view must not bleed onto the others.
    let guarded: Vec<String> = nodes_of_kind(&b, "HttpContract")
        .iter()
        .filter(|n| {
            b.store
                .edges_of(n.id, EdgeDirection::Outgoing)
                .expect("edges")
                .iter()
                .any(|e| {
                    e.kind.as_str() == "PassesThrough"
                        && b.store.get_node(e.to_id).ok().flatten().is_some_and(|m| m.name == "login_required")
                })
        })
        .map(|n| n.name.clone())
        .collect();
    assert_eq!(
        guarded,
        vec!["GET /admin".to_string()],
        "only the guarded contract may PassesThrough login_required, got: {guarded:?}"
    );
}
