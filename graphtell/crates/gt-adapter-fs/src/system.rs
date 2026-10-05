//! A `std::fs`-based filesystem implementation.

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::port::FileSystem;

pub struct StdFileSystem;

impl StdFileSystem {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StdFileSystem {
    fn default() -> Self {
        Self
    }
}

impl FileSystem for StdFileSystem {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_to_string(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).map_err(DomainError::infra)
    }

    fn len(&self, path: &Path) -> Result<u64> {
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(DomainError::infra)
    }
}

/// Normalise a path (resolving `.` / `..`; no symlink resolution).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gt-fs-system-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Edge cases not covered by the integration test's `normalize_resolves_dot_and_dotdot`
    /// (which pins `a/./b/../c`, `a/b/../../c` and the `pop()`-fails fallback `../a`).
    #[test]
    fn normalize_edge_cases() {
        // A trailing `..` consumes the last real component; consuming everything yields an empty path.
        assert_eq!(normalize(Path::new("a/b/..")), Path::new("a"));
        assert_eq!(normalize(Path::new("a/..")), Path::new(""));

        // Absolute paths keep their root, and `..` never escapes it.
        assert_eq!(normalize(Path::new("/a/../b")), Path::new("/b"));

        // Already-normalized input and the empty path are unchanged.
        assert_eq!(normalize(Path::new("a/b/c")), Path::new("a/b/c"));
        assert_eq!(normalize(Path::new("")), Path::new(""));
    }

    /// Repeated separators collapse and `.` is dropped wherever it appears (leading / trailing positions,
    /// which the cases above and the integration test do not reach).
    #[test]
    fn normalize_collapses_separators_and_dot_components() {
        assert_eq!(normalize(Path::new("a//b")), Path::new("a/b"));
        assert_eq!(normalize(Path::new("./a")), Path::new("a"));
        assert_eq!(normalize(Path::new("a/.")), Path::new("a"));
        assert_eq!(normalize(Path::new("a/b/../..")), Path::new(""));
    }

    // ---- the adapter itself: `StdFileSystem` had no unit coverage at all ----

    /// `exists` / `is_dir` are the gates every phase checks before reading: a file must not read as a
    /// directory, and a missing path must read as absent rather than surfacing an error.
    #[test]
    fn file_system_reports_existence_and_kind() {
        let fs = StdFileSystem::new();
        let dir = tmp("kind");
        let file = dir.join("a.txt");
        std::fs::write(&file, b"hi").unwrap();

        assert!(fs.exists(&dir));
        assert!(fs.exists(&file));
        assert!(!fs.exists(&dir.join("nope")), "不存在的路径应报告为不存在");

        assert!(fs.is_dir(&dir));
        assert!(!fs.is_dir(&file), "文件不是目录");
        assert!(!fs.is_dir(&dir.join("nope")), "不存在的路径也不是目录");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `read_to_string` yields the content on success and an `Err` — never a panic — on failure. P0's
    /// `fingerprint` depends on the error arm to degrade to a `path:size` fingerprint instead of aborting
    /// the whole ingest over a few unreadable files.
    #[test]
    fn read_to_string_reads_content_and_errors_instead_of_panicking() {
        let fs = StdFileSystem::default();
        let dir = tmp("read");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        assert_eq!(fs.read_to_string(&file).unwrap(), "hello");
        assert!(
            fs.read_to_string(&dir.join("missing.txt")).is_err(),
            "缺失文件应返回 Err（fingerprint 依赖它退化为 path:size）"
        );
        assert!(
            fs.read_to_string(&dir).is_err(),
            "把目录当文件读应返回 Err，而不是 panic"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `len` reports the byte count, and errors for a missing path rather than returning 0 — a silent 0
    /// would make "unreadable" indistinguishable from "empty".
    #[test]
    fn len_counts_bytes_and_errors_for_a_missing_path() {
        let fs = StdFileSystem::new();
        let dir = tmp("len");
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        assert_eq!(fs.len(&file).unwrap(), 5);
        assert!(
            fs.len(&dir.join("missing.txt")).is_err(),
            "缺失路径应返回 Err，而不是静默返回 0"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
