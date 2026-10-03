//! `gt-adapter-views` — the perspective-registry adapter.
//!
//! Loads the analysis perspectives declared in `views/*.yaml` (route perspective / table perspective /
//! domain aggregate …) and provides the [`ViewRegistryProvider`] port implementation.
//!
//! A perspective is an **analysis paradigm**, not a backend concept, so it is declared by configuration rather
//! than written into Rust: adding a perspective = adding a piece of YAML (open-closed principle).

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::ViewRegistry;
use gt_domain::port::ViewRegistryProvider;

/// A YAML-directory-based perspective registry.
pub struct YamlViewRegistry {
    registry: ViewRegistry,
    sources: Vec<PathBuf>,
}

impl YamlViewRegistry {
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            tracing::warn!("views directory does not exist: {}", root.display());
            return Ok(Self { registry: ViewRegistry::default(), sources: Vec::new() });
        }
        let mut merged = ViewRegistry::default();
        let mut sources = Vec::new();
        for path in collect_yaml(root) {
            match Self::load_file(&path) {
                Ok(reg) => {
                    tracing::info!("loading a perspective declaration: {}", path.display());
                    // Later files override perspectives with the same id
                    for spec in reg.perspectives {
                        if let Some(existing) =
                            merged.perspectives.iter_mut().find(|p| p.id == spec.id)
                        {
                            *existing = spec;
                        } else {
                            merged.perspectives.push(spec);
                        }
                    }
                    merged.node_views.extend(reg.node_views);
                    sources.push(path);
                }
                Err(e) => tracing::warn!("skipping a corrupt perspective declaration {}: {e}", path.display()),
            }
        }
        Ok(Self { registry: merged, sources })
    }

    pub fn load_file(path: &Path) -> Result<ViewRegistry> {
        let text = std::fs::read_to_string(path).map_err(DomainError::infra)?;
        let yaml: serde_yaml::Value = serde_yaml::from_str(&text).map_err(DomainError::infra)?;
        let json = serde_json::to_value(yaml).map_err(DomainError::infra)?;
        serde_json::from_value(json).map_err(DomainError::infra)
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }
}

impl ViewRegistryProvider for YamlViewRegistry {
    fn registry(&self) -> &ViewRegistry {
        &self.registry
    }
}

fn collect_yaml(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
                .unwrap_or(false);
            if is_yaml {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gtav_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const PERSPECTIVE_YAML: &str = "\
perspectives:
  - id: route
    label: Route Perspective
    mode: object
    node_kind: Route
    layout: radial
    depth: 2
node_views:
  Route: route
";

    /// A missing views directory must degrade to an empty registry (Ok), not error — Ingest bootstraps before any
    /// project exists, so an absent dir is normal.
    #[test]
    fn load_dir_missing_directory_is_empty() {
        let missing = std::env::temp_dir().join(format!("gtav_none_{}", std::process::id()));
        let reg = YamlViewRegistry::load_dir(&missing).expect("missing dir returns Ok");
        assert!(reg.registry().perspectives.is_empty(), "missing dir -> zero perspectives");
        assert!(reg.registry().node_views.is_empty());
        assert!(reg.sources().is_empty());
    }

    #[test]
    fn load_file_parses_perspective_and_node_views() {
        let dir = scratch("one");
        let p = dir.join("a.yaml");
        std::fs::write(&p, PERSPECTIVE_YAML).unwrap();
        let reg = YamlViewRegistry::load_dir(&dir).expect("load_dir succeeds");
        assert_eq!(reg.registry().perspectives.len(), 1);
        let spec = reg.registry().by_id("route").expect("route perspective present");
        assert_eq!(spec.label, "Route Perspective");
        assert_eq!(spec.node_kind.as_deref(), Some("Route"));
        assert_eq!(
            reg.registry().node_views.get("Route").map(|s| s.as_str()),
            Some("route")
        );
        assert_eq!(reg.sources(), &[p]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_recurses_into_subdirs() {
        let root = scratch("sub");
        let sub = root.join("group/a");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("root.yaml"), PERSPECTIVE_YAML).unwrap();
        std::fs::write(
            sub.join("nested.yaml"),
            "perspectives:\n  - id: table\n    mode: object\nnode_views:\n  Table: table\n",
        )
        .unwrap();
        let reg = YamlViewRegistry::load_dir(&root).expect("load_dir succeeds");
        assert!(reg.registry().by_id("route").is_some(), "root rule missing");
        assert!(
            reg.registry().by_id("table").is_some(),
            "subdir rule missing (recursion broken)"
        );
        assert_eq!(reg.sources().len(), 2, "both files recorded as sources");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Same id across files: the later (lexicographically sorted) declaration overrides the earlier.
    #[test]
    fn load_dir_later_file_overrides_same_id() {
        let dir = scratch("dup");
        std::fs::write(
            dir.join("first.yaml"),
            "perspectives:\n  - id: dup\n    label: first\n    mode: object\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("second.yaml"),
            "perspectives:\n  - id: dup\n    label: second\n    mode: object\n",
        )
        .unwrap();
        let reg = YamlViewRegistry::load_dir(&dir).expect("load_dir succeeds");
        assert_eq!(reg.registry().perspectives.len(), 1, "duplicate id collapses to one");
        assert_eq!(reg.registry().by_id("dup").unwrap().label, "second", "later wins");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `node_views` is a map that accumulates across files: a same key is overwritten by the later file, distinct
    /// keys from every file are kept.
    #[test]
    fn load_dir_accumulates_node_views_across_files() {
        let dir = scratch("nv");
        std::fs::write(
            dir.join("a.yaml"),
            "perspectives: []\nnode_views:\n  Route: route\n  Table: table\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.yaml"),
            "perspectives: []\nnode_views:\n  Table: table2\n  Cache: cache\n",
        )
        .unwrap();
        let reg = YamlViewRegistry::load_dir(&dir).expect("load_dir succeeds");
        let nv = &reg.registry().node_views;
        assert_eq!(nv.get("Route").map(|s| s.as_str()), Some("route"));
        assert_eq!(
            nv.get("Table").map(|s| s.as_str()),
            Some("table2"),
            "same key -> later wins"
        );
        assert_eq!(
            nv.get("Cache").map(|s| s.as_str()),
            Some("cache"),
            "new key kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_recognizes_yml_extension() {
        let dir = scratch("yml");
        std::fs::write(dir.join("x.yml"), PERSPECTIVE_YAML).unwrap();
        let reg = YamlViewRegistry::load_dir(&dir).expect("load_dir succeeds");
        assert!(
            reg.registry().by_id("route").is_some(),
            ".yml extension must be picked up"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single corrupt file must not take down the rest: it is skipped (warn) and the good file still loads.
    #[test]
    fn load_dir_skips_corrupt_file_but_keeps_others() {
        let dir = scratch("corrupt");
        std::fs::write(dir.join("good.yaml"), PERSPECTIVE_YAML).unwrap();
        std::fs::write(dir.join("bad.yaml"), "this: : is: not: valid: yaml: [\n").unwrap();
        let reg = YamlViewRegistry::load_dir(&dir).expect("load_dir tolerates a corrupt file");
        assert_eq!(reg.registry().perspectives.len(), 1, "good file still loads");
        assert_eq!(reg.registry().by_id("route").unwrap().id, "route");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_file_errors_on_corrupt_input() {
        let dir = scratch("badfile");
        let p = dir.join("bad.yaml");
        std::fs::write(&p, "this: : is: not: valid: yaml: [\n").unwrap();
        let err = YamlViewRegistry::load_file(&p);
        assert!(err.is_err(), "load_file should surface a parse error");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
