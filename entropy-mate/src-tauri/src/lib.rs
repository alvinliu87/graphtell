//! EntropyMate 桌面端（Tauri 入站适配器）。
//!
//! 设计要点：
//! * **后端常驻在进程内** —— 直接启动 axum 服务，不额外拉起子进程，
//!   因此桌面端与 Web 端共用**同一套 HTTP 契约**（`/api/...`）。
//! * 端口由内核分配后通过 `api_port` 命令告知前端，避免端口冲突。
//! * 所有业务能力复用 `em-app` 组装好的容器，不在 Tauri 层重复实现。

use std::sync::OnceLock;

use em_app::{AppConfig, Container};
use serde::Serialize;

/// 后端实际监听的端口（进程内全局，只写一次）。
static API_PORT: OnceLock<u16> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub struct BackendInfo {
    pub port: u16,
    pub base_url: String,
    pub languages: Vec<String>,
    pub frameworks: usize,
}

/// 供前端查询的后端信息。
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

/// 组装容器并启动进程内 API 服务；返回端口供前端使用。
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
    let port = em_adapter_http::spawn_server(container.router(), None).map_err(|e| e.to_string())?;
    let _ = API_PORT.set(port);
    Ok(port)
}

/// Tauri 入口（由 `main.rs` 调用）。
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
