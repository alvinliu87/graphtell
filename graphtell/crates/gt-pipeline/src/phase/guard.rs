//! P14 Guard: turn "which middleware this route passed through" into edges on the graph.
//!
//! # Why a **bridge edge**, not a semantic edge
//!
//! `HttpContract --PassesThrough--> <middleware>` has two ends of different nature: the start is a semantic node, the end is a
//! `Class` (syntax node), and the middleware itself doesn't need "multiple references converging to exist". Same family as `HandledBy`
//! (contract → handler), `HasColumn` (table → column): **traversable along the chain, but not counted in "semantic in/out-edge N",
//! nor drawn as a canvas edge** — otherwise a route with 3 middleware would have its "out-edge N" magically +3,
//! mixed into the same metric as the adjacent "read/wrote how many tables".
//!
//! # Why build the edge first, not promote the node first
//!
//! Whether a middleware should later appear as a neighbor node in the folded view is a posteriori choice; but "does this relation really exist,
//! how many hits, does the combination have discriminative power" needs data first. Once the edge is on the graph, promoting or not becomes a
//! **quantifiable decision**, not a taste debate.
//!
//! # Known boundaries
//!
//! * Only accept **route-level / route-group-level** mounts (consistent with P3's trade-off): global middleware holds for every endpoint, it's an environment
//!   constant; connecting an edge to it equals adding the same few edges to every contract — numbers go up, information doesn't.
//! * When the middleware class isn't in the graph (vendor classes, dynamic aliases not yet resolved) **skip, no dangling edge, no guess**.

use gt_domain::model::{
    AnnotationChannel, EdgeKind, MergeStrategy, NewAnnotation, NewEdge, NewNode, NodeId, NodeKind,
    Phase, Span,
};
use serde_json::{json, Value};

use crate::context::PipelineContext;

/// Semantic-edge kind: `HttpContract --PassesThrough--> middleware`.
///
/// The name is neutral ("passes through" not "guard"): middleware includes both guards that reject requests and side paths that only add response headers /
/// log — calling them all "guard" over-declares for the latter. Whether auth is enforced is answered by the `Capability` annotation.
const PASSES_THROUGH: &str = "PassesThrough";

/// Annotation for "an auth middleware was mounted, but the arg explicitly says optional" (reachable without login, doesn't mean not mounted).
const OPTIONAL_AUTH: &str = "auth.optional";

/// Contract name → node (`HttpContract.name` shares the same source as `route_list`'s key).
fn contract_index(ctx: &PipelineContext) -> std::collections::HashMap<String, NodeId> {
    let mut out = std::collections::HashMap::new();
    for id in ctx.ws.nodes_of_kind(NodeKind::HTTP_CONTRACT) {
        if let Some(node) = ctx.ws.node(id) {
            out.insert(node.name.clone(), id);
        }
    }
    out
}

/// Shape of each guard row: `[(middleware short name, mount arg)]`.
fn row_guards(value: &Value) -> Vec<(String, Option<String>)> {
    let Some(arr) = value.get("guards").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|g| {
            let class = g.get("class").and_then(Value::as_str)?.trim();
            if class.is_empty() {
                return None;
            }
            let short = class.rsplit(['\\', '/']).next().unwrap_or(class).to_string();
            let arg = g
                .get("arg")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            Some((short, arg))
        })
        .collect()
}

/// P5.5: mark the capabilities brought by middleware onto the contract (the `Capability` channel).
///
/// # Why must be before P6
///
/// P6 rules like `sample_project-public-endpoint` use `none_of_capability` to decide "public endpoint".
/// A reverse criterion only holds when positive evidence **has actually existed**: if capabilities were annotated only at P14,
/// P6 would already have marked every contract `auth.public` (measured 1529 of 1603), and
/// patching later can't recover it (annotations already persisted).
///
/// # Why this isn't "inference"
///
/// The anchor is the middleware's **definite identity** (class name, which name maps to which capability is declared by FKB), not
/// "does this endpoint look like it needs login". When the mount arg explicitly says optional (`AuthToken::class, false`
/// == reachable without login) **don't mark the capability** — better missing than treating optional as mandatory.
pub fn run_capabilities(ctx: &mut PipelineContext) {
    let phase = Phase("GuardCapability".to_string());
    if ctx.middleware_capabilities.is_empty() {
        return;
    }
    let contracts = contract_index(ctx);
    if contracts.is_empty() {
        return;
    }
    let rows = ctx
        .ws
        .symbols
        .get("route_list")
        .cloned()
        .unwrap_or_default();

    let mut stamped = 0usize;
    let mut skipped_optional = 0usize;
    for (key, value) in rows.iter() {
        let Some(contract_id) = contracts.get(key) else {
            continue;
        };
        let guards = row_guards(value);
        if guards.is_empty() {
            continue;
        }
        for spec in ctx.middleware_capabilities.clone() {
            let lower = spec.matches.to_lowercase();
            let hits: Vec<&(String, Option<String>)> = guards
                .iter()
                .filter(|(short, _)| short.to_lowercase().contains(&lower))
                .collect();
            if hits.is_empty() {
                continue;
            }
            // When all hits are "optional", we can't claim this endpoint is guarded by this capability.
            let mandatory = hits
                .iter()
                .any(|(_, arg)| arg.as_deref().map(|a| a != "false").unwrap_or(true));
            if !mandatory {
                ctx.ws.annotate(NewAnnotation {
                    node_id: *contract_id,
                    channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
                    kind: OPTIONAL_AUTH.to_string(),
                    subkind: None,
                    confidence: 0.9,
                    evidence: json!({
                        "source": "route_guard",
                        "middleware": hits.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
                        "arg": hits.iter().filter_map(|(_, a)| a.clone()).next(),
                    }),
                    phase: phase.clone(),
                    merge: MergeStrategy::Coexist,
                });
                skipped_optional += 1;
                continue;
            }
            ctx.ws.annotate(NewAnnotation {
                node_id: *contract_id,
                channel: AnnotationChannel(AnnotationChannel::CAPABILITY.to_string()),
                kind: spec.capability.clone(),
                subkind: None,
                confidence: 0.9,
                evidence: json!({
                    "source": "route_guard",
                    "middleware": hits.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
                }),
                phase: phase.clone(),
                merge: MergeStrategy::Coexist,
            });
            stamped += 1;
        }
    }

    tracing::info!(
        "P5.5 guard capabilities done: {} capability annotations ({} attaches are explicitly optional, so no capability is applied — better missing than guessed)",
        stamped,
        skipped_optional
    );
}

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Guard".to_string());

    let mut contracts: std::collections::HashMap<String, NodeId> = std::collections::HashMap::new();
    for id in ctx.ws.nodes_of_kind(NodeKind::HTTP_CONTRACT) {
        if let Some(node) = ctx.ws.node(id) {
            contracts.insert(node.name.clone(), id);
        }
    }
    if contracts.is_empty() {
        return;
    }

    // ② plan: contract node + the list of middleware class names on it
    let rows = ctx.ws.symbols.get("route_list").cloned().unwrap_or_default();
    let mut plan: Vec<(NodeId, Vec<String>)> = Vec::new();
    for (key, value) in rows.iter() {
        let Some(contract_id) = contracts.get(key) else {
            continue;
        };
        let Some(guards) = value.get("guards").and_then(Value::as_array) else {
            continue;
        };
        let classes: Vec<String> = guards
            .iter()
            .filter_map(|g| g.get("class").and_then(Value::as_str))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if classes.is_empty() {
            continue;
        }
        plan.push((*contract_id, classes));
    }

    // ③ lay the edge, and promote / build the mounted thing into a `Middleware` semantic node
    let mut created = 0usize;
    let mut promoted = 0usize;
    let mut synthesized = 0usize;
    let mut unresolved = 0usize;
    let allow_synthesize = ctx.ws.synthesize_unresolved_guards;
    let mut known_middleware: std::collections::HashMap<String, Option<Value>> =
        std::collections::HashMap::new();
    for (key, val) in ctx
        .ws
        .symbols
        .get("middleware_classes")
        .cloned()
        .unwrap_or_default()
        .iter()
    {
        let key = crate::phase::prepare::norm_class(key);
        known_middleware.insert(key.clone(), Some(val.clone()));
        let short = key.rsplit(['\\', '/']).next().unwrap_or(&key).to_string();
        known_middleware.entry(short).or_insert_with(|| Some(val.clone()));
    }
    for (contract_id, classes) in plan {
        for class in classes {
            let target = ctx
                .ws
                .find_by_name(&class)
                .or_else(|| {
                    ctx.ws
                        .resolve_short_name(&class)
                        .and_then(|fqn| ctx.ws.find_by_name(&fqn))
                });
            let Some(target) = target else {
                let norm = crate::phase::prepare::norm_class(&class);
                let known = known_middleware.get(&norm).or_else(|| {
                    let short = norm.rsplit(['\\', '/']).next().unwrap_or(&norm);
                    known_middleware.get(short)
                });
                if known.is_none() && !allow_synthesize {
                    unresolved += 1;
                    continue;
                }
                let declared = known.cloned().unwrap_or(None);
                let full = declared
                    .as_ref()
                    .and_then(|v| v.get("class").and_then(|c| c.as_str()))
                    .unwrap_or(&class)
                    .to_string();
                // Display uses the **short name** (consistent with other middleware nodes in the graph); FQN stays in the `fqn` field for lookup.
                let name = if known.is_some() {
                    crate::phase::prepare::norm_class(&full)
                        .rsplit(['\\', '/'])
                        .next()
                        .unwrap_or(&full)
                        .to_string()
                } else {
                    full.clone()
                };
                let mut props = json!({ "source": "route_guard" });
                if let Some(cap) = declared.as_ref().and_then(|v| v.get("capability")) {
                    props["capability"] = cap.clone();
                }
                if known.is_some() {
                    props["declared_by"] = json!("fkb");
                }
                // Build a `Middleware` semantic node (declared / authorized by FKB).
                let id = ctx.ws.add_node(NewNode {
                    id: None,
                    project_id: ctx.project.id,
                    sub_project_id: None,
                    kind: NodeKind(NodeKind::MIDDLEWARE.to_string()),
                    name: name.clone(),
                    fqn: Some(name),
                    identity: None,
                    file_id: None,
                    span: Span::default(),
                    // Language follows the owning contract (a newly built middleware node has no file location of its own).
                    language: ctx
                        .ws
                        .node(contract_id)
                        .map(|n| n.language.clone())
                        .unwrap_or_default(),
                    phase: phase.clone(),
                    confidence: if known.is_some() { 1.0 } else { 0.9 },
                    properties: props,
                });
                synthesized += 1;
                if ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: EdgeKind(PASSES_THROUGH.to_string()),
                    from_id: contract_id,
                    to_id: id,
                    phase: phase.clone(),
                    confidence: 0.9,
                    properties: Value::Null,
                }) {
                    created += 1;
                }
                continue;
            };
            if target == contract_id {
                continue;
            }
            let kind = ctx
                .ws
                .node(target)
                .map(|n| n.kind.as_str().to_string())
                .unwrap_or_default();
            let is_class = kind == NodeKind::CLASS;
            let is_callable = kind == NodeKind::FUNCTION || kind == NodeKind::METHOD;
            if is_class || (allow_synthesize && is_callable) {
                ctx.ws.patch_kind(target, NodeKind::MIDDLEWARE);
                promoted += 1;
            }
            if ctx.ws.add_edge(NewEdge {
                project_id: ctx.project.id,
                kind: EdgeKind(PASSES_THROUGH.to_string()),
                from_id: contract_id,
                to_id: target,
                phase: phase.clone(),
                confidence: 0.9,
                properties: Value::Null,
            }) {
                created += 1;
            }
        }
    }

    tracing::info!(
        "P14 route guards done: {} PassesThrough edges / {} promoted to Middleware / {} built as Middleware by name ({} middleware absent from the graph and unauthorised, skipped rather than guessed)",
        created,
        promoted,
        synthesized,
        unresolved
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PipelineContext;
    use gt_adapter_fkb::YamlKnowledgeBase;
    use gt_domain::model::{IdentityKey, MiddlewareCapability, NewNode, ProjectId, ProjectStatus};

    fn project() -> gt_domain::model::Project {
        gt_domain::model::Project {
            id: ProjectId::new(1),
            name: "t".into(),
            root_path: "/t".into(),
            description: None,
            status: ProjectStatus::Ready,
            config: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Contract node: `name` must share the same shape as `route_list`'s key, otherwise it won't be found.
    fn add_contract(ctx: &mut PipelineContext, name: &str) -> NodeId {
        let mut n = NewNode::new(
            ProjectId::new(1),
            NodeKind(NodeKind::HTTP_CONTRACT.to_string()),
            name,
        );
        n.identity = Some(IdentityKey::contract("GET", name));
        let (id, _) = ctx.ws.get_or_create_synthesized(n);
        id
    }

    /// Middleware class node: `by_fqn` indexes by FQN, so `fqn` must be written.
    fn add_class(ctx: &mut PipelineContext, fqn: &str) -> NodeId {
        let mut n = NewNode::new(
            ProjectId::new(1),
            NodeKind(NodeKind::CLASS.to_string()),
            fqn,
        );
        n.fqn = Some(fqn.into());
        ctx.ws.add_node(n)
    }

    /// In-memory: insert a guard-bearing route row into the `route_list` symbol table.
    fn put_row(ctx: &mut PipelineContext, key: &str, guards: Value) {
        ctx.ws.put_symbol(
            ProjectId::new(1),
            "route_list",
            key,
            serde_json::json!({ "handler": "C@m", "guards": guards }),
        );
    }

    fn guarded_by(ctx: &PipelineContext) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for e in ctx.ws.edges() {
            if e.kind.as_str() != PASSES_THROUGH {
                continue;
            }
            let f = ctx.ws.node(e.from_id).map(|n| n.name.clone()).unwrap_or_default();
            let t = ctx
                .ws
                .node(e.to_id)
                .and_then(|n| n.fqn.clone())
                .unwrap_or_default();
            out.push((f, t));
        }
        out.sort();
        out
    }

    #[test]
    fn links_contract_to_middleware() {
        let mut ctx = PipelineContext::new(project());
        let c1 = add_contract(&mut ctx, "GET /pc/get_cart_list");
        let _c2 = add_contract(&mut ctx, "GET /pc/get_banner");
        let auth = add_class(&mut ctx, r"app\api\middleware\AuthTokenMiddleware");
        let cors = add_class(&mut ctx, r"app\http\middleware\AllowOriginMiddleware");

        put_row(
            &mut ctx,
            "GET /pc/get_cart_list",
            serde_json::json!([
                { "class": r"app\api\middleware\AuthTokenMiddleware", "arg": "true" },
                { "class": r"app\http\middleware\AllowOriginMiddleware", "arg": null },
            ]),
        );
        // Key mismatch (a contract not in `route_list`) shouldn't grow an edge from nothing
        put_row(
            &mut ctx,
            "GET /not-a-contract",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware" }]),
        );

        run(&mut ctx);

        let got = guarded_by(&ctx);
        assert_eq!(
            got,
            vec![
                ("GET /pc/get_cart_list".to_string(), r"app\api\middleware\AuthTokenMiddleware".to_string()),
                ("GET /pc/get_cart_list".to_string(), r"app\http\middleware\AllowOriginMiddleware".to_string()),
            ]
        );
        assert_eq!(ctx.ws.node(c1).map(|n| n.name.as_str()), Some("GET /pc/get_cart_list"));
        assert_ne!(auth, cors);
    }

    /// The mounted class must be **promoted** to `Middleware` (change kind, don't create a new node).
    #[test]
    fn promotes_class_to_middleware_without_duplicating() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        let mw = add_class(&mut ctx, r"app\api\middleware\AuthTokenMiddleware");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware" }]),
        );

        run(&mut ctx);

        let node = ctx.ws.node(mw).expect("the middleware class node must still exist");
        assert_eq!(node.kind.as_str(), NodeKind::MIDDLEWARE, "kind must be changed to Middleware");
        assert_eq!(node.fqn.as_deref(), Some(r"app\api\middleware\AuthTokenMiddleware"));
        // Key: promotion produces no second node — the graph still has only this one, fqn still finds the same id
        assert_eq!(ctx.ws.find_by_name(r"app\api\middleware\AuthTokenMiddleware"), Some(mw));
        assert!(NodeKind(NodeKind::MIDDLEWARE.to_string()).is_semantic());
        // Promotion must actually persist (the delta carries the kind patch), otherwise re-run loses it
        assert!(
            ctx.ws.take_delta().kind_patches.iter().any(|(id, _)| *id == mw),
            "the promotion must be written into the delta, otherwise the persistence layer never sees it"
        );
    }

    /// Middleware not in the graph (vendor / alias unresolved): skip, no dangling edge.
    #[test]
    fn skips_absent_middleware() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"think\middleware\SessionInit" }]),
        );
        run(&mut ctx);
        assert!(guarded_by(&ctx).is_empty(), "a class that is not in the graph must have no edges");
    }

    // ------------------------------------------------------- P5.5 `run_capabilities`
    //
    // The **only** producer of the `Capability` channel (and of `auth.optional`). Downstream, rules read
    // that channel; when it is empty the rule **deactivates** (see `rules_and_recall.rs`), so a silent
    // stop here reads as "no endpoint lacks auth", not as "not analysed".

    fn caps(specs: &[(&str, &str)]) -> Vec<MiddlewareCapability> {
        specs
            .iter()
            .map(|(m, c)| MiddlewareCapability {
                matches: (*m).into(),
                capability: (*c).into(),
            })
            .collect()
    }

    /// `(kind, channel)` of every annotation on a node.
    fn stamped(ctx: &PipelineContext, id: NodeId) -> Vec<(String, String)> {
        ctx.ws
            .annotations_of(id)
            .into_iter()
            .map(|a| (a.kind.clone(), a.channel.0.clone()))
            .collect()
    }

    fn has(ctx: &PipelineContext, id: NodeId, kind: &str, channel: &str) -> bool {
        stamped(ctx, id)
            .iter()
            .any(|(k, ch)| k.as_str() == kind && ch.as_str() == channel)
    }

    #[test]
    fn capability_is_stamped_from_the_mounted_middleware() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("AuthToken", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware" }]),
        );

        run_capabilities(&mut ctx);

        assert!(
            has(&ctx, c, "Authentication", AnnotationChannel::CAPABILITY),
            "attaching AuthTokenMiddleware must yield the Authentication capability, got: {:?}",
            stamped(&ctx, c)
        );
    }

    /// Matching is a case-insensitive **substring** test against the class's short name (the part after
    /// the last `\` / `/`), so a spec written in another case still matches.
    #[test]
    fn capability_matching_is_case_insensitive_on_the_short_name() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("authtoken", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware" }]),
        );

        run_capabilities(&mut ctx);

        assert!(
            has(&ctx, c, "Authentication", AnnotationChannel::CAPABILITY),
            "a lower-case spec must still match the short name AuthTokenMiddleware, got: {:?}",
            stamped(&ctx, c)
        );
    }

    /// "Mounted but explicitly optional" must NOT be claimed as a capability: it is recorded separately
    /// as `auth.optional` — the "prefer missing over guessing" direction.
    #[test]
    fn optional_mount_stamps_auth_optional_instead_of_the_capability() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("AuthToken", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware", "arg": "false" }]),
        );

        run_capabilities(&mut ctx);

        assert!(
            has(&ctx, c, OPTIONAL_AUTH, AnnotationChannel::FKB_MARK),
            "an explicitly optional attach must yield auth.optional, got: {:?}",
            stamped(&ctx, c)
        );
        assert!(
            !ctx.ws.has_annotation(c, "Authentication"),
            "an optional attach must not count as already having that capability: {:?}",
            stamped(&ctx, c)
        );
    }

    /// No match, and no vocabulary at all: stamp nothing. An empty `Capability` channel is what
    /// deactivates the downstream rule, so it must never be filled with a guess.
    #[test]
    fn unmatched_middleware_and_empty_vocabulary_stamp_nothing() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("AuthToken", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\middleware\Cors" }]),
        );
        run_capabilities(&mut ctx);
        assert!(
            stamped(&ctx, c).is_empty(),
            "no annotation at all when nothing matches: {:?}",
            stamped(&ctx, c)
        );

        // An empty vocabulary short-circuits before the symbol table is consulted at all.
        let mut bare = PipelineContext::new(project());
        let c2 = add_contract(&mut bare, "GET /y");
        put_row(
            &mut bare,
            "GET /y",
            serde_json::json!([{ "class": "AuthTokenMiddleware" }]),
        );
        run_capabilities(&mut bare);
        assert!(
            stamped(&bare, c2).is_empty(),
            "must return immediately when there is no capability vocabulary: {:?}",
            stamped(&bare, c2)
        );
    }

    // ------------------------------------------------------- P14 synthesis of declared middleware
    //
    // A mounted middleware that is not in the graph may still be **declared** by FKB
    // (`middleware_classes`, fed by `fkb/php/laravel.yaml` and `fkb/php/spatie-permission.yaml`). Then a
    // `Middleware` node is materialised instead of the mount being dropped.

    fn declare_middleware(ctx: &mut PipelineContext, key: &str, class: &str, capability: Option<&str>) {
        let mut v = serde_json::json!({ "class": class });
        if let Some(c) = capability {
            v["capability"] = serde_json::json!(c);
        }
        ctx.ws.put_symbol(ProjectId::new(1), "middleware_classes", key, v);
    }

    #[test]
    fn fkb_declared_middleware_is_materialised_as_a_middleware_node() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        declare_middleware(
            &mut ctx,
            "Authenticate",
            r"app\Http\Middleware\Authenticate",
            Some("Authentication"),
        );
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": "Authenticate" }]),
        );

        run(&mut ctx);

        let got = guarded_by(&ctx);
        assert_eq!(got.len(), 1, "exactly one PassesThrough edge must be created: {got:?}");
        assert_eq!(got[0].1, "Authenticate", "the node must be created from the short name: {got:?}");

        let id = ctx
            .ws
            .find_by_name("Authenticate")
            .expect("the synthesised middleware node must be findable by name");
        let node = ctx.ws.node(id).expect("the node must exist");
        assert_eq!(node.kind.as_str(), NodeKind::MIDDLEWARE, "a Middleware semantic node must be created");
        assert_eq!(
            node.properties.get("declared_by").and_then(|v| v.as_str()),
            Some("fkb"),
            "must be marked declared_by=fkb: {:?}",
            node.properties
        );
        assert_eq!(
            node.properties.get("capability").and_then(|v| v.as_str()),
            Some("Authentication"),
            "the capability must come from the FKB declaration: {:?}",
            node.properties
        );
        assert!(
            (node.confidence - 1.0).abs() < 1e-6,
            "a node declared by the FKB must have confidence 1.0, got {}",
            node.confidence
        );
    }

    /// Synthesis authorised but the middleware declared nowhere: still built, but at lower confidence
    /// and **without** `declared_by` — the graph must not claim FKB authorised it.
    #[test]
    fn authorized_but_undeclared_middleware_is_synthesized_at_lower_confidence() {
        let mut ctx = PipelineContext::new(project());
        ctx.ws.synthesize_unresolved_guards = true;
        add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"vendor\Some\Middleware" }]),
        );

        run(&mut ctx);

        let got = guarded_by(&ctx);
        assert_eq!(got.len(), 1, "one edge must be created after the authorised synthesis: {got:?}");
        let id = ctx
            .ws
            .find_by_name(&got[0].1)
            .expect("the synthesised node must be findable by name");
        let node = ctx.ws.node(id).expect("the node must exist");
        assert_eq!(node.kind.as_str(), NodeKind::MIDDLEWARE);
        assert!(
            node.properties.get("declared_by").is_none(),
            "not declared by the FKB means no declared_by marker: {:?}",
            node.properties
        );
        assert!(
            (node.confidence - 0.9).abs() < 1e-6,
            "an undeclared synthesised node must have confidence 0.9, got {}",
            node.confidence
        );
    }

    // ------------------------------------------------------- the shipped FKB's vocabulary

    /// `fkb/php/laravel.yaml` declares six `middleware_capabilities`. A typo in any `matches` string would
    /// silently stop stamping that capability, and an empty `Capability` channel deactivates the
    /// downstream rule — so nothing else would notice.
    #[test]
    fn real_laravel_fkb_capabilities_are_stamped_on_the_contract() {
        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/php/laravel.yaml");
        assert!(fkb_path.is_file(), "the real FKB must exist: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("the real FKB failed to parse: {}: {e}", fkb_path.display()));
        assert!(
            !real_fk.middleware_capabilities.is_empty(),
            "the laravel FKB must declare middleware_capabilities"
        );

        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = real_fk.middleware_capabilities.clone();
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([
                { "class": r"app\Http\Middleware\Authenticate" },
                { "class": r"app\Http\Middleware\ValidateSignature" },
                { "class": r"app\Http\Middleware\ThrottleRequests" },
            ]),
        );

        run_capabilities(&mut ctx);

        let got = stamped(&ctx, c);
        for capability in ["Authentication", "SignedRequest", "RateLimiting"] {
            assert!(
                has(&ctx, c, capability, AnnotationChannel::CAPABILITY),
                "the real FKB must yield the {capability} capability, got: {got:?}"
            );
        }
        assert!(
            !ctx.ws.has_annotation(c, OPTIONAL_AUTH),
            "with no optional argument auth.optional must not be applied: {got:?}"
        );
    }

    // ------------------------------------------------------- branch gaps
    //
    // The 10 tests above cover the principal paths; these pin branches that are otherwise only reached
    // indirectly (or not at all).

    /// A route references a middleware by its **short name** while the class node sits under its FQN.
    /// `run` must resolve the short name (`resolve_short_name`) and still promote the found class node —
    /// the direct `find_by_name` hit used by `links_contract_to_middleware` never exercises this.
    #[test]
    fn promotes_middleware_referenced_by_short_name() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        let mw = add_class(&mut ctx, r"app\Http\Middleware\Authenticate");
        // Route names it by the short name, not the FQN.
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": "Authenticate" }]),
        );

        run(&mut ctx);

        let node = ctx.ws.node(mw).expect("the class node must still be there");
        assert_eq!(
            node.kind.as_str(),
            NodeKind::MIDDLEWARE,
            "a class node found through the short-name fallback must be promoted to Middleware"
        );
        assert_eq!(
            guarded_by(&ctx),
            vec![(
                "GET /x".to_string(),
                r"app\Http\Middleware\Authenticate".to_string()
            )]
        );
    }

    /// A spec that matches BOTH an optional mount (`arg: "false"`) and a mandatory mount (`arg: "true"`)
    /// of the same capability must stamp the capability, not `auth.optional` — "when ALL hits are optional"
    /// is decided by `any`, so one mandatory hit carries the day.
    #[test]
    fn mixed_optional_and_mandatory_mount_stamps_capability() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("Auth", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([
                { "class": "OptionalAuth", "arg": "false" },
                { "class": "MandatoryAuth", "arg": "true" },
            ]),
        );

        run_capabilities(&mut ctx);

        assert!(
            has(&ctx, c, "Authentication", AnnotationChannel::CAPABILITY),
            "a single mandatory attach is enough to apply the capability, got: {:?}",
            stamped(&ctx, c)
        );
        assert!(
            !has(&ctx, c, OPTIONAL_AUTH, AnnotationChannel::FKB_MARK),
            "an additional optional attach must not switch it to auth.optional, got: {:?}",
            stamped(&ctx, c)
        );
    }

    /// A middleware resolved to a **callable** (METHOD / FUNCTION) target is only promoted to `Middleware`
    /// when synthesis is allowed; the edge is laid either way. The class-promotion test uses a CLASS, so
    /// this `is_callable` branch (line 311) is otherwise unexercised.
    #[test]
    fn callable_middleware_is_promoted_only_when_synthesis_allowed() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        let mut n = NewNode::new(
            ProjectId::new(1),
            NodeKind(NodeKind::METHOD.to_string()),
            "makeMiddleware",
        );
        n.fqn = Some("makeMiddleware".into());
        let m = ctx.ws.add_node(n);
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": "makeMiddleware" }]),
        );

        // Without synthesis: edge laid, but the METHOD node keeps its kind.
        run(&mut ctx);
        assert_eq!(guarded_by(&ctx).len(), 1, "the edge must still be created");
        assert_eq!(
            ctx.ws.node(m).unwrap().kind.as_str(),
            NodeKind::METHOD,
            "an unauthorised method node must not be promoted"
        );

        // With synthesis: the METHOD node is promoted to Middleware.
        let mut ctx2 = PipelineContext::new(project());
        ctx2.ws.synthesize_unresolved_guards = true;
        add_contract(&mut ctx2, "GET /x");
        let mut n2 = NewNode::new(
            ProjectId::new(1),
            NodeKind(NodeKind::METHOD.to_string()),
            "makeMiddleware",
        );
        n2.fqn = Some("makeMiddleware".into());
        let m2 = ctx2.ws.add_node(n2);
        put_row(
            &mut ctx2,
            "GET /x",
            serde_json::json!([{ "class": "makeMiddleware" }]),
        );
        run(&mut ctx2);
        assert_eq!(
            ctx2.ws.node(m2).unwrap().kind.as_str(),
            NodeKind::MIDDLEWARE,
            "when authorised, the method node must be promoted to Middleware"
        );
    }

    // `row_guards` is the pure parser of a guard row; only reached indirectly above, so pin its shape:
    // short name after the last `\` / `/`, trimmed, empty class skipped, `arg` taken when present.
    #[test]
    fn row_guards_extracts_short_name_and_arg() {
        let v = serde_json::json!({
            "guards": [
                { "class": r"app\api\middleware\AuthTokenMiddleware", "arg": "true" },
                { "class": "/var/www/AllowOrigin", "arg": null },
                { "class": "  ", "arg": "x" },          // empty after trim -> dropped
                { "class": "PlainName" },               // no arg -> None
            ]
        });
        let got = row_guards(&v);
        assert_eq!(
            got,
            vec![
                ("AuthTokenMiddleware".to_string(), Some("true".to_string())),
                ("AllowOrigin".to_string(), None),
                ("PlainName".to_string(), None),
            ],
            "short name / trim / empty-skip / arg parsing must behave as expected: {got:?}"
        );
        // Missing `guards` key yields nothing.
        assert!(row_guards(&serde_json::json!({ "handler": "C@m" })).is_empty());
    }

    /// `run_capabilities` must short-circuit when there are no `HttpContract` nodes (its own
    /// `contracts.is_empty()` guard) — an empty `Capability` / `auth.optional` channel would otherwise
    /// read downstream as "every endpoint lacks auth" rather than "not analysed".
    #[test]
    fn run_capabilities_stamps_nothing_without_contracts() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("Auth", "Authentication")]);
        // A route row exists, but no contract node was ever created.
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": "AuthTokenMiddleware", "arg": "true" }]),
        );
        run_capabilities(&mut ctx);
        assert!(ctx.ws.annotation_count() == 0, "with no contract node, no annotation must be applied");
    }

    /// `run` must link nothing when there are no `HttpContract` nodes to attach edges to (its own
    /// `contracts.is_empty()` guard) — a class node present but no contract must not yield a PassesThrough edge.
    #[test]
    fn run_links_nothing_without_contracts() {
        let mut ctx = PipelineContext::new(project());
        add_class(&mut ctx, r"app\api\middleware\AuthTokenMiddleware");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": r"app\api\middleware\AuthTokenMiddleware" }]),
        );
        run(&mut ctx);
        assert!(guarded_by(&ctx).is_empty(), "with no contract node, no edge must be created");
    }

    /// A guard whose resolved target *is* the contract node itself must not produce a self-edge
    /// (`target == contract_id` continue). Reached by giving the contract the same name as the mounted
    /// class, so `find_by_name` resolves the middleware name back to the contract node.
    #[test]
    fn run_skips_self_referential_guard() {
        let mut ctx = PipelineContext::new(project());
        // Contract named exactly like a middleware short name.
        add_contract(&mut ctx, "AuthTokenMiddleware");
        put_row(
            &mut ctx,
            "AuthTokenMiddleware",
            serde_json::json!([{ "class": "AuthTokenMiddleware" }]),
        );
        run(&mut ctx);
        assert!(
            guarded_by(&ctx).is_empty(),
            "a self-referencing guard must not produce a self-edge: {:?}",
            guarded_by(&ctx)
        );
    }

    /// `run` skips a route row that has no `guards` array (the `value.get("guards").and_then(as_array)`
    /// else-branch), so a row shaped only with a handler produces no edge and leaves the class node alone.
    #[test]
    fn run_skips_row_without_guards_array() {
        let mut ctx = PipelineContext::new(project());
        add_contract(&mut ctx, "GET /x");
        let mw = add_class(&mut ctx, r"app\api\middleware\AuthTokenMiddleware");
        ctx.ws.put_symbol(
            ProjectId::new(1),
            "route_list",
            "GET /x",
            serde_json::json!({ "handler": "C@m" }),
        );
        run(&mut ctx);
        assert!(
            guarded_by(&ctx).is_empty(),
            "a route line with no guards must not produce an edge: {:?}",
            guarded_by(&ctx)
        );
        assert_eq!(
            ctx.ws.node(mw).unwrap().kind.as_str(),
            NodeKind::CLASS,
            "the class node must not be modified"
        );
    }

    /// `run_capabilities` skips a route row whose key is not a contract name (the `contracts.get(key)`
    /// miss), even when a contract *does* exist for a different key.
    #[test]
    fn capabilities_skip_row_with_non_contract_key() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("Auth", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x"); // contract exists, but the row key won't match it
        put_row(
            &mut ctx,
            "GET /other",
            serde_json::json!([{ "class": "AuthTokenMiddleware", "arg": "true" }]),
        );
        run_capabilities(&mut ctx);
        assert!(
            stamped(&ctx, c).is_empty(),
            "no annotation when the route key does not match the contract name: {:?}",
            stamped(&ctx, c)
        );
    }

    /// The produced `Capability` / `auth.optional` annotations must carry an `evidence` map that
    /// downstream rules read (`source`, the mounted `middleware` list, and the optional `arg`). The
    /// other capability tests only check kind / channel, so a regression that dropped any of these
    /// fields would slip past them.
    #[test]
    fn capability_evidence_captures_mounted_middleware_and_arg() {
        let mut ctx = PipelineContext::new(project());
        ctx.middleware_capabilities = caps(&[("Auth", "Authentication")]);
        let c = add_contract(&mut ctx, "GET /x");
        put_row(
            &mut ctx,
            "GET /x",
            serde_json::json!([{ "class": "AuthTokenMiddleware", "arg": "true" }]),
        );
        run_capabilities(&mut ctx);

        let ann = ctx
            .ws
            .annotations_of(c)
            .into_iter()
            .find(|a| a.kind == "Authentication")
            .expect("an Authentication annotation must be present");
        assert_eq!(
            ann.evidence.get("source").and_then(|v| v.as_str()),
            Some("route_guard")
        );
        let mw = ann
            .evidence
            .get("middleware")
            .and_then(Value::as_array)
            .expect("middleware list");
        assert_eq!(mw.len(), 1);
        assert_eq!(mw[0].as_str(), Some("AuthTokenMiddleware"));

        // Optional variant: evidence should carry the `arg` that justified the downgrade.
        let mut ctx2 = PipelineContext::new(project());
        ctx2.middleware_capabilities = caps(&[("Auth", "Authentication")]);
        let c2 = add_contract(&mut ctx2, "GET /y");
        put_row(
            &mut ctx2,
            "GET /y",
            serde_json::json!([{ "class": "AuthTokenMiddleware", "arg": "false" }]),
        );
        run_capabilities(&mut ctx2);
        let opt = ctx2
            .ws
            .annotations_of(c2)
            .into_iter()
            .find(|a| a.kind == OPTIONAL_AUTH)
            .expect("an auth.optional annotation must be present");
        assert_eq!(
            opt.evidence.get("arg").and_then(|v| v.as_str()),
            Some("false")
        );
    }
}
