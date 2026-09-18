//! `gt-adapter-http` —— axum 入站适配器。
//!
//! 把应用层的用例暴露成 REST API，供 Web UI 与 Tauri 前端共用同一套接口
//! （Tauri 走 `fetch` 访问本机 HTTP 服务，从而前后端只有一份契约）。

pub mod dto;
pub mod router;
pub mod server;

pub use router::build_router;
pub use server::{serve, spawn_server};
