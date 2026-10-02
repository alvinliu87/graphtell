//! Rule loading: read check rules from a YAML directory.

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::CheckRule;
use gt_domain::port::RuleProvider;
use serde::Deserialize;
use tracing::{info, warn};

/// The top-level structure of a rule file.
///
/// Both spellings are supported:
/// ```yaml
/// rules:
///   - id: xxx
/// ```
/// or a bare array (`- id: xxx`), handy when a file holds exactly one rule.
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

/// A file-directory-based rule set.
///
/// Directory layout: `rules/<any subdirectory>/*.yaml`, each YAML describing several rules.
#[derive(Debug)]
pub struct YamlRuleSet {
    rules: Vec<CheckRule>,
    sources: Vec<PathBuf>,
}

impl YamlRuleSet {
    /// Recursively load every `*.yaml` / `*.yml` under the directory.
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("rules directory does not exist: {}", root.display());
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
                        info!("loading rules: {} ({} of them)", path.display(), batch.len());
                        sources.push(path);
                        rules.append(&mut batch);
                    }
                    Err(e) => warn!("skipping a corrupt rules file {}: {e}", path.display()),
                }
            }
        }
        Ok(Self::dedup(rules, sources))
    }

    /// Load from YAML text (for tests).
    pub fn from_str(text: &str) -> Result<Self> {
        let file: RuleFile = serde_yaml::from_str(text)
            .map_err(|e| DomainError::InvalidKnowledge(format!("failed to parse the rules YAML: {e}")))?;
        let rules = file.into_vec();
        Self::validate(&rules)?;
        Ok(Self::dedup(rules, Vec::new()))
    }

    pub fn load_file(path: &Path) -> Result<Vec<CheckRule>> {
        let text = std::fs::read_to_string(path).map_err(DomainError::infra)?;
        let file: RuleFile = serde_yaml::from_str(&text).map_err(|e| {
            DomainError::InvalidKnowledge(format!("{}: failed to parse the rules YAML: {e}", path.display()))
        })?;
        let rules = file.into_vec();
        Self::validate(&rules)?;
        Ok(rules)
    }

    /// Rule ids must be unique and non-empty; the copy must not be empty (otherwise the UI shows a worthless violation).
    fn validate(rules: &[CheckRule]) -> Result<()> {
        for r in rules {
            if r.id.trim().is_empty() {
                return Err(DomainError::InvalidKnowledge("a rule is missing the id field".into()));
            }
            if r.message.trim().is_empty() {
                return Err(DomainError::InvalidKnowledge(format!(
                    "rule {} is missing the message field",
                    r.id
                )));
            }
            // A `$key` referencing an undeclared parameter silently degrades to 0 / "" at evaluation time, which on
            // `limit` means the candidate set becomes empty outright — it must be rejected at load time.
            let missing = r.undeclared_params();
            if !missing.is_empty() {
                return Err(DomainError::InvalidKnowledge(format!(
                    "rule {} references an undeclared parameter: {} (declare it under params:, or write a literal instead)",
                    r.id,
                    missing.join(", ")
                )));
            }
        }
        Ok(())
    }

    /// Same id: the later one overrides the earlier (consistent with FKB loading semantics).
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
    title: Hot table
    severity: info
    params:
      - key: min_fan_in
        label: Fan-in threshold
        kind: number
        default: 50
    applies_to: { kinds: [Table] }
    when: [{ fan_in_gte: "$min_fan_in" }]
    message: "table {name} is hot"
"#;

    const MISSING: &str = r#"
rules:
  - id: hot-table
    title: Hot table
    severity: info
    applies_to: { kinds: [Table], limit: "$max_nodes" }
    when: [{ fan_in_gte: 50 }]
    message: "table {name} is hot"
"#;

    #[test]
    fn a_declared_parameter_reference_passes_validation() {
        let set = YamlRuleSet::from_str(OK).expect("the parameter is declared, so it should load");
        assert_eq!(set.rules().len(), 1);
        assert!(set.rules()[0].undeclared_params().is_empty());
    }

    #[test]
    fn an_undeclared_parameter_reference_is_rejected() {
        // `limit: "$max_nodes"` degrades to 0 at evaluation -> the candidate set becomes empty (the rule silently
        // reports 0 hits). This class of error must fail at load time, not wait for the user to read "0 violations"
        // as "the code is clean".
        let err = YamlRuleSet::from_str(MISSING).expect_err("referencing an undeclared parameter should be rejected");
        assert!(err.to_string().contains("max_nodes"), "actual error: {err}");
    }

    #[test]
    fn the_built_in_rule_set_is_entirely_valid() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rules");
        let set = YamlRuleSet::load_dir(&root).expect("the built-in rules directory should load");
        assert!(!set.rules().is_empty(), "the built-in rules must not be empty");
        for r in set.rules() {
            assert!(
                r.undeclared_params().is_empty(),
                "rule {} references undeclared parameters: {:?}",
                r.id,
                r.undeclared_params()
            );
        }
    }
}
