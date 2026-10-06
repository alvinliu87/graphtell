//! Shared test support: locating the real `sample_project` (CRMEB) checkout.
//!
//! The full-pipeline integration tests need a real, oversized code corpus. That corpus is
//! intentionally **not** committed (see `samples/`'s gitignore rules). To keep CI green on
//! machines without the sample, a missing sample makes a test **skip** rather than fail.
//!
//! This crate is the single source of truth for *where* that sample lives, so the discovery
//! logic and the canonical directory names are defined in exactly one place.

use std::path::{Path, PathBuf};

/// Env var that, when set, points directly at a sample checkout (absolute path).
pub const SAMPLE_DIR_ENV: &str = "GRAPHTELL_SAMPLE_DIR";

/// Canonical directory names of the `sample_project` (CRMEB) checkout.
///
/// The checkout may or may not carry a `-master` suffix and lives somewhere under a multi-level
/// `samples/` taxonomy (e.g. `samples/php-projects/thinkphp/CRMEB`). Both accepted forms are
/// listed here so renaming the checkout only requires editing this one constant.
pub const SAMPLE_PROJECT_DIR_NAMES: &[&str] = &["CRMEB", "CRMEB-master"];

/// Friendly name used in skip/panic hints.
pub const SAMPLE_PROJECT_NAME: &str = "sample_project";

/// Bounded-depth recursive search under `dir` for a directory whose name is in `names`.
///
/// Returns the lexicographically first hit for a stable result across machines.
fn under_samples(dir: &Path, names: &[&str], depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let mut hits: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if names.contains(&name) {
            hits.push(path);
        } else if let Some(found) = under_samples(&path, names, depth - 1) {
            hits.push(found);
        }
    }
    hits.sort();
    hits.into_iter().next()
}

/// Generic sample discovery: the same algorithm as [`sample_root`], but for an arbitrary set of
/// accepted directory names. Other samples (e.g. `frontend-backend-link`) can reuse this instead
/// of each test crate re-deriving the upward-walk + bounded recursive search.
///
/// Resolution order:
/// 1. `GRAPHTELL_SAMPLE_DIR` — used verbatim if it points at an existing directory.
/// 2. Walk upward from `CARGO_MANIFEST_DIR` (works whether the workspace root *is* the repo
///    root or is nested), and do a bounded-depth recursive search under `samples/` for a
///    directory whose name is in `names`.
///
/// Returns `None` when no sample is found; callers should skip, not fail.
pub fn find_sample(names: &[&str]) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(SAMPLE_DIR_ENV) {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    let mut cur = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if let Some(candidate) = under_samples(&cur.join("samples"), names, 3) {
            return Some(candidate.canonicalize().unwrap_or(candidate));
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

/// Locate the `sample_project` (CRMEB) sample root directory.
///
/// See [`find_sample`] for the resolution order. Returns `None` when no sample is found; callers
/// should skip, not fail.
pub fn sample_root() -> Option<PathBuf> {
    find_sample(SAMPLE_PROJECT_DIR_NAMES)
}

/// Same as [`sample_root`] but joined with `rel` (e.g. `sample_file("crmeb/app/event.php")`).
pub fn sample_file(rel: &str) -> Option<PathBuf> {
    sample_root().map(|r| r.join(rel))
}

/// Standard hint used in skip/panic messages when the sample is absent.
pub fn missing_hint() -> String {
    format!(
        "{} sample missing: set the {} env var to the checkout, or check out samples/**/{{{}}}",
        SAMPLE_PROJECT_NAME,
        SAMPLE_DIR_ENV,
        SAMPLE_PROJECT_DIR_NAMES.join("/")
    )
}

/// Secondary env var (legacy): points at the `samples/` *parent* directory rather than the
/// checkout directly. Kept for backward compatibility with tooling (e.g. `tools/gen_demo.sh`)
/// that still sets it.
pub const SAMPLES_DIR_ENV: &str = "GRAPHTELL_SAMPLES_DIR";

/// Locate the `frontend-backend-link` sample root directory.
///
/// Uses the same discovery as [`find_sample`], plus a legacy `GRAPHTELL_SAMPLES_DIR` fallback
/// that points at the `samples/` *parent* (so existing tooling keeps working).
pub fn frontend_backend_link_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(SAMPLES_DIR_ENV) {
        let candidate = PathBuf::from(dir).join("frontend-backend-link");
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    find_sample(&["frontend-backend-link"])
}

/// Standard hint used in skip/panic messages when an arbitrary named sample is absent.
pub fn missing_hint_named(name: &str) -> String {
    format!(
        "{} sample missing: set the {} env var to the checkout, or check out samples/**/{}",
        name, SAMPLE_DIR_ENV, name
    )
}
