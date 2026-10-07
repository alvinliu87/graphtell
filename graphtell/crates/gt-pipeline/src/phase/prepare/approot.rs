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

#[cfg(test)]
mod tests {
    use super::apply_root_rules;
    use crate::context::PipelineContext;
    use gt_adapter_fs::StdFileSystem;
    use gt_adapter_parser::DefaultParserRegistry;
    use gt_adapter_techstack::{JsTechStackAdapter, PhpTechStackAdapter};
    use gt_domain::model::{
        FrameworkKnowledge, Language, Phase, PickStrategy, Project, ProjectId, ProjectStatus, RootRule,
        RootSource, SubProject, SubProjectId,
    };
    use gt_domain::port::DefaultTechStackRegistry;
    use serde_json::json;
    use std::path::Path;

    /// Build a throwaway directory that (a) is the project root and (b) holds a single PHP sub-project
    /// at the same root. Nothing is written into it, so every root rule resolves to `None` — exactly the
    /// negative path these tests pin.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "graphtell-approot-neg-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir scratch dir");
        dir
    }

    fn php_ctx(root: &Path) -> PipelineContext {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: root.to_path_buf(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        ctx.sub_projects = vec![SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "app".into(),
            root_path: root.to_path_buf(),
            language: Language::new("php"),
            role: "backend".into(),
            detected_by: "composer.json".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }];
        ctx
    }

    /// A real registry (PHP + JS adapters registered) so the *unresolved* path is triggered by a missing
    /// manifest file — not by the absence of an adapter. This is the realistic production shape.
    fn techstack() -> DefaultTechStackRegistry {
        DefaultTechStackRegistry::new()
            .register(Box::new(PhpTechStackAdapter::new()))
            .register(Box::new(JsTechStackAdapter::new()))
    }

    fn fk_with(rules: Vec<RootRule>) -> FrameworkKnowledge {
        let mut fk = FrameworkKnowledge::default();
        fk.id = "approot-neg".into();
        fk.root_rules = rules;
        fk
    }

    /// `RootSource::ManifestEntries` that points at a missing manifest must hit the `None` arm of the
    /// first `if let` block: emit a `RootRuleUnresolved` diagnostic and write **no** fact.
    #[test]
    fn manifest_entries_unresolved_emits_diagnostic_and_no_fact() {
        let root = scratch_dir("me");
        let mut ctx = php_ctx(&root);
        let fk = fk_with(vec![RootRule {
            id: "db-conns".into(),
            key: "db_connections".into(),
            source: RootSource::ManifestEntries {
                manifest: "config/database.php".into(),
                root: "connections".into(),
                fields: vec![],
                default_from: None,
            },
            confidence: 0.9,
            fallbacks: vec![],
        }]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            facts.is_empty(),
            "an unresolved ManifestEntries rule must not write any fact"
        );
        assert!(
            ctx.ws
                .diagnostics
                .iter()
                .any(|d| d.code == "RootRuleUnresolved"),
            "an unresolved ManifestEntries rule must emit a RootRuleUnresolved diagnostic, got: {:?}",
            ctx.ws.diagnostics
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `RootSource::DirectoryExists` whose dir is absent and with no fallback: the `None` arm with no
    /// fallback match must emit a `RootRuleUnresolved` diagnostic and write **no** fact.
    #[test]
    fn directory_exists_unresolved_without_fallback_emits_diagnostic() {
        let root = scratch_dir("de");
        let mut ctx = php_ctx(&root);
        let fk = fk_with(vec![RootRule {
            id: "app-root".into(),
            key: "app_root".into(),
            source: RootSource::DirectoryExists {
                path: "no_such_dir".into(),
            },
            confidence: 0.9,
            fallbacks: vec![],
        }]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            facts.is_empty(),
            "an unresolved rule with no fallback must not write any fact"
        );
        assert!(
            ctx.ws
                .diagnostics
                .iter()
                .any(|d| d.code == "RootRuleUnresolved"),
            "an unresolved rule with no fallback must emit a RootRuleUnresolved diagnostic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `RootSource::DirectoryExists` whose dir is absent but a fallback candidate dir exists: the probe
    /// fails, the fallback is taken, the fact is written with `fallback_used == true`, the source names the
    /// fallback, and confidence is scaled by 0.7. No diagnostic is emitted.
    #[test]
    fn directory_exists_unresolved_with_fallback_uses_fallback_value() {
        let root = scratch_dir("def");
        // The fallback candidate directory exists; the probed directory does not.
        std::fs::create_dir_all(root.join("fallback_dir")).expect("mkdir fallback dir");
        let mut ctx = php_ctx(&root);
        let fk = fk_with(vec![RootRule {
            id: "app-root".into(),
            key: "app_root".into(),
            source: RootSource::DirectoryExists {
                path: "no_such_dir".into(),
            },
            confidence: 0.9,
            fallbacks: vec!["fallback_dir".into()],
        }]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            ctx.ws.diagnostics.is_empty(),
            "a fallback hit must not emit a diagnostic"
        );
        let fact = facts
            .get("app_root")
            .expect("the fallback rule must still write its fact");
        assert_eq!(
            fact.get("value").and_then(|v| v.as_str()),
            Some("fallback_dir"),
            "the value must be the fallback relative path"
        );
        assert_eq!(
            fact.get("fallback_used").and_then(|v| v.as_bool()),
            Some(true),
            "fallback_used must be true when the fallback is taken"
        );
        assert!(
            fact.get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("fallback: fallback_dir"),
            "the source must record which fallback took effect"
        );
        assert!(
            (fact.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.0) - 0.63).abs() < 1e-6,
            "fallback confidence must be rule.confidence * 0.7"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `RootSource::Manifest` (single pointer) pointing at a missing manifest must hit the `None` arm and
    /// emit a `RootRuleUnresolved` diagnostic with no fact.
    #[test]
    fn manifest_pointer_unresolved_emits_diagnostic() {
        let root = scratch_dir("mp");
        let mut ctx = php_ctx(&root);
        let fk = fk_with(vec![RootRule {
            id: "db-prefix".into(),
            key: "db_prefix".into(),
            source: RootSource::Manifest {
                manifest: "config/database.php".into(),
                pointer: "connections.mysql.prefix".into(),
            },
            confidence: 0.9,
            fallbacks: vec![],
        }]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            facts.is_empty(),
            "an unresolved manifest pointer rule must not write any fact"
        );
        assert!(
            ctx.ws
                .diagnostics
                .iter()
                .any(|d| d.code == "RootRuleUnresolved"),
            "an unresolved manifest pointer rule must emit a RootRuleUnresolved diagnostic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `RootSource::ManifestJson` whose manifest is absent must hit the `None` arm (the file read fails) and
    /// emit a `RootRuleUnresolved` diagnostic with no fact. This pins the JSON arm of the resolution ladder,
    /// which the `root_rules_real_fkb.rs` positives always resolve directly.
    #[test]
    fn manifest_json_unresolved_emits_diagnostic() {
        let root = scratch_dir("mj");
        let mut ctx = php_ctx(&root); // no composer.json present
        let fk = fk_with(vec![RootRule {
            id: "app-root-json".into(),
            key: "app_root".into(),
            source: RootSource::ManifestJson {
                manifest: "composer.json".into(),
                pointer: "autoload.psr-4".into(),
                pick: PickStrategy::ShallowestDir,
            },
            confidence: 0.9,
            fallbacks: vec![],
        }]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        assert!(
            facts.is_empty(),
            "an unresolved manifest_json rule must not write any fact"
        );
        assert!(
            ctx.ws
                .diagnostics
                .iter()
                .any(|d| d.code == "RootRuleUnresolved"),
            "an unresolved manifest_json rule must emit a RootRuleUnresolved diagnostic"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// When several root rules are processed, an unresolved one must not poison a *later* resolvable one:
    /// only the failed rule gets a diagnostic, the others still write their facts.
    #[test]
    fn unresolved_rule_does_not_block_subsequent_rules() {
        let root = scratch_dir("mix");
        // Make the *second* rule resolve: a directory probe that hits directly under the root.
        std::fs::create_dir_all(root.join("app")).expect("mkdir app dir");
        let mut ctx = php_ctx(&root);
        let fk = fk_with(vec![
            RootRule {
                id: "bad".into(),
                key: "missing".into(),
                source: RootSource::DirectoryExists {
                    path: "no_such_dir".into(),
                },
                confidence: 0.9,
                fallbacks: vec![],
            },
            RootRule {
                id: "app-root".into(),
                key: "app_root".into(),
                source: RootSource::DirectoryExists { path: "app".into() },
                confidence: 0.9,
                fallbacks: vec![],
            },
        ]);
        let sub = ctx.sub_projects[0].clone();
        let mut facts = serde_json::Map::new();
        let fs = StdFileSystem::new();
        let parsers = DefaultParserRegistry::new();
        let ts = techstack();
        apply_root_rules(
            &mut ctx,
            &fk,
            &sub,
            &mut facts,
            &Phase(Phase::PREPARE.to_string()),
            &fs,
            &parsers,
            &ts,
        );

        // Exactly one diagnostic (for the unresolved `missing` rule), and the resolvable rule still wrote its fact.
        assert_eq!(
            ctx.ws.diagnostics.len(),
            1,
            "only the unresolved rule should be diagnosed"
        );
        assert_eq!(ctx.ws.diagnostics[0].code, "RootRuleUnresolved");
        assert!(
            facts.contains_key("app_root"),
            "the resolvable rule must still write its fact"
        );
        assert!(
            !facts.contains_key("missing"),
            "the unresolved rule must not write a fact"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

