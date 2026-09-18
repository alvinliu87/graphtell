//! `gt-adapter-fs` —— 文件系统适配器。
//!
//! 实现 `FileSystem` 与 `FileScanner` 两个出站端口，负责
//! * 目录遍历与**依赖目录 / 静态资源 / 编译产物**排除
//! * 标记文件查找（子工程识别）

pub mod scanner;
pub mod system;

pub use scanner::WalkDirScanner;
pub use system::StdFileSystem;
