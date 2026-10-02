//! GraphTell desktop app (Tauri inbound adapter).
//!
//! Design notes:
//! **the backend lives in-process** -- it starts the axum service directly, without spawning a separate subprocess,
//! so the desktop and web fronts share **the same HTTP contract** (`/api/...`).
//! The port is assigned by the kernel and told to the frontend via the `api_port` command, avoiding port conflicts.
//! All business capabilities reuse the container assembled by `gt-app`; nothing is reimplemented in the Tauri layer.

use std::sync::OnceLock;

use gt_app::{AppConfig, Container};
use serde::Serialize;

/// The port the backend actually listens on (process-wide global, written once).
static API_PORT: OnceLock<u16> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub struct BackendInfo {
    pub port: u16,
    pub base_url: String,
    pub languages: Vec<String>,
    pub frameworks: usize,
}

/// Backend info exposed for the frontend to query.
#[tauri::command]
fn api_port() -> BackendInfo {
    let port = API_PORT.get().copied().unwrap_or(5177);
    BackendInfo {
        port,
        base_url: format!("http://127.0.0.1:{port}"),
        languages: Vec::new(),
        frameworks: 0,
    }
}

/// Assemble the container and start the in-process API service; returns the port for the frontend.
pub fn bootstrap(config: AppConfig) -> Result<u16, String> {
    let container = Container::new(config).map_err(|e| e.to_string())?;
    let info = BackendInfo {
        port: 0,
        base_url: String::new(),
        languages: container.languages(),
        frameworks: container.framework_count(),
    };
    tracing::info!(
        "已装载 {} 份框架知识，支持语言: {:?}",
        info.frameworks,
        info.languages
    );
    let port = gt_adapter_http::spawn_server(container.router(), None).map_err(|e| e.to_string())?;
    let _ = API_PORT.set(port);
    Ok(port)
}

/// Tauri entry point (called from `main.rs`).
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![api_port])
        .setup(|_app| {
            let config = AppConfig::default();
            match bootstrap(config) {
                Ok(port) => tracing::info!("后端已启动，端口 {port}"),
                Err(e) => tracing::error!("后端启动失败: {e}"),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
