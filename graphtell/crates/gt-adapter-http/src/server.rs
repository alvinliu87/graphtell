//! HTTP 服务器启动。

use std::net::{SocketAddr, TcpListener as StdListener};

use axum::Router;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info};

/// 启动服务（阻塞直到收到停止信号）。
pub async fn serve(router: Router, addr: SocketAddr) -> anyhow::Result<()> {
    let app = router
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("GraphTell API 监听 http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// 在后台线程启动服务并返回实际端口。
///
/// 供 Tauri 桌面端使用：不能假设某个端口一定空闲，
/// 因此依次尝试 `preferred`（默认 5177–5187），失败则交给内核分配端口，
/// 再把最终端口通过 `api_port` 命令告知前端。
pub fn spawn_server(router: Router, preferred: Option<u16>) -> anyhow::Result<u16> {
    let candidates: Vec<u16> = match preferred {
        Some(p) => vec![p],
        None => (5177u16..=5187u16).collect(),
    };
    for port in candidates {
        if let Ok(listener) = StdListener::bind(("127.0.0.1", port)) {
            return start_with(listener, router);
        }
    }
    let listener = StdListener::bind(("127.0.0.1", 0))?;
    start_with(listener, router)
}

fn start_with(listener: StdListener, router: Router) -> anyhow::Result<u16> {
    let port = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                error!("无法创建 tokio 运行时: {e}");
                return;
            }
        };
        runtime.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    error!("转换监听器失败: {e}");
                    return;
                }
            };
            let app = router
                .layer(CorsLayer::permissive())
                .layer(TraceLayer::new_for_http());
            info!("GraphTell API 监听 http://127.0.0.1:{port}");
            if let Err(e) = axum::serve(listener, app).await {
                error!("API 服务退出: {e}");
            }
        });
    });
    Ok(port)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal;
        let mut term = match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return,
        };
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
