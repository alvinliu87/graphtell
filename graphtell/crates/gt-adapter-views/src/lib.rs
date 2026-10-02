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
