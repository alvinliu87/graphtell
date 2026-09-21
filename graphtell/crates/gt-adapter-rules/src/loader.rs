//! 规则装载：从 YAML 目录读取检查规则。

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::CheckRule;
use gt_domain::port::RuleProvider;
use serde::Deserialize;
use tracing::{info, warn};

/// 一份规则文件的顶层结构。
///
/// 两种写法都支持：
/// ```yaml
/// rules:
///   - id: xxx
/// ```
/// 或直接给一个数组（`- id: xxx`），便于一个文件只放一条规则。
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RuleFile {
    Named { rules: Vec<CheckRule> },
    Bare(Vec<CheckRule>),
}

impl RuleFile {
    fn into_vec(self) -> Vec<CheckRule> {
        match self {
            RuleFile::Named { rules } => rules,
            RuleFile::Bare(v) => v,
        }
    }
}

/// 基于文件目录的规则集实现。
///
/// 目录结构：`rules/<任意子目录>/*.yaml`，每份 YAML 描述若干条规则。
pub struct YamlRuleSet {
    rules: Vec<CheckRule>,
    sources: Vec<PathBuf>,
}

impl YamlRuleSet {
    /// 递归装载目录下的所有 `*.yaml` / `*.yml`。
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("规则目录不存在: {}", root.display());
            return Ok(Self { rules: Vec::new(), sources: Vec::new() });
        }
        let mut rules: Vec<CheckRule> = Vec::new();
        let mut sources: Vec<PathBuf> = Vec::new();
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
                    Ok(mut batch) => {
                        info!("装载规则: {} ({} 条)", path.display(), batch.len());
                        sources.push(path);
                        rules.append(&mut batch);
                    }
                    Err(e) => warn!("跳过损坏的规则文件 {}: {e}", path.display()),
                }
            }
        }
        Ok(Self::dedup(rules, sources))
    }

    /// 从 YAML 文本装载（供测试使用）。
    pub fn from_str(text: &str) -> Result<Self> {
        let file: RuleFile = serde_yaml::from_str(text)
            .map_err(|e| DomainError::InvalidKnowledge(format!("规则 YAML 解析失败: {e}")))?;
        let rules = file.into_vec();
        Self::validate(&rules)?;
        Ok(Self::dedup(rules, Vec::new()))
    }

    pub fn load_file(path: &Path) -> Result<Vec<CheckRule>> {
        let text = std::fs::read_to_string(path).map_err(DomainError::infra)?;
        let file: RuleFile = serde_yaml::from_str(&text).map_err(|e| {
            DomainError::InvalidKnowledge(format!("{}: 规则 YAML 解析失败: {e}", path.display()))
        })?;
        let rules = file.into_vec();
        Self::validate(&rules)?;
        Ok(rules)
    }

    /// 规则 id 必须唯一且非空；文案不能为空（否则 UI 上是一条没有价值的违规）。
    fn validate(rules: &[CheckRule]) -> Result<()> {
        for r in rules {
            if r.id.trim().is_empty() {
                return Err(DomainError::InvalidKnowledge("规则缺少 id 字段".into()));
            }
            if r.message.trim().is_empty() {
                return Err(DomainError::InvalidKnowledge(format!(
                    "规则 {} 缺少 message 字段",
                    r.id
                )));
            }
        }
        Ok(())
    }

    /// 同 id 后者覆盖前者（与 FKB 的装载语义一致）。
    fn dedup(rules: Vec<CheckRule>, sources: Vec<PathBuf>) -> Self {
        let mut merged: Vec<CheckRule> = Vec::new();
        for r in rules {
            if let Some(existing) = merged.iter_mut().find(|e| e.id == r.id) {
                *existing = r;
            } else {
                merged.push(r);
            }
        }
        Self { rules: merged, sources }
    }

    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }
}

impl RuleProvider for YamlRuleSet {
    fn rules(&self) -> &[CheckRule] {
        &self.rules
    }
}
