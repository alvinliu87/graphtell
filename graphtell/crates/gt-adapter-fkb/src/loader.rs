//! FKB loading: read framework knowledge from a YAML directory.

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{FrameworkKnowledge, Language};
use gt_domain::port::KnowledgeProvider;
use tracing::{info, warn};

/// A file-directory-based FKB provider.
///
/// Directory layout: `fkb/<any subdirectory>/*.yaml`, each YAML describing one framework.
pub struct YamlKnowledgeBase {
    entries: Vec<FrameworkKnowledge>,
    sources: Vec<PathBuf>,
}

impl YamlKnowledgeBase {
    /// Recursively load every `*.yaml` / `*.yml` under the directory.
    pub fn load_dir(root: &Path) -> Result<Self> {
        if !root.exists() {
            warn!("FKB directory does not exist: {}", root.display());
            return Ok(Self { entries: Vec::new(), sources: Vec::new() });
        }
        // Cross-language universal recognition rules (cache / config …) now live as ordinary `selector +
        // binding` rules in a shared, always-loaded FKB (`fkb/universal/common.yaml`, `apply_without_detection`
        // with `language: "*"`), so there is no per-language template instantiation left here.
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
                        info!("loading FKB: {} ({})", fk.id, path.display());
                        // **Semantic node kinds** declared by FKB are registered here: adding a semantic node
                        // (a front-end `Store`, a page `Page`…) needs no change to the kernel list.
                        if !fk.semantic_kinds.is_empty() {
                            gt_domain::model::kinds::register_semantic_kinds(
                                fk.semantic_kinds.iter().cloned(),
                            );
                        }
                        // **Semantic / bridge edge kinds** declared by FKB are registered here: adding an edge kind
                        // (such as some framework's `SendsWebhook`) needs no change to the `SEMANTIC` / `BRIDGE`
                        // lists in `kinds.rs` — isomorphic to the node registration.
                        if !fk.semantic_edge_kinds.is_empty() || !fk.bridge_edge_kinds.is_empty() {
                            gt_domain::model::kinds::register_edge_kinds(
                                fk.semantic_edge_kinds.iter().cloned(),
                                fk.bridge_edge_kinds.iter().cloned(),
                            );
                        }
                        // **Business-specific annotation kinds** declared by FKB are registered here: adding a
                        // business annotation semantics needs no kernel change (isomorphic to `semantic_kinds` /
                        // `semantic_edge_kinds`). Kernel-standard kinds (pii / data.criticality / auth.public …)
                        // are built-in and need no registration.
                        if !fk.annotation_kinds.is_empty() {
                            gt_domain::model::kinds::register_annotation_kinds(
                                fk.annotation_kinds.iter().cloned(),
                            );
                        }
                        entries.push(fk);
                        sources.push(path);
                    }
                    Err(e) => warn!("skipping a corrupt FKB {}: {e}", path.display()),
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
                "{}: missing the id field",
                path.display()
            )));
        }
        Ok(fk)
    }

    /// Deserialize from YAML text (for tests and single-file loading).
    pub fn from_str(text: &str) -> Result<FrameworkKnowledge> {
        deserialize_knowledge(text)
    }

    /// Load from several directories in order (later ones override entries with the same id).
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

/// YAML -> [`FrameworkKnowledge`].
///
/// Parse into `serde_yaml::Value` first, then convert to `serde_json::Value` before deserializing:
/// because `serde_yaml` 0.9 only accepts the YAML-tag spelling (`!Annotate`) for **externally tagged enums**
/// (`- Annotate:` / `target: matched` / `- strip_prefix: [...]`), while FKB needs a mapping spelling that stays
/// closer to natural YAML.
pub fn deserialize_knowledge(text: &str) -> Result<FrameworkKnowledge> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(text).map_err(DomainError::infra)?;
    let json = serde_json::to_value(yaml).map_err(DomainError::infra)?;
    serde_json::from_value(json).map_err(DomainError::infra)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::fkb::{Rule, Selector};
    use gt_domain::model::kinds::Phase;
    use gt_domain::model::Language;

    fn rule_with_langs(langs: Option<Vec<Language>>) -> Rule {
        Rule {
            id: "r".into(),
            phase: Phase("Synthesize".into()),
            selector: Selector::Call { callee: None, r#where: vec![] },
            binding: vec![],
            confidence: 0.9,
            languages: langs,
        }
    }

    #[test]
    fn universal_rule_applies_to_any_language() {
        let r = rule_with_langs(Some(vec![Language("*".into())]));
        assert!(r.applies_to(&Language("php".into()), &Language("java".into())));
        assert!(r.applies_to(&Language("php".into()), &Language("php".into())));
    }

    #[test]
    fn subset_rule_scoped_to_listed_languages() {
        let r = rule_with_langs(Some(vec![Language("java".into()), Language("kotlin".into())]));
        assert!(r.applies_to(&Language("java".into()), &Language("java".into())));
        assert!(!r.applies_to(&Language("java".into()), &Language("php".into())));
    }

    #[test]
    fn inherited_rule_uses_fk_language() {
        let r = rule_with_langs(None);
        assert!(r.applies_to(&Language("php".into()), &Language("php".into())));
        assert!(!r.applies_to(&Language("php".into()), &Language("java".into())));
    }

    #[test]
    fn parses_universal_yaml_with_languages_field() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: t
language: "*"
scope: framework
apply_without_detection: true
rules:
  - id: u
    languages: ["*"]
    phase: Synthesize
    selector: { kind: call, callee: "Cache::get" }
    binding:
      - Synthesize:
          node: ExternalSystem
          subtype: Cache
          identity: { kind: Named, value: { literal: "Cache" } }
          link: { kind: ReadsCache, direction: incoming }
"#,
        )
        .unwrap();
        assert_eq!(fk.rules.len(), 1);
        assert!(fk.rules[0].applies_to(&Language("*".into()), &Language("python".into())));
    }
}
