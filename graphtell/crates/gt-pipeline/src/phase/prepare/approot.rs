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
pub(crate) fn apply_root_rules(
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
                    format!("framework {}: {} could not be resolved", fk.id, rule.key),
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
                            format!("framework {}: {} could not be resolved", fk.id, rule.key),
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

