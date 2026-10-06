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
    /// in one FKB file (e.g. the removed legacy tag `kind: manifest_php`) therefore loses knowledge with
    /// zero test signal. Here we load the **real** `fkb/` tree file-by-file and fail loudly if any file
    /// cannot parse, and also catch duplicate `id`s (the loader does not dedupe within one dir, so a
    /// duplicate would be silently dropped downstream in the global rule collection).
    #[test]
    fn every_real_fkb_file_parses_and_ids_are_unique() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb");
        assert!(root.is_dir(), "the fkb directory must exist: {}", root.display());

        let files = collect_yaml(&root);
        assert!(!files.is_empty(), "the fkb directory must contain at least one yaml file");

        let mut ids = std::collections::HashSet::new();
        for f in &files {
            let fk = YamlKnowledgeBase::load_file(f)
                .unwrap_or_else(|e| panic!("the FKB file failed to parse (it would be skipped silently, losing knowledge): {}: {e}", f.display()));
            assert!(
                ids.insert(fk.id.clone()),
                "duplicate FKB id (one is dropped silently downstream): {}  ({})",
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
            "an illegal side must be rejected"
        );

        let good = dir.join("good.yaml");
        std::fs::write(
            &good,
            "id: good\ndisplay_name: Good\nlanguage: php\nside: backend\nrules: []\n",
        )
        .unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&good).is_ok(),
            "a legal side must pass validation"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A loader key the kernel does not read must be a **load error**, not a silent no-op.
    ///
    /// `format:` used to exist on `File` / `Glob` and was never consumed (the parser is chosen by the
    /// file's extension), and the retired `exclude_globs` sat in FKB exactly the same way. Without
    /// `deny_unknown_fields` both would still parse today and go on looking effective.
    #[test]
    fn loader_source_rejects_fields_the_kernel_never_reads() {
        let dir = std::env::temp_dir().join(format!(
            "gt-fkb-loader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let source = |extra: &str| {
            format!(
                "id: k\ndisplay_name: K\nlanguage: php\nloaders:\n  - id: event-listeners\n    table: event_listeners\n    from:\n      kind: file\n      path: \"{{app_root}}/event.php\"\n{extra}"
            )
        };

        let bad = dir.join("bad.yaml");
        std::fs::write(&bad, source("      format: php\n")).unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&bad).is_err(),
            "a `format:` the kernel never reads must be rejected (otherwise it keeps looking like it works)"
        );

        let good = dir.join("good.yaml");
        std::fs::write(&good, source("")).unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&good).is_ok(),
            "once the field is removed it must parse normally"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same guard one level down: a `where:` written **on the rule** instead of inside `selector` is not
    /// a rejected shape — it is silently ignored, and the rule then fires on every node of that kind.
    /// (`fkb/js/common.yaml` shipped exactly this: `mark-called` marked every HttpContract as
    /// `frontend.called` instead of only the ones with an incoming `CallsHttp` edge.)
    #[test]
    fn rule_rejects_unknown_fields() {
        let base = "id: k\ndisplay_name: K\nlanguage: php\nrules:\n  - id: mark-called\n    phase: AnnotatePost\n    selector:\n      kind: node\n      node_kind: HttpContract\n";
        let dir = std::env::temp_dir().join(format!(
            "gt-fkb-rule-{}-{}",
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
            format!("{base}    where:\n      - has_incoming: CallsHttp\n    binding: []\n"),
        )
        .unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&bad).is_err(),
            "a rule-level `where:` must be rejected (otherwise the rule matches unconditionally)"
        );

        let good = dir.join("good.yaml");
        std::fs::write(
            &good,
            format!("{base}      where:\n        - has_incoming: CallsHttp\n    binding: []\n"),
        )
        .unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&good).is_ok(),
            "a `where:` written into the selector must parse normally"
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
            "a side-aware kind must have side injected"
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
            "a non-side-aware kind must not have side injected"
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
            "an explicitly declared side wins over the FKB default"
        );
    }

    // ---- namespace_id: the pure prefixing rule behind every item id ----

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gt-fkb-loader-{}-{}-{}",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn namespace_id_covers_idempotency_and_edge_cases() {
        let mut id = String::from("pii");
        namespace_id("thinkphp", &mut id);
        assert_eq!(id, "thinkphp-pii");

        let mut already = String::from("thinkphp-pii");
        namespace_id("thinkphp", &mut already);
        assert_eq!(already, "thinkphp-pii", "an id that already carries the prefix must not be prefixed again");

        let mut same = String::from("thinkphp");
        namespace_id("thinkphp", &mut same);
        assert_eq!(same, "thinkphp", "an id equal to the prefix must not prefix itself");

        let mut empty = String::new();
        namespace_id("thinkphp", &mut empty);
        assert_eq!(empty, "", "an empty id must not be prefixed");

        // "contains" is not "starts with": a prefix occurring mid-string must still be namespaced.
        let mut contains = String::from("x-thinkphp");
        namespace_id("thinkphp", &mut contains);
        assert_eq!(contains, "thinkphp-x-thinkphp");
    }

    /// Every kind of item id must be namespaced — a forgotten loop here means two knowledge bases can
    /// collide on an id and one of them is silently dropped downstream.
    #[test]
    fn apply_id_namespaces_covers_every_item_kind() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: ns
language: php
rules:
  - id: r1
    phase: AnnotatePre
    selector: { kind: call, callee: "Db::name" }
    binding: []
loaders:
  - id: l1
    table: event_listeners
    from:
      kind: file
      path: "{app_root}/event.php"
root_rules:
  - id: root1
    key: app_root
    source:
      kind: directory_exists
      path: app
exclude_rules:
  - id: ex1
    glob: "__pycache__/**"
resolvers:
  - id: res1
    call: "app()->make|app"
    strategy: container
"#,
        )
        .unwrap();
        assert_eq!(fk.rules[0].id, "ns-r1");
        assert_eq!(fk.loaders[0].id, "ns-l1");
        assert_eq!(fk.root_rules[0].id, "ns-root1");
        assert_eq!(fk.exclude_rules[0].id, "ns-ex1");
        assert_eq!(fk.resolvers[0].id, "ns-res1");
    }

    // ---- load_dir / load_dirs contracts ----

    /// A missing FKB directory yields an **empty** knowledge base, not an error: callers that only check
    /// `!is_empty()` (container.rs) would otherwise never learn the directory was wrong.
    #[test]
    fn load_dir_missing_root_yields_empty_instead_of_error() {
        let kb = YamlKnowledgeBase::load_dir(Path::new("/no/such/fkb/dir/here"))
            .expect("a missing directory must not error, it just returns empty");
        assert!(kb.is_empty());
        assert_eq!(kb.len(), 0);
    }

    /// One corrupt file must be skipped (warn + continue) without losing the sibling files — this is the
    /// loader's resilience contract, and the flip side of `every_real_fkb_file_parses`.
    #[test]
    fn load_dir_skips_corrupt_file_and_keeps_the_good_one() {
        let dir = tmp_dir("mixed");
        std::fs::write(&dir.join("good.yaml"), "id: good\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&dir.join("bad.yaml"), "id: bad\nrules: [ unclosed\n").unwrap();
        let kb = YamlKnowledgeBase::load_dir(&dir).expect("a broken file must not fail the whole directory load");
        assert_eq!(kb.len(), 1, "only the one that parses must be loaded");
        assert_eq!(kb.sources().len(), 1);
        assert!(kb.by_id("good").is_some(), "the survivor must be good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `load_dirs` merges in order and a later directory **overrides** an entry with the same id.
    #[test]
    fn load_dirs_later_directory_overrides_same_id() {
        let a = tmp_dir("override_a");
        let b = tmp_dir("override_b");
        std::fs::write(&a.join("fkb.yaml"), "id: same\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&b.join("fkb.yaml"), "id: same\nlanguage: java\nrules: []\n").unwrap();

        let merged = YamlKnowledgeBase::load_dirs(&[a.clone(), b.clone()]).unwrap();
        assert_eq!(merged.len(), 1, "the same id must merge into one entry, with no duplicates");
        assert_eq!(
            merged.by_id("same").map(|f| f.language.clone()),
            Some(Language("java".into())),
            "the directory loaded later must override the entry with the same id"
        );

        let reversed = YamlKnowledgeBase::load_dirs(&[b, a]).unwrap();
        assert_eq!(
            reversed.by_id("same").map(|f| f.language.clone()),
            Some(Language("php".into())),
            "with the order reversed the later one wins"
        );
    }

    /// An FKB without `id` must be rejected: the id is the namespace prefix and the dedup key downstream.
    #[test]
    fn load_file_rejects_missing_id() {
        let dir = tmp_dir("noid");
        let p = dir.join("noid.yaml");
        std::fs::write(&p, "language: php\nrules: []\n").unwrap();
        assert!(
            YamlKnowledgeBase::load_file(&p).is_err(),
            "an FKB without an id must be rejected (the id is the namespace prefix and the de-duplication key)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- validate_exclude_rules: authoring feedback, never a load failure ----

    /// `validate_exclude_rules` only **warns**: a rule using `{value}` without a `source`, or a placeholder
    /// no `root_rules` key produces, must still load. A single bad rule must not drop the whole knowledge
    /// base — that is the entire point of warning instead of erroring.
    #[test]
    fn a_bad_exclude_rule_warns_but_does_not_fail_the_load() {
        let fk = YamlKnowledgeBase::from_str(
            r#"
id: excl
language: php
exclude_rules:
  - id: no-source
    glob: "{value}/**"
  - id: unknown-key
    glob: "{nope}/**"
  - id: fine
    glob: "vendor/**"
"#,
        )
        .expect("a broken exclude rule must only warn, not fail the whole FKB load");

        assert_eq!(fk.exclude_rules.len(), 3, "all three rules must be kept");
        // The rest of the load pipeline still ran: ids are namespaced as usual.
        assert_eq!(fk.exclude_rules[0].id, "excl-no-source");
    }

    // ---- the KnowledgeProvider surface the pipeline calls ----

    /// `for_language` filters by the FKB's own language, `by_id` is exact, and `all` sees everything.
    /// A `"*"` knowledge base is cross-language: it is not "of" any concrete language.
    #[test]
    fn knowledge_provider_serves_by_language_and_id() {
        let dir = tmp_dir("provider");
        std::fs::write(&dir.join("php.yaml"), "id: phpkb\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&dir.join("java.yaml"), "id: javakb\nlanguage: java\nrules: []\n").unwrap();
        std::fs::write(&dir.join("uni.yaml"), "id: unikb\nlanguage: \"*\"\nrules: []\n").unwrap();

        let kb = YamlKnowledgeBase::load_dir(&dir).expect("load ok");
        assert_eq!(kb.len(), 3);
        assert_eq!(kb.all().len(), 3);

        let ids = |lang: &str| -> Vec<String> {
            kb.for_language(&Language(lang.into()))
                .iter()
                .map(|f| f.id.clone())
                .collect()
        };
        assert_eq!(ids("php"), vec!["phpkb".to_string()], "for_language must return only that language's FKBs");
        assert_eq!(ids("java"), vec!["javakb".to_string()]);
        assert_eq!(ids("*"), vec!["unikb".to_string()], "a wildcard FKB appears only when querying with `*`");
        assert!(
            ids("python").is_empty(),
            "a wildcard FKB belongs to no concrete language, so it must not appear in a python query result"
        );

        assert!(kb.by_id("phpkb").is_some());
        assert!(kb.by_id("missing").is_none(), "an unknown id must return None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- load_dir file selection ----

    /// Selection rules: recursion into subdirectories, `.yml` accepted, the extension compared
    /// case-insensitively, and anything that is not YAML never even attempted (a stray `README.md`
    /// beside the knowledge must not become a "corrupt FKB" warning).
    #[test]
    fn load_dir_walks_subdirectories_and_selects_yaml_only() {
        let dir = tmp_dir("walk");
        std::fs::create_dir_all(dir.join("nested/deeper")).unwrap();
        std::fs::write(&dir.join("a.yaml"), "id: a\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&dir.join("nested/b.yml"), "id: b\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&dir.join("nested/deeper/c.YAML"), "id: c\nlanguage: php\nrules: []\n").unwrap();
        std::fs::write(&dir.join("README.md"), "not yaml").unwrap();
        std::fs::write(&dir.join("notes.txt"), "not yaml").unwrap();

        let kb = YamlKnowledgeBase::load_dir(&dir).expect("load ok");
        let mut got: Vec<String> = kb.all().iter().map(|f| f.id.clone()).collect();
        got.sort();
        assert_eq!(
            got,
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "yaml/yml must be collected recursively (upper-case extensions included), non-YAML files ignored"
        );
        assert_eq!(kb.sources().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
