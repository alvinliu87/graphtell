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

#[cfg(test)]
mod tests {
    use super::{
        collect_code_evidence, detect_frameworks, CodeEvidence,
    };
    use crate::context::PipelineContext;
    use crate::workspace::CallRecord;
    use gt_domain::model::{
        Detector, FrameworkKnowledge, KnowledgeScope, Language, NodeId, Project, ProjectId,
        ProjectStatus, Span, SubProject, SubProjectId,
    };
    use gt_domain::port::{FileSystem, KnowledgeProvider, TechStackAdapter, TechStackRegistry};
    use std::collections::HashMap;
    use std::path::Path;

    /// A knowledge base of exactly the given entries, so detection needs no YAML directory.
    struct StaticKb(Vec<FrameworkKnowledge>);
    impl KnowledgeProvider for StaticKb {
        fn all(&self) -> Vec<&FrameworkKnowledge> {
            self.0.iter().collect()
        }
        fn by_id(&self, id: &str) -> Option<&FrameworkKnowledge> {
            self.0.iter().find(|fk| fk.id == id)
        }
    }

    /// A filesystem where nothing exists: the detectors under test never touch the disk.
    struct NoFs;
    impl FileSystem for NoFs {
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &Path) -> gt_domain::error::Result<String> {
            Err(gt_domain::error::DomainError::infra("no fs"))
        }
        fn len(&self, _: &Path) -> gt_domain::error::Result<u64> {
            Ok(0)
        }
    }

    /// A registry with no adapter — fine for the negative paths below, where the manifest / lock file is
    /// absent and `manifest_has` / `lock_has` short-circuit on the missing read before reaching the adapter.
    struct EmptyTechStack;
    impl TechStackRegistry for EmptyTechStack {
        fn adapter_for(&self, _: &Language) -> Option<&dyn TechStackAdapter> {
            None
        }
    }

    fn fk(
        id: &str,
        scope: KnowledgeScope,
        language: Language,
        detectors: Vec<Detector>,
    ) -> FrameworkKnowledge {
        FrameworkKnowledge {
            id: id.into(),
            display_name: id.into(),
            language,
            scope,
            detectors,
            ..Default::default()
        }
    }

    fn sub(root: &Path, language: Language) -> SubProject {
        SubProject {
            id: SubProjectId(1),
            project_id: ProjectId(1),
            name: "app".into(),
            root_path: root.to_path_buf(),
            language,
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }
    }

    fn evidence(
        imports: &[&str],
        callees: &[(&str, Option<&str>, Option<&str>)],
    ) -> CodeEvidence {
        CodeEvidence {
            imports: imports.iter().map(|s| s.to_ascii_lowercase()).collect(),
            callees: callees
                .iter()
                .map(|(c, r, m)| (c.to_string(), (r.map(str::to_string), m.map(str::to_string))))
                .collect(),
        }
    }

    fn call(
        file: &str,
        sub_id: Option<SubProjectId>,
        callee: &str,
        receiver: Option<&str>,
        method: Option<&str>,
    ) -> CallRecord {
        CallRecord {
            node: NodeId::new(0),
            owner: NodeId::new(0),
            owner_fqn: String::new(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: receiver.map(str::to_string),
            method: method.map(str::to_string),
            args: Vec::new(),
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span {
                start_line: 0,
                end_line: 0,
                start_byte: 0,
                end_byte: 0,
            },
            file: file.to_string(),
            sub: sub_id,
            language: Language::new(Language::PHP),
        }
    }

    // ------------------------------------------------------- detect_frameworks gates

    /// The scope gate: a framework whose `scope` differs from the one being detected must be excluded even
    /// when its detector *would* match the evidence. This pins the `if fk.scope != scope` early `continue`.
    #[test]
    fn scope_mismatch_excludes_framework() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        // A `Project`-scoped framework whose detector would match — excluded because we asked for `Framework`.
        let kb = StaticKb(vec![fk(
            "proj-kb",
            KnowledgeScope::Project,
            Language::new(Language::PHP),
            vec![Detector::ImportExists {
                symbol: "GuzzleHttp\\Client".into(),
                confidence: 0.9,
            }],
        )]);
        let ev = evidence(&["guzzlehttp\\client"], &[]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &EmptyTechStack);
        assert!(
            hits.is_empty(),
            "a framework whose scope != requested scope must be excluded even though its detector matches"
        );

        // Positive control: a framework in the requested scope with matching evidence does fire.
        let kb2 = StaticKb(vec![fk(
            "php-fw",
            KnowledgeScope::Framework,
            Language::new(Language::PHP),
            vec![Detector::ImportExists {
                symbol: "GuzzleHttp\\Client".into(),
                confidence: 0.9,
            }],
        )]);
        let ev2 = evidence(&["guzzlehttp\\client"], &[]);
        let hits2 = detect_frameworks(
            &kb2,
            &NoFs,
            &sub,
            root,
            KnowledgeScope::Framework,
            &ev2,
            &EmptyTechStack,
        );
        assert_eq!(
            hits2,
            vec!["php-fw".to_string()],
            "a framework in the requested scope with matching evidence must fire"
        );
    }

    /// The language gate: a framework declared for another language must be excluded even when its detector
    /// matches the (unrelated) sub-project's evidence — pinned by `if fk.language != sub.language && ...`.
    #[test]
    fn language_mismatch_excludes_framework() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        let ev = evidence(&["guzzlehttp\\client"], &[]);
        // A JS framework whose detector matches the PHP code evidence.
        let kb = StaticKb(vec![fk(
            "js-fw",
            KnowledgeScope::Framework,
            Language::new(Language::JAVASCRIPT),
            vec![Detector::ImportExists {
                symbol: "GuzzleHttp\\Client".into(),
                confidence: 0.9,
            }],
        )]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &EmptyTechStack);
        assert!(
            hits.is_empty(),
            "a framework for another language must be excluded even when its detector matches"
        );
    }

    /// When the sub-project's language is `UNKNOWN`, the language gate is bypassed (`sub.language.as_str()
    /// != Language::UNKNOWN` short-circuits) so a framework of any language may still be recognised.
    #[test]
    fn unknown_language_sub_bypasses_language_gate() {
        let root = Path::new("/p");
        // An unknown-language sub-project.
        let sub = sub(root, Language::new(Language::UNKNOWN));
        let ev = evidence(&["guzzlehttp\\client"], &[]);
        let kb = StaticKb(vec![fk(
            "js-fw",
            KnowledgeScope::Framework,
            Language::new(Language::JAVASCRIPT),
            vec![Detector::ImportExists {
                symbol: "GuzzleHttp\\Client".into(),
                confidence: 0.9,
            }],
        )]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &EmptyTechStack);
        assert_eq!(
            hits,
            vec!["js-fw".to_string()],
            "an unknown-language sub must bypass the language gate and still recognise frameworks"
        );
    }

    // ------------------------------------------------------- detector-absent negatives

    /// `Detector::FileExists` whose characteristic file is absent (on both the sub root and the project root)
    /// must not activate the framework.
    #[test]
    fn file_exists_absent_is_not_a_hit() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        let kb = StaticKb(vec![fk(
            "fw",
            KnowledgeScope::Framework,
            Language::new(Language::PHP),
            vec![Detector::FileExists {
                path: "composer.json".into(),
                confidence: 0.9,
            }],
        )]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &evidence(&[], &[]), &EmptyTechStack);
        assert!(
            hits.is_empty(),
            "a missing characteristic file must not activate the framework"
        );
    }

    /// `Detector::ManifestDependency` whose manifest is absent must not activate the framework.
    #[test]
    fn manifest_dependency_absent_is_not_a_hit() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        let kb = StaticKb(vec![fk(
            "fw",
            KnowledgeScope::Framework,
            Language::new(Language::PHP),
            vec![Detector::ManifestDependency {
                manifest: "composer.json".into(),
                dependency: "laravel/framework".into(),
                confidence: 0.9,
            }],
        )]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &evidence(&[], &[]), &EmptyTechStack);
        assert!(
            hits.is_empty(),
            "an absent manifest dependency must not activate the framework"
        );
    }

    /// `Detector::LockDependency` whose lock file is absent must not activate the framework.
    #[test]
    fn lock_dependency_absent_is_not_a_hit() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        let kb = StaticKb(vec![fk(
            "fw",
            KnowledgeScope::Framework,
            Language::new(Language::PHP),
            vec![Detector::LockDependency {
                lock: "composer.lock".into(),
                dependency: "laravel/framework".into(),
                confidence: 0.9,
            }],
        )]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &evidence(&[], &[]), &EmptyTechStack);
        assert!(
            hits.is_empty(),
            "an absent lock dependency must not activate the framework"
        );
    }

    // ------------------------------------------------------- ordering

    /// Multiple matching frameworks are returned in descending confidence order (the `sort_by` at the end
    /// of `detect_frameworks`), independent of the order they appear in the knowledge base.
    #[test]
    fn frameworks_sorted_by_confidence_desc() {
        let root = Path::new("/p");
        let sub = sub(root, Language::new(Language::PHP));
        let ev = evidence(&["guzzlehttp\\client"], &[]);
        let kb = StaticKb(vec![
            fk(
                "low",
                KnowledgeScope::Framework,
                Language::new(Language::PHP),
                vec![Detector::ImportExists {
                    symbol: "GuzzleHttp\\Client".into(),
                    confidence: 0.6,
                }],
            ),
            fk(
                "high",
                KnowledgeScope::Framework,
                Language::new(Language::PHP),
                vec![Detector::ImportExists {
                    symbol: "GuzzleHttp\\Client".into(),
                    confidence: 0.95,
                }],
            ),
        ]);
        let hits =
            detect_frameworks(&kb, &NoFs, &sub, root, KnowledgeScope::Framework, &ev, &EmptyTechStack);
        assert_eq!(
            hits,
            vec!["high".to_string(), "low".to_string()],
            "frameworks must be returned in descending confidence order"
        );
    }

    // ------------------------------------------------------- collect_code_evidence isolation

    /// `collect_code_evidence` must attribute imports / calls to the **requested** sub-project only: a file
    /// outside the sub's prefix is dropped, a call assigned to another sub is dropped, while the two cases
    /// that *should* count (owned call, and an unassigned call whose file still sits under the sub) remain.
    #[test]
    fn collect_code_evidence_isolates_by_sub_project() {
        let mut ctx = PipelineContext::new(Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: std::path::PathBuf::from("/p"),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        });
        // The sub lives at `/p/app`, so its file-prefix is `app/`.
        let sub = SubProject {
            id: SubProjectId(1),
            project_id: ProjectId::new(1),
            name: "app".into(),
            root_path: std::path::PathBuf::from("/p/app"),
            language: Language::new(Language::PHP),
            role: "backend".into(),
            detected_by: "x".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        };
        // Imports: one file inside the sub, one outside it. The FQN is the *value* of the import map
        // (see `all_imported_fqns`, which reads `.values()`).
        ctx.ws.record_file_imports(
            1,
            "app/foo.php",
            HashMap::from([("order".to_string(), "app\\services\\order".to_string())]),
        );
        ctx.ws.record_file_imports(
            2,
            "lib/bar.php",
            HashMap::from([("thing".to_string(), "other\\thing".to_string())]),
        );
        ctx.ws.calls = vec![
            // (a) owned by the sub, inside its prefix -> collected.
            call("app/foo.php", Some(SubProjectId(1)), "A::used", Some("A"), Some("used")),
            // (b) no sub and outside the prefix -> excluded.
            call("lib/bar.php", None, "B::miss", Some("B"), Some("miss")),
            // (c) no sub but inside the prefix -> collected (the rare unassigned-file case).
            call("app/baz.php", None, "C::rare", Some("C"), Some("rare")),
            // (d) assigned to a *different* sub -> excluded.
            call("app/foo.php", Some(SubProjectId(99)), "D::other", Some("D"), Some("other")),
        ];

        let ev = collect_code_evidence(&ctx, &sub);
        assert!(
            ev.imports.contains("app\\services\\order"),
            "an import from a file inside the sub must be collected"
        );
        assert!(
            !ev.imports.contains("other\\thing"),
            "an import from a file outside the sub must be excluded"
        );
        assert!(
            ev.callees.contains_key("A::used"),
            "a call owned by the sub must be collected"
        );
        assert!(
            !ev.callees.contains_key("B::miss"),
            "a call in a foreign file with no sub must be excluded"
        );
        assert!(
            ev.callees.contains_key("C::rare"),
            "an unassigned call whose file is under the sub must be collected"
        );
        assert!(
            !ev.callees.contains_key("D::other"),
            "a call assigned to a different sub must be excluded"
        );
    }
}


