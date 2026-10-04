//! P3 Prepare: load FKB and authoritative symbol tables.
//!
//! * use FKB's `detectors` to identify which frameworks each sub-project uses
//! * use `root_rules` to parse `AppRoot` (e.g. from `composer.json`'s `autoload.psr-4`)
//! * use `loaders` to load authoritative sources: `schema` / `config_keys` / `i18n` / `facade_map`
//!   / `container_bindings` / `event_listeners` / `route_list` / `nginx`

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

/// Run Prepare.
pub fn run(
    ctx: &mut PipelineContext,
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) -> Result<()> {
    let phase = Phase(Phase::PREPARE.to_string());
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    // Union of frameworks recognized across all sub-projects — decides which framework rules qualify for the global rule set.
    let mut detected_frameworks: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sub in &subs {
        // Framework-level + project-level knowledge recognized separately: project-level loads only when that sub-project is recognized as the corresponding project,
        // its rules only enter `rules_by_sub` (not `ctx.frameworks`, not global), never leaking into other projects.
        let evidence = collect_code_evidence(ctx, sub);
        let frameworks = detect_frameworks(
            kb,
            fs,
            sub,
            &project_root,
            KnowledgeScope::Framework,
            &evidence,
            techstack,
        );
        info!("sub-project {} recognized framework: {:?}", sub.name, frameworks);
        detected_frameworks.extend(frameworks.iter().cloned());
        ctx.frameworks.insert(sub.id.get(), frameworks.clone());

        let projects = detect_frameworks(
            kb,
            fs,
            sub,
            &project_root,
            KnowledgeScope::Project,
            &evidence,
            techstack,
        );
        if !projects.is_empty() {
            info!("sub-project {} recognized project knowledge: {:?}", sub.name, projects);
        }

        // Route-handler resolution rules + consumer entry-method names: framework-level first, then project-level.
        // Both declared by FKB (how each framework writes handlers / what entry methods are called is framework knowledge).
        if let Some(mut spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.method_ref.clone())
        {
            // Derive `root_namespaces` / `app_segments` from the tech-stack adapter (PSR-4 autoload for PHP,
            // the equivalent manifest for other stacks; no controller-dir name assumed — the resolver learns
            // the controller location from the real class FQNs).
            if let Some(adapter) = techstack.adapter_for(&sub.language) {
                adapter.enrich_method_ref(&mut spec, &sub.root_path, &project_root);
            }
            if ctx.method_ref_spec_default.is_none() {
                ctx.method_ref_spec_default = Some(spec.clone());
            }
            ctx.method_ref_specs.insert(sub.id.get(), spec);
        }
        // `@method` magic-method forwarding target: also FKB knowledge (which property to forward to is a framework/project convention).
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.magic_delegation.clone())
            .filter(|s| !s.property.is_empty())
        {
            if ctx.magic_delegation_default.is_none() {
                ctx.magic_delegation_default = Some(spec.clone());
            }
            ctx.magic_delegation.insert(sub.id.get(), spec);
        }
        // Data-model read / write verbs: also FKB knowledge (what the framework's Model/Query API is called).
        if let Some(spec) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find_map(|fk| fk.db_verbs.clone())
            .filter(|s| !s.write.is_empty() || !s.read.is_empty())
        {
            if ctx.db_verbs_default.is_none() {
                ctx.db_verbs_default = Some(spec.clone());
            }
            ctx.db_verbs.insert(sub.id.get(), spec);
        }
        // These three lists are collected only from knowledge that **actually applies** to this sub-project:
        // a recognised framework / project, or the unconditional language layer (`apply_without_detection`).
        //
        // Merging them from **every** FKB of the language with no detector consulted would let a library
        // contribute its entries to projects that never went near it — `GuzzleHttp\Client::request` landing
        // in every PHP project's outbound-call list, `Db::transaction` in Laravel's.
        //
        // The `apply_without_detection` arm is not optional: that layer holds the genuinely
        // framework-independent entries (`curl_exec`, `Http::get`, `transaction`, `commit` …), and it is
        // never in `frameworks` because it has no detectors. Dropping it would silently disable every rule
        // that reads these lists.
        //
        // Measured safe on the real samples (6 PHP projects, `tests/detector_coverage.rs`): zero cases where
        // the code used a library and its detectors did not fire.
        for fk in kb.all() {
            if fk.language != sub.language {
                continue;
            }
            let applies = frameworks.contains(&fk.id)
                || projects.contains(&fk.id)
                || fk.apply_without_detection;
            if !applies {
                continue;
            }
            for c in &fk.external_calls {
                if !ctx.external_calls.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    ctx.external_calls.push(c.clone());
                }
            }
            for c in &fk.tx_calls {
                if !ctx.tx_calls.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    ctx.tx_calls.push(c.clone());
                }
            }
            // "middleware class → capability": names are framework/project conventions (`AuthTokenMiddleware`-style
            // naming the kernel shouldn't know), so also declared by FKB, here only dedup-merge.
            for mc in &fk.middleware_capabilities {
                if !ctx
                    .middleware_capabilities
                    .iter()
                    .any(|x| x.capability == mc.capability && x.matches == mc.matches)
                {
                    ctx.middleware_capabilities.push(mc.clone());
                }
            }
        }
        if let Some(methods) = frameworks
            .iter()
            .chain(projects.iter())
            .filter_map(|id| kb.by_id(id))
            .find(|fk| !fk.entry_methods.is_empty())
            .map(|fk| fk.entry_methods.clone())
        {
            if ctx.entry_methods_default.is_empty() {
                ctx.entry_methods_default = methods.clone();
            }
            ctx.entry_methods.insert(sub.id.get(), methods);
        }

        // Framework + project knowledge both apply root_rules / loaders (project knowledge usually none, but the channel is generic).
        let mut facts = serde_json::Map::new();
        for id in frameworks.iter().chain(projects.iter()) {
            let Some(fk) = kb.by_id(id) else { continue };
            apply_root_rules(ctx, fk, sub, &mut facts, &phase, fs, parsers, techstack);
            // Table prefix(es). A `db_connections` list fact covers **every** configured connection
            // (read/write splitting, several databases): all non-empty prefixes are merged into
            // `table_prefixes` (a Vec, deduped), and `db_prefix` records the default connection's —
            // the single value the old `connections.mysql.prefix` rule used to hard-code, which silently
            // cost any project whose connection is not named `mysql` its whole column-level schema.
            if let Some(connections) = facts.get("db_connections").cloned() {
                let default_name = connections
                    .get("default")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let empty: Vec<Value> = Vec::new();
                let entries = connections
                    .get("value")
                    .and_then(|v| v.as_array())
                    .unwrap_or(&empty);
                let mut prefixes = ctx.ws.table_prefixes().to_vec();
                let mut first_non_empty: Option<String> = None;
                let mut default_prefix: Option<String> = None;
                for entry in entries {
                    let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let prefix = entry.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
                    if prefix.is_empty() {
                        continue;
                    }
                    if !prefixes.iter().any(|p| p == prefix) {
                        prefixes.push(prefix.to_string());
                    }
                    if first_non_empty.is_none() {
                        first_non_empty = Some(prefix.to_string());
                    }
                    if default_name.as_deref() == Some(name) {
                        default_prefix = Some(prefix.to_string());
                    }
                }
                ctx.ws.set_table_prefixes(prefixes);
                if let Some(p) = default_prefix.or(first_non_empty) {
                    facts.insert(
                        "db_prefix".to_string(),
                        json!({
                            "value": p,
                            "confidence": 1.0,
                            "source": "db_connections (default connection)",
                            "fallback_used": false,
                        }),
                    );
                }
            }
            // Auto-detect table prefix: FKB's `db_prefix` root_rule reads it from project config,
            // merged with the prefix from the project's explicit config (dedup), for P3 loading and P5 normalization.
            if let Some(v) = facts
                .get("db_prefix")
                .and_then(|v| v.get("value"))
                .and_then(|v| v.as_str())
            {
                let mut prefixes = ctx.ws.table_prefixes().to_vec();
                if !prefixes.iter().any(|p| p == v) {
                    prefixes.push(v.to_string());
                }
                ctx.ws.set_table_prefixes(prefixes);
            }
        }
        // Authoritative symbol tables, under the same "does this knowledge apply" test as the lists above.
        // The `apply_without_detection` arm is what makes a **framework-independent** loader possible: an
        // unconditional FKB is never "detected", so without this arm its loaders never run and such a loader
        // would have to hide inside a framework file — costing every other framework the ability to read it.
        for fk in kb.all() {
            if fk.language != sub.language && fk.language.0 != "*" {
                continue;
            }
            let applies = frameworks.contains(&fk.id)
                || projects.contains(&fk.id)
                || fk.apply_without_detection;
            if !applies {
                continue;
            }
            run_loaders(ctx, fk, sub, &project_root, fs, parsers, &phase, techstack);
        }

        let facts_value = Value::Object(facts);
        for (k, v) in facts_value.as_object().cloned().unwrap_or_default() {
            ctx.ws.set_fact(sub.id, &k, v);
        }

        let mut rules: Vec<Rule> = frameworks
            .iter()
            .filter_map(|id| kb.by_id(id))
            .flat_map(|fk| fk.rules.iter().cloned())
            .collect();
        for fk in kb.all() {
            if fk.scope == KnowledgeScope::Framework
                && (fk.language == sub.language || fk.language.0 == "*")
                && !frameworks.contains(&fk.id)
                && fk.apply_without_detection
            {
                for r in &fk.rules {
                    if r.applies_to(&fk.language, &sub.language) {
                        rules.push(r.clone());
                    }
                }
            }
        }
        for id in &projects {
            if let Some(fk) = kb.by_id(id) {
                rules.extend(fk.rules.iter().cloned());
            }
        }
        ctx.rules_by_sub.insert(sub.id.get(), dedup_rules(rules));
    }

    let mut global: Vec<(Language, Rule)> = Vec::new();
    for fk in kb.all() {
        if fk.scope != KnowledgeScope::Framework {
            continue;
        }
        if !detected_frameworks.contains(&fk.id) && !fk.apply_without_detection {
            continue;
        }
        for r in &fk.rules {
            if !global.iter().any(|( _, g)| g.id == r.id) {
                global.push((fk.language.clone(), r.clone()));
            }
        }
    }
    ctx.rules_global = global;

    Ok(())
}

// ---------------------------------------------------------------- framework detection

fn detect_frameworks(
    kb: &dyn KnowledgeProvider,
    fs: &dyn FileSystem,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    scope: KnowledgeScope,
    evidence: &CodeEvidence,
    techstack: &dyn TechStackRegistry,
) -> Vec<String> {
    let mut hits: Vec<(String, f32)> = Vec::new();
    for fk in kb.all() {
        if fk.scope != scope {
            continue;
        }
        if fk.language != sub.language && sub.language.as_str() != Language::UNKNOWN {
            continue;
        }
        let mut best = 0f32;
        for d in &fk.detectors {
            let ok = match d {
                Detector::ManifestDependency { manifest, dependency, .. } => {
                    let path = sub.root_path.join(manifest);
                    manifest_has(&path, dependency, fs, techstack, &sub.language)
                        || manifest_has(&project_root.join(manifest), dependency, fs, techstack, &sub.language)
                }
                Detector::FileExists { path, .. } => {
                    sub.root_path.join(path).exists() || project_root.join(path).exists()
                }
                Detector::LockDependency { lock, dependency, .. } => {
                    let path = sub.root_path.join(lock);
                    lock_has(&path, dependency, fs, techstack, &sub.language)
                        || lock_has(&project_root.join(lock), dependency, fs, techstack, &sub.language)
                }
                Detector::ImportExists { symbol, .. } => evidence.imports(symbol),
                Detector::CallExists { callee, .. } => evidence.calls(callee),
            };
            if ok {
                best = best.max(d.confidence());
            }
        }
        if best > 0.0 {
            hits.push((fk.id.clone(), best));
        }
    }
    hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    expand_provided(hits, kb).into_iter().map(|(id, _)| id).collect()
}

/// What the sub-project's **code** says it uses, as opposed to what its manifest says it installed.
///
/// Both collections are de-duplicated while being built, so each detector costs O(distinct) rather than
/// O(call sites) — the gate itself must stay cheap, since its whole purpose is to keep the far more
/// expensive O(call sites × rules) matching in P5 down to the knowledge this project actually needs.
#[derive(Default)]
struct CodeEvidence {
    /// Imported FQNs, lowercased and stripped of a leading separator.
    imports: HashSet<String>,
    /// Distinct callees, each with the receiver / method P2 saw it with (needed because callee matching
    /// is three-way: pattern vs `callee` + `receiver` + `method`).
    callees: HashMap<String, (Option<String>, Option<String>)>,
}

impl CodeEvidence {
    /// Whether `symbol` is imported by this sub-project. A trailing `*` matches a namespace prefix.
    fn imports(&self, symbol: &str) -> bool {
        let pat = symbol.trim_start_matches('\\').to_ascii_lowercase();
        match pat.strip_suffix('*') {
            Some(prefix) => self.imports.iter().any(|fqn| fqn.starts_with(prefix)),
            None => self.imports.contains(&pat),
        }
    }

    /// Whether any call site of this sub-project matches `pattern` (grammar of `selector.callee`).
    ///
    /// Matching is three-way (`pattern` vs callee + receiver + method), so a call site the parser did not
    /// split into receiver / method would silently match nothing. A fully-qualified callee already carries
    /// both halves, so recover them — otherwise the detector misses precisely the inline spelling
    /// (`\GuzzleHttp\Client::request()`) that it exists to catch.
    fn calls(&self, pattern: &str) -> bool {
        self.callees.iter().any(|(callee, (receiver, method))| {
            let recovered =
                (receiver.is_none() || method.is_none()).then(|| split_callee(callee));
            let (r, m) = match &recovered {
                Some((r, m)) => (r.as_deref(), m.as_deref()),
                None => (receiver.as_deref(), method.as_deref()),
            };
            callee_matches(pattern, callee, r, m)
        })
    }
}

/// Split `A::m` / `A->m` into its halves. Both separators are two characters wide.
fn split_callee(callee: &str) -> (Option<String>, Option<String>) {
    match callee.rfind("::").or_else(|| callee.rfind("->")) {
        Some(pos) => (
            Some(callee[..pos].to_string()),
            Some(callee[pos + 2..].to_string()),
        ),
        None => (None, None),
    }
}

/// Collect one sub-project's code evidence from the P2 output.
fn collect_code_evidence(ctx: &PipelineContext, sub: &gt_domain::model::SubProject) -> CodeEvidence {
    // Files are recorded project-relative while `root_path` is absolute; a sub-project that *is* the
    // project root has no prefix, and then every file belongs to it.
    let prefix = match sub.root_path.strip_prefix(&ctx.project.root_path) {
        Ok(rel) if !rel.as_os_str().is_empty() => Some(
            format!("{}/", rel.to_string_lossy().replace('\\', "/").trim_matches('/')),
        ),
        _ => None,
    };
    let in_sub = |path: &str| match &prefix {
        Some(p) => path.starts_with(p.as_str()),
        None => true,
    };

    let mut ev = CodeEvidence::default();
    for (file_id, fqn) in ctx.ws.all_imported_fqns() {
        if !ctx.ws.source_path_of(file_id).is_some_and(|p| in_sub(&p)) {
            continue;
        }
        ev.imports.insert(fqn.trim_start_matches('\\').to_ascii_lowercase());
    }
    for call in &ctx.ws.calls {
        // `sub` is authoritative (P0 assigned it from the file); the path check only catches the rare
        // call site whose owning file was never assigned to a sub-project.
        if call.sub == Some(sub.id) || (call.sub.is_none() && in_sub(&call.file)) {
            ev.callees
                .entry(call.callee.clone())
                .or_insert_with(|| (call.receiver.clone(), call.method.clone()));
        }
    }
    ev
}

// ---------------------------------------------------------------- AppRoot

fn apply_root_rules(
    ctx: &mut PipelineContext,
    fk: &FrameworkKnowledge,
    sub: &gt_domain::model::SubProject,
    facts: &mut serde_json::Map<String, Value>,
    phase: &Phase,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    techstack: &dyn TechStackRegistry,
) {
    for rule in &fk.root_rules {
        // A collection rule yields a **list** fact (e.g. every configured DB connection), which the
        // single-value path below cannot represent: one project may declare several connections, each
        // with its own driver and table prefix.
        if let RootSource::ManifestEntries { manifest, root, .. } = &rule.source {
            let resolved = facts::resolve_root_entries(
                &rule.source,
                &ctx.project.root_path,
                sub,
                fs,
                parsers,
                techstack,
            );
            let Some(entries) = resolved else {
                ctx.ws.diagnose(
                    phase,
                    "RootRuleUnresolved",
                    gt_domain::model::Severity::Warning,
                    format!("框架 {} 的 {} 未能解析", fk.id, rule.key),
                    Some(sub.root_path.to_string_lossy().to_string()),
                );
                continue;
            };
            let records: Vec<Value> = entries
                .entries
                .iter()
                .map(|e| {
                    let mut m = serde_json::Map::new();
                    m.insert("name".to_string(), Value::String(e.key.clone()));
                    for (k, v) in &e.fields {
                        m.insert(k.clone(), Value::String(v.clone()));
                    }
                    Value::Object(m)
                })
                .collect();
            facts.insert(
                rule.key.clone(),
                json!({
                    "value": records,
                    "default": entries.default,
                    "confidence": rule.confidence,
                    "source": format!("{} {} ({} entries)", manifest, root, records.len()),
                    "fallback_used": false,
                }),
            );
            continue;
        }

        // Shared with P0 (`phase::exclude`): every source kind here reads a manifest or probes a
        // directory, so the same resolution works before anything has been parsed.
        let resolved = facts::resolve_root_source(
            &rule.source,
            &sub.root_path,
            &ctx.project.root_path,
            sub,
            fs,
            parsers,
            techstack,
        );

        let (value, source, fallback_used, confidence) = match resolved {
            Some((v, src)) => (v, src, false, rule.confidence),
            None => {
                let fallback = rule
                    .fallbacks
                    .iter()
                    .find(|c| sub.root_path.join(c).exists());
                match fallback {
                    Some(c) => (
                        c.clone(),
                        format!("fallback: {}", c),
                        true,
                        rule.confidence * 0.7,
                    ),
                    None => {
                        ctx.ws.diagnose(
                            phase,
                            "RootRuleUnresolved",
                            gt_domain::model::Severity::Warning,
                            format!("框架 {} 的 {} 未能解析", fk.id, rule.key),
                            Some(sub.root_path.to_string_lossy().to_string()),
                        );
                        continue;
                    }
                }
            }
        };

        facts.insert(
            rule.key.clone(),
            json!({
                "value": value,
                "confidence": confidence,
                "source": source,
                "fallback_used": fallback_used,
            }),
        );
    }
}

/// Parse the `directory_exists` root rule: search for the target relative path within a sub-project (incl. multi-level subdirs, depth-limited)
/// (e.g. `src/main/java`). On hit, return its **parent dir** as the source root (the semantics of `app_root` is the source root, not
/// `src/main/java` itself). A single-module project hitting directly under the root returns `"."`; a multi-module project returns the first hit module's
/// relative dir (e.g. `mall-admin`). Only when the whole tree can't be found do we return `None` (trigger fallback / warning).

// ---------------------------------------------------------------- loaders

fn run_loaders(
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
fn contract_steps_of(fk: &FrameworkKnowledge) -> Vec<NormalizeStep> {
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
fn parser_for_file<'a>(
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

fn run_loader(
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

fn load_i18n(
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

// ---------------------------------------------------------------- built-in loaders

fn run_builtin(
    ctx: &mut PipelineContext,
    name: &str,
    params: &Value,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    phase: &Phase,
    contract_steps: &[NormalizeStep],
    fk: &FrameworkKnowledge,
    techstack: &dyn TechStackRegistry,
) {
    match name {
        // Neutral ids (`db_schema` / `config_keys`): neither loader knows a language — the SQL install
        // script is SQL, and "which receivers / accessors name a table or a config key" is declared by
        // FKB `params`. The `php_*` spellings are kept as deprecated aliases so FKB written before the
        // rename keeps loading instead of silently collecting nothing.
        "db_schema" | "php_db_schema" => load_schema(ctx, params, sub, project_root, phase),
        "config_keys" | "php_config_keys" => load_config_keys(ctx, params, sub),
        // Generic alias loader: file / block marker / separator all declared by FKB `params`, bound to no language.
        "middleware_aliases" => load_middleware_aliases(ctx, sub, project_root, params),
        // Generic declarative-middleware loader: file name / key / scope all declared by FKB `params`,
        // merge the declared class list into `route_list`'s `guards`, nodes + edges unified by P14.
        "declared_middleware" => load_declared_middleware(ctx, sub, project_root, params),
        // Generic route-guard loader: recognition logic comes entirely from FKB's `route_guards` declaration, no framework hard-coded.
        "routes" => {
            if let Some(spec) = fk.route_guards.as_ref() {
                load_routes(ctx, spec, sub, contract_steps);
            }
        }
        "nginx_config" => load_nginx(ctx, sub, project_root, fs),
        // Any built-in loader the kernel does not own — e.g. `migration_schema`, whose id is language-agnostic but
        // whose format-specific parsing lives in each stack's adapter — is delegated to the tech-stack adapter
        // that matches `sub.language` — the kernel must not hard-code one language's loader names. The adapter
        // returns `Ok(vec![])` for ids it does not handle, so a built-in name no stack claims is a no-op (logged)
        // rather than silently adopting PHP behaviour.
        other => run_adapter_loader(ctx, techstack, sub, project_root, fs, parsers, other, params),
    }
}

/// Dispatch a tech-stack-specific built-in loader to the adapter matching the sub-project's language,
/// then persist the returned facts into the workspace.
fn run_adapter_loader(
    ctx: &mut PipelineContext,
    techstack: &dyn TechStackRegistry,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    fs: &dyn FileSystem,
    parsers: &dyn ParserRegistry,
    name: &str,
    params: &Value,
) {
    let Some(adapter) = techstack.adapter_for(&sub.language) else {
        return;
    };
    match adapter.load(
        name,
        params,
        sub,
        project_root,
        fs,
        parsers,
        &ctx.ws.table_prefixes(),
    ) {
        Ok(facts) => {
            for f in facts {
                apply_adapter_fact(ctx, f);
            }
        }
        Err(e) => warn!("tech-stack loader {name} failed: {e}"),
    }
}

/// Persist one fact returned by a tech-stack adapter into the workspace. The kernel owns all
/// `PipelineContext` mutation; the adapter only ever returns data.
fn apply_adapter_fact(ctx: &mut PipelineContext, fact: AdapterFact) {
    match fact {
        AdapterFact::Schema {
            table,
            columns,
            source,
        } => merge_schema_columns(ctx, &table, columns, &source),
        AdapterFact::Symbol { table, key, value } => {
            ctx.ws
                .put_symbol(ctx.project.id, &table, &key, value);
        }
    }
}

fn load_schema(
    ctx: &mut PipelineContext,
    params: &Value,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    _phase: &Phase,
) {
    // Table prefix follows what the project detected (P3 auto-reads from config/database.php and writes to
        // `workspace.table_prefixes`); YAML can still append extra prefixes via `params.prefixes`.
    let mut prefixes: Vec<String> = ctx.ws.table_prefixes().to_vec();
    if let Some(extra) = params
        .get("prefixes")
        .and_then(|p| serde_json::from_value::<Vec<String>>(p.clone()).ok())
    {
        for x in extra {
            if !prefixes.contains(&x) {
                prefixes.push(x);
            }
        }
    }

    // ① Parse table structure from the SQL install script
    for (path, text) in scan_text_files(project_root, &["sql"]) {
        for (table, columns) in parse_create_tables(&text) {
            let name = strip_prefixes(&table, &prefixes);
            merge_schema_columns(ctx, &name, columns, &path);
        }
    }

    // Which receivers / methods name a table is **stack knowledge** and comes from `params` — there is no
    // built-in default list (it used to spell out PHP's `Db` / `Query` / `Model`, which silently applied
    // PHP conventions to every stack that merely forgot to declare them). "Nothing declared" now means
    // "nothing collected", and it is logged rather than passed over silently.
    let table_receivers: Vec<String> = params
        .get("table_receivers")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    // Receivers that must match **exactly** (on their last namespace segment) instead of as a substring,
    // e.g. `Db` — a substring match would also swallow `DbHelper` / `MyDb`.
    let exact_table_receivers: Vec<String> = params
        .get("exact_table_receivers")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    let table_methods: Vec<String> = params
        .get("table_methods")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    if table_methods.is_empty()
        || (table_receivers.is_empty() && exact_table_receivers.is_empty())
    {
        warn!(
            "built-in loader `db_schema` declares no table_receivers / table_methods — no table names collected \
             (declare them in FKB `params`; which receivers name a table is stack knowledge)"
        );
        return;
    }
    // The namespace separator is this language's (`\` for PHP, `.` for Java …), never assumed.
    let ns_separators = ctx.lang_policy_for_sub(Some(sub.id)).ns_separators.clone();
    let mut found: Vec<(String, String)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // Only name()/table() with DB semantics count as table names:
        // `Route::name('xxx')` is route naming, can't be a table.
        let is_db_receiver = call
            .receiver
            .as_deref()
            .map(|raw| {
                let r = raw.trim_start_matches(|c| ns_separators.contains(&c));
                // `Db` must match `Db` and `think\facade\Db` — but never `DbHelper`.
                let last = r.rsplit(|c| ns_separators.contains(&c)).next().unwrap_or(r);
                table_receivers.iter().any(|p| {
                    // A pattern written with a leading separator (`\Db`) means "match the tail".
                    if p.starts_with(|c| ns_separators.contains(&c)) {
                        r.ends_with(p.trim_start_matches(|c| ns_separators.contains(&c)))
                    } else {
                        r.contains(p)
                    }
                }) || exact_table_receivers
                    .iter()
                    .any(|p| last.eq_ignore_ascii_case(p.trim_start_matches(|c| ns_separators.contains(&c))))
            })
            .unwrap_or(false);
        let is_table_call = call
            .method
            .as_deref()
            .map(|m| table_methods.iter().any(|t| m.eq_ignore_ascii_case(t)))
            .unwrap_or(false);
        if !is_table_call || !is_db_receiver {
            continue;
        }
        if let Some(gt_domain::model::FactValue::String(t)) = call.args.first() {
            let name = strip_prefixes(t, &prefixes);
            if name.is_empty() {
                continue;
            }
            found.push((name, format!("{}:{}", call.file, call.span.start_line)));
        }
    }
    for (name, loc) in found {
        let existing = ctx.ws.get_symbol("schema", &name).cloned();
        let mut value = existing.unwrap_or_else(|| json!({ "columns": [], "sources": [] }));
        if let Some(srcs) = value.get_mut("sources").and_then(|s| s.as_array_mut()) {
            if srcs.len() < 50 {
                srcs.push(json!(loc));
            }
        }
        ctx.ws.put_symbol(ctx.project.id, "schema", &name, value);
    }
}




/// **Implicit column names** for argument-less column declarations (Laravel convention).
///
/// Deliberately exclude bare `uuid()` / `ulid()`: their default column name is `uuid` / `ulid`,
        // but more commonly code writes `$table->uuid('id')` (covered by the arg form) — don't guess.



/// Merge a batch of columns into the `schema` symbol table (**union dedup**, not full overwrite).
///
/// The same table may have two sources: the SQL install script and the Laravel migration. Both loaders write `schema`,
/// if each `put_symbol` overwrote fully, the later one would wipe the earlier (***load order not guaranteed***) —
/// so unify through here for a union.
fn merge_schema_columns(
    ctx: &mut PipelineContext,
    table: &str,
    columns: Vec<String>,
    source: &str,
) {
    if table.is_empty() || columns.is_empty() {
        return;
    }
    let mut value = ctx
        .ws
        .get_symbol("schema", table)
        .cloned()
        .unwrap_or_else(|| json!({ "columns": [], "sources": [] }));
    let mut cols: Vec<String> = value
        .get("columns")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for c in columns {
        if !cols.iter().any(|x| x == &c) {
            cols.push(c);
        }
    }
    value["columns"] = json!(cols);
    if let Some(srcs) = value.get_mut("sources").and_then(|s| s.as_array_mut()) {
        if srcs.len() < 50 && !srcs.iter().any(|s| s.as_str() == Some(source)) {
            srcs.push(json!(source));
        }
    }
    ctx.ws.put_symbol(ctx.project.id, "schema", table, value);
}

/// Collect config keys (`php_config_keys`).
///
/// Which callables read configuration is **stack knowledge** and comes from `params`; there is no
/// built-in default list. That used to be different: the default spelled out one product's own helpers
/// (`sys_config` / `sys_config_all`), so every stack that merely forgot to declare `accessors` silently
/// inherited them. "No knowledge declared" now means "no keys collected", which is honest.
///
/// The same applies to `suffixes`: a blanket `::get` matched `Route::get('api/goods/detail')` — measured
/// on CRMEB, 791 route registrations against 141 real `Config::get` / `Env::get` reads, i.e. the config
/// table filled up with URL paths.
fn load_config_keys(ctx: &mut PipelineContext, params: &Value, _sub: &gt_domain::model::SubProject) {
    let accessors: Vec<String> = params
        .get("accessors")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    let suffixes: Vec<String> = params
        .get("suffixes")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    if accessors.is_empty() && suffixes.is_empty() {
        return;
    }
    // Where a key lives and whether it can change at runtime differ per accessor — `sys_config` reads a
    // settings table, `config()` reads a file, `env()` reads the environment. Both are stack knowledge,
    // so they are declared next to the accessors rather than hard-coded for every key.
    let default_storage = params
        .get("storage")
        .and_then(|v| v.as_str())
        .unwrap_or("Database");
    let default_mutable = params
        .get("mutable")
        .and_then(|v| v.as_str())
        .unwrap_or("RuntimeMutable");
    let by_accessor = params.get("by_accessor");

    let mut found: Vec<(String, String, String, String, u32)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let matched = accessors
            .iter()
            .find(|a| call.callee.eq_ignore_ascii_case(a))
            .cloned();
        let is_config =
            matched.is_some() || suffixes.iter().any(|s| call.callee.ends_with(s));
        if !is_config {
            continue;
        }
        let Some(gt_domain::model::FactValue::String(key)) = call.args.first() else {
            continue;
        };
        if key.is_empty() || key.contains(' ') {
            continue;
        }
        let (storage, mutable) = matched
            .as_deref()
            .and_then(|a| by_accessor.and_then(|m| m.get(a)))
            .map(|o| {
                (
                    o.get("storage").and_then(|v| v.as_str()).unwrap_or(default_storage),
                    o.get("mutable").and_then(|v| v.as_str()).unwrap_or(default_mutable),
                )
            })
            .unwrap_or((default_storage, default_mutable));
        found.push((
            key.clone(),
            storage.to_string(),
            mutable.to_string(),
            call.file.clone(),
            call.span.start_line,
        ));
    }
    for (key, storage, mutable, file, line) in found {
        ctx.ws.put_symbol(
            ctx.project.id,
            "config_keys",
            &key,
            json!({
                "storage": storage,
                "mutable": mutable,
                "value_type": "string",
                "file": file,
                "line": line,
            }),
        );
    }
}

/// Normalize a class name: collapse consecutive backslashes into one, drop empty leading/trailing segments.
///
/// Class names in alias tables / route guards go through multiple rounds of JSON and text scanning escapes, possibly yielding
/// `Illuminate\\Session\\Middleware\\X` (multiple backslashes). Normalize before comparing,
/// otherwise FKB-declared known middleware never matches (measured: laravel10 whole table empty).
pub fn norm_class(s: &str) -> String {
    s.split('\\')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\\")
}

/// Fold `callee` (possibly namespaced `org.x.Y` / `x\Y`) into a short name, for comparing with decorator / annotation names in declarations.
fn short_callee(callee: &str) -> &str {
    callee.rsplit(['\\', '.']).next().unwrap_or(callee)
}

/// Whether a call's `receiver` matches the `receiver` field of some `route_calls` pattern.
fn receiver_matches(rc: &RouteCallSpec, recv: &str) -> bool {
    let lower = recv.to_ascii_lowercase();
    let target = rc.receiver.to_ascii_lowercase();
    if rc.receiver_ends_with {
        lower.trim_start_matches('\\').ends_with(&target)
    } else {
        lower.contains(&target)
    }
}

/// Take a call arg's "middleware name".
///
/// Only recognize **literals** (`String` / `ClassConst`) — PHP middleware is always `X::class`.
/// Identifiers (`Unknown(Some(name))`, i.e. JS / Python function references) are collected **only when FKB explicitly declares `accept_identifier`**:
/// PHP's dynamic arg `->middleware($v)` if collected would be fabrication.
fn guard_arg_name(v: &FactValue, accept_identifier: bool) -> Option<String> {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) if !s.trim().is_empty() => {
            Some(s.trim().to_string())
        }
        FactValue::Unknown(Some(name)) if accept_identifier => {
            let base = name.split('(').next().unwrap_or(name).trim().to_string();
            if base.is_empty() {
                None
            } else {
                Some(base)
            }
        }
        _ => None,
    }
}

/// Judge whether a call site is a "route definition"; on hit return `(verb, path, handler)`.
fn match_route_call(call: &CallRecord, rc: &RouteCallSpec) -> Option<(String, String, Option<String>)> {
    let verb = match rc.by {
        RouteMatchBy::Receiver => {
            let recv = call.receiver.as_deref()?;
            if !receiver_matches(rc, recv) {
                return None;
            }
            let m = call.method.as_deref()?.to_ascii_lowercase();
            rc.verb_methods.get(&m).cloned()?
        }
        RouteMatchBy::Callee => {
            let callee = short_callee(&call.callee).to_ascii_lowercase();
            rc.verb_methods.get(&callee).cloned()?
        }
    };
    let path = call
        .args
        .get(rc.path_arg)
        .and_then(|a| a.as_str())
        .unwrap_or_default()
        .to_string();
    if path.is_empty() {
        return None;
    }
    let handler = rc
        .handler_arg
        .and_then(|i| call.args.get(i))
        .and_then(|a| a.as_str())
        .map(|s| s.to_string());
    Some((verb, path, handler))
}

/// Collect "which middleware guards which route segment" — **recognized entirely by FKB `route_guards` declaration**,
/// no framework's receiver / method name / verb table hard-coded.
///
/// Three mount models:
/// * `chain`: `Route::get(path)->middleware(X[, arg])` (ThinkPHP / Laravel);
/// * `positional`: `app.get(path, mw1, mw2, handler)` (Express / Koa);
/// * `decorator`: `@UseGuards(X)` / `@login_required` / `@PreAuthorize` lands on the **same method** as the decorated route, associated by `owner_fqn` (NestJS / Python / Spring).
fn collect_route_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
    // Multiple mount models may be declared (one framework often has several styles); results take the union.
    let mut out = Vec::new();
    for attach in spec.guard_attach.specs() {
        let mut scopes = match attach {
            GuardAttachSpec::Chain(chain) => extract_chain_guards(spec, chain, calls),
            GuardAttachSpec::Positional => extract_positional_guards(spec, calls),
            GuardAttachSpec::Decorator(d) => extract_decorator_guards(spec, d, calls),
            GuardAttachSpec::Consumer(cg) => extract_consumer_guards(spec, cg, calls),
        };
        out.append(&mut scopes);
    }
    out
}

/// Chained guard: `Route::get(path)->middleware(X[, arg])`.
///
/// The difficulty is the **chained call**: `Route::group('pc', fn)->middleware(A::class)` lands as a string of call sites on the graph,
/// the root node (`group`) and each `->middleware()` **share the same `start_byte`**
/// (`member_call_expression` starts from the root source segment). Pair by `(file, start_byte)` to hang the middleware back onto the segment it really decorates;
/// among chain candidates **prefer the `group` call** (only it wraps the closure range), else take the one with smallest `end_byte` as root.
fn extract_chain_guards(
    spec: &RouteGuardSpec,
    chain: &gt_domain::model::ChainGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    let is_route_receiver = |c: &CallRecord| -> bool {
        c.receiver
            .as_deref()
            .map(|r| spec.route_calls.iter().any(|rc| receiver_matches(rc, r)))
            .unwrap_or(false)
    };
    let is_verb = |m: Option<&str>| -> bool {
        m.map(|mm| {
            spec.route_calls
                .iter()
                .any(|rc| rc.verb_methods.keys().any(|k| k.eq_ignore_ascii_case(mm)))
        })
        .unwrap_or(false)
    };
    let is_group = |m: Option<&str>| -> bool {
        m.map(|mm| spec.route_calls.iter().any(|rc| rc.group_method.as_deref() == Some(mm)))
            .unwrap_or(false)
    };

    // ① each chain's root: `(file, start_byte)` → root call. Prefer `group`, else smallest `end_byte`.
    let mut roots: HashMap<(String, u32), &CallRecord> = HashMap::new();
    for call in calls {
        if !is_route_receiver(call) {
            continue;
        }
        let m = call.method.as_deref();
        if !is_group(m) && !is_verb(m) {
            continue;
        }
        let key = (call.file.clone(), call.span.start_byte);
        let prev_is_group = roots
            .get(&key)
            .and_then(|p| p.method.as_deref())
            .map(|mm| is_group(Some(mm)))
            .unwrap_or(false);
        let call_is_group = is_group(m);
        let shorter = roots
            .get(&key)
            .map_or(true, |p| (p.span.end_byte - p.span.start_byte) > (call.span.end_byte - call.span.start_byte));
        if call_is_group || (!prev_is_group && shorter) {
            roots.insert(key, call);
        }
    }

    // ② merge each `->middleware(X[, arg])` into its root. Arg may be single value or array.
    let mut scopes: HashMap<(String, u32), RouteGuardScope> = HashMap::new();
    for call in calls {
        if call.method.as_deref() != Some(chain.method.as_str()) || !is_route_receiver(call) {
            continue;
        }
        // `accept_identifier` comes from **the matched route_calls pattern** (other patterns of the same framework may differ).
        let accept_identifier = spec
            .route_calls
            .iter()
            .any(|rc| receiver_matches(rc, call.receiver.as_deref().unwrap_or("")) && rc.accept_identifier);
        let mut classes: Vec<(String, Option<String>)> = Vec::new();
        let arg2 = chain.arg2_index.and_then(|i| call.args.get(i));
        match call.args.get(chain.arg_index) {
            Some(a @ (FactValue::String(_) | FactValue::ClassConst(_) | FactValue::Unknown(_))) => {
                if let Some(name) = guard_arg_name(a, accept_identifier) {
                    classes.push((name, arg2.and_then(guard_arg_text)));
                }
            }
            Some(FactValue::Array(items)) => {
                for (_, v) in items {
                    if let Some(name) = guard_arg_name(v, accept_identifier) {
                        classes.push((name, arg2.and_then(guard_arg_text)));
                    }
                }
            }
            _ => {}
        }
        if classes.is_empty() {
            continue;
        }
        let Some(root) = roots.get(&(call.file.clone(), call.span.start_byte)) else {
            continue;
        };
        let scope_key = (root.file.clone(), root.span.start_byte);
        let scope = scopes.entry(scope_key).or_insert_with(|| RouteGuardScope {
            file: root.file.clone(),
            start_line: root.span.start_line,
            end_line: root.span.end_line,
            guards: Vec::new(),
        });
        for (class, arg) in classes {
            if !scope.guards.iter().any(|g| g.class == class) {
                scope.guards.push(RouteGuard { class, arg });
            }
        }
    }

    let mut out: Vec<RouteGuardScope> = scopes.into_values().collect();
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// Positional-arg guard: `app.get(path, mw1, mw2, handler)` — args between `path_arg` and `handler_arg`
/// (exclusive) are all middleware; when `handler_arg` is absent, treat the **last** arg as handler and drop it.
fn extract_positional_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
    let mut scopes: HashMap<(String, u32), RouteGuardScope> = HashMap::new();
    for call in calls {
        let Some(rc) = spec.route_calls.iter().find(|rc| match_route_call(call, rc).is_some())
        else {
            continue;
        };
        let path_arg = rc.path_arg;
        let start = path_arg + 1;
        if start >= call.args.len() {
            continue;
        }
        // With handler_arg take [start, handler_arg), else [start, len-1) (drop the last as handler).
        let end = if rc.handler_arg.is_some() {
            rc.handler_arg.unwrap()
        } else {
            call.args.len().saturating_sub(1)
        };
        if start >= end {
            continue;
        }
        let mut guards: Vec<RouteGuard> = Vec::new();
        for a in &call.args[start..end] {
            if let Some(name) = guard_arg_name(a, rc.accept_identifier) {
                guards.push(RouteGuard {
                    class: name,
                    arg: None,
                });
            }
        }
        if guards.is_empty() {
            continue;
        }
        let key = (call.file.clone(), call.span.start_byte);
        let scope = scopes.entry(key).or_insert_with(|| RouteGuardScope {
            file: call.file.clone(),
            start_line: call.span.start_line,
            end_line: call.span.end_line,
            guards: Vec::new(),
        });
        for g in guards {
            if !scope.guards.iter().any(|x| x.class == g.class) {
                scope.guards.push(g);
            }
        }
    }
    let mut out: Vec<RouteGuardScope> = scopes.into_values().collect();
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// Consumer-style mount (NestJS `MiddlewareConsumer`):
/// ```ts
/// // user.module.ts
/// consumer.apply(AuthMiddleware).forRoutes({ path: '*', method: RequestMethod.ALL });
/// ```
/// The middleware is declared in the **module**, acting on routes in the **controller** file — neither same owner nor
/// any handler arg to follow, hence:
/// * mount point: a call with `receiver == consumer` and `method == apply`, guard name from the arg;
/// * scope: the `forRoutes` arg on the same chain. Explicit path matches by path; on wildcard hit, expand by
///   the `ConsumerScope` declared in FKB (default "same dir as module").
fn extract_consumer_guards(
    spec: &RouteGuardSpec,
    cg: &gt_domain::model::ConsumerGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    // Route-definition calls (@Get/@Post...), with parsed (verb, path) attached, for matching by path + verb below.
    let route_entries: Vec<(&CallRecord, String, String)> = calls
        .iter()
        .filter_map(|c| {
            spec.route_calls
                .iter()
                .find_map(|rc| match_route_call(c, rc).map(|(v, p, _)| (c, v, p)))
        })
        .collect();
    if route_entries.is_empty() {
        return Vec::new();
    }
    let is_consumer = |c: &CallRecord| {
        c.receiver
            .as_deref()
            .map(|r| r.to_ascii_lowercase().contains(&cg.receiver.to_ascii_lowercase()))
            .unwrap_or(false)
    };

    let mut out = Vec::new();
    for call in calls {
        if !is_consumer(call) || call.method.as_deref() != Some(cg.apply_method.as_str()) {
            continue;
        }
        // Guard name: `apply(X)` / `apply(A, B)` — same value form as the positional-arg style.
        let mut guards: Vec<RouteGuard> = Vec::new();
        for a in &call.args {
            if let Some(name) = guard_arg_name(a, true) {
                guards.push(RouteGuard { class: name, arg: None });
            }
        }
        if guards.is_empty() {
            continue;
        }
        let targets: Vec<(String, Option<String>)> = calls
            .iter()
            .filter(|c| {
                c.file == call.file
                    && c.span.start_byte == call.span.start_byte
                    && c.method.as_deref() == Some(cg.for_routes_method.as_str())
            })
            .flat_map(|c| c.args.iter().filter_map(parse_for_route_target))
            .filter(|(p, _)| !p.is_empty())
            .collect();
        let wildcard = targets.is_empty()
            || targets
                .iter()
                .any(|(p, _)| cg.wildcards.iter().any(|w| p == w));

        let matched: Vec<&CallRecord> = if wildcard {
            match cg.scope {
                gt_domain::model::ConsumerScope::ExplicitOnly => Vec::new(),
                gt_domain::model::ConsumerScope::All => {
                    route_entries.iter().map(|(rc, _, _)| *rc).collect()
                }
                gt_domain::model::ConsumerScope::Directory => {
                    let dir = match call.file.rfind('/') {
                        Some(i) => call.file[..i].to_string(),
                        None => String::new(),
                    };
                    route_entries
                        .iter()
                        .filter(|(rc, _, _)| match rc.file.rfind('/') {
                            Some(i) => rc.file[..i] == dir,
                            None => dir.is_empty(),
                        })
                        .map(|(rc, _, _)| *rc)
                        .collect()
                }
            }
        } else {
            route_entries
                .iter()
                .filter(|(_, verb, path)| {
                    targets.iter().any(|(tp, tv)| {
                        let tp = tp.trim().trim_start_matches('/');
                        let path_ok = !tp.is_empty() && (*path == tp || path.starts_with(&format!("{}/", tp)));
                        let verb_ok = match tv {
                            None => true,
                            Some(v) => v.eq_ignore_ascii_case(verb),
                        };
                        path_ok && verb_ok
                    })
                })
                .map(|(rc, _, _)| *rc)
                .collect()
        };

        for rc in matched {
            out.push(RouteGuardScope {
                file: rc.file.clone(),
                start_line: rc.span.start_line,
                end_line: rc.span.end_line,
                guards: guards.clone(),
            });
        }
    }
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// Parse one `forRoutes` arg, extract `(path, verb)`.
///
/// * string arg: `forRoutes('user')` → `("user", None)` (verb unrestricted);
/// * object literal: `forRoutes({ path: 'user', method: RequestMethod.GET })` →
///   `("user", Some("GET"))`; `RequestMethod.ALL` treated as verb unrestricted (`None`);
/// * others (`Unknown` text) try to extract by `path: '...'` form; if not, return `None`.
fn parse_for_route_target(a: &FactValue) -> Option<(String, Option<String>)> {
    match a {
        FactValue::String(s) => Some((s.trim().to_string(), None)),
        FactValue::Array(items) => {
            let mut path = None;
            let mut verb = None;
            for (k, v) in items {
                match k.to_ascii_lowercase().as_str() {
                    "path" => path = v.as_str().map(|s| s.trim().to_string()),
                    "method" => verb = request_method_verb(v),
                    _ => {}
                }
            }
            path.map(|p| (p, verb))
        }
        FactValue::Unknown(Some(n)) if n.contains("path") => {
            // Fallback: extract path from `{path: 'x'}` form text (no regex dependency).
            let after = n.split_once("path")?.1;
            let quote = after.find('\'').or_else(|| after.find('"'))?;
            let rest = &after[quote + 1..];
            let end = rest.find(rest.chars().next()?)?;
            let p = rest[..end].trim().to_string();
            if p.is_empty() {
                None
            } else {
                Some((p, None))
            }
        }
        _ => None,
    }
}

/// Normalize `RequestMethod.GET` / `'GET'` / `ALL` into an HTTP verb (`GET`/`POST`...).
///
/// Return `None` means "verb unrestricted" (`RequestMethod.ALL` / empty), the caller matches by path dimension.
fn request_method_verb(v: &FactValue) -> Option<String> {
    let s = match v {
        FactValue::String(s) | FactValue::Unknown(Some(s)) => s.clone(),
        _ => return None,
    };
    let last = s.trim().rsplit('.').next().unwrap_or(s.trim()).trim().to_ascii_uppercase();
    if last.is_empty() || last == "ALL" {
        return None;
    }
    Some(last)
}

/// Decorator / annotation guard: `@UseGuards(X)` / `@login_required` / `@PreAuthorize` lands on the **same method** as the decorated route
/// (the parser already captures decorators as calls, with the decorated method's `owner_fqn` as owner),
/// so associate by `owner_fqn`: all guard decorators on an owner hang onto each route decorator on that owner.
///
/// "Which calls count as route definitions" is recognized both ways, covering two framework shapes:
/// * `route_decorators` list (by callee) — NestJS's `@Get`, Spring's `@GetMapping`;
/// * `route_calls` pattern (by receiver + verb, non-empty return counts) — Python's
///   `app.route(...)` / `router.get(...)` "decorator style but actually plain calls" routes,
///   whose guard hangs on another decorator of the same function, so the guard still associates by owner.
fn extract_decorator_guards(
    spec: &RouteGuardSpec,
    d: &gt_domain::model::DecoratorGuardSpec,
    calls: &[CallRecord],
) -> Vec<RouteGuardScope> {
    // Shape constraint: only recognize calls "that look like decorators", to avoid treating plain method calls as guards.
    let shape_ok = |c: &CallRecord| -> bool {
        if d.require_at_prefix && !c.callee.starts_with('@') {
            return false;
        }
        if d.require_no_receiver && c.receiver.is_some() {
            return false;
        }
        true
    };
    // Only apply the `@` prefix constraint to routes (TS decorators all have `@`); `require_no_receiver`
    // **can't** be used for routes — Python's routes are `app.route(...)`, which have a receiver.
    let route_shape_ok = |c: &CallRecord| -> bool {
        !d.require_at_prefix || c.callee.starts_with('@')
    };
    let is_route = |c: &CallRecord| {
        !c.owner_fqn.is_empty()
            && route_shape_ok(c)
            && (d
                .route_decorators
                .iter()
                .any(|x| short_callee(&c.callee).eq_ignore_ascii_case(x))
                || spec.route_calls.iter().any(|rc| match_route_call(c, rc).is_some()))
    };
    // Guard-name regex (declared by FKB): JS / Python guards are often project-written decorators, enumerating names is unrealistic,
    // so match by "what kind of name counts as a guard". Compile once and reuse; skip invalid regex (don't crash the whole load over it).
    let patterns: Vec<regex::Regex> = d
        .guard_name_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(&format!("(?i){}", p)).ok())
        .collect();
    // Exclude pattern (deny) takes **precedence over** include pattern: better to miss than treat a doc decorator as an auth guard.
    let excludes: Vec<regex::Regex> = d
        .guard_exclude_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(&format!("(?i){}", p)).ok())
        .collect();
    let is_guard = |c: &CallRecord| {
        if c.owner_fqn.is_empty() {
            return false;
        }
        // Shape constraint: must be a call "that looks like a decorator / annotation",
        // otherwise plain business methods like `this.userService.generateJWT(...)` would be treated as guards.
        if !shape_ok(c) {
            return false;
        }
        let name = short_callee(&c.callee);
        if excludes.iter().any(|re| re.is_match(name)) {
            return false;
        }
        d.guard_decorators
            .iter()
            .any(|x| name.eq_ignore_ascii_case(x))
            || patterns.iter().any(|re| re.is_match(name))
    };
    // One guard decorator → its guard name (may be several: array form `@UseGuards(A, B)`).
    let guards_of = |c: &CallRecord| -> Vec<RouteGuard> {
        let decorator_name = || RouteGuard {
            class: short_callee(&c.callee).to_string(),
            arg: None,
        };
        // Spring's `@PreAuthorize("hasRole('ADMIN')")`: the arg is a SpEL expression,
        // the real guard is the annotation itself — declared by `name_from_args: false`.
        if !d.name_from_args {
            return vec![decorator_name()];
        }
        // Exclude pattern also applies to **derived guard names**: the arg may be a local var
        // (measured `_user` treated as middleware), deny first.
        let keep = |name: &str| !excludes.iter().any(|re| re.is_match(name));
        let from_args: Vec<RouteGuard> = c
            .args
            .iter()
            .filter_map(|a| match a {
                FactValue::String(s) | FactValue::ClassConst(s) if !s.trim().is_empty() => {
                    Some(s.trim().to_string())
                }
                FactValue::Unknown(Some(n)) => {
                    // Identifier arg: take the call name (before `(`), e.g. `AuthGuard('jwt')` → `AuthGuard`
                    let base = n.split('(').next().unwrap_or(n).trim().to_string();
                    if base.is_empty() { None } else { Some(base) }
                }
                _ => None,
            })
            .filter(|n| keep(n))
            .map(|n| RouteGuard { class: n, arg: None })
            .collect();
        // Arg-less decorator (`@login_required`): the decorator name itself is the guard.
        if from_args.is_empty() {
            let name = decorator_name();
            if keep(&name.class) { vec![name] } else { vec![] }
        } else {
            from_args
        }
    };

    let route_calls: Vec<&CallRecord> = calls.iter().filter(|c| is_route(c)).collect();
    let guard_calls: Vec<&CallRecord> = calls.iter().filter(|c| is_guard(c)).collect();
    if route_calls.is_empty() || guard_calls.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for rc in route_calls {
        // Guard source ①: **same method / function** (same owner_fqn)
        let mut guards: Vec<RouteGuard> = guard_calls
            .iter()
            .filter(|g| g.owner_fqn == rc.owner_fqn)
            .flat_map(|g| guards_of(g))
            .collect();
        if d.include_class_level {
            let mut class_names: Vec<&str> = Vec::new();
            if let Some(cls) = rc.owner_class.as_deref() {
                class_names.push(cls);
            }
            if let Some(pos) = rc.owner_fqn.rfind(['.', ':', '\\']) {
                let derived = &rc.owner_fqn[..pos];
                if !derived.is_empty() {
                    class_names.push(derived);
                }
            }
            for cls in class_names {
                let from_class: Vec<RouteGuard> = guard_calls
                    .iter()
                    .filter(|g| g.owner_fqn == cls)
                    .flat_map(|g| guards_of(g))
                    .collect();
                for g in from_class {
                    if !guards.iter().any(|x| x.class == g.class) {
                        guards.push(g);
                    }
                }
            }
        }
        // Guard source ③: **cross-file associate by handler arg** (Django: route in `urls.py`,
        // guard decorator on the view function in `views.py`, owners differ).
        if let Some(arg_idx) = d.link_via_handler_arg {
            // handler may be a literal (`path("x", "pkg.mod.view")`) or an **identifier**
            // (`path("x", views.profile)` → `Unknown(Some("views.profile"))`), take both.
            let handler = rc
                .args
                .get(arg_idx)
                .and_then(|a| match a {
                    FactValue::String(s) | FactValue::ClassConst(s) => Some(s.as_str()),
                    FactValue::Unknown(Some(n)) => Some(n.as_str()),
                    _ => None,
                })
                .unwrap_or_default()
                .trim()
                .trim_start_matches('.')
                .to_string();
            if !handler.is_empty() {
                let hl = handler.to_ascii_lowercase();
                let from_handler: Vec<RouteGuard> = guard_calls
                    .iter()
                    .filter(|g| {
                        let gl = g.owner_fqn.to_ascii_lowercase();
                        gl == hl || gl.ends_with(&format!(".{}", hl))
                    })
                    .flat_map(|g| guards_of(g))
                    .collect();
                for g in from_handler {
                    if !guards.iter().any(|x| x.class == g.class) {
                        guards.push(g);
                    }
                }
            }
        }
        if guards.is_empty() {
            continue;
        }
        out.push(RouteGuardScope {
            file: rc.file.clone(),
            start_line: rc.span.start_line,
            end_line: rc.span.end_line,
            guards,
        });
    }
    out.sort_by_key(|s| (s.file.clone(), s.start_line, s.end_line));
    out
}

/// Middleware alias table: **file name, block marker, key-value separator all declared by FKB** (`params`).
///
/// What's written on the route is often an **alias** (Laravel's `->middleware('auth')`); the real class name is registered elsewhere.
/// Without restoration we only know "passed something called auth", can't connect to the real class node.
///
/// # Why it must be declared by FKB
///
/// All three are **framework / version conventions**, the kernel shouldn't know any of them:
/// * file name: Laravel is `app/Http/Kernel.php`, other frameworks may not have this file at all;
/// * block marker: Laravel ≤9 is `$routeMiddleware`, **10.x renamed it `$middlewareAliases`**
///   (measured: 10.x skeleton is the latter — hard-coding the former empties the whole table);
/// * separator: PHP is `=>`, other languages may be `:` / `=`.
///
/// So FKB gives `paths` / `markers` (**multiple, tried one by one**) / `end` / `separator` / `extensions`,
/// the kernel only does "scan fixed syntax by declaration" -- same as `load_nginx`:
/// **better only recognize fixed syntax than introduce a whole parser**.
fn load_middleware_aliases(
    ctx: &mut PipelineContext,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    params: &Value,
) {
    let app_root = app_root_of(ctx, sub);
    let strs = |k: &str| -> Vec<String> {
        params
            .get(k)
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default()
    };
    // The app directory is resolved by `root_rules` (`app_root`), not hard-coded — a project whose
    // psr-4 maps the app namespace to `application/` (ThinkPHP 5.x) or `src/` would otherwise be missed.
    let paths: Vec<String> = strs("paths")
        .into_iter()
        .map(|p| p.replace("{app_root}", &app_root))
        .collect();
    if paths.is_empty() {
        return;
    }
    let markers = strs("markers");
    // How the alias block ends, how a key is separated from its class, and which files to scan are all
    // **stack knowledge** (PHP's `];` / `=>` / `.php`), so FKB declares them — there is no built-in
    // default any more (a default would silently apply one stack's syntax to every other).
    let end = params.get("end").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let separator = params.get("separator").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let exts = strs("extensions");
    if end.is_empty() || separator.is_empty() || exts.is_empty() {
        warn!(
            "built-in loader `middleware_aliases` needs `end` / `separator` / `extensions` in `params` \
             (they are stack syntax, not kernel defaults)"
        );
        return;
    }
    let exts: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
    // Suffix marking a class reference (`::class` in PHP; empty where a bare identifier is used).
    let class_suffix = params
        .get("class_suffix")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    // Whether a value must contain a namespace separator to count as a class (PHP's `App\Http\X`).
    let require_namespace = params.get("require_namespace").and_then(Value::as_bool).unwrap_or(false);
    // The namespace separator is this language's, never assumed.
    let separators = ctx.lang_policy_for_sub(Some(sub.id)).ns_separators.clone();

    for (path, text) in scan_text_files(project_root, &exts) {
        // Both `**/X` and `X` match by "path ends with this".
        let hit = paths.iter().any(|p| path.ends_with(p.trim_start_matches("**/")));
        if !hit {
            continue;
        }
        let block: String = if markers.is_empty() {
            text.clone()
        } else {
            match markers.iter().find_map(|m| text.split(m.as_str()).nth(1)) {
                Some(b) => b.to_string(),
                None => continue,
            }
        };
        let block = block.split(&end).next().unwrap_or(&block);
        for line in block.lines() {
            let line = line.trim();
            let Some((key, rest)) = line.split_once(&separator) else {
                continue;
            };
            let key = key.trim().trim_matches('\'').trim_matches('"').trim();
            let mut class = rest.trim().trim_end_matches(',').trim();
            if !class_suffix.is_empty() {
                if let Some(c) = class.strip_suffix(&class_suffix) {
                    class = c.trim();
                }
            }
            let class = class.trim_start_matches(|c| separators.contains(&c)).trim();
            if key.is_empty() || class.is_empty() {
                continue;
            }
            if require_namespace && !class.contains(|c| separators.contains(&c)) {
                continue;
            }
            ctx.ws.put_symbol(
                ctx.project.id,
                MIDDLEWARE_ALIASES,
                key,
                json!({ "class": class, "file": path }),
            );
        }
    }
}

/// The symbol-table name for the middleware alias table.
const MIDDLEWARE_ALIASES: &str = "middleware_aliases";

/// Restore an **alias** in a mount arg to a class name (`auth` → `App\Http\Middleware\Authenticate`).
///
/// One with a namespace separator is treated as already a class name (`app\api\middleware\AuthToken` / `AuthToken::class`),
/// returned as-is; otherwise look up the alias table. When not found, **return the original name** (`throttle:60`-style param-bearing aliases
/// store `throttle` in the table, here look up again by the part before the colon).
fn resolve_guard_class(
    ctx: &PipelineContext,
    raw: &str,
    alias_table: Option<&str>,
    separators: &[char],
) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // A namespace separator (this language's) means it is already a class name, not an alias.
    if raw.contains(|c| separators.contains(&c)) || raw.contains('/') {
        return Some(raw.trim_start_matches(|c| separators.contains(&c)).to_string());
    }
    let base = raw.split(':').next().unwrap_or(raw).trim();
    let table = alias_table.unwrap_or(MIDDLEWARE_ALIASES);
    if let Some(entry) = ctx
        .ws
        .symbols
        .get(table)
        .and_then(|t| t.get(base))
        .and_then(|v| v.get("class"))
        .and_then(|c| c.as_str())
    {
        return Some(entry.to_string());
    }
    // Don't fabricate when not found: return None, let the caller skip (better missing than guessed).
    if raw.contains(|c| separators.contains(&c)) {
        Some(raw.to_string())
    } else {
        None
    }
}

/// Take a mount arg's readable text (`true` / `60` / `'auth:api'`...); return `None` when not statically evaluable.
fn guard_arg_text(v: &FactValue) -> Option<String> {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) => Some(s.clone()),
        FactValue::Bool(b) => Some(b.to_string()),
        FactValue::Int(i) => Some(i.to_string()),
        FactValue::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

fn load_routes(
    ctx: &mut PipelineContext,
    spec: &RouteGuardSpec,
    sub: &gt_domain::model::SubProject,
    contract_steps: &[NormalizeStep],
) {
    if ctx.ws.route_groups.is_empty() {
        let mut groups: Vec<RouteGroup> = Vec::new();
        for rc in &spec.route_calls {
            let Some(gm) = &rc.group_method else { continue };
            for call in ctx.ws.calls.iter() {
                if !receiver_matches(rc, call.receiver.as_deref().unwrap_or("")) {
                    continue;
                }
                if call.method.as_deref() != Some(gm.as_str()) {
                    continue;
                }
                // No-prefix style: `group(fn)`'s arg0 is a closure not a string, skip.
                let Some(gt_domain::model::FactValue::String(prefix)) = call.args.first() else {
                    continue;
                };
                let prefix = prefix.trim().trim_matches('/').trim().to_string();
                if prefix.is_empty() {
                    continue;
                }
                groups.push(RouteGroup {
                    file: call.file.clone(),
                    start_line: call.span.start_line,
                    end_line: call.span.end_line,
                    prefix,
                });
            }
        }
        if !groups.is_empty() {
            info!("P3 route groups: {} route groups with prefixes", groups.len());
            ctx.ws.add_route_groups(groups);
        }
    }

    // When FKB authorizes "building a Middleware node even for guards whose node isn't found", register it to the workspace for P14 to read.
    // (JS / Python middleware are function values, the parser doesn't build a syntax node for them, so this authorization is needed.)
    if spec.synthesize_unresolved {
        ctx.ws.synthesize_unresolved_guards = true;
    }

    // ② middleware mount: **must register after prefixes** (key building below needs to query), and register only once too.
    if ctx.ws.route_guard_scopes().is_empty() {
        let scopes = collect_route_guards(spec, &ctx.ws.calls);
        if !scopes.is_empty() {
            let total: usize = scopes.iter().map(|s| s.guards.len()).sum();
            info!("P3 route guards: {} scope ranges / {} middleware mounts", scopes.len(), total);
        }
        ctx.ws.add_route_guard_scopes(scopes);
    }

    // ③ build `route_list`: key = `METHOD /normalized path`, identical character-by-character to the `HttpContract.name` synthesized by P5.
    let alias_table = spec.alias_table.as_deref();
    let mut found: Vec<(String, String, String, u32, Vec<RouteGuard>)> = Vec::new();
    for call in ctx.ws.calls.iter() {
        // Find a matching route definition (incl. decorator model): try `route_calls` patterns one by one.
        let Some((method, path, handler)) =
            spec.route_calls.iter().find_map(|rc| match_route_call(call, rc))
        else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        // Key normalization copies P5's CONTRACT_ID three steps: ① FKB http-contract normalize;
        // ② insert route-group prefix after the leading slash; ③ pass the whole thing through the same normalize again (idempotent).
        let steps = contract_steps;
        let normed = crate::normalize::apply_normalize(&path, steps);
        let prefix = ctx.ws.route_group_prefix(&call.file, call.span.start_line);
        let with_prefix = if prefix.is_empty() {
            normed
        } else {
            format!("/{}/{}", prefix, normed.trim_start_matches('/'))
        };
        let key = format!(
            "{} {}",
            method.to_uppercase(),
            crate::normalize::apply_normalize(&with_prefix, steps)
        );
        let guards = ctx.ws.route_guards(&call.file, call.span.start_line);
        found.push((key, handler.unwrap_or_default(), call.file.clone(), call.span.start_line, guards));
    }
    for (key, handler, file, line, guards) in found {
        let mut value = json!({ "handler": handler, "file": file, "line": line });
        let guards: Vec<RouteGuard> = guards
            .into_iter()
            .map(|g| RouteGuard {
                class: resolve_guard_class(
                    ctx,
                    &g.class,
                    alias_table,
                    &ctx.lang_policy_for_sub(Some(sub.id)).ns_separators,
                )
                .unwrap_or_else(|| g.class.clone()),
                arg: g.arg,
            })
            .collect();
        if !guards.is_empty() {
            value["guards"] = json!(guards
                .iter()
                .map(|g| json!({ "class": g.class, "arg": g.arg }))
                .collect::<Vec<_>>());
        }
        ctx.ws.put_symbol(ctx.project.id, "route_list", &key, value);
    }
}

fn load_nginx(
    ctx: &mut PipelineContext,
    _sub: &gt_domain::model::SubProject,
    project_root: &Path,
    _fs: &dyn FileSystem,
) {
    for (path, text) in scan_text_files(project_root, &["conf"]) {
        let mut server_name = String::new();
        let mut root_dir = String::new();
        let mut locations: Vec<Value> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("server_name") {
                server_name = rest.trim_end_matches(';').trim().to_string();
            } else if let Some(rest) = line.strip_prefix("root") {
                root_dir = rest.trim_end_matches(';').trim().to_string();
            } else if line.starts_with("location") {
                locations.push(json!({ "raw": line }));
            }
        }
        if server_name.is_empty() && root_dir.is_empty() && locations.is_empty() {
            continue;
        }
        ctx.ws.put_symbol(
            ctx.project.id,
            "nginx",
            &path,
            json!({
                "server_name": server_name,
                "root": root_dir,
                "locations": locations,
            }),
        );
    }
}

/// Declarative middleware list: file name / key / scope **all declared by FKB `params`** — same idea as `middleware_aliases`,
/// `nginx_config`, the kernel only does "scan fixed syntax by declaration", knows no framework / la
///
/// Middleware is often registered in a **file**, not via a route call `->middleware()`:
/// * ThinkPHP global: `{app_root}/middleware.php` returns a bare class array `return [A::class, B::class];`;
/// * ThinkPHP multi-app: `{app_root}/<app>/config/route.php`'s `'middleware' => [A::class, ...]`.
/// These writings produce no route-call chain, so `guard_attach` can't recognize them, `route_list`'s `guards` is entirely empty,
/// and the graph has no middleware at all (likeadmin is typical).
///
/// This loader stores "declared class list + scope" into the workspace `declared_middleware` symbol table; **doesn't build nodes directly**
/// — because HttpContract nodes (especially auto-route ones like likeadmin, or contracts synthesized by the `frontend-js-common-http-contract` rule)
/// don't exist yet at prepare time. The real nodes / `PassesThrough` edges are hung by a post-build step
/// (see `gt-application::PipelineService::attach_declared_middleware`) once HttpContract nodes are complete, by scope,
/// reusing P14's same mechanism, kernel zero framework strings.
///
/// # params
/// * `paths`: files to scan (suffix match, supports `**/X`). `per_app` supports a single `*` wildcard segment
///   (e.g. `{app_root}/*/config/route.php`), the matched `*` segment is the app name. The `{app_root}`
///   placeholder is expanded from the `root_rules`-resolved app directory.
/// * `key`: optional. The key the declared array lives under; when omitted, take the first `[...]` array of the whole file
///   (i.e. `return [A::class, ...];` form).
/// * `scope`: `global` (hang on all routes, default) or `per_app` (only on routes whose name contains the `/<app>` prefix,
///   `prefix` is `/<app>`, extracted from the `*` wildcard segment of `paths`).
fn load_declared_middleware(
    ctx: &mut PipelineContext,
    sub: &gt_domain::model::SubProject,
    project_root: &Path,
    params: &Value,
) {
    let app_root = app_root_of(ctx, sub);
    // The app directory is resolved by `root_rules` (`app_root`), not hard-coded — a project whose
    // psr-4 maps the app namespace to `application/` (ThinkPHP 5.x) or `src/` would otherwise be missed.
    let paths: Vec<String> = params
        .get("paths")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.replace("{app_root}", &app_root))
        .collect();
    if paths.is_empty() {
        return;
    }
    let key = params.get("key").and_then(Value::as_str).map(|s| s.to_string());
    let scope = params
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or("global");
    let exts = params
        .get("extensions")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        .unwrap_or_default();
    // The array *syntax* is stack knowledge (PHP's `[ … ]` / `=>` / `//` / `::class`), declared by FKB.
    let Some(syn) = ArraySyntax::from_params(params, &ctx.lang_policy_for_sub(Some(sub.id)).ns_separators) else {
        warn!(
            "built-in loader `declared_middleware` needs `open` / `close` / `extensions` in `params` \
             (they are stack syntax, not kernel defaults)"
        );
        return;
    };
    let exts: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();

    let mut idx: u32 = 0;
    for (path, text) in scan_text_files(project_root, &exts) {
        let hit = paths
            .iter()
            .any(|p| declared_mw_path_matches(p, &path));
        if !hit {
            continue;
        }
        let classes = extract_middleware_classes(&text, key.as_deref(), &syn);
        if classes.is_empty() {
            continue;
        }
        // `per_app` extracts the app name from the `*` segment of `paths` like `app/*/config/route.php`
        // as the route prefix (e.g. `/adminapi`) — align at the end, unaffected by extra dir layers under project_root.
        let prefix: Option<String> = if scope == "per_app" {
            paths.iter().find_map(|p| {
                let dseg: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
                let aseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
                if aseg.len() < dseg.len() {
                    return None;
                }
                let suffix = &aseg[aseg.len() - dseg.len()..];
                dseg.iter().zip(suffix).find_map(|(d, a)| {
                    if *d == "*" {
                        Some(format!("/{}", a))
                    } else {
                        None
                    }
                })
            })
        } else {
            None
        };
        ctx.ws.put_symbol(
            ctx.project.id,
            "declared_middleware",
            &format!("{}_{}", scope, idx),
            json!({ "classes": classes, "scope": scope, "prefix": prefix }),
        );
        idx += 1;
        info!(
            "P3 声明式中间件：{} 个类（作用域 {:?}{}）",
            classes.len(),
            scope,
            prefix.map(|p| format!(", 前缀 {}", p)).unwrap_or_default()
        );
    }
}

/// Extract the middleware class-name list from file text.
///
/// * when `key` is given (e.g. `middleware`): take the `key => [ ... ]` array block;
///   note `key` may appear in a namespace (e.g. `app\...\middleware\Foo`), so require `=>` immediately after the key
///   (skip quotes / whitespace) to count as an assignment key, avoiding hitting a namespace.
/// * otherwise: take the first `[ ... ]` array of the whole file (i.e. `return [A::class, ...];` form).
/// Each line takes `X::class` (strip `//` comments, trailing comma, leading `\`), get the normalized FQN.
/// The **syntax of the declared array**, all of it stack knowledge (`[ … ]` / `=>` / `//` / `::class`
/// for PHP) and therefore declared by FKB — the kernel knows none of these characters.
struct ArraySyntax {
    open: char,
    close: char,
    /// Separates a key from its value (`=>` in PHP, `:` in JSON-ish syntaxes).
    pair_separator: String,
    /// Starts a line comment; empty means "no comment syntax declared".
    comment: String,
    /// Suffix marking a class reference (`::class` in PHP; empty where a bare identifier is used).
    class_suffix: String,
    /// This language's namespace separators, used to strip a leading one (`\App\X` -> `App\X`).
    separators: Vec<char>,
}

impl ArraySyntax {
    fn from_params(params: &Value, separators: &[char]) -> Option<Self> {
        let single = |k: &str| {
            params
                .get(k)
                .and_then(Value::as_str)
                .and_then(|s| s.chars().next())
        };
        let open = single("open")?;
        let close = single("close")?;
        let strs = |k: &str| {
            params
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let exts: Vec<String> = params
            .get("extensions")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default();
        if exts.is_empty() {
            return None;
        }
        Some(Self {
            open,
            close,
            pair_separator: strs("separator"),
            comment: strs("comment"),
            class_suffix: strs("class_suffix"),
            separators: separators.to_vec(),
        })
    }

    /// Index of the `close` that balances the `open` at index 0 of `s`.
    fn matching_close(&self, s: &str) -> Option<usize> {
        let mut depth = 0i32;
        for (i, b) in s.as_bytes().iter().enumerate() {
            if *b == self.open as u8 {
                depth += 1;
            } else if *b == self.close as u8 {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        None
    }
}

fn extract_middleware_classes(text: &str, key: Option<&str>, syn: &ArraySyntax) -> Vec<String> {
    let block = match key {
        Some(k) => {
            // Find "<key><pair_separator><open>": iterate all hits, take the one with the separator
            // immediately after the key.
            let mut from = 0;
            let mut found: Option<usize> = None;
            while let Some(rel) = text[from..].find(k) {
                let abs = from + rel;
                let rest = &text[abs + k.len()..];
                let after = rest
                    .trim_start_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace());
                if syn.pair_separator.is_empty() || after.starts_with(&syn.pair_separator) {
                    found = Some(abs);
                    break;
                }
                from = abs + k.len();
            }
            match found {
                Some(idx) => {
                    let after = &text[idx + k.len()..];
                    let after = after
                        .trim_start()
                        .strip_prefix(syn.pair_separator.as_str())
                        .unwrap_or(after);
                    match after.find(syn.open) {
                        Some(open) => {
                            let rest = &after[open..];
                            match syn.matching_close(rest) {
                                Some(close) => rest[1..close].to_string(),
                                None => return Vec::new(),
                            }
                        }
                        None => return Vec::new(),
                    }
                }
                None => return Vec::new(),
            }
        }
        None => match text.find(syn.open) {
            Some(open) => match syn.matching_close(&text[open..]) {
                Some(close) => text[open + 1..open + close].to_string(),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        },
    };
    let mut out = Vec::new();
    for line in block.lines() {
        let line = if syn.comment.is_empty() {
            line
        } else {
            line.split(&syn.comment).next().unwrap_or(line)
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A `key <separator> value` entry: only the value side names a class (a bare array entry has
        // no separator and is taken whole).
        let value = match line.split_once(&syn.pair_separator) {
            Some((_, v)) if !syn.pair_separator.is_empty() => v.trim(),
            _ => line,
        };
        let value = value
            .trim_end_matches(',')
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .trim();
        if value.is_empty() {
            continue;
        }
        let c = if syn.class_suffix.is_empty() {
            value
        } else {
            match value.strip_suffix(&syn.class_suffix) {
                Some(c) => c,
                None => continue,
            }
        };
        let c = c.trim().trim_start_matches(|c| syn.separators.contains(&c)).trim();
        if !c.is_empty() {
            out.push(c.to_string());
        }
    }
    out
}

/// Path suffix match (align at end), supports `*` wildcard segment (matches exactly one path segment).
///
/// For `declared_middleware`'s `paths`: declaring `{app_root}/*/config/route.php` hits
/// `server/{app_root}/adminapi/config/route.php` (no matter how many dir layers under project_root), unlike `engine::path_matches`
/// which requires the pattern to match from the start — auto-route projects' app dirs often hide in `server/app/<app>` subdirs.
/// Without `*` degenerates to plain suffix equality.
fn declared_mw_path_matches(pattern: &str, path: &str) -> bool {
    let pseg: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let aseg: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if aseg.len() < pseg.len() {
        return false;
    }
    let suffix = &aseg[aseg.len() - pseg.len()..];
    pseg.iter()
        .zip(suffix)
        .all(|(p, a)| *p == "*" || p == a)
}

/// Scan text files of the given extension (skip dependency dirs).
fn scan_text_files(root: &Path, exts: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut walker = walkdir::WalkDir::new(root).follow_links(false).into_iter();
    while let Some(entry) = walker.next() {
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_dir() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                "vendor" | "node_modules" | "target" | ".git" | "dist" | "build" | "runtime"
            ) {
                walker.skip_current_dir();
            }
            continue;
        }
        let ext = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, text));
        }
        if out.len() > 400 {
            break;
        }
    }
    out
}

/// Parse `CREATE TABLE`: take table name and column names (by paren pairing, avoid truncation by `int(11)`).
fn parse_create_tables(sql: &str) -> Vec<(String, Vec<String>)> {
    let re = regex::Regex::new(
        r#"(?is)CREATE\s+TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?[`"]?([\w.]+)[`"]?\s*\("#,
    )
    .unwrap();
    let mut out = Vec::new();
    for m in re.find_iter(sql) {
        let caps = re.captures(&sql[m.start()..]).unwrap();
        let table = match caps.get(1) {
            Some(t) => t.as_str().trim_matches(|c| c == '`' || c == '"').to_string(),
            None => continue,
        };
        // Paren pairing from `(`
        let bytes = sql.as_bytes();
        let open = m.start() + m.as_str().len() - 1;
        let mut depth = 0i32;
        let mut close = None;
        let mut i = open;
        let mut in_quote: Option<u8> = None;
        while i < bytes.len() {
            let b = bytes[i];
            match in_quote {
                Some(q) => {
                    if b == q {
                        in_quote = None;
                    }
                }
                None => match b {
                    b'`' | b'"' | b'\'' => in_quote = Some(b),
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(i);
                            break;
                        }
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        let Some(close) = close else { continue };
        let body = &sql[open + 1..close];
        out.push((table, split_columns(body)));
    }
    out
}

/// Split the first-level commas of a `CREATE TABLE` field list.
fn split_columns(body: &str) -> Vec<String> {
    let mut columns = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    let mut in_quote: Option<u8> = None;
    for b in body.bytes() {
        match in_quote {
            Some(q) => {
                current.push(b as char);
                if b == q {
                    in_quote = None;
                }
            }
            None => match b {
                b'`' | b'"' | b'\'' => {
                    current.push(b as char);
                    in_quote = Some(b);
                }
                b'(' => {
                    depth += 1;
                    current.push('(');
                }
                b')' => {
                    depth -= 1;
                    current.push(')');
                }
                b',' if depth == 0 => {
                    push_column(&mut columns, &current);
                    current.clear();
                }
                _ => current.push(b as char),
            },
        }
    }
    push_column(&mut columns, &current);
    columns
}

fn push_column(out: &mut Vec<String>, raw: &str) {
    let line = raw.trim().trim_end_matches(',');
    if line.is_empty() || line.starts_with("--") || line.starts_with('#') {
        return;
    }
    let upper = line.to_ascii_uppercase();
    for kw in [
        "PRIMARY KEY",
        "KEY ",
        "UNIQUE",
        "INDEX",
        "CONSTRAINT",
        "FOREIGN",
        "FULLTEXT",
        "CHECK",
        "SPATIAL",
    ] {
        if upper.starts_with(kw) {
            return;
        }
    }
    if let Some(first) = line.split_whitespace().next() {
        let col = first.trim_matches(|c| c == '`' || c == '"' || c == '[' || c == ']');
        if !col.is_empty() && col.len() < 64 && col.chars().all(|c| c.is_alphanumeric() || c == '_') {
            out.push(col.to_string());
        }
    }
}

/// For P5 to judge whether a config-entry file matches (incl. `{app_root}` placeholder expansion).
pub fn expand(path: &str, app_root: &str) -> String {
    path.replace("{app_root}", app_root)
}

/// Resolve the project's app-root directory name, falling back to `app` when `root_rules` has not
/// produced an `app_root` fact (or it is not a string). Used to expand the `{app_root}` placeholder
/// in loader `path` / `paths` declarations — the app directory is a project decision, never hard-coded.
pub(crate) fn app_root_of(ctx: &PipelineContext, sub: &gt_domain::model::SubProject) -> String {
    ctx.ws
        .get_fact(sub.id, "app_root")
        .and_then(|v| v.get("value"))
        .and_then(|v| v.as_str())
        .unwrap_or("app")
        .to_string()
}

/// For test use.
pub fn _sub_id(id: SubProjectId) -> SubProjectId {
    id
}

/// Textualize a `FactValue`.
trait LossyText {
    fn to_string_lossy(&self) -> String;
}

impl LossyText for gt_domain::model::FactValue {
    fn to_string_lossy(&self) -> String {
        match self {
            gt_domain::model::FactValue::String(s) | gt_domain::model::FactValue::ClassConst(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }
}

/// Rule dedup (by id).
fn dedup_rules(rules: Vec<Rule>) -> Vec<Rule> {
    let mut out: Vec<Rule> = Vec::new();
    for r in rules {
        if !out.iter().any(|x| x.id == r.id) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{collect_route_guards, guard_arg_text, load_schema};
    use crate::workspace::{CallRecord, GraphWorkspace};
    use gt_domain::model::Phase;

    use super::{detect_frameworks, expand_provided, lock_has, manifest_has, CodeEvidence};
    use super::{load_declared_middleware, PipelineContext};
    use std::collections::HashMap;
    use serde_json::json;

    /// Dependency / lock layouts are ecosystem knowledge, so these tests need a real registry:
    /// composer.json / composer.lock are read by the PHP adapter, package-lock.json by the JS one.
    fn techstack() -> gt_domain::port::DefaultTechStackRegistry {
        gt_domain::port::DefaultTechStackRegistry::new()
            .register(Box::new(gt_adapter_php::PhpTechStackAdapter::new()))
            .register(Box::new(gt_adapter_techstack::JsTechStackAdapter::new()))
    }
    use gt_domain::model::{
        ChainGuardSpec, ConsumerGuardSpec, ConsumerScope, Detector, FactValue, FrameworkKnowledge,
        GuardAttach, GuardAttachSpec, KnowledgeScope, Language, NodeId, Project, ProjectId,
        ProjectStatus, RouteCallSpec, RouteGuardSpec, RouteMatchBy, Span, SubProject, SubProjectId,
    };

    /// ThinkPHP 6's `route_guards` declaration (minimal set equivalent to `fkb/php/thinkphp.yaml`), for test reuse.
    fn tp6_spec() -> RouteGuardSpec {
        RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Route".into(),
                by: RouteMatchBy::Receiver,
                verb_methods: [
                    ("get".into(), "GET".into()),
                    ("post".into(), "POST".into()),
                    ("put".into(), "PUT".into()),
                    ("delete".into(), "DELETE".into()),
                    ("patch".into(), "PATCH".into()),
                    ("any".into(), "ANY".into()),
                    ("rule".into(), "ANY".into()),
                ]
                .into_iter()
                .collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: Some("group".into()),
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Chain(ChainGuardSpec {
                method: "middleware".into(),
                arg_index: 0,
                arg2_index: Some(1),
            })),
            alias_table: None,
            synthesize_unresolved: false,
        }
    }

    /// Build a call site: `Route::group('pc', fn)` / `Route::get('x','C@m')` / `->middleware(...)`.
    ///
    /// `byte` is this call's **start byte** in source: all call sites on the same chain share it
    /// (`member_call_expression` starts from the root source segment), this is the only basis for pairing.
    fn call(
        file: &str,
        receiver: &str,
        method: &str,
        args: Vec<FactValue>,
        start_line: u32,
        end_line: u32,
        byte: u32,
        end_byte: u32,
    ) -> CallRecord {
        CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "<file>".into(),
            owner_class: None,
            callee: format!("{}::{}", receiver, method),
            receiver: Some(receiver.into()),
            method: Some(method.into()),
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line,
                end_line,
                start_byte: byte,
                end_byte,
            },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        }
    }

    fn class(name: &str) -> FactValue {
        FactValue::ClassConst(name.into())
    }

    /// CRMEB's real style: `Route::group('pc', fn){ ... })->middleware(A)->middleware(B, true)`,
    /// multiple routes inside the group. Middleware must land on **every** route.
    #[test]
    fn group_level_guard_covers_inner_routes() {
        let f = "app/api/route/pc.php";
        let calls = vec![
            // root: `Route::group(function(){...})` (no-prefix style, spans lines 1..3), byte=10
            call(f, "Route", "group", vec![], 1, 3, 10, 200),
            call(f, "Route", "get", vec![FactValue::String("a".into())], 2, 2, 40, 90),
            // the two `->middleware` on the chain: share start_byte=10 with the root, longer span
            call(
                f,
                "Route",
                "middleware",
                vec![class(r"app\http\middleware\AllowOriginMiddleware")],
                1,
                4,
                10,
                230,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(true),
                ],
                1,
                5,
                10,
                260,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "一条链只应产出一段区间");
        assert_eq!((scopes[0].start_line, scopes[0].end_line), (1, 3), "区间必须取根的 span");
        assert_eq!(scopes[0].guards.len(), 2, "两个中间件都要收");

        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 2);
        let names: Vec<&str> = got.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(
            names,
            vec![
                r"app\http\middleware\AllowOriginMiddleware",
                r"app\api\middleware\AuthTokenMiddleware",
            ]
        );
        assert_eq!(got[1].arg.as_deref(), Some("true"), "`AuthToken::class, true` 的第二参数要留下");
        // Routes outside the range shouldn't be guarded by it (this is exactly the "global vs group-level" boundary)
        assert!(ws.route_guards(f, 40).is_empty());
    }

    /// Route's own `->middleware(X::class, false)`: granularity down to a single route, the second arg only appears here.
    #[test]
    fn route_level_guard_keeps_force_flag() {
        let f = "app/api/route/user.php";
        let calls = vec![
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("info".into())],
                7,
                7,
                10,
                60,
            ),
            call(
                f,
                "Route",
                "middleware",
                vec![
                    class(r"app\api\middleware\AuthTokenMiddleware"),
                    FactValue::Bool(false),
                ],
                7,
                8,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1);
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 7);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].arg.as_deref(), Some("false"));
    }

    /// Laravel's **modifier-prefix** style: `Route::middleware('auth')->group(fn){...}`.
    /// The key is the root node must be `group` (only it wraps the route range), not the chain-head `middleware('auth')`.
    #[test]
    fn laravel_prefix_form_binds_to_group() {
        let f = "routes/web.php";
        let calls = vec![
            // chain head: `Route::middleware('auth')`, very short span (only this small segment)
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::String("auth".into())],
                1,
                1,
                10,
                40,
            ),
            // chain tail: `->group(fn){...}`, span covers the whole closure (lines 1~9)
            call(f, "Route", "group", vec![], 1, 9, 10, 400),
            // a route inside the group
            call(
                f,
                "Route",
                "get",
                vec![FactValue::String("profile".into())],
                3,
                3,
                90,
                130,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes.len(), 1, "整条链只应产出一段区间");
        assert_eq!(
            (scopes[0].start_line, scopes[0].end_line),
            (1, 9),
            "区间必须取 group 的 span（否则包不住组内路由）"
        );
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(scopes);
        let got = ws.route_guards(f, 3);
        assert_eq!(got.len(), 1, "组内路由应当被这个别名守卫覆盖");
        assert_eq!(got[0].class, "auth");
    }

    /// Array-form multiple aliases (`->middleware(['auth', 'throttle:60'])`) must be collected separately.
    #[test]
    fn collects_array_form_guards() {
        let f = "routes/web.php";
        let calls = vec![
            call(f, "Route", "get", vec![FactValue::String("x".into())], 1, 1, 10, 60),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Array(vec![
                    ("0".into(), FactValue::String("auth".into())),
                    ("1".into(), FactValue::String("throttle:60".into())),
                ])],
                1,
                2,
                10,
                90,
            ),
        ];
        let scopes = collect_route_guards(&tp6_spec(), &calls);
        assert_eq!(scopes[0].guards.len(), 2, "数组里每一项都算一次挂载");
    }

    /// The same-name `->middleware()` on a non-Route receiver must not mis-match;
    /// non-scalar args (`->middleware($v)`) shouldn't produce pseudo-facts.
    #[test]
    fn ignores_foreign_and_dynamic_middleware() {
        let f = "app/Service.php";
        let calls = vec![
            call(f, "$router", "middleware", vec![class("app\\A")], 1, 2, 10, 30),
            call(f, "Route", "get", vec![FactValue::String("x".into())], 3, 3, 50, 80),
            call(
                f,
                "Route",
                "middleware",
                vec![FactValue::Unknown(Some("$mw".into()))],
                3,
                4,
                50,
                100,
            ),
        ];
        assert!(collect_route_guards(&tp6_spec(), &calls).is_empty());
        assert_eq!(guard_arg_text(&FactValue::Int(60)), Some("60".into()));
        assert_eq!(guard_arg_text(&FactValue::Null), None);
    }

    /// `declared_middleware`'s `paths` must honour the `root_rules`-resolved `app_root`, not a hard-coded
    /// `app/` — a ThinkPHP 5.x project whose psr-4 maps the app namespace to `application/` would otherwise
    /// never be scanned, silently losing every declarative middleware.
    #[test]
    fn declared_middleware_respects_app_root_fact_not_hardcoded_app() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        // Not `app/` — this project's app directory is `application/` (ThinkPHP 5.x convention).
        ctx.ws
            .set_fact(sub.id, "app_root", json!({ "value": "application" }));

        let dir = std::env::temp_dir().join(format!("gt_mw_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("application"));
        std::fs::write(
            dir.join("application").join("middleware.php"),
            "<?php\nreturn [\n    App\\Foo::class,\n    App\\Bar::class,\n];",
        )
        .unwrap();

        // `app/middleware.php` would not exist; only `{app_root}/middleware.php` (expanded to
        // `application/middleware.php`) should match.
        // The array syntax is declared, not assumed: PHP's `[ … ]` / `=>` / `//` / `::class`.
        let params = json!({
            "paths": ["{app_root}/middleware.php"],
            "scope": "global",
            "open": "[",
            "close": "]",
            "separator": "=>",
            "comment": "//",
            "class_suffix": "::class",
            "extensions": ["php"],
        });
        load_declared_middleware(&mut ctx, &sub, &dir, &params);

        let sym = ctx
            .ws
            .get_symbol("declared_middleware", "global_0")
            .expect("declared middleware should be recorded under the resolved app dir");
        let classes: Vec<String> = sym
            .get("classes")
            .and_then(|c| c.as_array())
            .unwrap_or(&Vec::new())
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            classes,
            vec!["App\\Foo".to_string(), "App\\Bar".to_string()],
            "must resolve middleware from the `{{app_root}}`-expanded path, not a hard-coded `app/`"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Express positional-arg style: `app.get('/x', mw1, mw2, handler)` — args after path are all middleware.
    #[test]
    fn express_positional_guards() {
        let f = "routes/index.js";
        let rc = RouteCallSpec {
            receiver: "app".into(),
            by: RouteMatchBy::Receiver,
            verb_methods: [("get".into(), "GET".into()), ("post".into(), "POST".into())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None, // Treat the last argument as a handler and drop it
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![rc],
            guard_attach: GuardAttach::One(GuardAttachSpec::Positional),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let calls = vec![
            call(f, "app", "get", vec![
                FactValue::String("/cart".into()),
                class("AuthMiddleware"),
                class("CartOwnerMiddleware"),
                FactValue::String("handler".into()),
            ], 1, 1, 10, 80),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        let names: Vec<&str> = scopes[0].guards.iter().map(|g| g.class.as_str()).collect();
        assert_eq!(names, vec!["AuthMiddleware", "CartOwnerMiddleware"]);
        // Only `app.get` is collected; bare `router.use` without a path shouldn't be mistaken as a route guard
        let calls2 = vec![call(f, "router", "use", vec![class("X")], 2, 2, 90, 120)];
        assert!(collect_route_guards(&spec, &calls2).is_empty());
    }

    /// Decorator / annotation style: `@UseGuards(X)` and the decorated route `@Get` share owner_fqn, associate by owner.
    #[test]
    fn decorator_guards_group_by_owner() {
        let f = "user.controller.ts";
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["Get".into(), "Post".into()],
            guard_decorators: vec!["UseGuards".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("Get".into(), "GET".into()), ("Post".into(), "POST".into())]
                    .into_iter()
                    .collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: false,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, arg: FactValue, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args: vec![arg],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: f.into(),
            sub: None,
            language: Language::new(Language::PHP), // Placeholder only; does not affect decorator matching
        };
        let calls = vec![
            mk("Get", "UserController.profile", FactValue::String("/profile".into()), 10),
            mk("UseGuards", "UserController.profile", class("JwtAuthGuard"), 9),
            mk("Post", "UserController.update", FactValue::String("/update".into()), 20),
            mk("UseGuards", "UserController.update", class("AdminGuard"), 19),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 2, "每个被修饰的路由方法各出一段");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(by_line.remove(&10).unwrap(), vec!["JwtAuthGuard".to_string()]);
        assert_eq!(by_line.remove(&20).unwrap(), vec!["AdminGuard".to_string()]);
    }

    /// A minimal declaration builder for "decorator route + consumer-style middleware" (avoid repetitive boilerplate).
    fn rc_callee(name: &str, verb: &str) -> RouteCallSpec {
        RouteCallSpec {
            receiver: name.into(),
            by: RouteMatchBy::Callee,
            verb_methods: [(name.to_ascii_lowercase(), verb.to_string())]
                .into_iter()
                .collect(),
            path_arg: 0,
            handler_arg: None,
            group_method: None,
            receiver_ends_with: false,
            accept_identifier: true,
        }
    }

    /// NestJS `MiddlewareConsumer`: `consumer.apply(AuthMiddleware).forRoutes({path, method})`'s **object-literal** arg
    /// must be parsed into (path, verb) and landed on the corresponding route by "path + verb".
    ///
    /// Reproduces the realworld project's `user.module.ts` style — without it `forRoutes` would only recognize
    /// string / `Unknown` names, object literals would be treated as nameless args, and the whole module would
    /// get 0 guard hits.
    #[test]
    fn nestjs_consumer_object_literal_routes() {
        let consumer_spec = ConsumerGuardSpec {
            receiver: "consumer".into(),
            apply_method: "apply".into(),
            for_routes_method: "forRoutes".into(),
            wildcards: vec!["*".into()],
            scope: ConsumerScope::Directory,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![
                rc_callee("@Get", "GET"),
                rc_callee("@Put", "PUT"),
                rc_callee("@Post", "POST"),
                rc_callee("@Delete", "DELETE"),
            ],
            guard_attach: GuardAttach::One(GuardAttachSpec::Consumer(consumer_spec)),
            alias_table: None,
            synthesize_unresolved: true,
        };
        let ctrl = "src/user/user.controller.ts";
        let module = "src/user/user.module.ts";
        let mk_route = |callee: &str, path: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserController".into(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: None,
            method: Some(callee.trim_start_matches('@').to_string()),
            args: vec![FactValue::String(path.into())],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: line,
                end_line: line,
                start_byte: line * 10,
                end_byte: line * 10 + 5,
            },
            file: ctrl.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        // The whole chain `consumer.apply(AuthMiddleware).forRoutes(...)` shares start_byte=100.
        let apply = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply".into(),
            receiver: Some("consumer".into()),
            method: Some("apply".into()),
            args: vec![FactValue::Unknown(Some("AuthMiddleware".into()))],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 17,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let for_routes = CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: "UserModule".into(),
            owner_class: None,
            callee: "consumer.apply.forRoutes".into(),
            receiver: Some("consumer.apply(AuthMiddleware)".into()),
            method: Some("forRoutes".into()),
            args: vec![
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.GET".into()))),
                ]),
                FactValue::Array(vec![
                    ("path".into(), FactValue::String("user".into())),
                    ("method".into(), FactValue::Unknown(Some("RequestMethod.PUT".into()))),
                ]),
            ],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 18,
                end_line: 21,
                start_byte: 100,
                end_byte: 800,
            },
            file: module.into(),
            sub: None,
            language: Language::new(Language::JAVASCRIPT),
        };
        let calls = vec![
            mk_route("@Get", "user", 21),
            mk_route("@Put", "user", 26),
            mk_route("@Post", "users", 32),
            mk_route("@Delete", "users/:slug", 37),
            apply,
            for_routes,
        ];
        let scopes = collect_route_guards(&spec, &calls);
        // Should hit only 'user' GET and 'user' PUT (the verb dimension filters out POST / DELETE).
        assert_eq!(scopes.len(), 2, "只应命中显式路径 + 动词匹配到的两条路由");
        let mut by_line: std::collections::HashMap<u32, Vec<String>> = scopes
            .iter()
            .map(|s| (s.start_line, s.guards.iter().map(|g| g.class.clone()).collect()))
            .collect();
        assert_eq!(
            by_line.remove(&21).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "GET 'user'"
        );
        assert_eq!(
            by_line.remove(&26).unwrap(),
            vec!["AuthMiddleware".to_string()],
            "PUT 'user'"
        );
    }

    /// Django: `path('profile', views.profile)`'s route and view function are **in two files**,
    /// associating by owner would necessarily miss — rely on `link_via_handler_arg` to connect cross-file by handler arg.
    #[test]
    fn django_handler_link_guards() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec![],
            guard_decorators: vec!["login_required".into()],
            guard_name_patterns: vec![],
            guard_exclude_patterns: vec![],
            require_at_prefix: false,
            require_no_receiver: true,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: Some(1),
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "path".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("path".into(), "ANY".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: Some(1),
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, args: Vec<FactValue>, file: &str, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: None,
            method: None,
            args,
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: file.into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            // urls.py: route, handler is identifier `views.profile`
            mk("path", "urls", vec![FactValue::String("profile".into()), FactValue::Unknown(Some("views.profile".into()))], "urls.py", 5),
            // views.py: guard decorator hangs on the view function (owner is myapp.views.profile)
            mk("login_required", "myapp.views.profile", vec![], "views.py", 12),
            // Another view's guard shouldn't bleed onto profile
            mk("login_required", "myapp.views.other", vec![], "views.py", 20),
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["login_required"]
        );
    }

    /// A broad include pattern must not treat **doc decorators** and **plain member calls** as guards (deny + shape constraint).
    #[test]
    fn decorator_guards_reject_docs_and_member_calls() {
        let d = gt_domain::model::DecoratorGuardSpec {
            route_decorators: vec!["@Get".into()],
            guard_decorators: vec!["@UseGuards".into()],
            // broad pattern (would hit ApiBearerAuth and generateJWT)
            guard_name_patterns: vec!["(auth|jwt)".into()],
            guard_exclude_patterns: vec!["^@?api".into(), "^_".into()],
            require_at_prefix: true,
            require_no_receiver: false,
            name_from_args: true,
            include_class_level: true,
            link_via_handler_arg: None,
        };
        let spec = RouteGuardSpec {
            route_calls: vec![RouteCallSpec {
                receiver: "@Get".into(),
                by: RouteMatchBy::Callee,
                verb_methods: [("@get".into(), "GET".into())].into_iter().collect(),
                path_arg: 0,
                handler_arg: None,
                group_method: None,
                receiver_ends_with: false,
                accept_identifier: true,
            }],
            guard_attach: GuardAttach::One(GuardAttachSpec::Decorator(d)),
            alias_table: None,
            synthesize_unresolved: false,
        };
        let mk = |callee: &str, owner: &str, recv: Option<&str>, line: u32| CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: owner.into(),
            owner_class: None,
            callee: callee.into(),
            receiver: recv.map(|r| r.into()),
            method: None,
            args: vec![],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span { start_line: line, end_line: line, start_byte: line * 10, end_byte: line * 10 + 5 },
            file: "c.ts".into(),
            sub: None,
            language: Language::new(Language::PHP),
        };
        let calls = vec![
            mk("@Get", "C.f", None, 10),
            mk("@UseGuards", "C.f", None, 9),   // Real guard (with @)
            mk("@ApiBearerAuth", "C.f", None, 8), // Doc decorator -> exclude
            mk("generateJWT", "C.f", Some("this.userService"), 11), // Member call -> exclude
        ];
        let scopes = collect_route_guards(&spec, &calls);
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].guards.iter().map(|g| g.class.as_str()).collect::<Vec<_>>(),
            vec!["@UseGuards"],
            "只剩真守卫：文档装饰器与成员调用都不该算"
        );
    }

    /// Inner overrides outer: group `AuthToken(true)`, route `AuthToken(false)` → the latter should win.
    #[test]
    fn inner_guard_overrides_outer() {
        let f = "app/api/route/pc.php";
        let mut ws = GraphWorkspace::new(ProjectId::new(1));
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 1,
            end_line: 30,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("true".into()),
            }],
        }]);
        ws.add_route_guard_scopes(vec![crate::workspace::RouteGuardScope {
            file: f.into(),
            start_line: 5,
            end_line: 6,
            guards: vec![crate::workspace::RouteGuard {
                class: r"app\Auth".into(),
                arg: Some("false".into()),
            }],
        }]);
        let got = ws.route_guards(f, 5);
        assert_eq!(got.len(), 1, "同名中间件只算一个");
        assert_eq!(got[0].arg.as_deref(), Some("false"), "内层实参覆盖外层");
    }

    // ------------------------------------------------------------ code-evidence detectors

    /// A knowledge base of exactly the given entries, so detection needs no YAML directory.
    struct StaticKb(Vec<FrameworkKnowledge>);

    impl gt_domain::port::KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    /// A filesystem where nothing exists: the detectors under test never touch the disk.
    struct NoFs;

    impl gt_domain::port::FileSystem for NoFs {
        fn exists(&self, _: &std::path::Path) -> bool {
            false
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &std::path::Path) -> gt_domain::error::Result<String> {
            Err(gt_domain::error::DomainError::infra("no fs"))
        }
        fn len(&self, _: &std::path::Path) -> gt_domain::error::Result<u64> {
            Ok(0)
        }
    }

    fn fk(id: &str, detectors: Vec<Detector>) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.into(),
            display_name: id.into(),
            language: Language("php".into()),
            detectors,
            ..Default::default()
        }
    }

    fn evidence(imports: &[&str], callees: &[(&str, Option<&str>, Option<&str>)]) -> CodeEvidence {
        CodeEvidence {
            imports: imports.iter().map(|s| s.to_ascii_lowercase()).collect(),
            callees: callees
                .iter()
                .map(|(c, r, m)| {
                    (c.to_string(), (r.map(str::to_string), m.map(str::to_string)))
                })
                .collect(),
        }
    }

    #[test]
    fn import_exists_matches_fqn_exactly_and_by_prefix() {
        let ev = evidence(&["guzzlehttp\\client", "app\\services\\order"], &[]);
        assert!(ev.imports("GuzzleHttp\\Client"), "大小写不敏感");
        assert!(ev.imports("\\GuzzleHttp\\Client"), "前导分隔符忽略");
        assert!(ev.imports("GuzzleHttp\\*"), "命名空间前缀");
        assert!(!ev.imports("GuzzleHttp\\HandlerStack"), "前缀内未导入的符号不算");
        assert!(!ev.imports("GuzzleHttp"), "无通配时是完整 FQN 比较，不是前缀");
    }

    #[test]
    fn call_exists_uses_the_rule_callee_grammar() {
        let ev = evidence(
            &[],
            &[
                ("GuzzleHttp\\Client::request", Some("GuzzleHttp\\Client"), Some("request")),
                ("Client::get", Some("Client"), Some("get")),
            ],
        );
        // Fully-qualified inline call: what `import_exists` cannot see.
        assert!(ev.calls("GuzzleHttp\\Client::*"));
        assert!(ev.calls("GuzzleHttp\\Client::request|GuzzleHttp\\Client::get"));
        assert!(!ev.calls("GuzzleHttp\\Client::pool"));
        // Bare method names and `*` tail matching behave as they do in a rule selector.
        assert!(ev.calls("*:get"));
        assert!(!ev.calls("*:delete"));
    }

    #[test]
    fn detectors_activate_knowledge_without_any_manifest() {
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: std::path::PathBuf::from("/p"),
            language: Language("php".into()),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        };
        let root = std::path::Path::new("/p");
        // No composer.json anywhere (NoFs says nothing exists), so only code evidence can fire.
        let ev = evidence(&["guzzlehttp\\client"], &[]);

        let kb = StaticKb(vec![
            fk(
                "guzzle",
                vec![Detector::ImportExists { symbol: "GuzzleHttp\\Client".into(), confidence: 0.95 }],
            ),
            fk(
                "unrelated",
                vec![Detector::CallExists { callee: "Redis::get".into(), confidence: 0.9 }],
            ),
        ]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &techstack());
        assert_eq!(hits, vec!["guzzle".to_string()], "只激活代码里真用到的库");

        // And the alias spelling — `use GuzzleHttp\Client as G;` — records the same FQN, so it too fires.
        let kb2 = StaticKb(vec![fk(
            "guzzle",
            vec![Detector::CallExists { callee: "GuzzleHttp\\Client::*".into(), confidence: 0.9 }],
        )]);
        let ev2 = evidence(&[], &[("GuzzleHttp\\Client::request", None, None)]);
        assert_eq!(
            detect_frameworks(&kb2, &NoFs, &sub, root, KnowledgeScope::Framework, &ev2, &techstack()),
            vec!["guzzle".to_string()]
        );
    }

    /// An in-memory filesystem, so lock-file parsing needs no temp files.
    struct MemFs(HashMap<std::path::PathBuf, String>);

    impl gt_domain::port::FileSystem for MemFs {
        fn exists(&self, p: &std::path::Path) -> bool {
            self.0.contains_key(p)
        }
        fn is_dir(&self, _: &std::path::Path) -> bool {
            false
        }
        fn read_to_string(&self, p: &std::path::Path) -> gt_domain::error::Result<String> {
            self.0.get(p).cloned().ok_or_else(|| gt_domain::error::DomainError::infra("missing"))
        }
        fn len(&self, p: &std::path::Path) -> gt_domain::error::Result<u64> {
            Ok(self.0.get(p).map(|s| s.len() as u64).unwrap_or(0))
        }
    }

    #[test]
    fn lock_dependency_sees_the_resolved_closure() {
        let lock = std::path::PathBuf::from("/p/composer.lock");
        let fs = MemFs(HashMap::from([(
            lock.clone(),
            r#"{"packages": [
                 {"name": "laravel/framework"},
                 {"name": "illuminate/database"},
                 {"name": "guzzlehttp/guzzle"}
               ], "packages-dev": [{"name": "phpunit/phpunit"}]}"#
                .into(),
        )]));
        // None of these are in the app's own composer.json — that is the whole point.
        let ts = techstack();
        let php = Language::new(Language::PHP);
        assert!(lock_has(&lock, "illuminate/database", &fs, &ts, &php));
        assert!(lock_has(&lock, "guzzlehttp/guzzle", &fs, &ts, &php));
        assert!(lock_has(&lock, "phpunit/phpunit", &fs, &ts, &php), "packages-dev 也算");
        assert!(!lock_has(&lock, "spatie/laravel-permission", &fs, &ts, &php));
        assert!(!lock_has(
            &std::path::PathBuf::from("/p/other.lock"),
            "illuminate/database",
            &fs,
            &ts,
            &php
        ));
    }

    #[test]
    fn lock_dependency_reads_npm_layouts_too() {
        // package-lock.json v2+: an object keyed by install path.
        let v2 = std::path::PathBuf::from("/p/package-lock.json");
        let fs = MemFs(HashMap::from([(
            v2.clone(),
            r#"{"packages": {"node_modules/express": {"version": "4.18.0"}, "": {"name": "app"}}}"#.into(),
        )]));
        let ts = techstack();
        let js = Language::new(Language::JAVASCRIPT);
        assert!(lock_has(&v2, "express", &fs, &ts, &js), "key 去掉 node_modules/ 前缀后应命中");
        // A TypeScript sub-project must not lose the JS adapter (`serves`), or npm layouts would
        // silently degrade to the whole-file text probe.
        let ts_lang = Language::new(Language::TYPESCRIPT);
        assert!(lock_has(&v2, "express", &fs, &ts, &ts_lang), "typescript 也由 JS adapter 承担");

        // v1: an object keyed by package name under `dependencies`.
        let v1 = std::path::PathBuf::from("/q/package-lock.json");
        let fs1 = MemFs(HashMap::from([(
            v1.clone(),
            r#"{"dependencies": {"koa": {"version": "2.14.0"}}}"#.into(),
        )]));
        assert!(lock_has(&v1, "koa", &fs1, &ts, &js));
    }

    /// The dependency sections are read by the **ecosystem's** adapter; an ecosystem with none
    /// (Java's `pom.xml`, Python's `requirements.txt` …) must still match, via the whole-file text
    /// probe — degrading to silence would silently drop every framework of that stack.
    #[test]
    fn manifest_dependency_uses_the_adapter_then_falls_back_to_text() {
        let ts = techstack();
        let composer = std::path::PathBuf::from("/p/composer.json");
        let pom = std::path::PathBuf::from("/j/pom.xml");
        let fs = MemFs(HashMap::from([
            (
                composer.clone(),
                r#"{"require": {"topthink/framework": "^6"}, "replace": {"nothing/x": "*"}}"#.into(),
            ),
            (pom.clone(), "<dependency><artifactId>spring-boot</artifactId></dependency>".into()),
        ]));

        let php = Language::new(Language::PHP);
        assert!(manifest_has(&composer, "topthink/framework", &fs, &ts, &php));
        assert!(!manifest_has(&composer, "laravel/framework", &fs, &ts, &php));

        let java = Language::new(Language::JAVA);
        assert!(
            manifest_has(&pom, "spring-boot", &fs, &ts, &java),
            "没有 adapter 的生态必须退回文本探测，而不是静默失效"
        );
    }

    /// `db_schema` keeps **no** built-in receiver / method list: which receivers name a table is stack
    /// knowledge, declared by FKB `params`. It used to default to PHP's `Db` / `Query` / `Model`, which
    /// silently applied PHP conventions to every stack that merely forgot to declare them.
    #[test]
    fn db_schema_collects_nothing_without_declared_receivers() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        // Exactly the shape the old PHP default would have matched.
        ctx.ws
            .calls
            .push(call("app/a.php", "Db", "name", vec![FactValue::String("eb_user".into())], 1, 1, 10, 30));

        load_schema(&mut ctx, &json!({}), &sub, std::path::Path::new("/t"), &Phase(Phase::PREPARE.to_string()));
        assert!(
            ctx.ws.get_symbol("schema", "eb_user").is_none(),
            "未声明 table_receivers / table_methods 时不应采集任何表名"
        );

        // Declared by FKB (as thinkphp.yaml / illuminate-database.yaml do) → collected.
        ctx.ws.calls.clear();
        ctx.ws.calls.push(call("app/a.php", "Db", "name", vec![FactValue::String("eb_user".into())], 1, 1, 10, 30));
        load_schema(
            &mut ctx,
            &json!({ "table_receivers": ["Db"], "table_methods": ["name"] }),
            &sub,
            std::path::Path::new("/t"),
            &Phase(Phase::PREPARE.to_string()),
        );
        assert!(
            ctx.ws.get_symbol("schema", "eb_user").is_some(),
            "FKB 声明后应采集到表名"
        );
    }

    /// A built-in loader whose name is **not** a kernel-neutral id (a stack-specific one, e.g. a Java adapter's
    /// own loader) must be delegated to the tech-stack adapter matching the sub-project's language — `run_builtin`
    /// must not gate on a hard-coded PHP name. This locks the `other => run_adapter_loader` routing that replaced
    /// the explicit `php_migration_schema` arm (now the language-agnostic `migration_schema` id, whose
    /// format-specific parsing lives in each stack's own adapter).
    #[test]
    fn non_php_builtin_loader_routes_to_adapter() {
        use gt_domain::model::{
            FrameworkKnowledge, Language, Phase, Project, ProjectId, ProjectStatus, SubProject,
            SubProjectId,
        };
        use gt_domain::port::{AdapterFact, TechStackAdapter, TechStackRegistry};

        struct MockAdapter;
        impl TechStackAdapter for MockAdapter {
            fn language(&self) -> Language {
                Language::new(Language::JAVA)
            }
            fn load(
                &self,
                loader_id: &str,
                _params: &serde_json::Value,
                _sub: &gt_domain::model::SubProject,
                _project_root: &std::path::Path,
                _fs: &dyn gt_domain::port::FileSystem,
                _parsers: &dyn gt_domain::port::ParserRegistry,
                _table_prefixes: &[String],
            ) -> gt_domain::error::Result<Vec<AdapterFact>> {
                if loader_id == "mock_schema" {
                    Ok(vec![AdapterFact::Schema {
                        table: "mock_table".into(),
                        columns: vec!["id".into()],
                        source: "mock.php".into(),
                    }])
                } else {
                    Ok(Vec::new())
                }
            }
        }

        struct MockRegistry;
        impl TechStackRegistry for MockRegistry {
            fn adapter_for(&self, language: &Language) -> Option<&dyn TechStackAdapter> {
                if language.0.as_str() == Language::JAVA {
                    static A: MockAdapter = MockAdapter;
                    Some(&A)
                } else {
                    None
                }
            }
        }

        struct EmptyParsers;
        impl gt_domain::port::ParserRegistry for EmptyParsers {
            fn parser_for(&self, _: &Language) -> Option<&dyn gt_domain::port::LanguageParser> {
                None
            }
            fn language_for_extension(&self, _: &str) -> Option<Language> {
                None
            }
            fn supported_languages(&self) -> Vec<Language> {
                Vec::new()
            }
        }

        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::JAVA),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec![],
            facts: serde_json::Value::Object(Default::default()),
        };
        let fk = FrameworkKnowledge {
            id: "x".into(),
            display_name: "x".into(),
            language: Language::new(Language::JAVA),
            ..Default::default()
        };
        let registry = MockRegistry;
        let parsers = EmptyParsers;
        let fs = NoFs;

        super::run_builtin(
            &mut ctx,
            "mock_schema",
            &json!({}),
            &sub,
            std::path::Path::new("/t"),
            &fs,
            &parsers,
            &Phase(Phase::PREPARE.to_string()),
            &[],
            &fk,
            &registry,
        );

        assert!(
            ctx.ws.get_symbol("schema", "mock_table").is_some(),
            "non-PHP stack-specific builtin loader must reach the adapter and land its facts"
        );
    }

    /// End-to-end pin for the language-agnostic `migration_schema` id: FKB declares it (see
    /// `fkb/php/illuminate-database.yaml`), the kernel delegates it to the tech-stack adapter matched by
    /// `sub.language`, and the PHP adapter's migration parser turns `database/migrations/*.php` into `schema`
    /// symbol-table rows. Guards the rename away from the PHP-only `php_migration_schema` spelling.
    #[test]
    fn migration_schema_loader_writes_schema_table() {
        use gt_domain::model::{
            FrameworkKnowledge, Language, Phase, Project, ProjectId, ProjectStatus, SubProject,
            SubProjectId,
        };

        struct NoParsers;
        impl gt_domain::port::ParserRegistry for NoParsers {
            fn parser_for(&self, _: &Language) -> Option<&dyn gt_domain::port::LanguageParser> {
                None
            }
            fn language_for_extension(&self, _: &str) -> Option<Language> {
                None
            }
            fn supported_languages(&self) -> Vec<Language> {
                Vec::new()
            }
        }

        let dir = std::env::temp_dir().join(format!("gt_mig_schema_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("database/migrations")).unwrap();
        std::fs::write(
            dir.join("database/migrations/2019_01_01_create_users.php"),
            "<?php\nSchema::create('users', function (Blueprint $table) {\n    $table->id();\n    $table->string('email');\n});",
        )
        .unwrap();

        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: dir.clone(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["laravel".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        let fk = FrameworkKnowledge {
            id: "x".into(),
            display_name: "x".into(),
            language: Language::new(Language::PHP),
            ..Default::default()
        };
        let registry = techstack();
        let parsers = NoParsers;
        let fs = NoFs;

        super::run_builtin(
            &mut ctx,
            "migration_schema",
            &json!({ "paths": ["database/migrations"], "extensions": ["php"] }),
            &sub,
            &dir,
            &fs,
            &parsers,
            &Phase(Phase::PREPARE.to_string()),
            &[],
            &fk,
            &registry,
        );

        assert!(
            ctx.ws.get_symbol("schema", "users").is_some(),
            "FKB 声明的 `migration_schema` 必须经 PHP adapter 解析迁移并落入 schema 符号表"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `exact_table_receivers` matches the receiver's **last namespace segment**, so `Db` / `think\facade\Db`
    /// count while `DbHelper` does not — the substring form (`table_receivers`) would swallow the latter.
    #[test]
    fn db_schema_exact_receivers_match_the_last_segment_only() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: std::path::PathBuf::from("/t"),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        let params = json!({ "exact_table_receivers": ["Db"], "table_methods": ["name"] });
        let run = |ctx: &mut PipelineContext, receiver: &str, table: &str| {
            ctx.ws.calls.clear();
            ctx.ws
                .calls
                .push(call("app/a.php", receiver, "name", vec![FactValue::String(table.into())], 1, 1, 10, 30));
            load_schema(ctx, &params, &sub, std::path::Path::new("/t"), &Phase(Phase::PREPARE.to_string()));
            ctx.ws.get_symbol("schema", table).is_some()
        };

        assert!(run(&mut ctx, "Db", "t_users"), "裸 `Db` 应命中");
        assert!(run(&mut ctx, "think\\facade\\Db", "t_ns"), "命名空间下的 `Db` 应命中（末段相同）");
        assert!(run(&mut ctx, "DB", "t_upper"), "大小写不敏感");
        assert!(!run(&mut ctx, "DbHelper", "t_junk"), "`DbHelper` 不该被当成查询构造器");
    }

    /// The array syntax (`[ … ]` / `=>` / `//` / `::class`) used to be hard-coded, so only PHP-shaped
    /// declarations could ever be read. It is declared by FKB now: the same loader must read a
    /// completely different syntax when FKB says so.
    #[test]
    fn declared_middleware_reads_whatever_array_syntax_fkb_declares() {
        let dir = std::env::temp_dir().join(format!("gt_mw_syntax_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("app")).unwrap();

        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "backend".into(),
            root_path: dir.clone(),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: vec!["thinkphp".into()],
            facts: serde_json::Value::Object(Default::default()),
        };
        let ctx_of = || PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: dir.clone(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });

        // ① PHP array literal.
        std::fs::write(
            dir.join("app/middleware.php"),
            "<?php\nreturn [\n    App\\Foo::class,\n    App\\Bar::class, // trailing comment\n];",
        )
        .unwrap();
        let mut ctx = ctx_of();
        load_declared_middleware(
            &mut ctx,
            &sub,
            &dir,
            &json!({
                "paths": ["app/middleware.php"],
                "open": "[", "close": "]", "separator": "=>", "comment": "//",
                "class_suffix": "::class", "extensions": ["php"],
            }),
        );
        assert_eq!(
            ctx.ws
                .get_symbol("declared_middleware", "global_0")
                .and_then(|v| v.get("classes"))
                .and_then(|c| c.get(0))
                .and_then(|c| c.as_str()),
            Some("App\\Foo"),
            "PHP 数组字面量应照声明解析"
        );

        // ② A different stack's syntax: `{}` blocks, `:` pairs, `#` comments, quoted values.
        std::fs::write(
            dir.join("app/middleware.conf"),
            "{\n  \"a\": \"Acme\\Foo\",\n  \"b\": \"Acme\\Bar\" # trailing\n}",
        )
        .unwrap();
        let mut ctx2 = ctx_of();
        load_declared_middleware(
            &mut ctx2,
            &sub,
            &dir,
            &json!({
                "paths": ["app/middleware.conf"],
                "open": "{", "close": "}", "separator": ":", "comment": "#",
                "extensions": ["conf"],
            }),
        );
        let classes: Vec<String> = ctx2
            .ws
            .get_symbol("declared_middleware", "global_0")
            .and_then(|v| v.get("classes").cloned())
            .and_then(|c| serde_json::from_value::<Vec<String>>(c).ok())
            .unwrap_or_default();
        assert_eq!(
            classes,
            vec!["Acme\\Foo".to_string(), "Acme\\Bar".to_string()],
            "非 PHP 语法也应照声明解析"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provides_switches_on_component_knowledge_and_ranks_below_direct_hits() {
        // A framework bundles components the app never declared; recognising the bundle recognises them.
        let mut laravel = fk("laravel", vec![]);
        laravel.provides = vec!["illuminate-database".into()];
        let mut db = fk("illuminate-database", vec![]);
        db.provides = vec!["illuminate-support".into()]; // transitive: a component may bundle further ones
        let kb = StaticKb(vec![laravel, db, fk("illuminate-support", vec![])]);

        let hits = expand_provided(vec![("laravel".to_string(), 0.95)], &kb);
        let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["laravel", "illuminate-database", "illuminate-support"]);

        // Directly detected knowledge keeps its place ahead of provided knowledge, whatever the confidence.
        let hits2 = expand_provided(
            vec![("laravel".to_string(), 0.95), ("guzzle".to_string(), 0.5)],
            &kb,
        );
        let ids2: Vec<&str> = hits2.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids2,
            vec!["laravel", "guzzle", "illuminate-database", "illuminate-support"],
            "派生知识排在直接命中之后"
        );
        assert!(
            hits2.iter().find(|(id, _)| id == "illuminate-database").unwrap().1 < 0.95,
            "被 provides 引入的置信度低于其提供者"
        );
    }

    #[test]
    fn provides_is_cycle_safe() {
        let mut a = fk("a", vec![]);
        a.provides = vec!["b".into()];
        let mut b = fk("b", vec![]);
        b.provides = vec!["a".into(), "b".into()]; // mutual, and self-referential
        let kb = StaticKb(vec![a, b]);
        let hits = expand_provided(vec![("a".to_string(), 0.9)], &kb);
        assert_eq!(hits.len(), 2, "环不应导致重复或无限展开");
    }

    #[test]
    fn detector_confidence_is_declared_not_defaulted() {
        use gt_domain::model::Detector;
        assert_eq!(
            Detector::ImportExists { symbol: "x".into(), confidence: 0.5 }.confidence(),
            0.5
        );
        assert_eq!(
            Detector::CallExists { callee: "x".into(), confidence: 0.7 }.confidence(),
            0.7
        );
    }

}
