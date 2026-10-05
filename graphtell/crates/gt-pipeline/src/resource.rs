//! Applying **resource** facts — facts that come from files that are not source code.
//!
//! See `gt_domain::port::resource` for the port itself. This module is the kernel counterpart: it decides *when*
//! an adapter runs (P3 must have recognised its knowledge id), asks it for facts, and then applies them to the
//! workspace. Everything else — which files to read, what they mean, how to synthesise a call — belongs to the
//! adapter, which lives in `gt-adapter-*` and knows its own library.
//!
//! The kernel keeps only three generic steps, none of which mention any concrete library:
//! * **owner resolution** — a resource fact is anchored by name, and only the graph can resolve that name into a
//!   node (the adapter cannot see the graph);
//! * **materialisation** — the pseudo call becomes the same `CallSite` node + `HAS_CALL_SITE` edge + `CallRecord`
//!   a parser fact would produce, so P5 matches it like any other call site;
//! * **provenance** — the adapter's evidence properties are carried onto the node for the UI.

use tracing::{info, warn};

use gt_domain::model::{EdgeKind, NewEdge, NewNode, NodeKind, Phase, SubProject, SubProjectId};
use gt_domain::port::{FileSystem, PseudoCall, ResourceAdapterRegistry, ResourceFact};

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// Inject resource facts. Runs after P3 (the nodes to anchor to exist, and detection has answered which knowledge
/// applies) and before P5 (whose rules consume the injected calls).
pub fn run(
    ctx: &mut PipelineContext,
    resources: &dyn ResourceAdapterRegistry,
    fs: &dyn FileSystem,
) {
    let subs = ctx.sub_projects.clone();
    let project_root = ctx.project.root_path.clone();
    // A pseudo call site describes "where some code lives" just like a parser fact does, so it carries the same
    // phase mark.
    let phase = Phase(Phase::CF_AST.to_string());
    let mut total = 0usize;

    for adapter in resources.adapters() {
        let id = adapter.id().to_string();
        for sub in &subs {
            // Whether a library applies is knowledge, not something the kernel decides: an unrecognised project
            // has nothing to contribute and costs nothing to skip.
            if !detected(ctx, sub.id.get(), &id) {
                continue;
            }
            let facts = match adapter.scan(sub, &project_root, fs) {
                Ok(facts) => facts,
                Err(e) => {
                    warn!("resource adapter {id} failed on sub-project {}: {e}", sub.name);
                    continue;
                }
            };
            let mut injected = 0usize;
            for fact in facts {
                let ResourceFact::PseudoCall(call) = fact;
                if apply(ctx, sub, &call, &phase) {
                    injected += 1;
                }
            }
            if injected > 0 {
                info!(
                    "resource scan: adapter {id} injected {injected} pseudo call sites in {}",
                    sub.name
                );
            }
            total += injected;
        }
    }

    if total > 0 {
        info!("resource scan: {total} pseudo call sites injected in total");
    }
}

/// Whether P3 recognised this sub-project under `knowledge_id`.
fn detected(ctx: &PipelineContext, sub_id: i64, knowledge_id: &str) -> bool {
    ctx.frameworks
        .get(&sub_id)
        .map(|ids| ids.iter().any(|id| id == knowledge_id))
        .unwrap_or(false)
}

/// Turn one pseudo call into a call site node + `HAS_CALL_SITE` edge + call record.
///
/// Returns `false` when the anchor cannot be resolved — better missing than attached to the wrong node.
fn apply(ctx: &mut PipelineContext, sub: &SubProject, call: &PseudoCall, phase: &Phase) -> bool {
    // Prefer the member node P2 built (`namespace.statementId`); fall back to the owning class.
    let owner = ctx
        .ws
        .find_by_name(&call.owner_fqn)
        .or_else(|| {
            call.owner_class
                .as_deref()
                .and_then(|class| ctx.ws.find_by_name(class))
        });
    let Some(owner) = owner else {
        return false;
    };

    let line = call.span.start_line;
    let call_node = ctx.ws.add_node(NewNode {
        id: None,
        project_id: ctx.project.id,
        sub_project_id: Some(sub.id),
        kind: NodeKind(NodeKind::CALL_SITE.to_string()),
        name: call.callee.clone(),
        fqn: Some(format!("{}#{}:{}", call.owner_fqn, call.callee, line)),
        identity: None,
        file_id: None,
        span: call.span,
        language: sub.language.clone(),
        phase: phase.clone(),
        confidence: call.confidence,
        properties: evidence_of(call),
    });
    ctx.ws.add_edge(NewEdge {
        project_id: ctx.project.id,
        kind: EdgeKind(EdgeKind::HAS_CALL_SITE.to_string()),
        from_id: owner,
        to_id: call_node,
        phase: phase.clone(),
        confidence: 1.0,
        properties: serde_json::Value::Null,
    });
    ctx.ws.calls.push(CallRecord {
        node: call_node,
        owner,
        owner_fqn: call.owner_fqn.clone(),
        owner_class: call.owner_class.clone(),
        callee: call.callee.clone(),
        receiver: call.receiver.clone(),
        method: call.method.clone(),
        args: call.args.clone(),
        db_table: None,
        // A resource file has no notion of "inside a loop body".
        in_loop: false,
        entity: None,
        span: call.span,
        file: call.file.clone(),
        sub: Some(SubProjectId(sub.id.get())),
        language: sub.language.clone(),
    });
    true
}

/// Merge the adapter's evidence properties with the snippet the UI shows.
fn evidence_of(call: &PseudoCall) -> serde_json::Value {
    let mut props = serde_json::json!({ "snippet": call.snippet });
    if let Some(extra) = call.props.as_object() {
        for (k, v) in extra {
            props[k] = v.clone();
        }
    }
    props
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_domain::error::DomainError;
    use gt_domain::model::{
        FactValue, Language, NodeId, Project, ProjectConfig, ProjectId, ProjectStatus, Span,
    };
    use gt_domain::port::{DefaultResourceAdapterRegistry, ResourceAdapter};
    use std::path::{Path, PathBuf};

    // ---------------------------------------------------------------- fixtures

    fn ctx() -> PipelineContext {
        PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: PathBuf::from("/p"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        })
    }

    fn sub(id: i64, name: &str, root: &str, language: &str) -> SubProject {
        SubProject {
            id: SubProjectId::new(id),
            project_id: ProjectId(1),
            name: name.to_string(),
            root_path: PathBuf::from(root),
            language: Language::new(language),
            role: "backend".into(),
            detected_by: "test".into(),
            frameworks: Vec::new(),
            facts: serde_json::Value::Null,
        }
    }

    fn add_node(ctx: &mut PipelineContext, kind: &str, name: &str, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(kind.to_string()),
            name: name.to_string(),
            fqn: Some(fqn.to_string()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::new("java"),
            phase: Phase("Test".to_string()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    /// A MyBatis-shaped pseudo call: the mapper XML declares a `select` that no source file contains.
    fn pseudo_call(owner_fqn: &str, owner_class: Option<&str>) -> PseudoCall {
        PseudoCall {
            owner_fqn: owner_fqn.to_string(),
            owner_class: owner_class.map(|s| s.to_string()),
            callee: "mybatis::select".to_string(),
            receiver: Some("mybatis".to_string()),
            method: Some("select".to_string()),
            args: vec![FactValue::String("UserMapper.find".to_string())],
            span: Span {
                start_line: 12,
                end_line: 12,
                start_byte: 0,
                end_byte: 0,
            },
            file: "src/main/resources/UserMapper.xml".to_string(),
            snippet: "<select id=\"find\">".to_string(),
            props: serde_json::json!({ "mapper": "UserMapper.xml" }),
            confidence: 0.8,
        }
    }

    struct FakeAdapter {
        id: String,
        facts: Vec<ResourceFact>,
        fail: bool,
    }

    impl ResourceAdapter for FakeAdapter {
        fn id(&self) -> &str {
            &self.id
        }
        fn scan(
            &self,
            _sub: &SubProject,
            _root: &Path,
            _fs: &dyn FileSystem,
        ) -> gt_domain::error::Result<Vec<ResourceFact>> {
            if self.fail {
                return Err(DomainError::NotFound("scan blew up".into()));
            }
            Ok(self.facts.clone())
        }
    }

    /// The adapters ignore the filesystem entirely (they are handed canned facts), so this only has to exist.
    struct NoFs;

    impl FileSystem for NoFs {
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn is_dir(&self, _: &Path) -> bool {
            false
        }
        fn read_to_string(&self, _: &Path) -> gt_domain::error::Result<String> {
            Ok(String::new())
        }
        fn len(&self, _: &Path) -> gt_domain::error::Result<u64> {
            Ok(0)
        }
    }

    fn registry(adapters: Vec<FakeAdapter>) -> DefaultResourceAdapterRegistry {
        let mut reg = DefaultResourceAdapterRegistry::new();
        for a in adapters {
            reg = reg.register(Box::new(a));
        }
        reg
    }

    fn call_sites(ctx: &PipelineContext) -> Vec<NodeId> {
        ctx.ws
            .calls
            .iter()
            .map(|c| c.node)
            .collect()
    }

    // ---------------------------------------------------------------- `detected()`

    /// "Does this library apply" is knowledge, not a kernel decision: only a sub-project P3 recognised under
    /// this very id runs the adapter.
    #[test]
    fn detected_requires_the_knowledge_id_p3_recognised() {
        let mut ctx = ctx();
        ctx.frameworks
            .insert(1, vec!["mybatis".to_string(), "spring-boot".to_string()]);

        assert!(detected(&ctx, 1, "mybatis"));
        assert!(!detected(&ctx, 1, "hibernate"));
        // A sub-project P3 recorded nothing about is not detected either.
        assert!(!detected(&ctx, 2, "mybatis"));
        // Matching is exact: knowledge ids are declared, never case-folded or fuzzy-matched.
        assert!(!detected(&ctx, 1, "MyBatis"));
    }

    // ---------------------------------------------------------------- `run()` gating

    #[test]
    fn run_skips_a_sub_project_that_did_not_detect_the_library() {
        let mut ctx = ctx();
        ctx.sub_projects.push(sub(1, "app", "/p/app", "java"));
        add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");
        // `ctx.frameworks` has no entry for sub 1, so the adapter must not even be asked.
        let reg = registry(vec![FakeAdapter {
            id: "mybatis".into(),
            facts: vec![ResourceFact::PseudoCall(pseudo_call(
                "com.x.UserMapper.find",
                Some("com.x.UserMapper"),
            ))],
            fail: false,
        }]);

        run(&mut ctx, &reg, &NoFs);

        assert!(ctx.ws.calls.is_empty(), "未识别该库的子项目不应注入任何调用点");
        assert!(
            !ctx
                .ws
                .edges()
                .iter()
                .any(|e| e.kind.as_str() == EdgeKind::HAS_CALL_SITE),
            "不应产出锚定边"
        );
    }

    #[test]
    fn run_is_a_noop_without_adapters() {
        let mut ctx = ctx();
        ctx.sub_projects.push(sub(1, "app", "/p/app", "java"));
        ctx.frameworks.insert(1, vec!["mybatis".to_string()]);
        add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");

        run(&mut ctx, &DefaultResourceAdapterRegistry::new(), &NoFs);

        assert!(ctx.ws.calls.is_empty());
    }

    /// The failure of one adapter must not cost the others their facts.
    #[test]
    fn run_continues_when_an_adapter_fails() {
        let mut ctx = ctx();
        ctx.sub_projects.push(sub(1, "app", "/p/app", "java"));
        ctx.frameworks
            .insert(1, vec!["broken".to_string(), "mybatis".to_string()]);
        add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");
        let reg = registry(vec![
            FakeAdapter {
                id: "broken".into(),
                facts: vec![],
                fail: true,
            },
            FakeAdapter {
                id: "mybatis".into(),
                facts: vec![ResourceFact::PseudoCall(pseudo_call(
                    "com.x.UserMapper.find",
                    None,
                ))],
                fail: false,
            },
        ]);

        run(&mut ctx, &reg, &NoFs);

        assert_eq!(ctx.ws.calls.len(), 1, "失败的适配器被跳过，健康的仍然注入");
    }

    #[test]
    fn run_covers_every_detected_sub_project() {
        let mut ctx = ctx();
        ctx.sub_projects.push(sub(1, "app", "/p/app", "java"));
        ctx.sub_projects.push(sub(2, "other", "/p/other", "java"));
        ctx.sub_projects.push(sub(3, "third", "/p/third", "java"));
        ctx.frameworks.insert(1, vec!["mybatis".to_string()]);
        ctx.frameworks.insert(3, vec!["mybatis".to_string()]);
        add_node(&mut ctx, "Method", "a", "com.x.A.find");
        add_node(&mut ctx, "Method", "b", "com.x.B.find");
        let reg = registry(vec![FakeAdapter {
            id: "mybatis".into(),
            facts: vec![
                ResourceFact::PseudoCall(pseudo_call("com.x.A.find", None)),
                ResourceFact::PseudoCall(pseudo_call("com.x.B.find", None)),
            ],
            fail: false,
        }]);

        run(&mut ctx, &reg, &NoFs);

        // Two detected sub-projects x two facts; sub 2 is skipped.
        let subs: Vec<i64> = ctx
            .ws
            .calls
            .iter()
            .filter_map(|c| c.sub.map(|s| s.get()))
            .collect();
        assert_eq!(subs, vec![1, 1, 3, 3]);
        assert!(!subs.contains(&2), "未识别的子项目不应出现");
    }

    // ---------------------------------------------------------------- `apply()` materialisation

    /// The pseudo call becomes the same node + edge + record a parser fact would produce, so P5 matches it
    /// like any other call site.
    #[test]
    fn apply_materialises_a_call_site_just_like_a_parser_fact() {
        let mut ctx = ctx();
        let s = sub(1, "app", "/p/app", "java");
        let owner = add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");
        let phase = Phase("CfAst".to_string());
        let call = pseudo_call("com.x.UserMapper.find", Some("com.x.UserMapper"));

        assert!(apply(&mut ctx, &s, &call, &phase));

        let rec = &ctx.ws.calls[0];
        assert_eq!(rec.owner, owner);
        assert_eq!(rec.owner_fqn, "com.x.UserMapper.find");
        assert_eq!(rec.owner_class.as_deref(), Some("com.x.UserMapper"));
        // The matcher splits `A::b`, so receiver / method must survive verbatim.
        assert_eq!(rec.callee, "mybatis::select");
        assert_eq!(rec.receiver.as_deref(), Some("mybatis"));
        assert_eq!(rec.method.as_deref(), Some("select"));
        assert_eq!(rec.args, vec![FactValue::String("UserMapper.find".to_string())]);
        assert_eq!(rec.file, "src/main/resources/UserMapper.xml");
        assert_eq!(rec.sub, Some(SubProjectId::new(1)));
        assert_eq!(rec.language.0, "java");
        assert_eq!(rec.span.start_line, 12);
        // A resource file has no loop notion and no propagated table.
        assert!(!rec.in_loop);
        assert!(rec.db_table.is_none());
        assert!(rec.entity.is_none());

        let node = ctx.ws.node(rec.node).expect("call site node");
        assert_eq!(node.kind.as_str(), NodeKind::CALL_SITE);
        assert_eq!(node.name, "mybatis::select");
        assert_eq!(
            node.fqn.as_deref(),
            Some("com.x.UserMapper.find#mybatis::select:12")
        );
        assert_eq!(node.sub_project_id, Some(SubProjectId::new(1)));
        assert_eq!(node.language.0, "java");
        // The same phase mark a parser fact carries, and the adapter's own trust.
        assert_eq!(node.phase.0, Phase::CF_AST);
        assert!((node.confidence - 0.8).abs() < 1e-6);
        assert_eq!(node.properties["mapper"], serde_json::json!("UserMapper.xml"));

        assert!(
            ctx.ws.edges().iter().any(|e| {
                e.kind.as_str() == EdgeKind::HAS_CALL_SITE
                    && e.from_id == owner
                    && e.to_id == rec.node
            }),
            "必须锚定到 owner"
        );
    }

    #[test]
    fn apply_prefers_the_member_node_over_the_owning_class() {
        let mut ctx = ctx();
        let s = sub(1, "app", "/p/app", "java");
        let cls = add_node(&mut ctx, "Class", "UserMapper", "com.x.UserMapper");
        let member = add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");
        let phase = Phase("CfAst".to_string());

        assert!(apply(
            &mut ctx,
            &s,
            &pseudo_call("com.x.UserMapper.find", Some("com.x.UserMapper")),
            &phase
        ));
        assert_eq!(ctx.ws.calls[0].owner, member);
        assert_ne!(ctx.ws.calls[0].owner, cls);
    }

    #[test]
    fn apply_falls_back_to_the_owning_class() {
        let mut ctx = ctx();
        let s = sub(1, "app", "/p/app", "java");
        let cls = add_node(&mut ctx, "Class", "UserMapper", "com.x.UserMapper");
        let phase = Phase("CfAst".to_string());
        // The member node does not exist (the mapper is an interface method with no body).
        let call = pseudo_call("com.x.UserMapper.find", Some("com.x.UserMapper"));

        assert!(apply(&mut ctx, &s, &call, &phase));
        assert_eq!(ctx.ws.calls[0].owner, cls);
    }

    /// Better missing than attached to the wrong node.
    #[test]
    fn apply_refuses_when_no_anchor_resolves() {
        let mut ctx = ctx();
        let s = sub(1, "app", "/p/app", "java");
        let phase = Phase("CfAst".to_string());

        assert!(!apply(
            &mut ctx,
            &s,
            &pseudo_call("com.x.Missing.find", None),
            &phase
        ));
        assert!(!apply(
            &mut ctx,
            &s,
            &pseudo_call("com.x.Missing.find", Some("com.x.Missing")),
            &phase
        ));

        assert!(ctx.ws.calls.is_empty());
        assert!(call_sites(&ctx).is_empty());
        assert!(
            !ctx
                .ws
                .edges()
                .iter()
                .any(|e| e.kind.as_str() == EdgeKind::HAS_CALL_SITE),
            "解析不到锚点时不建边"
        );
    }

    /// The line number is part of the identity, so two statements of the same callee stay distinct.
    #[test]
    fn apply_gives_each_pseudo_call_its_own_identity() {
        let mut ctx = ctx();
        let s = sub(1, "app", "/p/app", "java");
        add_node(&mut ctx, "Method", "find", "com.x.UserMapper.find");
        let phase = Phase("CfAst".to_string());
        let mut later = pseudo_call("com.x.UserMapper.find", None);
        later.span = Span {
            start_line: 30,
            end_line: 30,
            start_byte: 0,
            end_byte: 0,
        };

        assert!(apply(
            &mut ctx,
            &s,
            &pseudo_call("com.x.UserMapper.find", None),
            &phase
        ));
        assert!(apply(&mut ctx, &s, &later, &phase));

        assert_eq!(ctx.ws.calls.len(), 2);
        let fqns: Vec<String> = ctx
            .ws
            .calls
            .iter()
            .map(|c| ctx.ws.node(c.node).unwrap().fqn.clone().unwrap())
            .collect();
        assert_eq!(
            fqns,
            vec![
                "com.x.UserMapper.find#mybatis::select:12".to_string(),
                "com.x.UserMapper.find#mybatis::select:30".to_string()
            ]
        );
    }

    // ---------------------------------------------------------------- provenance

    /// The adapter's evidence is merged in so the UI can verify where a synthesised fact came from; the key
    /// names belong to the adapter, the kernel only supplies `snippet` as a default.
    #[test]
    fn evidence_of_merges_the_adapter_properties() {
        let mut call = pseudo_call("x", None);
        call.props = serde_json::json!({ "mapper": "UserMapper.xml", "statement": 3 });
        let props = evidence_of(&call);
        assert_eq!(props["snippet"], serde_json::json!("<select id=\"find\">"));
        assert_eq!(props["mapper"], serde_json::json!("UserMapper.xml"));
        assert_eq!(props["statement"], serde_json::json!(3));

        // On collision the adapter wins.
        call.props = serde_json::json!({ "snippet": "from adapter" });
        assert_eq!(
            evidence_of(&call)["snippet"],
            serde_json::json!("from adapter")
        );

        // A `props` that is not an object is ignored rather than panicking.
        for junk in [serde_json::Value::Null, serde_json::json!("scalar")] {
            call.props = junk;
            assert_eq!(
                evidence_of(&call)["snippet"],
                serde_json::json!("<select id=\"find\">")
            );
        }
    }
}
