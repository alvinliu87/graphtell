//! `gt-adapter-http` — the axum inbound adapter.
//!
//! Exposes the application layer's use cases as a REST API, so the Web UI and the Tauri front end share one
//! interface (Tauri reaches the local HTTP service via `fetch`, so there is only one contract for both ends).

pub mod dto;
pub mod router;
pub mod server;

pub use router::build_router;
pub use server::{serve, spawn_server};
