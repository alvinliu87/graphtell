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
