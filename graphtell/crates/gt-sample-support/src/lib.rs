//! Shared test support: locating the real sample checkouts used by the integration tests.
//!
//! The full-pipeline integration tests need a real, oversized code corpus (e.g. the `sample_project`
//! checkout, a secondary PHP framework sample, the Node.js samples, the `link` sample). That corpus is
//! intentionally **not** committed (see `samples/`'s gitignore rules). To keep CI green on
//! machines without the sample, a missing sample makes a test **skip** rather than fail.
//!
//! This crate is the single source of truth for *where* those samples live: one discovery core
//! ([`find_sample`], honoring the `GRAPHTELL_SAMPLE_DIR` env var or a bounded search under `samples/`).
//!
//! # Sample catalog
//!
//! The concrete on-disk directory names of each sample are **not** written in source; they live in
//! `samples.toml`, embedded at compile time by [`CATALOG_TOML`] and read through [`catalog`]. Every
//! sample is addressed by a generic key (`sample_project`, `php_alt`, `node_a`, `node_b`, `link`), so
//! renaming or adding a checkout touches only that file.

use std::path::{Path, PathBuf};

/// Env var that, when set, points directly at a sample checkout (absolute path).
pub const SAMPLE_DIR_ENV: &str = "GRAPHTELL_SAMPLE_DIR";

/// Secondary env var (legacy): points at the `samples/` *parent* directory rather than the
/// checkout directly. Kept for backward compatibility with tooling (e.g. `tools/gen_demo.sh`)
/// that still sets it.
pub const SAMPLES_DIR_ENV: &str = "GRAPHTELL_SAMPLES_DIR";

/// Friendly name used in skip/panic hints.
pub const SAMPLE_PROJECT_NAME: &str = "sample_project";

/// The sample catalog, embedded at compile time. See the crate docs: this is the only place the
/// concrete checkout directory names are recorded.
const CATALOG_TOML: &str = include_str!("../samples.toml");

/// One entry of [`CATALOG_TOML`]: the on-disk names a sample is discovered by.
#[derive(Debug, Default)]
struct CatalogEntry {
    /// Accepted checkout directory names; discovery uses the first one that exists.
    dirs: Vec<String>,
    /// Application sub-directory inside the checkout; empty when the checkout root is the project root.
    inner_dir: String,
    /// PHP namespace prefix the project's `composer.json` maps onto `inner_dir`.
    ns: String,
}

/// Strip surrounding quotes / whitespace from a scalar value.
fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim_matches('\'').to_string()
}

/// Read one section of the catalog by its generic key.
///
/// Returns a default (empty) entry for an unknown key so callers never panic on a missing section;
/// discovery then simply finds nothing and the test skips.
fn catalog(section: &str) -> CatalogEntry {
    let mut entry = CatalogEntry::default();
    let mut in_section = false;
    for line in CATALOG_TOML.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name.trim() == section;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "dirs" => {
                let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
                entry.dirs.extend(
                    inner
                        .split(',')
                        .map(unquote)
                        .filter(|s| !s.is_empty()),
                );
            }
            "inner_dir" => entry.inner_dir = unquote(value),
            "ns" => entry.ns = unquote(value),
            _ => {}
        }
    }
    entry
}

/// The accepted checkout directory names of one sample, as a slice for [`find_sample`].
fn dirs_of(section: &str) -> Vec<String> {
    catalog(section).dirs
}

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

/// Generic sample discovery for an arbitrary set of accepted directory names.
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

/// Locate the primary `sample_project` (PHP) sample root directory.
///
/// See [`find_sample`] for the resolution order. Returns `None` when no sample is found; callers
/// should skip, not fail.
pub fn sample_root() -> Option<PathBuf> {
    let dirs = dirs_of(SAMPLE_PROJECT_NAME);
    find_sample(&dirs.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Same as [`sample_root`] but joined with `rel` (e.g. `sample_file("app/event.php")`).
pub fn sample_file(rel: &str) -> Option<PathBuf> {
    sample_root().map(|r| r.join(rel))
}

/// The application sub-directory inside the `sample_project` checkout (from the catalog).
pub fn sample_project_inner_dir() -> String {
    catalog(SAMPLE_PROJECT_NAME).inner_dir
}

/// The PHP namespace prefix the `sample_project` maps onto its inner directory (from the catalog).
pub fn sample_project_ns() -> String {
    catalog(SAMPLE_PROJECT_NAME).ns
}

/// Build a repository-relative path string under the `sample_project` checkout's inner application
/// directory (e.g. `sample_project_rel("app/event.php")`).
pub fn sample_project_rel(rel: &str) -> String {
    PathBuf::from(sample_project_inner_dir())
        .join(rel)
        .to_string_lossy()
        .to_string()
}

/// Build a `sample_project` FQN from an inner-directory-relative suffix
/// (e.g. `sample_project_fqn("services\\CacheService::remember")`).
pub fn sample_project_fqn(suffix: &str) -> String {
    format!("{}\\{suffix}", sample_project_ns())
}

/// Standard hint used in skip/panic messages when the sample is absent.
pub fn missing_hint() -> String {
    format!(
        "{} sample missing: set the {} env var to the checkout, or check out samples/**/{{{}}}",
        SAMPLE_PROJECT_NAME,
        SAMPLE_DIR_ENV,
        dirs_of(SAMPLE_PROJECT_NAME).join("/")
    )
}

/// Locate the `link` sample root directory.
///
/// Uses the same discovery as [`find_sample`], plus a legacy `GRAPHTELL_SAMPLES_DIR` fallback
/// that points at the `samples/` *parent* (so existing tooling keeps working).
pub fn link_sample_root() -> Option<PathBuf> {
    let dirs = dirs_of("link");
    if let Ok(dir) = std::env::var(SAMPLES_DIR_ENV) {
        if let Some(name) = dirs.first() {
            let candidate = PathBuf::from(dir).join(name);
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    find_sample(&dirs.iter().map(String::as_str).collect::<Vec<_>>())
}

/// The checkout directory name of the `link` sample (from the catalog).
pub fn link_sample_dir_name() -> String {
    dirs_of("link").into_iter().next().unwrap_or_default()
}

/// Standard hint used in skip/panic messages when an arbitrary named sample is absent.
pub fn missing_hint_named(name: &str) -> String {
    format!(
        "{} sample missing: set the {} env var to the checkout, or check out samples/**/{}",
        name, SAMPLE_DIR_ENV, name
    )
}

/// Locate the secondary PHP framework sample root directory.
///
/// Delegates to the single [`find_sample`] discovery (honors the generic `GRAPHTELL_SAMPLE_DIR` env
/// var, or a bounded search under `samples/`). Returns `None` when no sample is found; callers should
/// skip, not fail.
pub fn php_alt_sample_root() -> Option<PathBuf> {
    let dirs = dirs_of("php_alt");
    find_sample(&dirs.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Locate the `(node_a, node_b)` sample root directories.
///
/// Delegates to the single [`find_sample`] discovery for each sample independently. Returns `None`
/// when either sample is missing; callers should skip, not fail.
pub fn node_samples_root() -> Option<(PathBuf, PathBuf)> {
    let a = dirs_of("node_a");
    let b = dirs_of("node_b");
    match (
        find_sample(&a.iter().map(String::as_str).collect::<Vec<_>>()),
        find_sample(&b.iter().map(String::as_str).collect::<Vec<_>>()),
    ) {
        (Some(n), Some(e)) => Some((n, e)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every sample the tests ask for must resolve out of the catalog.
    ///
    /// This guards a silent failure mode: if `samples.toml` were unparseable, `dirs` would come back
    /// empty, `find_sample` would find nothing, and **every** integration test would quietly skip
    /// instead of failing — CI would stay green with zero coverage.
    #[test]
    fn catalog_resolves_every_sample() {
        for key in [
            SAMPLE_PROJECT_NAME,
            "php_alt",
            "node_a",
            "node_b",
            "link",
        ] {
            let dirs = dirs_of(key);
            assert!(!dirs.is_empty(), "catalog has no `dirs` for `{key}`");
            assert!(
                dirs.iter().all(|d| !d.is_empty()),
                "catalog has an empty directory name for `{key}`: {dirs:?}"
            );
        }
    }

    /// The `sample_project` entry also drives path / FQN building, so its two extra fields must parse.
    #[test]
    fn catalog_resolves_sample_project_shape() {
        assert!(
            !sample_project_inner_dir().is_empty(),
            "sample_project `inner_dir` did not parse"
        );
        assert!(
            !sample_project_ns().is_empty(),
            "sample_project `ns` did not parse"
        );
        assert!(
            sample_project_rel("app/event.php").ends_with("app/event.php"),
            "`sample_project_rel` must keep the suffix"
        );
        assert!(
            sample_project_fqn("A::b").contains("A::b"),
            "`sample_project_fqn` must keep the suffix"
        );
    }
}
