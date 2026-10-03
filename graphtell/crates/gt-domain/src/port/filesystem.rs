//! Filesystem and file-scanning ports.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::model::Language;

/// Read-only filesystem capability.
///
/// Only the methods the pipeline really needs are exposed (interface segregation), so an in-memory filesystem can be
/// substituted in tests.
pub trait FileSystem: Send + Sync {
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn len(&self, path: &Path) -> Result<u64>;
}

/// A scan request.
#[derive(Debug, Clone)]
pub struct ScanRequest {
    pub root: PathBuf,
    /// Exclude globs layered on top of the language defaults.
    pub extra_excludes: Vec<String>,
    /// Target language; empty means every supported language.
    pub languages: Vec<Language>,
    /// "Language -> extension list", supplied by the parser registry.
    ///
    /// Used to decide the language from an extension. The extension table comes from the parser registry, so it
    /// lines up with the sub-project marker table (`composer.json` / `go.mod` / `pyproject.toml` …) — otherwise
    /// a sub-project would be detected yet not a single source file scanned.
    /// Empty falls back to the scanner's built-in table.
    pub language_extensions: Vec<(String, Vec<String>)>,
}

/// A candidate file produced by scanning.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub path: PathBuf,
    /// Path relative to the scan root, `/`-separated.
    pub relative: String,
    pub language: Language,
    pub size_bytes: u64,
}

/// File scanner.
///
/// Excludes `vendor/`, `node_modules/`, static assets, build artifacts and so on, returning only the source files
/// "worth graphing".
pub trait FileScanner: Send + Sync {
    /// Scan the source files to be graphed.
    fn scan(&self, request: &ScanRequest) -> Result<Vec<ScannedFile>>;

    /// Find "marker files" with a given name (e.g. `composer.json`) within a bounded depth.
    ///
    /// Used for sub-project detection: one project root may nest several tech-stack sub-projects.
    fn find_markers(&self, root: &Path, names: &[&str], max_depth: usize) -> Result<Vec<PathBuf>>;
}
