//! `em-adapter-views` —— 视角注册表适配器。
//!
//! 装载 `views/*.yaml` 中声明的分析视角（路由视角 / 表视角 / 领域聚合 …），
//! 并提供 [`ViewRegistryProvider`] 端口实现。
//!
//! 视角是**分析范式**而非后端概念，因此用配置声明而不是写进 Rust：
//! 新增视角 = 加一段 YAML（开闭原则）。

use std::path::{Path, PathBuf};

use em_domain::error::{DomainError, Result};
use em_domain::model::ViewRegistry;
use em_domain::port::ViewRegistryProvider;

/// 基于 YAML 目录的视角注册表。
pub struct YamlViewRegistry {
    registry: ViewRegistry,
    sources: Vec<PathBuf>,
}

impl YamlViewRegistry {
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            tracing::warn!("视角目录不存在: {}", root.display());
            return Ok(Self { registry: ViewRegistry::default(), sources: Vec::new() });
        }
        let mut merged = ViewRegistry::default();
        let mut sources = Vec::new();
        for path in collect_yaml(root) {
            match Self::load_file(&path) {
                Ok(reg) => {
                    tracing::info!("装载视角声明: {}", path.display());
                    // 后者覆盖同 id 的视角
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
                Err(e) => tracing::warn!("跳过损坏的视角声明 {}: {e}", path.display()),
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
