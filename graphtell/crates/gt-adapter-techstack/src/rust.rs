//! `gt-adapter-techstack` — the Rust tech-stack adapter.
//!
//! Rust's build manifests are `Cargo.toml` (declared dependencies) and `Cargo.lock` (the resolved
//! closure). Both are parsed here so FKB's `manifest_dependency` detectors resolve the precise crate
//! name instead of falling back to a whole-file text probe.
//!
//! What Rust does **not** have (left at the neutral default):
//! * a namespace manifest — Rust modules are inferred from the file tree (handled by the parser's
//!   `module_fqn`), so no `manifest_namespaces`;
//! * a `return [...]`-style config file — `config.toml` / `dotenv` are read by the running program, not
//!   extracted statically, so no `read_manifest`;
//! * a migration-schema loader — Diesel / SQLx schema is either a `diesel::table!` macro (a call site the
//!   parser already captures) or `sqlx::query` (a string), not a separate migration file, so no
//!   `load(migration_schema)`;
//! * i18n path conventions — Rust has none worth stamping, so `i18n_path_patterns` is empty.

use gt_domain::model::Language;
use gt_domain::port::TechStackAdapter;

/// The Rust tech-stack adapter.
pub struct RustTechStackAdapter;

impl RustTechStackAdapter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RustTechStackAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl TechStackAdapter for RustTechStackAdapter {
    fn language(&self) -> Language {
        Language::new(Language::RUST)
    }

    fn manifest_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        match file_name {
            "Cargo.toml" => Some(deps_from_cargo_toml(text)),
            _ => None,
        }
    }

    fn lock_dependencies(&self, file_name: &str, text: &str) -> Option<Vec<String>> {
        if file_name == "Cargo.lock" {
            Some(deps_from_cargo_lock(text))
        } else {
            None
        }
    }

    fn i18n_path_patterns(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Collect crate names from `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` tables in a
/// `Cargo.toml`: `name = "1.0"` or `name = { version = "1.0", ... }`.
fn deps_from_cargo_toml(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r#"(?im)^\s*([A-Za-z0-9_.\-]+)\s*=\s*(?:\{|["'])"#) {
        for c in re.captures_iter(text) {
            if let Some(m) = c.get(1) {
                let n = m.as_str();
                // skip the table header lines themselves (`[dependencies]` has no `=`, so it won't match)
                if n.eq_ignore_ascii_case("version")
                    || n.eq_ignore_ascii_case("edition")
                    || n.eq_ignore_ascii_case("name")
                    || n.eq_ignore_ascii_case("authors")
                    || n.eq_ignore_ascii_case("description")
                    || n.eq_ignore_ascii_case("license")
                    || n.eq_ignore_ascii_case("publish")
                    || n.eq_ignore_ascii_case("workspace")
                {
                    continue;
                }
                if !out.iter().any(|x: &String| x == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// Collect crate names from `[[package]]` entries in a `Cargo.lock`: `name = "x"`.
fn deps_from_cargo_lock(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(re) = regex::Regex::new(r#"(?im)^\s*name\s*=\s*"([^"]+)""#) {
        for c in re.captures_iter(text) {
            if let Some(m) = c.get(1) {
                let n = m.as_str();
                if !out.iter().any(|x: &String| x == n) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_is_rust() {
        assert_eq!(RustTechStackAdapter::new().language().as_str(), "rust");
    }

    #[test]
    fn cargo_toml_yields_crate_names() {
        let toml = r#"
[package]
name = "mylib"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = { version = "1", features = ["full"] }
sqlx = "0.7"

[dev-dependencies]
pytest-rs = "0.2"
"#;
        let mut deps = RustTechStackAdapter::new()
            .manifest_dependencies("Cargo.toml", toml)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"serde".to_string()));
        assert!(deps.contains(&"tokio".to_string()));
        assert!(deps.contains(&"sqlx".to_string()));
        assert!(deps.contains(&"pytest-rs".to_string()));
        // package metadata keys must not leak.
        assert!(!deps.contains(&"mylib".to_string()));
        assert!(!deps.contains(&"name".to_string()));
        assert!(RustTechStackAdapter::new()
            .manifest_dependencies("pom.xml", toml)
            .is_none());
    }

    #[test]
    fn cargo_lock_yields_names() {
        let lock = r#"
[[package]]
name = "serde"
version = "1.0.0"

[[package]]
name = "tokio"
version = "1.0.0"
"#;
        let mut deps = RustTechStackAdapter::new()
            .lock_dependencies("Cargo.lock", lock)
            .unwrap();
        deps.sort();
        assert!(deps.contains(&"serde".to_string()));
        assert!(deps.contains(&"tokio".to_string()));
        assert!(RustTechStackAdapter::new()
            .lock_dependencies("Cargo.toml", lock)
            .is_none());
    }

    #[test]
    fn no_i18n_pattern_for_rust() {
        assert!(RustTechStackAdapter::new().i18n_path_patterns().is_empty());
    }
}
