//! HTTP server startup.

use std::net::{SocketAddr, TcpListener as StdListener};

use axum::Router;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info};

/// Start the service (blocks until a stop signal arrives).
pub async fn serve(router: Router, addr: SocketAddr) -> anyhow::Result<()> {
    let app = router
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("GraphTell API listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Start the service on a background thread and return the actual port.
///
/// For the Tauri desktop build: no port can be assumed free, so `preferred` (5177-5187 by default) is tried in
/// turn, and on failure the kernel assigns a port; the final port is then reported to the frontend through the
/// `api_port` command.
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
                error!("failed to create the tokio runtime: {e}");
                return;
            }
        };
        runtime.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    error!("failed to convert the listener: {e}");
                    return;
                }
            };
            let app = router
                .layer(CorsLayer::permissive())
                .layer(TraceLayer::new_for_http());
            info!("GraphTell API listening on http://127.0.0.1:{port}");
            if let Err(e) = axum::serve(listener, app).await {
                error!("API service exited: {e}");
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
