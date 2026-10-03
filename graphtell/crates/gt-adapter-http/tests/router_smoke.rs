//! Contract smoke test of the HTTP inbound adapter: assemble a real `AppState` from an in-memory database plus an
//! empty asset directory, and verify that the router can be built and the health / status endpoints return 200.
//!
//! This does not try to cover every handler; it only pins "the route exists and the basic endpoints
//! respond", so a route cannot be deleted by accident or a handler signature drift unnoticed.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
