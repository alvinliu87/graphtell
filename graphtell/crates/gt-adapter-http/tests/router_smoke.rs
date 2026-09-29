//! HTTP 入站适配器的契约冒烟测试：用内存库 + 空资产目录组装出真实的 `AppState`，
//! 验证路由能建起来、健康/状态端点能 200。
//!
//! 这是此前**完全零测试**的高风险 crate（整个 REST 契约面都在 `router.rs`）。这里不追求
//! 覆盖每个处理器，只钉住"路由存在且基础端点可响应"，避免路由被误删 / 处理器签名漂移。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use gt_adapter_http::router::{build_router, state};
use gt_adapter_sqlite::SqliteStore;
use gt_application::pipeline_runner::PipelineDeps;
use gt_domain::port::Persistence;
use tower::ServiceExt;

/// 用内存库 + 空资产目录组装出一个真实可用的 HTTP 状态（不依赖磁盘上的 fkb/rules/views）。
///
/// `load_dir` 在目录不存在时返回空集合（仅告警），所以传不存在的路径即可拿到空 KB/规则/视角。
fn test_state() -> gt_adapter_http::router::Shared {
    let store: Arc<dyn Persistence> = Arc::new(SqliteStore::in_memory().expect("内存库应可打开"));
    let deps = Arc::new(PipelineDeps {
        fs: Arc::new(gt_adapter_fs::StdFileSystem::new()),
        scanner: Arc::new(gt_adapter_fs::WalkDirScanner::new(Vec::new())),
        parsers: Arc::new(gt_adapter_parser::DefaultParserRegistry::new()),
        kb: Arc::new(
            gt_adapter_fkb::YamlKnowledgeBase::load_dir(std::path::Path::new("/__nonexistent_fkb__"))
                .expect("空 KB 应可装载"),
        ),
    });
    let views = Arc::new(
        gt_adapter_views::YamlViewRegistry::load_dir(std::path::Path::new("/__nonexistent_views__"))
            .expect("空视角应可装载"),
    );
    let rules = Arc::new(
        gt_adapter_rules::YamlRuleSet::load_dir(std::path::Path::new("/__nonexistent_rules__"))
            .expect("空规则应可装载"),
    );
    state(store, deps, 0, views, rules)
}

async fn get(uri: &str) -> StatusCode {
    let app = build_router(test_state(), None);
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .expect("请求应成功");
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

/// 生产部署：当 `ui_dir` 指向含 `index.html` 的目录时，根路径与未命中路径都应回退到 SPA
/// 入口（index.html），而非 404；且 `/compose` 独立页仍可用。
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
    // 根路径 → SPA 入口
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
    assert!(body.contains("graphtell-spa"), "根路径应返回 SPA 入口");

    // 前端路由路径（不存在的文件）→ 同样回退到 index.html
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

    // /compose 独立页仍可用
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
