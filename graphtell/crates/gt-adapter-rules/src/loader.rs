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

    /// Rule ids must be unique and non-empty; the set must contain at least one rule, and each rule's
    /// `message` (the copy shown to the user) must be non-empty — otherwise the UI surfaces a worthless,
    /// empty violation. A `$key` referencing an undeclared parameter is also rejected (it silently
    /// degrades to 0 / "" at evaluation time).
    fn validate(rules: &[CheckRule]) -> Result<()> {
        if rules.is_empty() {
            return Err(DomainError::InvalidKnowledge(
                "a rule file must declare at least one rule".into(),
            ));
        }
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

    /// The bare-array form (`- id: ...`, no `rules:` wrapper) is a documented convenience for single-rule files.
    #[test]
    fn bare_array_yaml_is_loaded() {
        let yaml = "- id: lone-rule\n  title: Lone\n  message: \"hi\"\n";
        let set = YamlRuleSet::from_str(yaml).expect("bare array should parse");
        assert_eq!(set.rules().len(), 1);
        assert_eq!(set.rules()[0].id, "lone-rule");
    }

    #[test]
    fn empty_id_is_rejected() {
        let yaml = "rules:\n  - id: \"\"\n    title: t\n    message: \"m\"\n";
        let err = YamlRuleSet::from_str(yaml).expect_err("empty id must be rejected");
        assert!(err.to_string().contains("id"), "actual error: {err}");
    }

    /// An empty message renders as a worthless violation in the UI — catching it at load time avoids shipping that.
    #[test]
    fn empty_message_is_rejected() {
        let yaml = "rules:\n  - id: r1\n    title: t\n    message: \"\"\n";
        let err = YamlRuleSet::from_str(yaml).expect_err("empty message must be rejected");
        assert!(err.to_string().contains("message"), "actual error: {err}");
    }

    /// Same id: the later declaration overrides the earlier (matches FKB loading semantics).
    #[test]
    fn duplicate_id_keeps_last() {
        let yaml = "rules:\n  - id: dup\n    title: t\n    message: \"first\"\n  - id: dup\n    title: t\n    message: \"second\"\n";
        let set = YamlRuleSet::from_str(yaml).expect("duplicate ids are allowed, deduped");
        assert_eq!(set.rules().len(), 1, "duplicate id should collapse to one rule");
        assert_eq!(set.rules()[0].message, "second", "later declaration should win");
    }

    #[test]
    fn load_dir_recurses_into_subdirs() {
        let root = std::env::temp_dir().join(format!("gtar_rules_sub_{}", std::process::id()));
        let sub = root.join("group/a");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            root.join("root.yaml"),
            "rules:\n  - id: in-root\n    title: t\n    message: \"m\"\n",
        )
        .unwrap();
        std::fs::write(
            sub.join("nested.yaml"),
            "rules:\n  - id: in-sub\n    title: t\n    message: \"m\"\n",
        )
        .unwrap();

        let set = YamlRuleSet::load_dir(&root).expect("load_dir should succeed");
        let ids: Vec<&str> = set.rules().iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"in-root"), "root-level rule missing: {ids:?}");
        assert!(ids.contains(&"in-sub"), "subdirectory rule missing (recursion broken): {ids:?}");
        assert_eq!(set.sources().len(), 2, "both files should be recorded as sources");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn load_dir_missing_directory_is_empty() {
        let missing = std::env::temp_dir().join(format!("gtar_rules_none_{}", std::process::id()));
        let set = YamlRuleSet::load_dir(&missing).expect("missing dir returns Ok");
        assert!(set.rules().is_empty(), "missing dir should yield zero rules");
        assert!(set.sources().is_empty());
    }

    /// A single corrupt file must not take down the whole rule set: it is skipped (with a warning) and the
    /// other files still load.
    #[test]
    fn load_dir_skips_corrupt_file_but_keeps_others() {
        let root = std::env::temp_dir().join(format!("gtar_rules_corrupt_{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("good.yaml"),
            "rules:\n  - id: good\n    title: t\n    message: \"m\"\n",
        )
        .unwrap();
        std::fs::write(root.join("bad.yaml"), "this: : is: not: valid: yaml: [\n").unwrap();

        let set = YamlRuleSet::load_dir(&root).expect("load_dir should tolerate a corrupt file");
        assert_eq!(set.rules().len(), 1, "the good file should still load");
        assert_eq!(set.rules()[0].id, "good");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `load_file` is a public entry point (used directly by `graphtell validate`-style callers), so its own
    /// error paths need covering — not just through `load_dir`.
    #[test]
    fn load_file_reports_errors_for_missing_and_corrupt_files() {
        let root = std::env::temp_dir().join(format!("gtar_rules_file_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        assert!(
            YamlRuleSet::load_file(&root.join("nope.yaml")).is_err(),
            "a missing file must error, not return empty"
        );

        let bad = root.join("bad.yaml");
        std::fs::write(&bad, "rules: [ unclosed").unwrap();
        let err = YamlRuleSet::load_file(&bad).expect_err("bad YAML must error");
        assert!(
            err.to_string().contains("bad.yaml"),
            "the error message must carry the file path so it can be located: {err}"
        );

        let good = root.join("good.yaml");
        std::fs::write(&good, "rules:\n  - id: ok\n    title: t\n    message: \"m\"\n").unwrap();
        let rules = YamlRuleSet::load_file(&good).expect("a good file must load normally");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].id, "ok");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Extension filtering is by extension only: `.yaml` / `.yml` (any case) are loaded, everything else is
    /// left alone — a stray `.md` / `.txt` in the rules directory must not be parsed.
    #[test]
    fn load_dir_skips_non_yaml_and_accepts_yml_variants() {
        let root = std::env::temp_dir().join(format!("gtar_rules_ext_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let rule = |id: &str| format!("rules:\n  - id: {id}\n    title: t\n    message: \"m\"\n");
        std::fs::write(root.join("a.yaml"), rule("from-yaml")).unwrap();
        std::fs::write(root.join("b.yml"), rule("from-yml")).unwrap();
        std::fs::write(root.join("c.YAML"), rule("from-upper")).unwrap();
        std::fs::write(root.join("notes.md"), "# not a rule file").unwrap();
        std::fs::write(root.join("README.txt"), "not a rule file").unwrap();

        let set = YamlRuleSet::load_dir(&root).expect("load_dir should succeed");
        let mut ids: Vec<&str> = set.rules().iter().map(|r| r.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["from-upper", "from-yaml", "from-yml"], "only yaml/yml must be collected (any case), everything else excluded: {ids:?}");
        assert_eq!(set.sources().len(), 3, "only 3 files must be recorded as sources");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An empty rule set (`rules: []`) is rejected, not loaded as a worthless, empty set — and the same check
    /// applies through `load_file`.
    #[test]
    fn empty_rule_set_is_rejected() {
        let err = YamlRuleSet::from_str("rules: []\n")
            .expect_err("an empty rule set must be rejected");
        assert!(
            err.to_string().contains("at least one rule"),
            "actual error: {err}"
        );

        let root =
            std::env::temp_dir().join(format!("gtar_rules_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let f = root.join("empty.yaml");
        std::fs::write(&f, "rules: []\n").unwrap();
        let err = YamlRuleSet::load_file(&f).expect_err("load_file must reject an empty rule set");
        assert!(err.to_string().contains("at least one rule"), "actual error: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `from_str` must reject structurally malformed YAML (not just semantically invalid rules) — the parse
    /// error path (`DomainError::InvalidKnowledge`) is distinct from the `validate` path.
    #[test]
    fn from_str_rejects_malformed_yaml() {
        let err = YamlRuleSet::from_str("rules: [ unclosed")
            .expect_err("malformed YAML should be rejected");
        assert!(err.to_string().contains("rules YAML"), "actual error: {err}");
    }

    /// `load_dir` on a path that exists but is a file (not a directory) hits the `read_dir` error path and
    /// must return `Err` (infra), not silently yield zero rules.
    #[test]
    fn load_dir_on_a_file_path_errors() {
        let f = std::env::temp_dir().join(format!("gtar_rules_filepath_{}", std::process::id()));
        std::fs::write(&f, "not a dir").unwrap();
        let res = YamlRuleSet::load_dir(&f);
        assert!(res.is_err(), "calling load_dir on a file must return an error, not an empty set: {res:?}");
        let _ = std::fs::remove_file(&f);
    }

    /// `load_file` runs the same `validate` as `from_str`, but that call site (returning an
    /// `InvalidKnowledge` error for a structurally-valid-but-invalid rule) needs its own coverage.
    #[test]
    fn load_file_rejects_an_invalid_rule() {
        let root = std::env::temp_dir().join(format!("gtar_rules_file_invalid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bad = root.join("empty_id.yaml");
        std::fs::write(&bad, "rules:\n  - id: \"\"\n    title: t\n    message: \"m\"\n").unwrap();
        let err = YamlRuleSet::load_file(&bad).expect_err("load_file must reject a rule with an empty id");
        assert!(err.to_string().contains("id"), "actual error: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `load_dir` must also tolerate a file that parses to valid YAML but fails `validate` (e.g. an empty
    /// `message`), not just a YAML-syntax error — otherwise a single semantically-broken rule file in a user or
    /// built-in directory would silently drop the rest. Both failure kinds hit the same `warn!` skip branch.
    #[test]
    fn load_dir_skips_validation_failing_file_but_keeps_others() {
        let root = std::env::temp_dir().join(format!("gtar_rules_valfail_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("good.yaml"),
            "rules:\n  - id: good\n    title: t\n    message: \"m\"\n",
        )
        .unwrap();
        // Valid YAML, but the rule has an empty message -> validate() rejects it.
        std::fs::write(
            root.join("broken.yaml"),
            "rules:\n  - id: broken\n    title: t\n    message: \"\"\n",
        )
        .unwrap();

        let set = YamlRuleSet::load_dir(&root).expect("load_dir must tolerate a validation failure");
        assert_eq!(set.rules().len(), 1, "only the valid file should load");
        assert_eq!(set.rules()[0].id, "good");
        assert_eq!(set.sources().len(), 1, "the broken file must not be recorded as a source");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Blank input is not valid YAML (not a mapping or sequence) and must be rejected on the parse path,
    /// distinct from the `validate` "at least one rule" path.
    #[test]
    fn from_str_rejects_blank_input() {
        let err = YamlRuleSet::from_str("").expect_err("blank input must be rejected");
        assert!(err.to_string().contains("rules YAML"), "actual error: {err}");
    }

    /// A bare `[]` (no `rules:` wrapper) is the empty-set case for the `Bare` variant of `RuleFile`, and must be
    /// rejected by `validate` exactly like `rules: []`.
    #[test]
    fn from_str_rejects_empty_bare_array() {
        let err = YamlRuleSet::from_str("[]\n").expect_err("an empty bare array must be rejected");
        assert!(err.to_string().contains("at least one rule"), "actual error: {err}");
    }

    /// `load_file` on a directory (not a file) must error on the read path, not silently return an empty rule list.
    #[test]
    fn load_file_on_a_directory_errors() {
        let dir = std::env::temp_dir().join(format!("gtar_rules_dir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let res = YamlRuleSet::load_file(&dir);
        assert!(
            res.is_err(),
            "load_file on a directory must error, not yield an empty list: {res:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
