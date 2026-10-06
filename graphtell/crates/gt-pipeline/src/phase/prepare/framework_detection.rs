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
pub(crate) fn detect_frameworks(
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
pub(crate) struct CodeEvidence {
    /// Imported FQNs, lowercased and stripped of a leading separator.
    pub(crate) imports: HashSet<String>,
    /// Distinct callees, each with the receiver / method P2 saw it with (needed because callee matching
    /// is three-way: pattern vs `callee` + `receiver` + `method`).
    pub(crate) callees: HashMap<String, (Option<String>, Option<String>)>,
}

impl CodeEvidence {
    /// Whether `symbol` is imported by this sub-project. A trailing `*` matches a namespace prefix.
    pub(crate) fn imports(&self, symbol: &str) -> bool {
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
    pub(crate) fn calls(&self, pattern: &str) -> bool {
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
pub(crate) fn split_callee(callee: &str) -> (Option<String>, Option<String>) {
    match callee.rfind("::").or_else(|| callee.rfind("->")) {
        Some(pos) => (
            Some(callee[..pos].to_string()),
            Some(callee[pos + 2..].to_string()),
        ),
        None => (None, None),
    }
}

/// Collect one sub-project's code evidence from the P2 output.
pub(crate) fn collect_code_evidence(ctx: &PipelineContext, sub: &gt_domain::model::SubProject) -> CodeEvidence {
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

