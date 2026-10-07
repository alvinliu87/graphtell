#![allow(unused_imports)]
use super::*;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use gt_domain::error::Result;
use gt_domain::model::{
    Action, Detector, FactValue, FrameworkKnowledge, GuardAttachSpec, KnowledgeScope, Language,
    NormalizeStep, Phase, RootSource, RouteCallSpec, RouteGuardSpec, RouteMatchBy, Rule,
    SubProjectId, SynthesizedKind,
};
use gt_domain::port::{
    AdapterFact, FileSystem, KnowledgeProvider, LanguageParser, ParserRegistry, TechStackRegistry,
};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::context::PipelineContext;
use crate::engine::{callee_matches, path_matches};
use crate::normalize::strip_prefixes;
use crate::phase::facts::{self, expand_provided, lock_has, manifest_has};
use crate::workspace::{CallRecord, RouteGroup, RouteGuard, RouteGuardScope};
pub(crate) fn run_loaders(
    ctx: &mut PipelineContext,
    fk: &FrameworkKnowledge,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
    techstack: &dyn TechStackRegistry,
) {
    let contract_steps = contract_steps_of(fk);
    for loader in &fk.loaders {
        if let Err(e) = run_loader(
            ctx,
            loader,
            sub,
            project_root,
            fs,
            parsers,
            phase,
            &contract_steps,
            fk,
            techstack,
        ) {
            warn!("loader {} failed: {e}", loader.id);
        }
    }
}

/// Take the contract-path normalization steps from one FKB (the first `ContractId` rule).
///
/// When not obtainable, fall back to `[LeadingSlash]`: at least keeps the leading-slash convention consistent, won't write a half-key.
pub(crate) fn contract_steps_of(fk: &FrameworkKnowledge) -> Vec<NormalizeStep> {
    for rule in &fk.rules {
        for action in &rule.binding {
            let Action::Synthesize(s) = action else { continue };
            if s.identity.kind.as_str() != SynthesizedKind::CONTRACT_ID {
                continue;
            }
            if let Some(steps) = s.identity.path.as_ref().and_then(|p| p.normalize.clone()) {
                if !steps.is_empty() {
                    return steps;
                }
            }
        }
    }
    vec![NormalizeStep::LeadingSlash]
}

/// Choose the parser **per file**: prefer the extension to determine language, fall back to the sub-project language when not obtainable.
///
/// Loaders process concrete files (`config/database.php`, `app/event.php`, `lang/zh-cn/*.php`),
/// the language should be decided by the file itself. Hard-coding one language would silently break all loaders when a new language is added
/// (no error, just load nothing) — this is where "kernel knows no concrete language" lands in P3.
pub(crate) fn parser_for_file<'a>(
    parsers: &'a dyn ParserRegistry,
    fallback_sub: Option<&gt_domain::model::SubProject>,
    path: &Path,
) -> Option<&'a dyn LanguageParser> {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        if let Some(p) = parsers
            .language_for_extension(ext)
            .and_then(|lang| parsers.parser_for(&lang))
        {
            return Some(p);
        }
    }
    fallback_sub.and_then(|sub| parsers.parser_for(&sub.language))
}

pub(crate) fn run_loader(
    ctx: &mut PipelineContext,
    loader: &gt_domain::model::LoaderSpec,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
    contract_steps: &[NormalizeStep],
    fk: &FrameworkKnowledge,
    techstack: &dyn TechStackRegistry,
) -> Result<()> {
    let app_root = app_root_of(ctx, sub);

    match &loader.from {
        gt_domain::model::LoaderSource::File { path, key_path, .. } => {
            let rel = path.replace("{app_root}", &app_root);
            let abs = sub.root_path.join(&rel);
            let abs = if abs.exists() { abs } else { project_root.join(&rel) };
            if !abs.exists() {
                return Ok(());
            }
            let text = fs.read_to_string(&abs)?;
            let Some(parser) = parser_for_file(parsers, Some(sub), &abs) else {
                return Ok(());
            };
            let rel_display = abs
                .strip_prefix(project_root)
                .unwrap_or(&abs)
                .to_string_lossy()
                .replace('\\', "/");
            match parser.parse(&rel_display, &text) {
                Ok(facts) => {
                    for entry in &facts.config_entries {
                        let key = match key_path {
                            Some(prefix) => match entry.key_path.strip_prefix(&format!("{}.", prefix)) {
                                Some(rest) => rest.to_string(),
                                None => continue,
                            },
                            None => entry.key_path.clone(),
                        };
                        if key.is_empty() {
                            continue;
                        }
                        ctx.ws.put_symbol(
                            ctx.project.id,
                            &loader.table,
                            &key,
                            json!({
                                "value": entry.value,
                                "file": rel_display,
                                "line": entry.span.start_line,
                            }),
                        );
                    }
                }
                Err(e) => warn!("failed to parse {}: {e}", rel_display),
            }
        }
        gt_domain::model::LoaderSource::Glob { pattern, locale_regex, .. } => {
            load_i18n(
                ctx,
                loader,
                pattern,
                project_root,
                fs,
                parsers,
                locale_regex.as_deref(),
                techstack,
            );
        }
        gt_domain::model::LoaderSource::Inline { rows } => {
            for row in rows {
                // The key field name depends on the table: `facade_map` uses `facade`, generic tables use `key`,
                // class-list tables (e.g. `middleware_classes`) use `class` — all up to FKB.
                let key = row
                    .get("facade")
                    .or_else(|| row.get("key"))
                    .or_else(|| row.get("class"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if key.is_empty() {
                    continue;
                }
                ctx.ws.put_symbol(ctx.project.id, &loader.table, &key, row.clone());
            }
        }
        gt_domain::model::LoaderSource::Builtin { name, params } => {
            run_builtin(
                ctx,
                name,
                params,
                sub,
                project_root,
                fs,
                parsers,
                phase,
                contract_steps,
                fk,
                techstack,
            );
        }
    }
    Ok(())
}

pub(crate) fn load_i18n(
    ctx: &mut PipelineContext,
    loader: &gt_domain::model::LoaderSpec,
    pattern: &str,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    locale_regex: Option<&str>,
    techstack: &dyn TechStackRegistry,
) {
    let files: Vec<gt_domain::model::SourceFile> = ctx.files.clone();

    for file in files {
        if !path_matches(pattern, &file.path) {
            continue;
        }
        // ① FKB's own `locale_regex` wins (it is the most specific, and it can name any capture group);
        // ② otherwise the **stack's** path convention (`lang/{locale}/*.php`, `locales/{locale}/*.json` …)
        // as declared by its tech-stack adapter. A stack that declares neither yields no locale — the
        // i18n table then simply has no entries for it, rather than guessing one.
        let locale = match locale_regex {
            Some(re) => match regex::Regex::new(re) {
                Ok(re) => re
                    .captures(&file.path)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string()),
                Err(_) => None,
            },
            None => None,
        };
        let fallback_to_stack = locale.is_none() && locale_regex.is_none();
        let locale = match locale {
            Some(l) => Some(l),
            None if fallback_to_stack => {
                facts::locale_of_path(&file.path, &file.language, techstack)
            }
            None => None,
        };
        let Some(locale) = locale else { continue };
        let abs = project_root.join(&file.path);
        let Ok(text) = fs.read_to_string(&abs) else { continue };
        let Some(parser) = parser_for_file(parsers, None, &abs) else {
            continue;
        };
        let Ok(facts) = parser.parse(&file.path, &text) else {
            continue;
        };
        let stem = std::path::Path::new(&file.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        for entry in &facts.config_entries {
            let key = if stem.is_empty() {
                entry.key_path.clone()
            } else {
                format!("{}.{}", stem, entry.key_path)
            };
            let text_value = match &entry.value {
                gt_domain::model::FactValue::String(s) => s.clone(),
                other => other.to_string_lossy(),
            };
            let mut merged = ctx
                .ws
                .get_symbol(&loader.table, &key)
                .cloned()
                .unwrap_or_else(|| json!({ "texts": {} }));
            if let Some(texts) = merged.get_mut("texts").and_then(|t| t.as_object_mut()) {
                texts.insert(locale.clone(), json!(text_value));
            }
            merged["file"] = json!(file.path);
            ctx.ws.put_symbol(ctx.project.id, &loader.table, &key, merged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::model::{
        ConfigEntryFact, FactValue, FileId, FrameworkKnowledge, Language, LoaderSource, LoaderSpec,
        NormalizeStep, Phase, Project, ProjectId, ProjectStatus, Rule, SourceFile, Span,
        SubProject, SubProjectId, SyntaxFacts,
    };
    use gt_domain::port::{
        DefaultTechStackRegistry, FileSystem, LanguageParser, ParserRegistry,
    };
    use gt_adapter_fs::StdFileSystem;
    use serde_json::{json, Value};
    use std::path::{Path, PathBuf};

    // ------------------------------------------------------- test doubles

    /// A parser that returns a fixed set of config entries regardless of file content — used to exercise
    /// the `File` loader's key_path filtering without depending on a real language parser.
    struct ConfigStubParser;
    impl LanguageParser for ConfigStubParser {
        fn language(&self) -> Language {
            Language::new("php")
        }
        fn extensions(&self) -> &'static [&'static str] {
            &["php"]
        }
        fn parse(&self, _path: &str, _source: &str) -> gt_domain::error::Result<SyntaxFacts> {
            Ok(SyntaxFacts {
                config_entries: vec![
                    ConfigEntryFact {
                        key_path: "db.host".into(),
                        value: FactValue::String("localhost".into()),
                        span: Span::default(),
                    },
                    ConfigEntryFact {
                        key_path: "db.pass".into(),
                        value: FactValue::String("secret".into()),
                        span: Span::default(),
                    },
                    ConfigEntryFact {
                        key_path: "app.name".into(),
                        value: FactValue::String("demo".into()),
                        span: Span::default(),
                    },
                    ConfigEntryFact {
                        key_path: "cache.ttl".into(),
                        value: FactValue::Int(60),
                        span: Span::default(),
                    },
                ],
                ..Default::default()
            })
        }
        fn namespace_separator(&self) -> &'static [char] {
            &['\\']
        }
        fn member_separator(&self) -> &'static str {
            "::"
        }
    }

    /// Maps only the `php` language / extension; everything else is unknown (so fallback-to-sub and
    /// no-parser paths can be exercised).
    struct PhpOnlyRegistry;
    impl ParserRegistry for PhpOnlyRegistry {
        fn parser_for(&self, lang: &Language) -> Option<&dyn LanguageParser> {
            if lang == &Language::new("php") {
                Some(&ConfigStubParser)
            } else {
                None
            }
        }
        fn supported_languages(&self) -> Vec<Language> {
            vec![Language::new("php")]
        }
    }

    fn make_sub(root: &Path, lang: &str) -> SubProject {
        SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: root.to_path_buf(),
            language: Language::new(lang),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: Vec::new(),
            facts: Value::Null,
        }
    }

    fn make_ctx(root: &Path) -> PipelineContext {
        PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: root.to_path_buf(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        })
    }

    fn tmp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("gt_loaders_{}_{}", tag, std::process::id()))
    }

    // ------------------------------------------------------- contract_steps_of

    /// Build an FKB whose only rule is a `Synthesize` with a `ContractId` identity carrying the given
    /// `path.normalize` (null / empty / non-empty).
    fn fk_with_contract_rule(normalize: Value) -> FrameworkKnowledge {
        let rule_json = json!({
            "id": "r",
            "phase": "Synthesize",
            "selector": { "kind": "node" },
            "binding": [{
                "Synthesize": {
                    "node": "HttpContract",
                    "identity": { "kind": "ContractId", "path": { "normalize": normalize } },
                    "confidence": 0.9
                }
            }]
        });
        let rule: Rule = serde_json::from_value(rule_json).unwrap();
        FrameworkKnowledge {
            rules: vec![rule],
            ..Default::default()
        }
    }

    /// With no `ContractId` rule the kernel must fall back to `[LeadingSlash]` rather than write a
    /// half-baked / empty normalization.
    #[test]
    fn contract_steps_of_falls_back_without_contract_rule() {
        assert_eq!(
            contract_steps_of(&FrameworkKnowledge::default()),
            vec![NormalizeStep::LeadingSlash]
        );
    }

    /// A declared `path.normalize` is taken verbatim.
    #[test]
    fn contract_steps_of_returns_declared_steps() {
        let fk = fk_with_contract_rule(json!(["leading_slash", "lower"]));
        assert_eq!(
            contract_steps_of(&fk),
            vec![NormalizeStep::LeadingSlash, NormalizeStep::Lower]
        );
    }

    /// An empty `normalize` does not count as "obtainable" — the fallback still applies.
    #[test]
    fn contract_steps_of_falls_back_when_normalize_empty() {
        let fk = fk_with_contract_rule(json!([]));
        assert_eq!(
            contract_steps_of(&fk),
            vec![NormalizeStep::LeadingSlash],
            "an empty normalize list must not be returned"
        );
    }

    // ------------------------------------------------------- parser_for_file

    /// The parser is chosen by file extension first, then by the sub-project language, and never guessed.
    #[test]
    fn parser_for_file_prefers_extension_then_sub_language() {
        let reg = PhpOnlyRegistry;
        let php_sub = make_sub(Path::new("/x"), "php");
        let py_sub = make_sub(Path::new("/x"), "python");

        assert!(
            parser_for_file(&reg, Some(&php_sub), Path::new("a.php")).is_some(),
            "a known extension resolves a parser"
        );
        assert!(
            parser_for_file(&reg, Some(&php_sub), Path::new("a.unk")).is_some(),
            "an unknown extension falls back to the sub-project language"
        );
        assert!(
            parser_for_file(&reg, None, Path::new("a.unk")).is_none(),
            "no extension match and no sub-project -> no parser (never guess)"
        );
        assert!(
            parser_for_file(&reg, Some(&py_sub), Path::new("a.unk")).is_none(),
            "a sub-project whose language has no parser yields none"
        );
    }

    // ------------------------------------------------------- run_loader: Inline

    /// `Inline` rows pick their key from `facade` / `key` / `class` (whichever FKB declares); a row with
    /// none of those (or an empty key) is silently skipped, never written.
    #[test]
    fn run_loader_inline_picks_facade_key_or_class_and_skips_empty() {
        let root = tmp_root("inline");
        let mut ctx = make_ctx(&root);
        let sub = make_sub(&root, "php");
        let loader = LoaderSpec {
            id: "inline".into(),
            table: "facade_map".into(),
            from: LoaderSource::Inline {
                rows: vec![
                    json!({ "key": "db.host", "value": "localhost" }),
                    json!({ "facade": "Db", "value": "think\\facade\\Db" }),
                    json!({ "class": "App\\X\\Auth", "value": "x" }),
                    json!({ "other": "ignored" }), // no facade/key/class -> skipped
                    json!({ "key": "" }),          // empty key -> skipped
                ],
            },
            confidence: 1.0,
        };
        run_loader(
            &mut ctx,
            &loader,
            &sub,
            &root,
            &StdFileSystem::new(),
            &PhpOnlyRegistry,
            &Phase::default(),
            &[NormalizeStep::LeadingSlash],
            &FrameworkKnowledge::default(),
            &DefaultTechStackRegistry::new(),
        )
        .unwrap();

        assert_eq!(ctx.ws.get_symbol("facade_map", "db.host").unwrap()["value"], json!("localhost"));
        assert_eq!(ctx.ws.get_symbol("facade_map", "Db").unwrap()["value"], json!("think\\facade\\Db"));
        assert_eq!(ctx.ws.get_symbol("facade_map", "App\\X\\Auth").unwrap()["value"], json!("x"));
        assert!(ctx.ws.get_symbol("facade_map", "other").is_none(), "row without a key field is skipped");
        assert!(ctx.ws.get_symbol("facade_map", "").is_none(), "empty key is skipped");
    }

    // ------------------------------------------------------- run_loader: File (missing / key_path)

    /// A `File` loader whose target does not exist must be a silent no-op (Ok, no symbol) — never an error.
    #[test]
    fn run_loader_file_missing_is_noop() {
        let root = tmp_root("missing");
        let _ = std::fs::create_dir_all(&root);
        let mut ctx = make_ctx(&root);
        let sub = make_sub(&root, "php");
        let loader = LoaderSpec {
            id: "cfg".into(),
            table: "config_keys".into(),
            from: LoaderSource::File {
                path: "does_not_exist.php".into(),
                key_path: None,
            },
            confidence: 1.0,
        };
        run_loader(
            &mut ctx,
            &loader,
            &sub,
            &root,
            &StdFileSystem::new(),
            &PhpOnlyRegistry,
            &Phase::default(),
            &[NormalizeStep::LeadingSlash],
            &FrameworkKnowledge::default(),
            &DefaultTechStackRegistry::new(),
        )
        .unwrap();
        assert!(ctx.ws.get_symbol("config_keys", "db.host").is_none(), "a missing file writes nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// With `key_path`, only entries *under* that dotted prefix are kept, and the prefix is stripped from
    /// the resulting symbol key — entries in other namespaces are dropped.
    #[test]
    fn run_loader_file_keeps_only_key_path_prefix() {
        let root = tmp_root("keypath");
        let _ = std::fs::create_dir_all(&root);
        std::fs::write(root.join("database.php"), "<?php return [];").unwrap();
        let mut ctx = make_ctx(&root);
        let sub = make_sub(&root, "php");
        let loader = LoaderSpec {
            id: "cfg".into(),
            table: "config_keys".into(),
            from: LoaderSource::File {
                path: "database.php".into(),
                key_path: Some("db".into()),
            },
            confidence: 1.0,
        };
        run_loader(
            &mut ctx,
            &loader,
            &sub,
            &root,
            &StdFileSystem::new(),
            &PhpOnlyRegistry,
            &Phase::default(),
            &[NormalizeStep::LeadingSlash],
            &FrameworkKnowledge::default(),
            &DefaultTechStackRegistry::new(),
        )
        .unwrap();

        // `db.host` / `db.pass` survive, with the `db.` prefix stripped.
        // `entry.value` is a `FactValue`, serialised as `{"t": "String", "v": <s>}` in the symbol row.
        assert_eq!(ctx.ws.get_symbol("config_keys", "host").unwrap()["value"]["v"], json!("localhost"));
        assert_eq!(ctx.ws.get_symbol("config_keys", "pass").unwrap()["value"]["v"], json!("secret"));
        // Entries outside the `db` namespace are dropped (no silent cross-namespace merge).
        assert!(ctx.ws.get_symbol("config_keys", "app.name").is_none());
        assert!(ctx.ws.get_symbol("config_keys", "cache.ttl").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------- load_i18n (negative)

    /// A file whose path does not match the loader `pattern` contributes no i18n symbol.
    #[test]
    fn load_i18n_skips_files_not_matching_pattern() {
        let dir = tmp_root("i18n_mismatch");
        let _ = std::fs::create_dir_all(dir.join("lang/en"));
        let p = "lang/en/messages.php";
        std::fs::write(dir.join(p), "<?php return ['hello' => 'Hi'];").unwrap();
        let mut ctx = make_ctx(&dir);
        ctx.files = vec![SourceFile {
            id: FileId::new(1),
            project_id: ProjectId::new(1),
            sub_project_id: None,
            path: p.into(),
            language: Language::new("php"),
            size_bytes: 1,
            content_hash: "x".into(),
        }];
        let loader = LoaderSpec {
            id: "i18n".into(),
            table: "i18n".into(),
            from: LoaderSource::File {
                path: "".into(),
                key_path: None,
            },
            confidence: 1.0,
        };
        load_i18n(
            &mut ctx,
            &loader,
            "config/*", // does not match lang/en/messages.php
            &dir,
            &StdFileSystem::new(),
            &PhpOnlyRegistry,
            Some(r"(en|zh)"),
            &DefaultTechStackRegistry::new(),
        );
        assert!(
            ctx.ws.get_symbol("i18n", "messages.hello").is_none(),
            "a non-matching path writes no i18n symbol"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}


