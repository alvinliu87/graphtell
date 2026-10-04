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
                        "知识库 {} 的排除规则 `{}` 未能解析：{reason}",
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

    Err("占位符无法解析，且没有存在的 fallback 目录".to_string())
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
    if g.is_empty() || g == "**" {
        return Err("glob 解析后为空或覆盖整个子项目".to_string());
    }
    let segments: Vec<&str> = g.split('/').collect();
    if segments.iter().any(|s| *s == ".." || s.is_empty()) {
        return Err(format!("glob `{g}` 含非法片段"));
    }
    // The dangerous case: a rule that resolves to the sub-project root itself (`{app_root}/**` with
    // `app_root = "."`) would exclude every source file. Refuse it outright.
    if let Some(dir) = probe_dir(&g) {
        if dir == "." || dir.is_empty() {
            return Err(format!("glob `{g}` 会排除整个子项目根，已拒绝"));
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
            .register(Box::new(gt_adapter_php::PhpTechStackAdapter::new()))
    }

    fn resolve_sync(
        root: &Path,
        kb: &StaticKb,
    ) -> SubExcludes {
        let fs = gt_adapter_fs::StdFileSystem::new();
        let parsers = gt_adapter_parser::DefaultParserRegistry::new();
        let ts = techstack();
        resolve_for_sub(root, root, &Language::new("php"), kb, &fs, &parsers, &ts)
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
        assert!(got.globs.is_empty(), "不得排除整个子项目根: {:?}", got.globs);
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
}
