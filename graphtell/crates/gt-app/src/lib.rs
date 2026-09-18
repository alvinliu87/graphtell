//! `gt-app` —— 组装根（Composition Root）。
//!
//! 六边形架构里**唯一**知道全部具体技术的地方：
//! 在这里把 SQLite / tree-sitter / 文件系统 / YAML 知识库装配成端口实现，
//! 注入给应用层。其它任何 crate 都不依赖具体技术。

pub mod config;
pub mod container;

pub use config::AppConfig;
pub use container::Container;
