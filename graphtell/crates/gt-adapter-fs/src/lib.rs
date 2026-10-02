//! `gt-adapter-fs` — the filesystem adapter.
//!
//! Implements the two outbound ports `FileSystem` and `FileScanner`, responsible for
//! * directory traversal and excluding **dependency directories / static assets / build artifacts**
//! * marker-file lookup (sub-project detection)

pub mod scanner;
pub mod system;

pub use scanner::WalkDirScanner;
pub use system::StdFileSystem;
