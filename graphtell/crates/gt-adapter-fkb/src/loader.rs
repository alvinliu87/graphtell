//! FKB loading: read framework knowledge from a YAML directory.

use std::path::{Path, PathBuf};

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    Action, FieldSpec, FrameworkKnowledge, Language, SynthesizeAction, ValueSource,
};
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
        let mut fk = deserialize_knowledge(&text)
            .map_err(|e| DomainError::InvalidKnowledge(format!("{}: {e}", path.display())))?;
        if fk.id.is_empty() {
            return Err(DomainError::InvalidKnowledge(format!(
                "{}: missing the id field",
                path.display()
            )));
        }
        // A closed vocabulary keeps `side` comparable: every consumer (identity scope, the derived `sides` set,
        // `side_any` filters) compares plain strings, so free-form values would silently match nothing.
        if let Some(side) = &fk.side {
            if !matches!(side.as_str(), "frontend" | "backend" | "external") {
                return Err(DomainError::InvalidKnowledge(format!(
                    "{}: side must be one of frontend | backend | external, got `{side}`",
                    path.display()
                )));
            }
        }
        apply_id_namespaces(&mut fk);
        apply_side_defaults(&mut fk);
        validate_exclude_rules(&fk);
        Ok(fk)
    }

    /// Deserialize from YAML text (for tests and single-file loading).
    pub fn from_str(text: &str) -> Result<FrameworkKnowledge> {
        let mut fk = deserialize_knowledge(text)?;
        apply_id_namespaces(&mut fk);
        apply_side_defaults(&mut fk);
        validate_exclude_rules(&fk);
        Ok(fk)
    }
}

/// Check that every `{placeholder}` an `exclude_rules` entry references can actually be produced.
///
/// A placeholder that names no `root_rules` key can never resolve at P0: the rule would silently
/// degrade to its fallbacks, and the author would never learn why. Warn at load time — this is
/// knowledge-authoring feedback, not a runtime error (a single bad rule must not drop the whole FKB).
fn validate_exclude_rules(fk: &FrameworkKnowledge) {
    if fk.exclude_rules.is_empty() {
        return;
    }
    let keys: std::collections::HashSet<&str> =
        fk.root_rules.iter().map(|r| r.key.as_str()).collect();
    for rule in &fk.exclude_rules {
        let templates = std::iter::once(&rule.glob).chain(rule.fallbacks.iter());
        for template in templates {
            for name in gt_domain::model::template_placeholders(template) {
                if name == "value" && rule.source.is_none() {
                    warn!(
                        "FKB {}: exclude rule `{}` uses `{{value}}` but declares no `source`",
                        fk.id, rule.id
                    );
                } else if name != "value" && !keys.contains(name.as_str()) {
                    warn!(
                        "FKB {}: exclude rule `{}` references `{{{name}}}`, which no root_rule produces",
                        fk.id, rule.id
                    );
                }
            }
        }
    }
}

/// Node kinds for which a missing `side` is filled in from the knowledge base's declaration.
///
/// Restricted on purpose: apart from `HttpContract`, `side` is folded into the identity's **scope**
/// ([`IdentityKey::with_scope`]), i.e. it decides which nodes merge. Inheriting it into every kind would silently
/// re-key nodes that were never party-aware (Column, Schedule …). This list is exactly "the kinds that already
/// carry a `side` somewhere in the shipped knowledge", so inheritance can only fill gaps — including the real
/// omissions it was written for: `python/celery.yaml` (Queue) and `java/spring-boot.yaml` (ConfigKey / Event /
/// Queue) + `java/spring-cache.yaml` (Cache) declared none, which made their nodes invisible to every side filter.
const SIDE_AWARE_KINDS: &[&str] = &[
    "Cache",
    "ConfigKey",
    "Event",
    "EventBus",
    "HttpContract",
    "I18nKey",
    "Page",
    "Queue",
    "Store",
    "Table",
];

/// Namespace every **item id** of a knowledge base under its own `id`: `<fkb-id>-<local-id>`.
///
/// Ids are not private labels: a rule's id is written into the graph as provenance (a node's
/// `sources`, an edge's `evidence.rule`) and is the dedup key of `dedup_rules` / the global rule
/// collection, so it must be globally unique — two knowledge bases sharing an id means one of them
/// is **silently dropped**, decided by concatenation order. Hence the namespace.
///
/// But making the author repeat it on every one of forty rules is exactly how it gets forgotten
/// (the same argument that moved `side` from a per-rule literal to a top-level field), so the prefix
/// is **derived at load time** from `fk.id` instead of being written by hand.
///
/// Idempotent on purpose: an id that already starts with `<fkb-id>-` is left alone. That keeps a
/// hand-written fully-qualified id working (an escape hatch for a knowledge base that wants a
/// different prefix) and makes this a no-op for files that have not been migrated to short ids yet.
fn apply_id_namespaces(fk: &mut FrameworkKnowledge) {
    if fk.id.is_empty() {
        return;
    }
    let prefix = fk.id.clone();
    for rule in &mut fk.rules {
        namespace_id(&prefix, &mut rule.id);
    }
    for loader in &mut fk.loaders {
        namespace_id(&prefix, &mut loader.id);
    }
    for root in &mut fk.root_rules {
        namespace_id(&prefix, &mut root.id);
    }
    for rule in &mut fk.exclude_rules {
        namespace_id(&prefix, &mut rule.id);
    }
    for resolver in &mut fk.resolvers {
        namespace_id(&prefix, &mut resolver.id);
    }
}

fn namespace_id(prefix: &str, id: &mut String) {
    if id.is_empty() || *id == prefix || id.starts_with(&format!("{prefix}-")) {
        return;
    }
    *id = format!("{prefix}-{id}");
}

/// Fill in `side` for every synthesis action that should have one and does not.
fn apply_side_defaults(fk: &mut FrameworkKnowledge) {
    let Some(side) = fk.side.clone() else { return };
    for rule in &mut fk.rules {
        for action in &mut rule.binding {
            let Action::Synthesize(syn) = action else { continue };
            let syn: &mut SynthesizeAction = syn;
            let kind = syn.subtype.as_deref().unwrap_or_else(|| syn.node.as_str());
            if !SIDE_AWARE_KINDS.contains(&kind) {
                continue;
            }
            if syn.fields.iter().any(|f| f.name == "side") {
                continue; // an explicit declaration always wins
            }
            syn.fields.push(FieldSpec {
                name: "side".into(),
                value: Some(ValueSource { literal: Some(side.clone()), ..Default::default() }),
                ..Default::default()
            });
        }
    }
}

impl YamlKnowledgeBase {
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

    #[test]
    fn item_ids_are_namespaced_under_the_knowledge_base_id() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: thinkphp
language: php
rules:
  - id: pii
    phase: AnnotatePre
    selector: { kind: call, callee: "Db::name" }
    binding: []
resolvers:
  - id: container-make
    call: "app()->make|app"
    strategy: container
"#,
        )
        .unwrap();
        // The prefix is added at load time, so the author writes the local name only.
        assert_eq!(fk.rules[0].id, "thinkphp-pii");
        assert_eq!(fk.resolvers[0].id, "thinkphp-container-make");
    }

    #[test]
    fn namespacing_is_idempotent() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: thinkphp
language: php
rules:
  - id: thinkphp-pii
    phase: AnnotatePre
    selector: { kind: call, callee: "Db::name" }
    binding: []
"#,
        )
        .unwrap();
        // Already qualified: not prefixed twice (this is what lets a file opt out of the convention).
        assert_eq!(fk.rules[0].id, "thinkphp-pii");
    }

    // ---- loader invariants (silent-failure surface) ----

    /// Recursively collect every `*.yaml` / `*.yml` under a directory (mirrors `load_dir`'s walk).
    fn collect_yaml(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for entry in rd.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
                    .unwrap_or(false)
                {
                    out.push(path);
                }
            }
        }
        out
    }

    /// The single most valuable guard: `load_dir` **silently skips** any corrupt / un-deserialisable file
    /// (warn + continue, loader.rs:80), and `common/mod.rs` only checks `!is_empty()`. A typo'd enum variant
    /// in one FKB file (the historical `kind: manifest_php` miss) therefore loses knowledge with zero test
    /// signal. Here we load the **real** `fkb/` tree file-by-file and fail loudly if any file cannot parse,
    /// and also catch duplicate `id`s (the loader does not dedupe within one dir, so a duplicate would be
    /// silently dropped downstream in the global rule collection).
    #[test]
    fn every_real_fkb_file_parses_and_ids_are_unique() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb");
        assert!(root.is_dir(), "fkb 目录应存在: {}", root.display());

        let files = collect_yaml(&root);
        assert!(!files.is_empty(), "fkb 目录下应至少有 1 个 yaml 文件");

        let mut ids = std::collections::HashSet::new();
        for f in &files {
            let fk = YamlKnowledgeBase::load_file(f)
                .unwrap_or_else(|e| panic!("FKB 文件解析失败（会被静默跳过，导致知识丢失）: {}: {e}", f.display()));
            assert!(
                ids.insert(fk.id.clone()),
                "重复 FKB id（下游会静默丢弃一个）: {}  ({})",
                fk.id,
                f.display()
            );
        }
    }

    /// `side` is a closed vocabulary validated at load time; an unknown value must be rejected, not coerced.
    #[test]
    fn side_must_be_a_closed_vocabulary() {
        let dir = std::env::temp_dir().join(format!(
            "gt-fkb-side-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bad = dir.join("bad.yaml");
        std::fs::write(
            &bad,
            "id: bad\ndisplay_name: Bad\nlanguage: php\nside: sideways\nrules: []\n",
        )
        .unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&bad).is_err(),
            "非法的 side 必须被拒绝"
        );

        let good = dir.join("good.yaml");
        std::fs::write(
            &good,
            "id: good\ndisplay_name: Good\nlanguage: php\nside: backend\nrules: []\n",
        )
        .unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&good).is_ok(),
            "合法的 side 应通过校验"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn side_of_first_synth(fk: &FrameworkKnowledge) -> Option<String> {
        for rule in &fk.rules {
            for action in &rule.binding {
                if let Action::Synthesize(syn) = action {
                    if let Some(f) = syn.fields.iter().find(|f| f.name == "side") {
                        return f.value.as_ref().and_then(|v| v.literal.clone());
                    }
                }
            }
        }
        None
    }

    /// Side-aware kinds (HttpContract / Queue / Event / Cache …) must receive the FKB's `side` when they
    /// declare none — this is what makes those nodes visible to `side` filters.
    #[test]
    fn apply_side_defaults_injects_side_for_side_aware_kinds() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: demo
language: php
side: backend
rules:
  - id: cache-read
    phase: Synthesize
    selector: { kind: call, callee: "Cache::get" }
    binding:
      - Synthesize:
          node: Cache
          subtype: Cache
          identity: { kind: Named, value: { literal: "Cache" } }
          fields: []
          confidence: 0.9
"#,
        )
        .unwrap();
        assert_eq!(
            side_of_first_synth(&fk).as_deref(),
            Some("backend"),
            "side-aware kind 应被注入 side"
        );
    }

    /// Non-side-aware kinds (Method / Class …) must NOT inherit `side` — inheritance is restricted to
    /// `SIDE_AWARE_KINDS` on purpose.
    #[test]
    fn apply_side_defaults_skips_non_side_aware_kinds() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: demo
language: php
side: backend
rules:
  - id: meth
    phase: Synthesize
    selector: { kind: call, callee: "Foo::bar" }
    binding:
      - Synthesize:
          node: Method
          identity: { kind: Named, value: { literal: "Foo::bar" } }
          fields: []
          confidence: 0.9
"#,
        )
        .unwrap();
        assert_eq!(
            side_of_first_synth(&fk),
            None,
            "非 side-aware kind 不应被注入 side"
        );
    }

    /// An explicit per-rule `side` must win over the FKB default.
    #[test]
    fn explicit_side_wins_over_default() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: demo
language: php
side: backend
rules:
  - id: cache-read
    phase: Synthesize
    selector: { kind: call, callee: "Cache::get" }
    binding:
      - Synthesize:
          node: Cache
          subtype: Cache
          identity: { kind: Named, value: { literal: "Cache" } }
          fields:
            - name: side
              value: { literal: "frontend" }
          confidence: 0.9
"#,
        )
        .unwrap();
        assert_eq!(
            side_of_first_synth(&fk).as_deref(),
            Some("frontend"),
            "显式声明的 side 应优先于 FKB 默认值"
        );
    }
}
