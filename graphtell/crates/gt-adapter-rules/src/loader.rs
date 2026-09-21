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
#[derive(Debug)]
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
            // `$key` 引用了没声明的参数：求值时静默退化成 0 / ""，
            // 用在 `limit` 上就是候选集直接变空 —— 必须装载时就拦下。
            let missing = r.undeclared_params();
            if !missing.is_empty() {
                return Err(DomainError::InvalidKnowledge(format!(
                    "规则 {} 引用了未声明的参数: {}（请在 params: 下声明，或改写字面量）",
                    r.id,
                    missing.join(", ")
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

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
rules:
  - id: hot-table
    title: 热点表
    severity: info
    params:
      - key: min_fan_in
        label: 扇入阈值
        kind: number
        default: 50
    applies_to: { kinds: [Table] }
    when: [{ fan_in_gte: "$min_fan_in" }]
    message: "表 {name} 热"
"#;

    const MISSING: &str = r#"
rules:
  - id: hot-table
    title: 热点表
    severity: info
    applies_to: { kinds: [Table], limit: "$max_nodes" }
    when: [{ fan_in_gte: 50 }]
    message: "表 {name} 热"
"#;

    #[test]
    fn 声明过的参数引用可以通过校验() {
        let set = YamlRuleSet::from_str(OK).expect("参数已声明，应能装载");
        assert_eq!(set.rules().len(), 1);
        assert!(set.rules()[0].undeclared_params().is_empty());
    }

    #[test]
    fn 未声明的参数引用会被拒绝() {
        // `limit: "$max_nodes"` 求值时退化成 0 → 候选集直接变空（规则静默 0 命中）。
        // 这类错误必须在装载阶段就报错，而不是等用户把"0 违规"读成"代码干净"。
        let err = YamlRuleSet::from_str(MISSING).expect_err("引用未声明参数应被拒绝");
        assert!(err.to_string().contains("max_nodes"), "实际错误: {err}");
    }

    #[test]
    fn 内置规则集全部合法() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rules");
        let set = YamlRuleSet::load_dir(&root).expect("内置规则目录应能装载");
        assert!(!set.rules().is_empty(), "内置规则不应为空");
        for r in set.rules() {
            assert!(
                r.undeclared_params().is_empty(),
                "规则 {} 引用了未声明的参数: {:?}",
                r.id,
                r.undeclared_params()
            );
        }
    }
}
