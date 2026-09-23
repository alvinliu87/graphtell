//! FKB 装载：从 YAML 目录读取框架知识。

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{FrameworkKnowledge, Language};
use gt_domain::port::KnowledgeProvider;
use tracing::{info, warn};

/// 基于文件目录的 FKB 供给实现。
///
/// 目录结构：`fkb/<任意子目录>/*.yaml`，每份 YAML 描述一个框架。
pub struct YamlKnowledgeBase {
    entries: Vec<FrameworkKnowledge>,
    sources: Vec<PathBuf>,
}

impl YamlKnowledgeBase {
    /// 递归装载目录下的所有 `*.yaml` / `*.yml`。
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("FKB 目录不存在: {}", root.display());
            return Ok(Self { entries: Vec::new(), sources: Vec::new() });
        }
        let mut entries = Vec::new();
        let mut sources = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = std::fs::read_dir(&dir).map_err(DomainError::infra)?;
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
                if !is_yaml {
                    continue;
                }
                match Self::load_file(&path) {
                    Ok(fk) => {
                        info!("装载 FKB: {} ({})", fk.id, path.display());
                        // FKB 声明的**语义节点种类**在此登记：新增一种语义节点
                        // （前端 `Store`、页面 `Page`…）无需再改内核清单。
                        if !fk.semantic_kinds.is_empty() {
                            gt_domain::model::kinds::register_semantic_kinds(
                                fk.semantic_kinds.iter().cloned(),
                            );
                        }
                        // FKB 声明的**语义 / 桥边种类**在此登记：新增一种边种类
                        // （如某框架的 `SendsWebhook`）无需再改 `kinds.rs` 的
                        // `SEMANTIC` / `BRIDGE` 清单 —— 与节点的登记同构。
                        if !fk.semantic_edge_kinds.is_empty()
                            || !fk.bridge_edge_kinds.is_empty()
                        {
                            gt_domain::model::kinds::register_edge_kinds(
                                fk.semantic_edge_kinds.iter().cloned(),
                                fk.bridge_edge_kinds.iter().cloned(),
                            );
                        }
                        entries.push(fk);
                        sources.push(path);
                    }
                    Err(e) => warn!("跳过损坏的 FKB {}: {e}", path.display()),
                }
            }
        }
        Ok(Self { entries, sources })
    }

    pub fn load_file(path: &Path) -> Result<FrameworkKnowledge> {
        let text = std::fs::read_to_string(path).map_err(DomainError::infra)?;
        let fk = deserialize_knowledge(&text)
            .map_err(|e| DomainError::InvalidKnowledge(format!("{}: {e}", path.display())))?;
        if fk.id.is_empty() {
            return Err(DomainError::InvalidKnowledge(format!(
                "{}: 缺少 id 字段",
                path.display()
            )));
        }
        Ok(fk)
    }

    /// 从 YAML 文本反序列化（供测试与单文件装载使用）。
    pub fn from_str(text: &str) -> Result<FrameworkKnowledge> {
        deserialize_knowledge(text)
    }

    /// 按多个目录顺序装载（后者覆盖同 id 的条目）。
    pub fn load_dirs(dirs: &[PathBuf]) -> Result<Self> {
        let mut merged: Vec<FrameworkKnowledge> = Vec::new();
        let mut sources = Vec::new();
        for dir in dirs {
            let kb = Self::load_dir(dir)?;
            for fk in kb.entries {
                if let Some(existing) = merged.iter_mut().find(|e| e.id == fk.id) {
                    *existing = fk;
                } else {
                    merged.push(fk);
                }
            }
            sources.extend(kb.sources);
        }
        Ok(Self { entries: merged, sources })
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl KnowledgeProvider for YamlKnowledgeBase {
    fn all(&self) -> Vec<&FrameworkKnowledge> {
        self.entries.iter().collect()
    }

    fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
        self.entries.iter().find(|e| e.id == id)
    }

    fn for_language(&self, language: &Language) -> Vec<&FrameworkKnowledge> {
        self.entries.iter().filter(|e| e.language == *language).collect()
    }
}

/// YAML → [`FrameworkKnowledge`]。
///
/// 先解析成 `serde_yaml::Value`，再转成 `serde_json::Value` 后反序列化：
/// 因为 `serde_yaml` 0.9 对**外部标签枚举**（`- Annotate:` / `target: matched` /
/// `- strip_prefix: [...]`）只接受 YAML 标签（`!Annotate`）写法，而 FKB 需要
/// 更接近自然 YAML 的映射写法。
pub fn deserialize_knowledge(text: &str) -> Result<FrameworkKnowledge> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(text).map_err(DomainError::infra)?;
    let json = serde_json::to_value(yaml).map_err(DomainError::infra)?;
    serde_json::from_value(json).map_err(DomainError::infra)
}
