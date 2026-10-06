//! P0: resolve the directories / files that must not be scanned — **before** the scan runs.
//!
//! # Why this exists (and why it is not a hard-coded directory list)
//!
//! Which directories are "not source code" is **framework knowledge**: ThinkPHP writes its cache and
//! logs under `runtime/`, Laravel compiles Blade into `storage/framework/views`, uni-app generates
//! `unpackage/`. Worse, the names are only conventions — a project may move `public/` to `web/`, point
//! ThinkPHP's runtime elsewhere, or rename Composer's `vendor-dir`. So the knowledge base declares
//! *rules* ([`ExcludeRule`]) whose globs are rendered from the project's own configuration, and the
//! kernel stays ignorant of every one of those names.
//!
//! # Why it must happen in P0
//!
//! Excluding after the scan would waste the whole parse (P2) and pollute the graph with thousands of
//! generated files first. P0 is the only phase that can still *not* ingest them — so resolution here
//! may only use sources that need no parsed code: manifests and directory probes, i.e. the same three
//! [`RootSource`] kinds `root_rules` uses (`super::resolve`).
//!
//! The price is that framework detection here is the **manifest-only subset** of P3's: a framework that
//! can only be recognised from code (imports / call sites) is detected too late to contribute excludes.
//! That asymmetry is acceptable precisely because an unresolved exclusion fails in the safe direction:
//! the directory is scanned, never silently dropped.

use std::collections::HashMap;
use std::path::Path;

use gt_domain::model::{
    template_placeholders, ExcludeRule, FrameworkKnowledge, Language,
};
use gt_domain::port::{FileSystem, KnowledgeProvider, ParserRegistry, TechStackRegistry};
use serde_json::{json, Value};

use super::facts;

/// A rule that could not be turned into a glob: reported, never guessed.
#[derive(Debug)]
pub struct ExcludeDiagnostic {
    pub code: String,
    pub message: String,
    pub location: Option<String>,
}

/// What one sub-project must keep out of the scan.
#[derive(Debug, Default)]
pub struct SubExcludes {
    /// Glob patterns **relative to the sub-project root** (the caller re-prefixes them for the scan,
    /// which is rooted at the project root).
    pub globs: Vec<String>,
    /// Provenance: which rule produced which glob, and from what. Written to the sub-project's facts.
    pub facts: Value,
    pub diagnostics: Vec<ExcludeDiagnostic>,
}

/// Resolve every applicable exclusion rule for one sub-project.
pub fn resolve_for_sub(
    project_root: &Path,
    sub_root: &Path,
    language: &Language,
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> SubExcludes {
    let ids = facts::detect_without_code(kb, fs, sub_root, project_root, language, techstack);
    // The tech-stack adapter only reads `root_path` / `language` to locate a manifest, so the
    // stand-in id is never observed anywhere.
    let sub = facts::provisional_sub(sub_root, language);

    let mut globs: Vec<String> = Vec::new();
    let mut facts: Vec<Value> = Vec::new();
    let mut diagnostics: Vec<ExcludeDiagnostic> = Vec::new();

    for id in ids {
        let Some(fk) = kb.by_id(&id) else { continue };
        for rule in &fk.exclude_rules {
            match resolve_rule(rule, fk, sub_root, project_root, &sub, fs, parsers, techstack) {
                Ok(r) => {
                    globs.push(r.glob.clone());
                    facts.push(json!({ "id": rule.id, "glob": r.glob, "source": r.source }));
                }
                Err(reason) => diagnostics.push(ExcludeDiagnostic {
                    code: "ExcludeRuleUnresolved".to_string(),
                    message: format!(
                        "the exclude rule `{}` of knowledge base {} could not be resolved: {reason}",
                        fk.id, rule.id
                    ),
                    location: Some(sub_root.to_string_lossy().to_string()),
                }),
            }
        }
    }

    SubExcludes {
        globs,
        facts: json!({ "excludes": facts }),
        diagnostics,
    }
}

/// One rule's outcome: the rendered glob plus how it was arrived at.
struct Resolved {
    glob: String,
    source: String,
}

fn resolve_rule(
    rule: &ExcludeRule,
    fk: &FrameworkKnowledge,
    sub_root: &Path,
    project_root: &Path,
    sub: &gt_domain::model::SubProject,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Result<Resolved, String> {
    // (1) the rule's own source -> `{value}`
    let own = rule.source.as_ref().and_then(|s| {
        facts::resolve_root_source(s, sub_root, project_root, sub, fs, parsers, techstack)
    });
    let mut values: HashMap<String, String> = HashMap::new();
    if let Some((v, _)) = &own {
        values.insert("value".to_string(), v.clone());
    }
    // (2) `{<root rule key>}` -> that rule's resolved value (`app_root`, `db_prefix` …)
    for name in template_placeholders(&rule.glob)
        .into_iter()
        .chain(rule.fallbacks.iter().flat_map(|f| template_placeholders(f)))
    {
        if values.contains_key(&name) {
            continue;
        }
        let Some(root_rule) = fk.root_rules.iter().find(|r| r.key == name) else {
            continue;
        };
        if let Some((v, _)) = facts::resolve_root_source(
            &root_rule.source,
            sub_root,
            project_root,
            sub,
            fs,
            parsers,
            techstack,
        ) {
            values.insert(name, v);
        }
    }

    // (3) the declared glob wins whenever it can be rendered
    if let Some(glob) = render(&rule.glob, &values) {
        let glob = sanitize(&glob)?;
        return Ok(Resolved {
            glob,
            source: format!("rule {} (resolved)", rule.id),
        });
    }

    // (4) otherwise the fallbacks — but only those whose directory really exists, so a stale
    //     convention can never delete a directory that merely happens to share its name.
    for candidate in &rule.fallbacks {
        let Some(glob) = render(candidate, &values) else { continue };
        let Ok(glob) = sanitize(&glob) else { continue };
        let Some(dir) = probe_dir(&glob) else { continue };
        if dir.is_empty() || fs.is_dir(&sub_root.join(&dir)) {
            return Ok(Resolved {
                glob,
                source: format!("fallback: {candidate}"),
            });
        }
    }

    Err("the placeholder cannot be resolved and no existing fallback directory was found".to_string())
}

/// Substitute every `{name}`; `None` when a placeholder has no value (a half-rendered glob would
/// exclude a directory literally called `{app_root}`, i.e. nothing — silently).
fn render(template: &str, values: &HashMap<String, String>) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let end = after.find('}')?;
        let name = after[..end].trim();
        let value = values.get(name)?;
        out.push_str(&rest[..start]);
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Normalise a rendered glob, refusing everything that would delete more than the rule meant to.
fn sanitize(glob: &str) -> Result<String, String> {
    let mut g = glob.trim().replace('\\', "/");
    while let Some(rest) = g.strip_prefix("./") {
        g = rest.to_string();
    }
    let g = g.trim_matches('/').to_string();
    if g.is_empty() || g == "**" || g == "." {
        // A bare `.` is the resolved form of `{app_root}` when the manifest points the autoload
        // root at the sub-project root itself. It would exclude every source file in the
        // sub-project, so it is refused — exactly like the two-segment `./**` form below. Without
        // this, `probe_dir(".")` returns `None` (single segment) and the root-guard check is skipped.
        return Err("the glob resolves to nothing or covers the whole sub-project".to_string());
    }
    let segments: Vec<&str> = g.split('/').collect();
    if segments.iter().any(|s| *s == ".." || s.is_empty()) {
        return Err(format!("glob `{g}` contains an illegal segment"));
    }
    // The dangerous case: a rule that resolves to the sub-project root itself (`{app_root}/**` with
    // `app_root = "."`) would exclude every source file. Refuse it outright.
    if let Some(dir) = probe_dir(&g) {
        if dir == "." || dir.is_empty() {
            return Err(format!("glob `{g}` would exclude an entire sub-project root; rejected"));
        }
    }
    Ok(g)
}

/// The directory a glob points at (`a/b/**` -> `a/b`); `None` for a bare file pattern (`*.log`).
fn probe_dir(glob: &str) -> Option<String> {
    let segments: Vec<&str> = glob.split('/').collect();
    if segments.len() < 2 {
        return None;
    }
    let dir: Vec<&str> = segments
        .iter()
        .take(segments.len() - 1)
        .copied()
        .filter(|s| !matches!(*s, "**" | "*"))
        .collect();
    if dir.is_empty() {
        return Some(String::new());
    }
    Some(dir.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_adapter_fkb::YamlKnowledgeBase;
    use gt_domain::model::{Detector, PickStrategy, RootRule, RootSource};

    struct StaticKb(Vec<FrameworkKnowledge>);

    impl KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gt-exclude-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn fk_with(exclude_rules: Vec<ExcludeRule>, root_rules: Vec<RootRule>) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: "tp".into(),
            display_name: "TP".into(),
            language: Language::new("php"),
            detectors: vec![Detector::ManifestDependency {
                manifest: "composer.json".into(),
                dependency: "topthink/framework".into(),
                confidence: 0.95,
            }],
            root_rules,
            exclude_rules,
            ..Default::default()
        }
    }

    fn app_root_rule() -> RootRule {
        RootRule {
            id: "app-root".into(),
            key: "app_root".into(),
            source: RootSource::ManifestJson {
                manifest: "composer.json".into(),
                pointer: "autoload.psr-4".into(),
                pick: PickStrategy::ShallowestDir,
            },
            confidence: 1.0,
            fallbacks: vec![],
        }
    }

    fn techstack() -> gt_domain::port::DefaultTechStackRegistry {
        gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()))
    }

    /// A framework with its own id / detector, so several of them can be detected side by side.
    fn fk_named(
        id: &str,
        dependency: &str,
        exclude_rules: Vec<ExcludeRule>,
        root_rules: Vec<RootRule>,
    ) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.into(),
            display_name: id.to_uppercase(),
            language: Language::new("php"),
            detectors: vec![Detector::ManifestDependency {
                manifest: "composer.json".into(),
                dependency: dependency.into(),
                confidence: 0.95,
            }],
            root_rules,
            exclude_rules,
            ..Default::default()
        }
    }

    /// A rule with a literal glob: no placeholder, no source, no fallback.
    fn rule_static(id: &str, glob: &str) -> ExcludeRule {
        ExcludeRule {
            id: id.into(),
            glob: glob.into(),
            source: None,
            fallbacks: vec![],
        }
    }

    /// A provider that **detects** a framework it cannot then serve — pins the
    /// `let Some(fk) = kb.by_id(&id) else { continue }` arm.
    struct DanglingKb(FrameworkKnowledge);

    impl KnowledgeProvider for DanglingKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            vec![&self.0]
        }
        fn by_id(&self, _: &str) -> Option<&FrameworkKnowledge> {
            None
        }
    }

    fn resolve_sync(root: &Path, kb: &dyn KnowledgeProvider) -> SubExcludes {
        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let ts = techstack();
        resolve_for_sub(root, root, &Language::new("php"), kb, &fs, &parsers, &ts)
    }

    /// Resolve exclude rules against a **real** `KnowledgeProvider` (e.g. the shipped FKB), reusing the
    /// same synthetic-fs / parser / techstack stack as `resolve_sync`.
    fn resolve_real(
        root: &Path,
        language: &Language,
        kb: &dyn KnowledgeProvider,
        ts: &dyn TechStackRegistry,
    ) -> SubExcludes {
        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        resolve_for_sub(root, root, language, kb, &fs, &parsers, ts)
    }

    /// The whole point: `runtime/` is found **through the resolved `app_root`**, not by its name.
    #[test]
    fn glob_is_rendered_from_the_resolved_app_root() {
        let root = scratch("app_root");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "autoload": {"psr-4": {"app\\": "app/"}}}"#,
        );
        write(&root, "app/runtime/cache/x.php", "<?php");
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "runtime-dir".into(),
                glob: "{app_root}/runtime/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            vec![app_root_rule()],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["app/runtime/**".to_string()]);
        assert!(got.diagnostics.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Fallbacks are guesses, so they apply only when the directory exists — a stale convention must
    /// never exclude a directory that merely shares its name.
    #[test]
    fn fallback_applies_only_when_the_directory_exists() {
        let root = scratch("fallback");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^6"}}"#);
        write(&root, "public/index.php", "<?php");
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "public-dir".into(),
                glob: "{value}/**".into(),
                source: Some(RootSource::ManifestJson {
                    manifest: "composer.json".into(),
                    pointer: "extra.public-dir".into(),
                    pick: PickStrategy::FirstDir,
                }),
                fallbacks: vec!["public/**".into(), "web/**".into()],
            }],
            vec![],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["public/**".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Nothing resolvable and nothing existing: report it instead of inventing a glob.
    #[test]
    fn unresolved_rule_reports_a_diagnostic() {
        let root = scratch("missing");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^6"}}"#);
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "runtime-dir".into(),
                glob: "{app_root}/runtime/**".into(),
                source: None,
                fallbacks: vec!["runtime/**".into()],
            }],
            vec![app_root_rule()],
        )]);
        let got = resolve_sync(&root, &kb);
        assert!(got.globs.is_empty());
        assert_eq!(got.diagnostics.len(), 1);
        assert_eq!(got.diagnostics[0].code, "ExcludeRuleUnresolved");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A rule that resolves to the sub-project root would delete every source file — refused.
    #[test]
    fn rule_that_would_exclude_the_whole_sub_project_is_refused() {
        let root = scratch("suicide");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "autoload": {"psr-4": {"app\\": "./"}}}"#,
        );
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "everything".into(),
                glob: "{app_root}/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            vec![app_root_rule()],
        )]);
        let got = resolve_sync(&root, &kb);
        assert!(got.globs.is_empty(), "must not exclude an entire sub-project root: {:?}", got.globs);
        assert_eq!(got.diagnostics.len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A bare static declaration (no placeholder) is taken as written — TP5's `thinkphp/` framework
    /// source has no configuration to read it from.
    #[test]
    fn static_declaration_is_taken_as_written() {
        let root = scratch("static");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^5"}}"#);
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "framework-src".into(),
                glob: "thinkphp/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            vec![],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["thinkphp/**".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// End-to-end guard for the **real shipped FKB** — the 5 unit tests above inject a hand-built
    /// `StaticKb`, so a typo / dangling placeholder in the real `fkb/php/thinkphp.yaml` `exclude_rules`
    /// would never be caught there. And `ingest` treats an unresolved rule as a non-fatal diagnostic,
    /// so it would also slip past the full-pipeline sample tests. Here we load the real FKB and resolve
    /// its rules against a synthetic-but-realistic ThinkPHP tree, then assert every rule resolves (zero
    /// diagnostics) and produces exactly the expected globs.
    ///
    /// `runtime/` is materialised (not `config/app.php`) so `runtime-dir` resolves via its fallback —
    /// the manifest-source path is already pinned by `glob_is_rendered_from_the_resolved_app_root`.
    #[test]
    fn real_thinkphp_fkb_exclude_rules_resolve_without_diagnostics() {
        let root = scratch("real-fkb");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "autoload": {"psr-4": {"app\\": "app/"}}, "extra": {"public-dir": "public"}}"#,
        );
        for d in ["runtime", "public", "app", "extend", "thinkphp"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }

        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php/thinkphp.yaml");
        assert!(fkb_path.is_file(), "the real FKB must exist: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("the real FKB failed to parse: {}: {e}", fkb_path.display()));
        let kb = StaticKb(vec![real_fk]);

        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let ts = techstack();
        let got = resolve_for_sub(
            &root,
            &root,
            &Language::new("php"),
            &kb,
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            got.diagnostics.is_empty(),
            "the real FKB exclude rules must have no unresolved entries: {:?}",
            got.diagnostics
        );
        for expected in [
            "extend/**",
            "public/**",
            "runtime/**",
            "thinkphp/**",
        ] {
            assert!(
                got.globs.iter().any(|g| g == expected),
                "the real FKB must yield `{expected}`, got: {:?}",
                got.globs
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real-FKB guard for a **JS/TS** framework: `fkb/js/nestjs.yaml`'s `exclude_rules` are all static
    /// globs (`dist/**`, `coverage/**`), so they resolve with no manifest source and no fallback dir. This
    /// pins that a real non-PHP FKB parses and its rules survive end-to-end — the hand-built `StaticKb`
    /// unit tests never exercise a real JS/TS FKB.
    #[test]
    fn real_js_framework_fkb_exclude_rules_resolve_without_diagnostics() {
        let root = scratch("real-fkb-js-framework");
        write(
            &root,
            "package.json",
            r#"{"name": "svc", "dependencies": {"@nestjs/core": "^10", "typescript": "^5"}}"#,
        );

        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/js/nestjs.yaml");
        assert!(fkb_path.is_file(), "the real FKB must exist: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("the real FKB failed to parse: {}: {e}", fkb_path.display()));
        let kb = StaticKb(vec![real_fk]);

        // JS adapter registered for faithful detection; the plain-text fallback in `detect_without_code`
        // would also match `@nestjs/core`, so detection is robust either way.
        let ts = gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()))
            .register(Box::new(gt_adapter_techstack::JsTechStackAdapter::new()));
        let got = resolve_real(&root, &Language::new("javascript"), &kb, &ts);

        assert!(
            got.diagnostics.is_empty(),
            "the real FKB exclude rules must have no unresolved entries: {:?}",
            got.diagnostics
        );
        for expected in ["coverage/**", "dist/**"] {
            assert!(
                got.globs.iter().any(|g| g == expected),
                "the real FKB must yield `{expected}`, got: {:?}",
                got.globs
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real-FKB guard for a **Python** framework: `fkb/python/django.yaml`'s `exclude_rules` are all static
    /// globs (`**/__pycache__/**`, `**/.venv/**`, `**/venv/**`, `**/migrations/**`). Detection keys off
    /// `requirements.txt` containing `django`; there is no Python tech-stack adapter, so `detect_without_code`
    /// falls back to a plain-text probe — exactly the real pipeline's behaviour, and enough for the static
    /// globs to resolve.
    #[test]
    fn real_django_fkb_exclude_rules_resolve_without_diagnostics() {
        let root = scratch("real-fkb-django");
        write(&root, "requirements.txt", "django==5.0\n");

        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/python/django.yaml");
        assert!(fkb_path.is_file(), "the real FKB must exist: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("the real FKB failed to parse: {}: {e}", fkb_path.display()));
        let kb = StaticKb(vec![real_fk]);

        // No Python adapter exists, so detection relies on the plain-text probe — faithful to the real
        // pipeline; the PHP adapter registered here is irrelevant but harmless.
        let ts = gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_techstack::PhpTechStackAdapter::new()));
        let got = resolve_real(&root, &Language::new("python"), &kb, &ts);

        assert!(
            got.diagnostics.is_empty(),
            "the real FKB exclude rules must have no unresolved entries: {:?}",
            got.diagnostics
        );
        for expected in [
            "**/__pycache__/**",
            "**/.venv/**",
            "**/venv/**",
            "**/migrations/**",
        ] {
            assert!(
                got.globs.iter().any(|g| g == expected),
                "the real FKB must yield `{expected}`, got: {:?}",
                got.globs
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real-FKB guard for **Laravel** — the one framework whose `exclude_rules` mix a `manifest_json`
    /// source (`public-dir` reads `composer.json`'s `extra.public-dir`) with static globs
    /// (`storage/framework/**`, `storage/logs/**`, `bootstrap/cache/**`). This pins that the real FKB's
    /// *manifest-sourced* rule resolves end-to-end (not just the static/fallback paths the thinkphp and
    /// JS/TS and Python framework tests exercise), with zero diagnostics.
    #[test]
    fn real_laravel_fkb_exclude_rules_resolve_without_diagnostics() {
        let root = scratch("real-fkb-laravel");
        write(
            &root,
            "composer.json",
            r#"{"require": {"laravel/framework": "^10"}, "autoload": {"psr-4": {"App\\": "app/"}}, "extra": {"public-dir": "public"}}"#,
        );

        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php/laravel.yaml");
        assert!(fkb_path.is_file(), "the real FKB must exist: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("the real FKB failed to parse: {}: {e}", fkb_path.display()));
        let kb = StaticKb(vec![real_fk]);

        let ts = techstack();
        let got = resolve_real(&root, &Language::new("php"), &kb, &ts);

        assert!(
            got.diagnostics.is_empty(),
            "the real FKB exclude rules must have no unresolved entries: {:?}",
            got.diagnostics
        );
        for expected in [
            "bootstrap/cache/**",
            "public/**",
            "storage/framework/**",
            "storage/logs/**",
        ] {
            assert!(
                got.globs.iter().any(|g| g == expected),
                "the real FKB must yield `{expected}`, got: {:?}",
                got.globs
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- pure-helper direct tests: `render` / `sanitize` / `probe_dir` ----
    // The integration tests above only exercise these three through `resolve_for_sub`, so their
    // silent-failure branches (render -> None, sanitize -> traversal / whole-root refusal) are left
    // unpinned. These tests isolate them with no fs / kb.

    /// `render` must return `None` — never a half-rendered string — when a placeholder is
    /// unresolvable; otherwise the rule would silently exclude a directory literally named
    /// `{app_root}` (i.e. nothing).
    #[test]
    fn render_returns_none_when_placeholder_unresolved() {
        let values = std::collections::HashMap::<String, String>::new();
        assert!(render("{app_root}/runtime/**", &values).is_none());
    }

    /// `render` substitutes a known placeholder and leaves the rest of the template intact.
    #[test]
    fn render_substitutes_known_placeholder() {
        let mut values = std::collections::HashMap::<String, String>::new();
        values.insert("app_root".to_string(), "app".to_string());
        assert_eq!(
            render("{app_root}/runtime/**", &values),
            Some("app/runtime/**".to_string())
        );
    }

    /// `sanitize` refuses path traversal, a bare `**` (whole subtree), and a glob whose directory
    /// collapses to the sub-project root — while still normalising `./x/` -> `x`.
    #[test]
    fn sanitize_rejects_traversal_wildcard_and_root() {
        assert!(sanitize("a/../b").is_err()); // path traversal
        assert!(sanitize("**").is_err()); // whole subtree
        assert!(sanitize("**/**").is_err()); // directory collapses to root
        assert_eq!(sanitize("./x/"), Ok("x".to_string())); // normalisation
    }

    /// `probe_dir` returns `None` for a bare file pattern (single segment, e.g. `*.log`) and the
    /// parent directory for a multi-segment glob.
    #[test]
    fn probe_dir_none_for_single_segment_glob() {
        assert_eq!(probe_dir("*.log"), None);
        assert_eq!(probe_dir("a/b/**"), Some("a/b".to_string()));
    }

    /// A rule's *own* `source` (not a root rule) must populate `{value}` and render the glob. The static
    /// test uses a literal glob, and the others feed `{value}` from a *root rule*, so this is the only
    /// path that exercises `rule.source -> own value`.
    #[test]
    fn rule_own_source_fills_value_and_renders() {
        let root = scratch("own-source");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "extra": {"public-dir": "public"}}"#,
        );
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "public-dir".into(),
                glob: "{value}/**".into(),
                source: Some(RootSource::ManifestJson {
                    manifest: "composer.json".into(),
                    pointer: "extra.public-dir".into(),
                    pick: PickStrategy::FirstDir,
                }),
                fallbacks: vec![],
            }],
            vec![],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["public/**".to_string()]);
        assert!(got.diagnostics.is_empty());
        // Provenance records which rule produced the glob (the `facts` array is otherwise unasserted).
        let facts = got.facts.get("excludes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].get("id").and_then(|v| v.as_str()), Some("public-dir"));
        assert_eq!(facts[0].get("glob").and_then(|v| v.as_str()), Some("public/**"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A glob placeholder that matches no root rule (and the rule has no own `source`) can never render,
    /// and with no fallbacks must be reported rather than silently dropped — pins the `else { continue }`
    /// when a placeholder name has no matching root rule.
    #[test]
    fn glob_placeholder_with_no_matching_root_rule_reports() {
        let root = scratch("no-root-rule");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^6"}}"#);
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "bogus".into(),
                glob: "{bogus}/x/**".into(),
                source: None,
                fallbacks: vec![],
            }],
            vec![], // no root rule named `bogus`
        )]);
        let got = resolve_sync(&root, &kb);
        assert!(got.globs.is_empty());
        assert_eq!(got.diagnostics.len(), 1);
        assert_eq!(got.diagnostics[0].code, "ExcludeRuleUnresolved");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `sanitize` must also reject an empty glob (collapses to nothing) and a glob with an empty path
    /// segment (double slash) — both branches the single `sanitize_rejects_traversal_wildcard_and_root`
    /// test leaves unpinned.
    #[test]
    fn sanitize_rejects_empty_and_empty_segment() {
        assert!(sanitize("").is_err()); // collapses to nothing
        assert!(sanitize("   ").is_err()); // whitespace-only
        assert!(sanitize("a//b/**").is_err()); // empty segment from a double slash
    }

    /// `probe_dir` returns `Some("")` (an empty directory) for a glob whose only directory component is a
    /// wildcard — the `dir.is_empty()` arm, which the integration tests never reach directly.
    #[test]
    fn probe_dir_empty_directory_for_double_wildcard() {
        assert_eq!(probe_dir("**/**"), Some(String::new()));
        assert_eq!(probe_dir("a/*.log"), Some("a".to_string()));
    }

    /// `render`: an unclosed `{` (no matching `}`) must yield `None` (never a half-rendered string), and a
    /// placeholder name with surrounding whitespace must be trimmed so `{ app_root }` matches `app_root`.
    #[test]
    fn render_unclosed_brace_and_whitespace_name() {
        let empty = std::collections::HashMap::<String, String>::new();
        assert!(render("{app_root/runtime/**", &empty).is_none()); // no closing brace
        let mut values = std::collections::HashMap::<String, String>::new();
        values.insert("app_root".to_string(), "app".to_string());
        assert_eq!(
            render("{ app_root }/runtime/**", &values),
            Some("app/runtime/**".to_string())
        );
    }

    // ===================================================================
    // Negative coverage: detection gaps, fallback-chain filtering, guard refusals
    //
    // Everything below pins branches that produce **nothing** (or that must refuse to produce
    // anything), plus the precedence rules inside the fallback chain. They never show up in the
    // happy path, and failing in the wrong direction here means silently deleting source from the scan.
    // ===================================================================

    #[test]
    fn nothing_detected_yields_no_globs_and_empty_provenance() {
        let root = scratch("no-detection");
        // A manifest that exists but declares none of the detector's dependencies.
        write(&root, "composer.json", r#"{"require": {"some/other": "^1"}}"#);
        let kb = StaticKb(vec![fk_with(vec![rule_static("runtime-dir", "runtime/**")], vec![])]);
        let got = resolve_sync(&root, &kb);
        assert!(got.globs.is_empty(), "an undetected framework contributes no rules");
        assert!(
            got.diagnostics.is_empty(),
            "not detecting a framework is not an error — only an *unresolved* rule is"
        );
        assert_eq!(got.facts, json!({ "excludes": [] }), "provenance stays empty");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_detected_framework_the_provider_cannot_serve_is_skipped() {
        let root = scratch("dangling-id");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^6"}}"#);
        let kb = DanglingKb(fk_with(vec![rule_static("runtime-dir", "runtime/**")], vec![]));
        let got = resolve_sync(&root, &kb);
        assert!(got.globs.is_empty(), "a dangling id must be skipped, not guessed at");
        assert!(
            got.diagnostics.is_empty(),
            "and it must not be reported — nothing was left unresolved, nothing exists to exclude"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_detected_framework_contributes_its_own_rules() {
        let root = scratch("multi-fk");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6", "vendor/plugin": "^2"}}"#,
        );
        let kb = StaticKb(vec![
            fk_named("tp", "topthink/framework", vec![rule_static("runtime-dir", "runtime/**")], vec![]),
            fk_named("plugin", "vendor/plugin", vec![rule_static("generated-dir", "generated/**")], vec![]),
        ]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs.len(), 2, "both frameworks must contribute: {:?}", got.globs);
        assert!(got.globs.contains(&"runtime/**".to_string()));
        assert!(got.globs.contains(&"generated/**".to_string()));
        assert!(got.diagnostics.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn identical_rules_from_two_frameworks_are_not_deduplicated() {
        // Each detected framework's rules are appended verbatim; a later de-duplication would be a
        // deliberate change, so the duplication is pinned rather than hidden.
        let root = scratch("dup-fk");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6", "vendor/plugin": "^2"}}"#,
        );
        let kb = StaticKb(vec![
            fk_named("tp", "topthink/framework", vec![rule_static("runtime-dir", "runtime/**")], vec![]),
            fk_named("plugin", "vendor/plugin", vec![rule_static("cache-dir", "runtime/**")], vec![]),
        ]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(
            got.globs,
            vec!["runtime/**".to_string(), "runtime/**".to_string()],
            "the same glob declared twice reaches the caller twice"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fallback_chain_skips_traversal_file_pattern_and_missing_dirs() {
        let root = scratch("fallback-chain");
        write(&root, "composer.json", r#"{"require": {"topthink/framework": "^6"}}"#);
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::create_dir_all(root.join("later")).unwrap();
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "public-dir".into(),
                glob: "{missing}/**".into(), // unrenderable on purpose
                source: None,
                fallbacks: vec![
                    "../../outside/**".into(), // `sanitize` refuses (path traversal)
                    "*.log".into(),            // `probe_dir` -> None for a bare file pattern
                    "nope/**".into(),          // renders fine, but the directory does not exist
                    "web/**".into(),           // <- accepted
                    "later/**".into(),         // unreachable: the first existing candidate wins
                ],
            }],
            vec![],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["web/**".to_string()], "only the first *usable* fallback wins");
        assert!(got.diagnostics.is_empty());
        let facts = got.facts.get("excludes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            facts[0].get("source").and_then(|v| v.as_str()),
            Some("fallback: web/**"),
            "provenance must say it came from a fallback"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fallback_placeholders_are_resolved_from_root_rules() {
        // The fallback list is part of the placeholder scan, so a fallback may itself depend on `app_root`.
        let root = scratch("fallback-placeholder");
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "autoload": {"psr-4": {"app\\": "app/"}}}"#,
        );
        std::fs::create_dir_all(root.join("app/runtime")).unwrap();
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "runtime-dir".into(),
                glob: "{missing}/**".into(),
                source: None,
                fallbacks: vec!["{app_root}/runtime/**".into()],
            }],
            vec![app_root_rule()],
        )]);
        let got = resolve_sync(&root, &kb);
        assert_eq!(got.globs, vec!["app/runtime/**".to_string()]);
        let facts = got.facts.get("excludes").and_then(|v| v.as_array()).unwrap();
        assert_eq!(
            facts[0].get("source").and_then(|v| v.as_str()),
            Some("fallback: {app_root}/runtime/**"),
            "the provenance records the *template* it came from, not the rendered glob"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_refused_main_glob_does_not_fall_back() {
        let root = scratch("no-fallback-after-refusal");
        // `app_root` collapses to the sub-project root, so `{app_root}/**` is refused by `sanitize`.
        write(
            &root,
            "composer.json",
            r#"{"require": {"topthink/framework": "^6"}, "autoload": {"psr-4": {"app\\": "./"}}}"#,
        );
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        let kb = StaticKb(vec![fk_with(
            vec![ExcludeRule {
                id: "suicide".into(),
                glob: "{app_root}/**".into(),
                source: None,
                fallbacks: vec!["runtime/**".into()],
            }],
            vec![app_root_rule()],
        )]);
        let got = resolve_sync(&root, &kb);
        // `let glob = sanitize(&glob)?` propagates out of the whole rule: once the *declared* glob is
        // refused, the fallback list is never consulted. Pinned because trying the fallback is equally
        // plausible — and would let a mis-configured rule quietly exclude a different directory.
        assert!(got.globs.is_empty(), "the fallback must not rescue a refused glob: {:?}", got.globs);
        assert_eq!(got.diagnostics.len(), 1);
        assert!(
            got.diagnostics[0].message.contains("covers the whole sub-project"),
            "the diagnostic must report the real refusal reason, got: {}",
            got.diagnostics[0].message
        );
        assert_eq!(
            got.diagnostics[0].location.as_deref(),
            Some(root.to_string_lossy().as_ref()),
            "and point at the sub-project root"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_normalises_separators_and_strips_leading_slashes() {
        assert_eq!(
            sanitize("app\\runtime\\**"),
            Ok("app/runtime/**".to_string()),
            "Windows separators are normalised"
        );
        assert_eq!(
            sanitize("/vendor/**"),
            Ok("vendor/**".to_string()),
            "a leading slash is trimmed: globs are always sub-root relative, so an absolute-looking \
             declaration is reinterpreted rather than escaping upwards"
        );
        assert_eq!(sanitize("././x/**"), Ok("x/**".to_string()), "every leading `./` is stripped");
        assert_eq!(sanitize("a/**"), Ok("a/**".to_string()));
    }

    /// The bare-dot hole is closed: a single segment `.` (the resolved form of `{app_root}` when the
    /// autoload root points at the sub-project root) would exclude every source file, so `sanitize`
    /// refuses it — symmetry with the two-segment `./**` form. An *interior* `.` (`a/./b/**`) is a
    /// no-op segment, not the root, and stays valid.
    #[test]
    fn sanitize_refuses_a_bare_dot_that_would_exclude_the_root() {
        assert!(sanitize(".").is_err(), "a bare `.` would exclude the entire sub-project root");
        assert_eq!(
            sanitize("a/./b/**"),
            Ok("a/./b/**".to_string()),
            "an interior `.` is a no-op segment, not the root"
        );
        assert!(sanitize("./**").is_err(), "the two-segment form of the same thing is refused");
    }

    #[test]
    fn render_needs_every_placeholder_and_treats_empty_as_resolved() {
        let mut values = std::collections::HashMap::new();
        values.insert("a".to_string(), "x".to_string());
        values.insert("b".to_string(), "y".to_string());
        assert_eq!(render("{a}/{b}/**", &values), Some("x/y/**".to_string()));
        values.remove("b");
        assert!(
            render("{a}/{b}/**", &values).is_none(),
            "one unresolved placeholder kills the whole template"
        );
        // An empty value counts as resolved: the directory level silently disappears ...
        values.insert("b".to_string(), String::new());
        assert_eq!(render("{a}/{b}/**", &values), Some("x//**".to_string()));
        // ... and `sanitize` catches it afterwards, so no wrong directory is excluded.
        assert!(sanitize("x//**").is_err());
    }

    #[test]
    fn probe_dir_skips_wildcard_segments_but_needs_two_segments() {
        assert_eq!(probe_dir("**"), None, "a single-segment glob has no directory part");
        assert_eq!(probe_dir("a/**"), Some("a".to_string()));
        assert_eq!(
            probe_dir("a/*/c/**"),
            Some("a/c".to_string()),
            "a wildcard level is skipped, not treated as a literal directory name"
        );
        assert_eq!(probe_dir("a/b/c/**"), Some("a/b/c".to_string()));
    }
}
