//! Contract smoke test of the HTTP inbound adapter: assemble a real `AppState` from an in-memory database plus an
//! empty asset directory, and verify that the router can be built and the health / status endpoints return 200.
//!
//! This does not try to cover every handler; it only pins "the route exists and the basic endpoints
//! respond", so a route cannot be deleted by accident or a handler signature drift unnoticed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use gt_adapter_http::router::{build_router, state};
use gt_adapter_sqlite::SqliteStore;
use gt_application::pipeline_runner::PipelineDeps;
use gt_domain::port::{DefaultResourceAdapterRegistry, DefaultTechStackRegistry, Persistence};
use gt_adapter_techstack::DefaultMarkerProvider;
use tower::ServiceExt;

/// Assemble a real usable HTTP state from an in-memory database plus an empty asset directory (no dependency on the
/// fkb/rules/views on disk).
///
/// `load_dir` returns an empty collection when the directory does not exist (with a warning only), so passing a
/// non-existent path yields an empty KB / rules / views.
fn test_state() -> gt_adapter_http::router::Shared {
    let store: Arc<dyn Persistence> = Arc::new(SqliteStore::in_memory().expect("the in-memory database should open"));
    let deps = Arc::new(PipelineDeps {
        fs: Arc::new(gt_adapter_fs::StdFileSystem::new()),
        scanner: Arc::new(gt_adapter_fs::WalkDirScanner::new(Vec::new())),
        parsers: Arc::new(gt_adapter_parser::DefaultParserRegistry::new()),
        kb: Arc::new(
            gt_adapter_fkb::YamlKnowledgeBase::load_dir(std::path::Path::new("/__nonexistent_fkb__"))
                .expect("an empty KB should load"),
        ),
        techstack: Arc::new(DefaultTechStackRegistry::new()),
        markers: Arc::new(DefaultMarkerProvider::new()),
        resources: Arc::new(DefaultResourceAdapterRegistry::new()),
    });
    let views = Arc::new(
        gt_adapter_views::YamlViewRegistry::load_dir(std::path::Path::new("/__nonexistent_views__"))
            .expect("empty views should load"),
    );
    let rules = Arc::new(
        gt_adapter_rules::YamlRuleSet::load_dir(std::path::Path::new("/__nonexistent_rules__"))
            .expect("empty rules should load"),
    );
    state(store, deps, 0, views, rules)
}

async fn get(uri: &str) -> StatusCode {
    let app = build_router(test_state(), None);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("the request should succeed");
    resp.status()
}

/// Issue a request against an already-built app (so an in-memory project created on it is visible to later
/// calls — `get` rebuilds the state each time and would lose it).
async fn req(app: &Router, method: &str, uri: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("the request should succeed")
        .status()
}

/// Create a project in the shared in-memory store and return its id (as a URI-ready string).
async fn create_project(app: &Router) -> String {
    let body = serde_json::to_vec(
        &serde_json::json!({"name": "smoke", "root_path": "/tmp/smoke"}),
    )
    .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/projects")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .expect("create project");
    assert_eq!(resp.status(), StatusCode::OK, "creating a project should succeed");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["data"]["id"].to_string()
}

#[tokio::test]
async fn health_endpoint_returns_ok() {
    assert_eq!(get("/api/health").await, StatusCode::OK);
}

#[tokio::test]
async fn server_status_endpoint_returns_ok() {
    assert_eq!(get("/api/server/status").await, StatusCode::OK);
}

#[tokio::test]
async fn unknown_api_path_returns_404() {
    assert_eq!(get("/api/does-not-exist").await, StatusCode::NOT_FOUND);
}

/// Production deployment: when `ui_dir` points at a directory containing `index.html`, both the root path and
/// unmatched paths should fall back to the SPA entry (index.html) rather than 404; and the standalone `/compose`
/// page remains available.
#[tokio::test]
async fn spa_fallback_serves_index_html() {
    let dir = std::env::temp_dir().join(format!(
        "graphtell-spa-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>graphtell-spa</title>")
        .unwrap();

    let app = build_router(test_state(), Some(dir.clone()));
    // Root path -> SPA entry
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec(),
    )
    .unwrap();
    assert!(body.contains("graphtell-spa"), "the root path should return the SPA entry");

    // A frontend route path (a file that does not exist) -> also falls back to index.html
    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/projects/1/graph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // The standalone /compose page is still available
    let resp3 = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/compose")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::OK);

    std::fs::remove_dir_all(&dir).ok();
}

/// The SPA fallback is opt-in: only when `ui_dir` is given should unknown frontend paths fall back to
/// `index.html`. With `ui_dir = None` (the default wiring) there is no fallback, so a frontend route that
/// has no matching API must 404 — while the root still resolves to the standalone compose page. This pins
/// the two-mode behaviour that `spa_fallback_serves_index_html` only exercises on the `Some` side.
#[tokio::test]
async fn spa_fallback_only_active_with_ui_dir() {
    let app = build_router(test_state(), None);

    let root = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK, "the root must still serve the compose page without a ui_dir");

    let missing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/projects/1/graph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing.status(),
        StatusCode::NOT_FOUND,
        "without a ui_dir an unknown frontend path must 404 (no SPA fallback)"
    );
}

/// A representative cross-section of the real route surface must be *registered*. This is the guard the
/// file's header promises ("a route cannot be deleted by accident"): a deleted route — or a handler whose
/// signature no longer compiles into the router — surfaces here as 404 (no such route), whereas a route that
/// exists returns 200 (read routes on a real, empty project) or 405 (a write-only route hit with GET).
///
/// We assert only "not 404" so the guard stays independent of how each handler behaves on an empty in-memory
/// store / empty KB (the suite deliberately does not cover every handler's body).
#[tokio::test]
async fn project_scoped_routes_exist() {
    let app = build_router(test_state(), None);
    let id = create_project(&app).await;

    // (method, uri) pairs. Write-only routes are hit with GET on purpose: a registered route answers 405,
    // a deleted one answers 404 — so 405 still proves "the route exists".
    let checks: Vec<(&str, String)> = vec![
        ("GET", format!("/api/projects/{id}")),
        ("GET", format!("/api/projects/{id}/sub-projects")),
        ("GET", format!("/api/projects/{id}/stats")),
        ("GET", format!("/api/projects/{id}/nodes")),
        ("GET", format!("/api/projects/{id}/diagnostics")),
        ("GET", format!("/api/projects/{id}/diagnostics/summary")),
        ("GET", format!("/api/projects/{id}/perspectives")),
        ("GET", format!("/api/projects/{id}/warmup")),
        ("GET", format!("/api/projects/{id}/violations")),
        ("GET", format!("/api/projects/{id}/check/summary")),
        ("GET", format!("/api/projects/{id}/rules/config")),
        ("GET", "/api/projects".to_string()),
        ("GET", "/api/fs/browse".to_string()),
        ("GET", "/api/rules".to_string()),
        ("GET", "/api/symbols/tables".to_string()),
        // write-only routes, detected via a wrong-method GET (expect 405, never 404)
        ("GET", format!("/api/projects/{id}/run")),
        ("GET", format!("/api/projects/{id}/recall")),
        ("GET", format!("/api/projects/{id}/prompt")),
        ("GET", format!("/api/projects/{id}/check")),
        ("GET", format!("/api/projects/{id}/rules/config/batch")),
    ];

    for (method, uri) in checks {
        let status = req(&app, method, &uri).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "route {method} {uri} must be registered (got 404 — a route was deleted or its handler drifted)"
        );
    }
}
