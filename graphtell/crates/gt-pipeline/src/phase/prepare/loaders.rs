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

