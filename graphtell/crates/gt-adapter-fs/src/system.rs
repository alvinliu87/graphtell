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
