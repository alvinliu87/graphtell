//! P7 Resolve: build dynamic edges.
//!
//! Core is the **resolution funnel + fixed-point iteration**:
//!
//! | tier | source                                  | confidence |
//! | ---- | ----------------------------------- | --- |
//! | L1   | literal FQN `make(StoreOrderServices::class)` | 1.0 |
//! | L2   | container registry (file declared by FKB)        | 0.95 |
//! | L3   | alias index (Facade / event name / getter)           | 0.85 |
//! | L4   | convention (namespace join, class-name inference)                    | 0.8  |
//! | L5   | constant propagation                                | 0.6  |
//! | L6   | intersect with the finite universe (schema's 203 tables)            | 0.7  |
//! | L7   | completely unknown                                | 0.3  |
//!
//! Iteration criterion: **continue to the next round only if a candidate set grows**; until convergence; then build all edges uniformly.

use std::collections::{HashMap, HashSet};

use gt_domain::model::{
    AnnotationChannel, EdgeKind, FactValue, MethodRefSpec, MergeStrategy, NewAnnotation,
    NewEdge, NodeId, NodeKind, Phase, ResolveStrategy, ResolveTier, Resolution, Severity,
    SubProjectId,
};
use gt_domain::port::KnowledgeProvider;
use serde_json::Value;
use tracing::info;

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// One dynamic-resolution request (deduplicated, participates in fixed-point iteration).
#[derive(Debug, Clone)]
struct Locator {
    owner: NodeId,
    /// The `CallSite` node of this call site itself — when persisting the DB edge, also tag `db-query`,
    /// so the "query in a loop (N+1)" rule judges by **call site** not the whole method.
    call_node: NodeId,
    /// FQN of the owning method (for looking up param / property types when resolving by "variable type").
    owner_fqn: String,
    strategy: ResolveStrategy,
    raw: String,
    /// Variable-type strategy uses: receiver (`$services` / `$this->services`).
    receiver: Option<String>,
    /// Variable-type strategy uses: method name.
    method: Option<String>,
    /// `EventListen` strategy uses: listener class (arg1, e.g. `OrderListener::class`).
    consumer: Option<String>,
    file: String,
    line: u32,
    /// Owning sub-project: used to fetch this sub-project's route-handler resolution rules (declared by FKB).
    sub: Option<SubProjectId>,
}

/// Run Resolve.
pub fn run(ctx: &mut PipelineContext, kb: &dyn KnowledgeProvider) {
    let phase = Phase(Phase::RESOLVE.to_string());

    // ① First resolve the pending links left by P5 (route handlers, etc.)
    resolve_pending_links(ctx, &phase);

    // ①.5 Generic call resolution: build `Calls` edges, truly connecting the call chain
    //      "route → handler → service method → semantic node" (otherwise semantic edges sit isolated on the callee, and the view can only detour via structural edges).
    resolve_calls(ctx, &phase);

    classify_facade_db_calls(ctx);

    // ② Collect FKB-declared dynamic calls
    let resolvers: Vec<(String, ResolveStrategy)> = ctx
        .frameworks
        .values()
        .flatten()
        .filter_map(|id| kb.by_id(id))
        .flat_map(|fk| fk.resolvers.iter())
        .map(|r| (r.call.clone().unwrap_or_default(), r.strategy))
        .filter(|(call, _)| !call.is_empty())
        .collect();

    // Dedupe by (strategy, raw): the same string only needs resolving once
    let mut uniques: Vec<Locator> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for call in ctx.ws.calls.iter() {
        for (pattern, strategy) in &resolvers {
            if !crate::engine::aliased_callee_matches(&ctx.ws, pattern, call) {
                continue;
            }
            // Variable type: build the locator by "owning method + receiver + method", not depending on args.
            if *strategy == ResolveStrategy::VariableType {
                let (Some(recv), Some(method)) =
                    (call.receiver.as_deref(), call.method.as_deref())
                else {
                    continue;
                };
                // Bare (non-`$`, non-`(new `) receivers are field/instance accesses only on stacks whose
                // field notation uses bare identifiers (Java / JS / Python). Stacks that mark fields with a
                // prefix (PHP's `$`) set this to `false`, so this reads the language policy instead of
                // hard-coding `language == java / javascript`.
                let bare_field = ctx.lang_policy_for_sub(call.sub).bare_field_receivers;
                if (!recv.starts_with('$') && !recv.starts_with("(new ") && !bare_field)
                    || method.is_empty()
                {
                    continue;
                }
                let raw = format!("{}::{recv}::{method}", call.owner_fqn);
                if keys.insert(format!("{strategy:?}:{raw}")) {
                    uniques.push(Locator {
                        owner: call.owner,
                        call_node: call.node,
                        owner_fqn: call.owner_fqn.clone(),
                        strategy: *strategy,
                        raw,
                        receiver: Some(recv.to_string()),
                        method: Some(method.to_string()),
                        consumer: None,
                        file: call.file.clone(),
                        line: call.span.start_line,
                        sub: call.sub,
                    });
                }
                continue;
            }
            let raw = call.args.first().map(fact_to_string).unwrap_or_default();
            if raw.is_empty() || raw.len() > 256 {
                continue;
            }
            // `EventListen` needs arg1 (listener class) as the other end of the edge.
            let consumer = if *strategy == ResolveStrategy::EventListen {
                call.args.get(1).map(fact_to_string)
            } else {
                None
            };
            let key = format!("{strategy:?}:{raw}");
            if keys.insert(key) {
                uniques.push(Locator {
                    owner: call.owner,
                    call_node: call.node,
                    owner_fqn: call.owner_fqn.clone(),
                    strategy: *strategy,
                    raw,
                    receiver: call.receiver.clone(),
                    method: call.method.clone(),
                    consumer,
                    file: call.file.clone(),
                    line: call.span.start_line,
                    sub: call.sub,
                });
            }
        }
    }

    // ③ Fixed-point iteration: until no locator's candidate set grows again
    let mut resolutions: HashMap<String, Resolution> = HashMap::new();
    let mut rounds = 0usize;
    const MAX_ROUNDS: usize = 16;
    loop {
        rounds += 1;
        let mut grew = false;
        for loc in &uniques {
            let key = format!("{:?}:{}", loc.strategy, loc.raw);
            let before = resolutions.get(&key).map(|r| r.candidates.len()).unwrap_or(0);
            let res = resolve_once(ctx, loc);
            if res.candidates.len() > before {
                resolutions.insert(key, res);
                grew = true;
            } else if !resolutions.contains_key(&key) {
                resolutions.insert(key, res);
            }
        }
        if !grew || rounds >= MAX_ROUNDS {
            break;
        }
    }

    // ④ After convergence, build edges and account uniformly
    for loc in &uniques {
        let key = format!("{:?}:{}", loc.strategy, loc.raw);
        if let Some(res) = resolutions.get(&key) {
            apply_resolution(ctx, loc, res, &phase);
        }
    }

    info!("P7 resolution complete: {} resolution points, {} iteration rounds", uniques.len(), rounds);
}

fn resolve_once(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match loc.strategy {
        ResolveStrategy::Container => resolve_container(ctx, loc),
        ResolveStrategy::Event => resolve_event(ctx, loc),
        ResolveStrategy::EventListen => resolve_event_listen(ctx, loc),
        ResolveStrategy::Facade => resolve_facade(ctx, loc),
        ResolveStrategy::Accessor => resolve_accessor(ctx, loc),
        ResolveStrategy::Handler => resolve_handler(ctx, loc),
        ResolveStrategy::VariableType => resolve_variable_type(ctx, loc),
    }
}

/// Variable-type resolution: `$svc->method()` / `$this->prop->method()` → `Type::method`.
///
/// Type sources (recorded by P2):
/// * `$var` —— the **param type hint** of the owning method (common DI style in ThinkPHP controllers / services);
/// * `$this->prop` —— constructor injection (`__construct(T $x){ $this->p = $x; }`).
///
/// When the type can't be found, return unknown —
fn resolve_variable_type(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let Some(recv) = loc.receiver.as_deref() else {
        return Resolution::unknown("无接收者".to_string());
    };
    let method = loc.method.as_deref().unwrap_or("");
    if method.is_empty() {
        return Resolution::unknown("无方法名".to_string());
    }
    let type_fqn = receiver_type_fqn(ctx, &loc.owner_fqn, recv, loc.sub);
    let Some(type_fqn) = type_fqn else {
        return Resolution::unknown(format!("接收者 {recv} 的类型未知"));
    };

    // Target method: first this class, then walk up the inheritance chain (methods are often inherited from a base class).
    // Member separator is language-dependent: PHP `Class::method`, Java `Class.method` — try both.
    let mut stack = vec![type_fqn.clone()];
    let mut visited: HashSet<String> = HashSet::new();
    let mut steps = 0;
    while let Some(t) = stack.pop() {
        steps += 1;
        if steps > 50 || !visited.insert(t.clone()) {
            continue;
        }
        let candidates = [
            format!("{t}::{method}"),
            format!("{t}.{method}"),
        ];
        if let Some(id) = candidates.iter().find_map(|f| ctx.ws.find_by_name(f)) {
            classify_db_action(ctx, loc, &type_fqn, method);
            return Resolution::resolved(
                ResolveTier::Convention,
                id,
                format!("按变量类型解析 {} -> {method}", t),
            );
        }
        for parent in ctx.ws.parents_of(&t) {
            stack.push(parent);
        }
    }
    let magic = resolve_magic_delegation(ctx, loc, &type_fqn, method);
    if magic.is_none() && is_db_verb(ctx, loc, method).is_some() {
        classify_db_action(ctx, loc, &type_fqn, method);
    }
    // The member separator in the message is this language's, so a Java / JS sub-project does not get a
    // PHP-shaped `Class::method` string.
    let member_sep = ctx.lang_policy_for_sub(loc.sub).member_separator.clone();
    magic.unwrap_or_else(|| {
        Resolution::unknown(format!("{type_fqn}{member_sep}{method} 未找到"))
    })
}

/// Take the "class part" of a method FQN: `app\model\User::login` → `app\model\User`.
/// The class / member separator is this language's (`::` for PHP, `.` for Java / JS), never assumed.
fn owner_class_fqn(owner_fqn: &str, member_sep: &str) -> String {
    match owner_fqn.rfind(member_sep) {
        Some(i) => owner_fqn[..i].to_string(),
        None => owner_fqn.to_string(),
    }
}

/// Whether the method name hits a read / write verb declared by FKB; if so return the edge kind to lay.
fn is_db_verb(
    ctx: &PipelineContext,
    loc: &Locator,
    method: &str,
) -> Option<(EdgeKind, &'static str)> {
    let spec = loc
        .sub
        .and_then(|s| ctx.db_verbs.get(&s.get()).cloned())
        .or_else(|| ctx.db_verbs_default.clone())
        .filter(|s| !s.write.is_empty() || !s.read.is_empty())?;
    let m = method.to_ascii_lowercase();
    if spec.write.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
        Some((EdgeKind(EdgeKind::WRITES_DB.to_string()), "写"))
    } else if spec.read.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
        Some((EdgeKind(EdgeKind::READS_DB.to_string()), "读"))
    } else {
        None
    }
}

/// Magic-method forwarding: `Services::getList` → `getList` on the type of `Services`' `dao` property.
///
/// Preconditions (all given by FKB / parser, kernel knows no framework):
/// * FKB declares the forwarded property name (`magic_delegation.property`);
/// * this class or its ancestor declares this method name via `@method`;
/// * the forwarded property's type is statically determinable (constructor injection / typed property / assignment inference).
///
/// By the way: if the forwarded method itself is a read / write verb declared by FKB (CRMEB's `$this->save()` forwards via
/// `__call` to `dao->save`; `dao` maps to a table), then treat the forwarding target (`dep`'s type) as the data-model
/// category and lay `WritesDb` / `ReadsDb` directly — otherwise such writes can only fall back to a vague "maps to"
/// (CRMEB's 156 "mappings" almost all come from this).
fn resolve_magic_delegation(
    ctx: &mut PipelineContext,
    loc: &Locator,
    type_fqn: &str,
    method: &str,
) -> Option<Resolution> {
    let spec = loc
        .sub
        .and_then(|s| ctx.magic_delegation.get(&s.get()).cloned())
        .or_else(|| ctx.magic_delegation_default.clone())?;
    if spec.property.is_empty() || !ctx.ws.declares_magic_method(type_fqn, method) {
        return None;
    }
    // The forwarded property's type is taken by the **concrete receiver type** (a subclass's constructor injects its own Dao).
    let mut cur = Some(type_fqn.to_string());
    let mut visited: HashSet<String> = HashSet::new();
    let mut steps = 0;
    while let Some(c) = cur {
        steps += 1;
        if steps > 50 || !visited.insert(c.clone()) {
            break;
        }
        if let Some(dep) = ctx.ws.prop_type(&c, &spec.property) {
            let dep = dep.to_string();
            // If the forwarded method is a read / write verb, treat `dep` (its real type, mapping to a table) as the data-model category.
            if is_db_verb(ctx, loc, method).is_some() {
                classify_db_action(ctx, loc, &dep, method);
            }
            // The target method also walks up the inheritance chain (Dao methods are mostly defined in `BaseDao`).
            let mut stack = vec![dep.clone()];
            let mut seen: HashSet<String> = HashSet::new();
            let mut n = 0;
            while let Some(t) = stack.pop() {
                n += 1;
                if n > 50 || !seen.insert(t.clone()) {
                    continue;
                }
                for cand in [format!("{t}::{method}"), format!("{t}.{method}")] {
                    if let Some(id) = ctx.ws.find_by_name(&cand) {
                        return Some(Resolution {
                            candidates: vec![id],
                            tier: ResolveTier::Convention,
                            confidence: spec.confidence,
                            evidence: format!(
                                "魔法方法转发 {type_fqn}::{method} → {dep}::{method}（经 {}）",
                                spec.property
                            ),
                        });
                    }
                }
                for p in ctx.ws.parents_of(&t) {
                    stack.push(p);
                }
            }
        }
        cur = ctx.ws.parents_of(&c).into_iter().next();
    }
    None
}

/// Data-model read / write action classification (`$model->save()` → `WritesDb → goods`).
///
/// All three inputs are "knowledge" not hard-coding:
/// * the verb list comes from FKB (`db_verbs`, ThinkPHP / Laravel Model & Query APIs each declare their own);
/// * "this type is a data model" is proven by a **`MapsTo` edge** (the static identity produced by P5's model-convention rules);
/// * both candidate type sources are tried: the `type_fqn` from variable-type resolution (e.g. when `$this->where()` chain end can't be inferred, fall back to the caller's `owner` class — `$this->find()` inside a model method is exactly this).
///
/// Directly lay a `method → table` action edge and register the propagation seed — P8 will carry it along the `Calls` chain to
/// the controller / entry, so the route view shows "writes DB / reads DB" instead of a vague "maps to".
fn classify_db_action(
    ctx: &mut PipelineContext,
    loc: &Locator,
    type_fqn: &str,
    method: &str,
) {
    let Some((kind, _)) = is_db_verb(ctx, loc, method) else {
        return;
    };
    let member_sep = ctx.lang_policy_for_sub(loc.sub).member_separator.clone();
    let mut candidates: Vec<String> = vec![
        type_fqn.to_string(),
        owner_class_fqn(&loc.owner_fqn, &member_sep),
    ];
    if let Some(spec) = loc
        .sub
        .and_then(|s| ctx.magic_delegation.get(&s.get()).cloned())
        .or_else(|| ctx.magic_delegation_default.clone())
    {
        if !spec.property.is_empty() {
            for t in [
                type_fqn.to_string(),
                owner_class_fqn(&loc.owner_fqn, &member_sep),
            ] {
                if let Some(dep) = ctx.ws.prop_type(&t, &spec.property) {
                    candidates.push(dep.to_string());
                }
            }
        }
    }
    let mut tried = HashSet::new();
    for t in candidates {
        if !tried.insert(t.clone()) {
            continue;
        }
        for table in ctx.ws.mapped_tables(&t, EdgeKind::MAPS_TO) {
            emit_db_edge(
                ctx,
                loc.owner,
                Some(loc.call_node),
                loc.sub,
                &loc.owner_fqn,
                table,
                &kind,
                method,
                0.85,
                &loc_line(loc),
            );
        }
        if ctx.ws.mapped_tables(&t, EdgeKind::MAPS_TO).is_empty() {
            let mut subtype_tables: Vec<NodeId> = Vec::new();
            let mut seen_tbl: HashSet<i64> = HashSet::new();
            for sub in ctx.ws.subtypes_bfs(&t, 6, 60) {
                for table in ctx.ws.mapped_tables(&sub, EdgeKind::MAPS_TO) {
                    if seen_tbl.insert(table.get()) {
                        subtype_tables.push(table);
                    }
                }
            }
            if subtype_tables.len() <= 6 {
                for table in subtype_tables {
                    emit_db_edge(
                        ctx,
                        loc.owner,
                        Some(loc.call_node),
                        loc.sub,
                        &loc.owner_fqn,
                        table,
                        &kind,
                        method,
                        0.7,
                        &loc_line(loc),
                    );
                }
            }
        }
    }
}

/// Facade chained write / read: `Db::name('goods')->insert()` / `Db::name('goods')->where()->find()`.
///
/// The terminal verb (`insert` / `find`) lands on a Query with no type annotation, variable-type resolution can't infer the table;
/// but `name('goods')` / `table('goods')` inside the chain has already propagated the table name into `CallRecord.db_table`
/// (the parser walks back along the object chain). Here we directly use that table name to look up the Table node synthesized by P5,
/// laying the verb as `WritesDb` / `ReadsDb` — otherwise such entries can only fall back to a vague "maps to".
fn classify_facade_db_calls(ctx: &mut PipelineContext) {
    // Collect first (read-only) then lay edges (mutable), avoiding the borrow conflict of "iterating `calls` while mutating `ctx`".
    // `table_id` is `Copy`, collected straight into a vec, no re-lookup needed at edge-laying time.
    let mut pending: Vec<(
        NodeId,
        Option<NodeId>,
        Option<SubProjectId>,
        String,
        String,
        EdgeKind,
        NodeId,
        String,
    )> = Vec::new();
    for call in ctx.ws.calls.iter() {
        let (Some(table_name), Some(method)) = (&call.db_table, call.method.as_deref()) else {
            continue;
        };
        let spec = call
            .sub
            .and_then(|s| ctx.db_verbs.get(&s.get()).cloned())
            .or_else(|| ctx.db_verbs_default.clone())
            .filter(|s| !s.write.is_empty() || !s.read.is_empty());
        let Some(spec) = spec else {
            continue;
        };
        let m = method.to_ascii_lowercase();
        let kind = if spec.write.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
            EdgeKind(EdgeKind::WRITES_DB.to_string())
        } else if spec.read.iter().any(|v| v.eq_ignore_ascii_case(&m)) {
            EdgeKind(EdgeKind::READS_DB.to_string())
        } else {
            continue;
        };
        let Some(table_id) = ctx.ws.find_table_by_name(table_name) else {
            continue;
        };
        pending.push((
            call.owner,
            Some(call.node),
            call.sub,
            call.owner_fqn.clone(),
            method.to_string(),
            kind,
            table_id,
            call_line(call),
        ));
    }
    for (owner, call_node, sub, owner_fqn, method, kind, table_id, location) in pending {
        emit_db_edge(
            ctx,
            owner,
            call_node,
            sub,
            &owner_fqn,
            table_id,
            &kind,
            &method,
            0.9,
            &location,
        );
    }
}

/// The call-site fact's `file:line` (for `evidence.location`).
fn call_line(call: &crate::workspace::CallRecord) -> String {
    format!("{}:{}", call.file, call.span.start_line)
}

/// The resolution point's `file:line` (same as above, for `classify_db_action`).
fn loc_line(loc: &Locator) -> String {
    format!("{}:{}", loc.file, loc.line)
}

/// Lay a `method → table` read / write action edge and register the P8 propagation seed (dedup handled by `add_edge`).
///
/// `location` (`file:line`) must be written into `evidence.location`: the view relies on it to give "where this link accesses the resource"
/// (`view_service::inline_chain_locations` fills `to_call_site`, `edge_evidence` takes the evidence location, both read it). Before, only a text scap evid was stored here, shape mismatched,
/// so the facade-chained `Db::name('x')->update()` edge was forever a dead edge you can't jump from.
fn emit_db_edge(
    ctx: &mut PipelineContext,
    owner: NodeId,
    // The call site that triggered this edge (`CallSite`) — tag it `db-query` when laying the edge.
    call_node: Option<NodeId>,
    sub: Option<SubProjectId>,
    owner_fqn: &str,
    table_id: NodeId,
    kind: &EdgeKind,
    method: &str,
    confidence: f32,
    location: &str,
) {
    let added = ctx.ws.add_edge(NewEdge {
        project_id: ctx.project.id,
        kind: kind.clone(),
        from_id: owner,
        to_id: table_id,
        phase: Phase(Phase::RESOLVE.to_string()),
        confidence,
        properties: serde_json::json!({
            "evidence": {
                "rule": "db-verb-classify",
                "location": location,
                "evidence": format!("{} 调用 {}（{} 动词）", owner_fqn, method, kind_label(kind)),
            },
            "verb": method,
        }),
    });
    if added {
        ctx.propagation_seeds.push(crate::context::PropSeed {
            source: owner,
            target: table_id,
            kind: kind.0.clone(),
            confidence,
            sub,
            phase: Phase(Phase::RESOLVE.to_string()),
        });
        if let Some(node_id) = call_node {
            ctx.ws.annotate(NewAnnotation {
                node_id,
                channel: AnnotationChannel(DbQuery::CHANNEL.to_string()),
                // Reads / writes are split into two annotations: in-loop "read per row" and "write per row" are two different
                // fixes (batch read vs batch write), and the rule copy differs, so each stands as its own item.
                kind: if kind.0 == EdgeKind::WRITES_DB {
                    DbQuery::WRITE.to_string()
                } else {
                    DbQuery::READ.to_string()
                },
                subkind: Some(method.to_string()),
                confidence,
                evidence: serde_json::json!({
                    "rule": "db-verb-classify",
                    "location": location,
                    "verb": method,
                    "action": kind_label(kind),
                }),
                phase: Phase(Phase::RESOLVE.to_string()),
                merge: MergeStrategy::Coexist,
            });
        }
    }
}

/// Call-site-level "this is a DB read / write" annotation.
///
/// Only call sites that **actually laid a `ReadsDb` / `WritesDb` edge** get tagged — the criterion reuses
/// P7's existing verb classification, not a second verb list written in the parser (otherwise Laravel / ThinkPHP each write one, and graph facts vs rule criteria would necessarily drift).
struct DbQuery;

impl DbQuery {
    const CHANNEL: &'static str = "DbQuery";
    /// One read (`ReadsDb`).
    const READ: &'static str = "db-query";
    /// One write (`WritesDb`).
    const WRITE: &'static str = "db-write";
}

/// Human-readable labels for `WritesDb` / `ReadsDb` (only for edge-evidence copy).
fn kind_label(kind: &EdgeKind) -> &'static str {
    match kind.0.as_str() {
        EdgeKind::WRITES_DB => "写",
        EdgeKind::READS_DB => "读",
        _ => "",
    }
}

/// Container resolution: L1 literal → L2 registry → L4 convention → L6 intersect with the class universe.
fn resolve_container(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    // The namespace separator is this sub-project language's (`\` for PHP, `.` for Java / JS …), never assumed:
    // a container id is written in the project's own notation, so hard-coding `\` silently resolves nothing for
    // every other stack.
    let ns = ctx.lang_policy_for_sub(loc.sub).ns_separators.clone();
    let raw = loc.raw.trim_start_matches(|c: char| ns.contains(&c));

    // L1: literal FQN
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Resolution::resolved(ResolveTier::Exact, id, format!("字面 FQN {raw}"));
    }
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Exact, id, format!("短名解析 {raw} → {fqn}"));
        }
    }

    // L2: container registry (chained references converge via the outer fixed point)
    if let Some(Value::Object(entry)) = ctx.ws.get_symbol("container_bindings", raw).cloned() {
        let target = entry
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim_start_matches(|c: char| ns.contains(&c))
            .to_string();
        if !target.is_empty() {
            if let Some(id) = ctx
                .ws
                .find_by_name(&target)
                .or_else(|| {
                    ctx.ws
                        .resolve_name_in_file(Some(&loc.file), &target)
                        .and_then(|f| ctx.ws.find_by_name(&f))
                })
            {
                return Resolution::resolved(
                    ResolveTier::Registry,
                    id,
                    // The registry file name is whatever FKB declared (`provider.php` on ThinkPHP, a service
                    // container config elsewhere) — the kernel only knows "a container binding".
                    format!("容器绑定 {raw} → {target}"),
                );
            }
            return Resolution::unknown(format!("间接绑定 {raw} → {target}"));
        }
        return Resolution::unknown(format!("闭包绑定 {raw}"));
    }

    // L4: convention (FQN string) — "looks qualified" means it contains **this language's** namespace separator.
    if raw.contains(|c: char| ns.contains(&c)) {
        if let Some(id) = ctx.ws.find_by_name(raw) {
            return Resolution::resolved(ResolveTier::Convention, id, format!("约定 {raw}"));
        }
    }

    // L6: intersect with the class short-name universe
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Intersection, id, format!("与类全集求交 {raw}"));
        }
    }

    Resolution::unknown(format!("容器无法解析 {raw}"))
}

/// Event trigger: query the `by_alias(event_name, x)` registered by P6.
fn resolve_event(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventTriggerUnresolved",
                Severity::Warning,
                format!(
                    "event('{}') 未找到注册的事件节点 —— 若事件注册表中该键为空数组，\
                     监听器会被整体误判为死代码",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// Runtime event-listen registration: `Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`.
///
/// arg0 (event name) reuses the `event_name` alias index to resolve to an event node (same set as `event('x')`),
/// the resolution result is used in `apply_resolution` to point the `HandledBy` edge from the event node to arg1's listener class.
fn resolve_event_listen(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventListenUnresolved",
                Severity::Warning,
                format!(
                    "Event::listen('{}', …) 未找到对应事件节点 —— 该事件未在事件注册表注册，\
                     监听器仍可经 `listener` 标注识别，但无法精确挂到具体事件",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// Resolve a class literal into an in-graph class node: PHP `Foo::class` / `\App\X`, Java `Foo.class`.
/// Both the namespace separator and the class-literal suffix come from this sub-project's language.
fn resolve_class_node(
    ctx: &PipelineContext,
    raw: &str,
    file: Option<&str>,
    sub: Option<SubProjectId>,
) -> Option<NodeId> {
    let policy = ctx.lang_policy_for_sub(sub);
    let mut s = raw
        .trim_start_matches(|c: char| policy.ns_separators.contains(&c))
        .to_string();
    // `Foo::class` (PHP) / `Foo.class` (Java): the suffix is `<member separator>class`. Only applied when the
    // language actually declares one — with an empty separator the suffix would degenerate to a bare `class`
    // and bite off the tail of any name ending in it.
    if !policy.member_separator.is_empty() {
        let class_literal = format!("{}class", policy.member_separator);
        if let Some(stripped) = s.strip_suffix(&class_literal) {
            s = stripped.to_string();
        }
    }
    ctx.ws.find_by_name(&s).or_else(|| {
        ctx.ws
            .resolve_name_in_file(file, &s)
            .and_then(|f| ctx.ws.find_by_name(&f))
    })
}

/// Facade: query the FacadeMap loaded by P3.
fn resolve_facade(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let ns = ctx.lang_policy_for_sub(loc.sub).ns_separators.clone();
    let receiver = loc.raw.trim_start_matches(|c: char| ns.contains(&c));
    let Some(entry) = ctx.ws.get_symbol("facade_map", receiver).cloned() else {
        return Resolution::unknown(format!("非门面调用 {receiver}"));
    };
    let target = entry
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match ctx.ws.find_by_name(&target) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("门面 {receiver} → {target}")),
        None => {
            // vendor is excluded by P0, the target not being in the graph is **expected**, must not be reported as an error
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "AliasTargetMissing",
                Severity::Info,
                format!("门面目标 {target} 不在图内（ExcludedByIngest）"),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("门面目标 {target} 缺失"))
        }
    }
}

/// Getter: `$order->status_text` → `getStatusTextAttr` (composite key, avoids name collision).
fn resolve_accessor(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("accessor", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("获取器 {}", loc.raw)),
        None => Resolution::unknown(format!("获取器 {} 未注册", loc.raw)),
    }
}

/// Route handler: `'Login/appleLogin'` → `app\api\controller\Login::appleLogin`.
fn resolve_handler(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match resolve_handler_target(ctx, &loc.raw, &loc.file, loc.sub, None) {
        Some((id, note)) => Resolution::resolved(
            ResolveTier::Convention,
            id,
            format!("handler {}（{}）", loc.raw, note),
        ),
        None => Resolution::unknown(format!("无法解析 handler {}", loc.raw)),
    }
}

/// Resolve a route handler, returning (target node, hit explanation).
///
/// Key: **fall back to the class when the method doesn't exist**. Controller methods are often inherited from a base class,
/// requiring `Controller::method` to always exist would break the whole route chain.
///
/// `method` is an **entry method additionally given by framework knowledge** (e.g. the `index` / `delete` expanded from a resource route,
/// see `PendingLink.method`). When the handler string already carries a method name, prefer that (it's explicit in source),
/// only use the externally-given one when the string has only a class name (resource route).
pub fn resolve_handler_target(
    ctx: &PipelineContext,
    raw: &str,
    file: &str,
    sub: Option<gt_domain::model::SubProjectId>,
    method: Option<&str>,
) -> Option<(NodeId, &'static str)> {
    // The handler's shape (separator / class-name template / app segment) is all declared by FKB — kernel knows no framework.
    let spec = sub
        .and_then(|s| ctx.method_ref_specs.get(&s.get()).cloned())
        .or_else(|| ctx.method_ref_spec_default.clone())
        .unwrap_or_default();
    // Namespace / member separator comes from the language strategy (PHP `\` + `::`, Java `.` + `.`)
    let policy = ctx.lang_policy_for_sub(sub).clone();

    // `policy` already carries this language's separators — use it here too (trimming with a hard-coded `\`
    // would silently miss a fully qualified name on every other stack).
    let raw = raw.trim_start_matches(|c: char| policy.ns_separators.contains(&c));
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Some((id, "完全限定名直接命中"));
    }

    // ① Split controller / method by the FKB-declared separator.
    //    Only use the FKB-additionally-given entry method (resource route's `expand_entry`) when no method name can be split out.
    let (controller_part, inline_method) = split_handler(raw, &spec.method_separators);
    let method = if inline_method.is_empty() {
        method.unwrap_or("").to_string()
    } else {
        inline_method
    };

    // ② controller-internal hierarchy chars → namespace separator
    //    e.g. ThinkPHP's `v1.agent.AgentManage` → `v1\agent\AgentManage`
    let mut controller = controller_part.to_string();
    if let Some(ns_sep_char) = policy.ns_separator {
        let ns_sep = ns_sep_char.to_string();
        for sep in &spec.hierarchy_separators {
            if !sep.is_empty() {
                controller = controller.replace(sep.as_str(), &ns_sep);
            }
        }
    }

    // ③ Resolve the controller class without assuming any controller *directory name*.
    //    We only know: the stack's root namespaces (from the tech-stack manifest) + the app module (inferred from
    //    the route file path, plus any framework-declared extras, plus the single-app shape "") + the structural fact that the
    //    controller sits `controller_layer_depth` namespace segments below the module. The actual directory name
    //    is learned from the real class FQNs on the graph (`resolve_controller`), so `controller` / `Http/Controllers`
    //    / anything custom all work.
    let app_seg = route_app_segment(file, &spec);
    let mut module_candidates: Vec<String> = vec![app_seg];
    module_candidates.extend(spec.app_segments.iter().cloned());
    module_candidates.push(String::new()); // single-app shape
    let mut seen = std::collections::HashSet::new();
    let module_candidates: Vec<&str> = module_candidates
        .iter()
        .filter(|m| seen.insert(m.as_str()))
        .map(|m| m.as_str())
        .collect();

    let class_fqn = module_candidates.iter().find_map(|m| {
        ctx.ws.resolve_controller(
            &controller,
            &spec.root_namespaces,
            m,
            spec.controller_layer_depth,
            policy.ns_separator,
        )
    });

    // ④ method first, then fall back to class (controller methods often inherit from base, requiring method existence breaks the chain)
    if let Some(fqn) = &class_fqn {
        if !method.is_empty() {
            if let Some(id) = ctx.ws.find_by_name(&policy.join_member(fqn, &method)) {
                return Some((id, "方法精确命中"));
            }
        }
        if let Some(id) = ctx.ws.find_by_name(fqn) {
            return Some((id, "类命中（方法未在图内，可能继承自基类）"));
        }
    }

    // ⑤ short-name fallback: restore by the import in the route file
    let short = controller
        .rsplit(|c: char| policy.ns_separators.contains(&c))
        .next()
        .unwrap_or(&controller);
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(file), short) {
        if !method.is_empty() {
            if let Some(id) = ctx.ws.find_by_name(&policy.join_member(&fqn, &method)) {
                return Some((id, "短名方法命中"));
            }
        }
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Some((id, "短名类命中"));
        }
    }
    None
}

/// Try separators in the **order declared by FKB** to split controller and method.
///
/// ThinkPHP uses `/`, Laravel and Symfony use `@` or `::`, Rails uses `#` — all supported by this.
fn split_handler(raw: &str, seps: &[String]) -> (String, String) {
    for s in seps {
        if s.is_empty() {
            continue;
        }
        if let Some((c, m)) = raw.split_once(s.as_str()) {
            return (c.to_string(), m.to_string());
        }
    }
    (raw.to_string(), String::new())
}

/// Infer the app segment from the route file path: `app/api/route/pc.php` + anchor `route` → `api`.
fn route_app_segment(file: &str, spec: &MethodRefSpec) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    if let Some(anchor) = &spec.app_anchor_dir {
        if let Some(pos) = parts.iter().position(|p| *p == anchor.as_str()) {
            if pos > 0 {
                return parts[pos - 1].to_string();
            }
        }
    }
    if !spec.app_fallback.is_empty() {
        return spec.app_fallback.clone();
    }
    parts.first().copied().unwrap_or("").to_string()
}

/// Turn one dynamic resolution into a **locatable** `evidence` object (same shape as the FKB rule edges in `engine.rs`).
///
/// The key is `location`: the view layer reads `properties.evidence.location` for "which line this edge was built on"
/// (`view_service::inline_chain_locations` fills `to_call_site`, `edge_evidence` takes the evidence location, both read it). Before, only `res.evidence` — **one string** — was stored here, shape mismatched, so
/// semantic edges like `Triggers` / `HandledBy` could never get a location — the drawer only had "no per-hop evidence to inspect",
/// when in fact that line (e.g. the call site of `event('X')`) was already in the graph.
fn evidence_of(loc: &Locator, res: &Resolution) -> serde_json::Value {
    serde_json::json!({
        "rule": format!("{:?}", loc.strategy),
        "evidence": res.evidence.clone(),
        "location": format!("{}:{}", loc.file, loc.line),
    })
}

fn apply_resolution(ctx: &mut PipelineContext, loc: &Locator, res: &Resolution, phase: &Phase) {
    // Runtime event-listen registration: point the event node resolved from arg0 to arg1's listener class via a `HandledBy` edge.
    // Note: this is not "trigger", so it doesn't take the default owner→target (Triggers) edge.
    if loc.strategy == ResolveStrategy::EventListen {
        if let Some(event_id) = res.candidates.first() {
            match loc.consumer.as_deref() {
                Some(consumer) => {
                    if let Some(listener_id) =
                        resolve_class_node(ctx, consumer, Some(&loc.file), loc.sub)
                    {
                        ctx.ws.add_edge(NewEdge {
                            project_id: ctx.project.id,
                            kind: EdgeKind(EdgeKind::HANDLED_BY.to_string()),
                            from_id: *event_id,
                            to_id: listener_id,
                            phase: phase.clone(),
                            confidence: res.confidence,
                            properties: serde_json::json!({
                                "tier": format!("{:?}", res.tier),
                                "evidence": evidence_of(loc, res),
                                "via": "Event::listen/subscribe 运行时注册",
                            }),
                        });
                    } else {
                        ctx.ws.diagnose(
                            phase,
                            "EventListenTargetMissing",
                            Severity::Warning,
                            format!("Event::listen 监听器 {} 不在图内", consumer),
                            Some(format!("{}:{}", loc.file, loc.line)),
                        );
                    }
                }
                None => {
                    ctx.ws.diagnose(
                        phase,
                        "EventListenNoConsumer",
                        Severity::Info,
                        format!("Event::listen('{}') 缺少监听器参数", loc.raw),
                        Some(format!("{}:{}", loc.file, loc.line)),
                    );
                }
            }
        }
        return;
    }
    let kind = match loc.strategy {
        ResolveStrategy::Event => EdgeKind(EdgeKind::TRIGGERS.to_string()),
        ResolveStrategy::Handler => EdgeKind(EdgeKind::HANDLED_BY.to_string()),
        // Instance-method call: caller → target method, a real "call" edge.
        ResolveStrategy::VariableType => EdgeKind(EdgeKind::CALLS.to_string()),
        _ => EdgeKind(EdgeKind::RESOLVES_TO.to_string()),
    };
    for target in &res.candidates {
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: kind.clone(),
            from_id: loc.owner,
            to_id: *target,
            phase: phase.clone(),
            confidence: res.confidence,
            properties: serde_json::json!({
                "tier": format!("{:?}", res.tier),
                "evidence": evidence_of(loc, res),
            }),
        });
    }
    if res.candidates.is_empty()
        && !matches!(
            loc.strategy,
            ResolveStrategy::Facade | ResolveStrategy::VariableType
        )
    {
        ctx.ws.diagnose(
            phase,
            "UnresolvedLink",
            Severity::Info,
            format!("{:?} 解析失败: {}", loc.strategy, res.evidence),
            Some(format!("{}:{}", loc.file, loc.line)),
        );
    }
}

/// Resolve the pending links left by P5 (route handlers / event listeners, etc.).
fn resolve_pending_links(ctx: &mut PipelineContext, phase: &Phase) {
    let pending = std::mem::take(&mut ctx.ws.pending_links);
    for link in &pending {
        let target = match link.resolve {
            gt_domain::model::ResolveAs::MethodRef => {
                let (file, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                resolve_handler_target(ctx, &link.raw, file, link.sub, link.method.as_deref())
                    .map(|(id, _)| id)
                    .or_else(|| {
                        ctx.ws
                            .resolve_name_in_file(Some(file), &link.raw)
                            .and_then(|f| ctx.ws.find_by_name(&f))
                    })
            }
            _ => {
                let (f, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                ctx.ws.find_by_name(&link.raw).or_else(|| {
                    ctx.ws
                        .resolve_name_in_file(Some(f), &link.raw)
                        .and_then(|x| ctx.ws.find_by_name(&x))
                })
            }
        };
        match target {
            Some(t) => {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: link.kind.clone(),
                    from_id: link.from,
                    to_id: t,
                    phase: phase.clone(),
                    confidence: link.confidence,
                    properties: Value::Null,
                });
            }
            None => {
                ctx.ws.diagnose(
                    phase,
                    "UnresolvedLink",
                    Severity::Warning,
                    format!(
                        "{} 指向 {}，但图中找不到目标 —— 路由指向不存在的 handler 会导致运行时 500",
                        link.kind, link.raw
                    ),
                    Some(link.file.clone()),
                );
            }
        }
    }
}

fn fact_to_string(v: &FactValue) -> String {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

/// Generic call resolution: build `Calls` edges for statically-determinable calls (caller method → target method / function).
///
/// Why it must be done: `ReadsConfig` / `ReadsCache` / `MapsTo` and other semantic edges hang on the **callee's node**;
/// if there's no edge between caller and callee, the whole "route → cache / config / table" link is broken on the graph,
/// and the view can only detour via structural edges like `Declares`/`HasCallSite`, eventually pulling in unrelated nodes.
///
/// Only resolve statically-determinable calls, to avoid wrong connections:
/// * `Class::method` (static / facade call): receiver is a class name → resolve the class short name then join `Fqn::method`;
/// * free function `foo()` (excluding `new Foo`) → resolve by name / short name;
/// * variable receiver (`$this->x`, `$obj->y`) needs type inference, **not resolved** (better missing edge than wrong edge).
fn resolve_calls(ctx: &mut PipelineContext, phase: &Phase) {
    let calls = ctx.ws.calls.clone();
    let mut seen: HashSet<(i64, i64)> = HashSet::new();
    let mut added = 0usize;
    for call in &calls {
        if let Some(target) = resolve_call_target(ctx, call) {
            if target != call.owner && seen.insert((call.owner.get(), target.get())) {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: EdgeKind(EdgeKind::CALLS.to_string()),
                    from_id: call.owner,
                    to_id: target,
                    phase: phase.clone(),
                    confidence: 1.0,
                    properties: serde_json::json!({
                        "callee": call.callee,
                        // Precisely record the CallSite node id this call corresponds to, so the view layer directly gets "call site" from it,
                        // no need to heuristically match by callee name.
                        "call_site": call.node.get(),
                    }),
                });
                added += 1;
            }
        }
        if let Some(recv) = call.receiver.as_deref() {
            if let Some(type_fqn) = receiver_type_fqn(ctx, &call.owner_fqn, recv, call.sub) {
                let cid = ctx.ws.find_by_name(&type_fqn).or_else(|| {
                    ctx.ws
                        .resolve_name_at(call.owner, &type_fqn)
                        .and_then(|f| ctx.ws.find_by_name(&f))
                });
                if let Some(cid) = cid {
                    if cid != call.owner && seen.insert((call.owner.get(), cid.get())) {
                        ctx.ws.add_edge(NewEdge {
                            project_id: ctx.project.id,
                            kind: EdgeKind(EdgeKind::CALLS.to_string()),
                            from_id: call.owner,
                            to_id: cid,
                            phase: phase.clone(),
                            confidence: 0.7,
                            properties: serde_json::json!({
                                "via": "receiver_type",
                                "callee": call.callee,
                                "call_site": call.node.get(),
                            }),
                        });
                        added += 1;
                    }
                }
            }
        }
    }
    info!("P7 call-chain resolution: {} Calls edges", added);
}

/// Resolve the call receiver's type FQN:
/// * `$this->prop` → property type (walk back up the inheritance chain, if `WechatServices` not found look at the parent);
/// * `$var` → the param type of the owning method.
fn receiver_type_fqn(
    ctx: &PipelineContext,
    owner_fqn: &str,
    recv: &str,
    sub: Option<SubProjectId>,
) -> Option<String> {
    // `Class::method` / `Class.method` — the member separator comes from this sub-project's language, as does
    // the namespace separator trimmed below. Hard-coding PHP's notation silently resolves nothing elsewhere.
    let policy = ctx.lang_policy_for_sub(sub);
    let member_sep = policy.member_separator.clone();
    let ns = policy.ns_separators.clone();
    if recv == "$this" {
        let class_fqn = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
        return if class_fqn.is_empty() {
            None
        } else {
            Some(class_fqn.to_string())
        };
    }
    if recv == "static" || recv == "self" {
        let class_fqn = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
        return if class_fqn.is_empty() {
            None
        } else {
            Some(class_fqn.to_string())
        };
    }
    // `parent::method()`: the receiver type is "the direct parent of the class defining this method", letting the
    // `Model --MapsTo--> Table` class-level semantic edge surface along the call chain (paired with the target resolution above).
    if recv == "parent" {
        let class_fqn = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
        if !class_fqn.is_empty() {
            if let Some(p) = ctx.ws.parents_of(class_fqn).into_iter().next() {
                return Some(p);
            }
        }
        return None;
    }
    if let Some(inner) = recv.strip_prefix("(new ") {
        let class = inner
            .trim_end_matches(')')
            .trim()
            .trim_start_matches(|c: char| ns.contains(&c));
        if class == "static" || class == "self" || class.is_empty() {
            let c = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
            return if c.is_empty() { None } else { Some(c.to_string()) };
        }
        if !class.is_empty() && ctx.ws.find_by_name(class).is_some() {
            return Some(class.to_string());
        }
        if !class.is_empty() {
            if let Some(owner_id) = ctx.ws.find_by_name(owner_fqn) {
                if let Some(fqn) = file_import_of(ctx, owner_id, class) {
                    if ctx.ws.find_by_name(&fqn).is_some() {
                        return Some(fqn);
                    }
                }
                if let Some(fqn) = ctx.ws.resolve_name_at(owner_id, class) {
                    if ctx.ws.find_by_name(&fqn).is_some() {
                        return Some(fqn);
                    }
                }
            }
        }
        let c = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
        return if c.is_empty() { None } else { Some(c.to_string()) };
    }
    if let Some(prop) = recv.strip_prefix("$this->") {
        let class_fqn = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
        let mut cur = Some(class_fqn.to_string());
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(c) = cur {
            if !visited.insert(c.clone()) {
                break;
            }
            if let Some(ty) = ctx.ws.prop_type(&c, prop) {
                return Some(resolve_impl(ctx, &ty));
            }
            cur = ctx.ws.parents_of(&c).into_iter().next();
        }
        None
    } else if recv.starts_with('$') {
        let var = recv.trim_start_matches('$');
        // First by **param type hint** (DI injection), then by in-method assignment inference (`$x = new Y()`).
        let t = ctx
            .ws
            .param_type(owner_fqn, var)
            .or_else(|| ctx.ws.local_type(owner_fqn, var));
        if let Some(s) = t {
            return Some(resolve_impl(ctx, &s));
        }
        if var == "model" {
            let class_fqn = owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
            if !class_fqn.is_empty() {
                return Some(class_fqn.to_string());
            }
        }
        None
    } else {
        if !recv.is_empty() && ctx.ws.find_by_name(recv).is_some() {
            return Some(recv.to_string());
        }
        // Java field (bare identifier): infer from the field type of the owning class (incl. parent)
        // `service.mapper.findX()` where `mapper` is an `@Autowired` injected field.
        let class_fqn = owner_class_of(owner_fqn, &member_sep);
        let mut cur = Some(class_fqn);
        let mut visited: HashSet<String> = HashSet::new();
        while let Some(c) = cur {
            if !visited.insert(c.clone()) {
                break;
            }
            if let Some(ty) = ctx.ws.prop_type(&c, recv) {
                return Some(resolve_impl(ctx, &ty));
            }
            cur = ctx.ws.parents_of(&c).into_iter().next();
        }
        // Generic dotted-receiver head reduction: `ClassName.objects` -> `ClassName`, used by Django ORM
        // (`Model.objects.create(...)`) and any stack where a receiver is a class followed by a static /
        // manager accessor that is not a typed field. The full dotted receiver is not a known type/field,
        // so fall back to its head (resolved via imports). Language-agnostic: it only triggers when the
        // whole receiver failed to resolve, never on a bare field.
        if let Some(pos) = recv.rfind(member_sep.as_str()) {
            let head = &recv[..pos];
            if !head.is_empty() {
                if ctx.ws.find_by_name(head).is_some() {
                    return Some(head.to_string());
                }
                for scope in [owner_fqn.to_string(), owner_class_of(owner_fqn, &member_sep)] {
                    if let Some(id) = ctx.ws.find_by_name(&scope) {
                        if let Some(fqn) = file_import_of(ctx, id, head) {
                            if ctx.ws.find_by_name(&fqn).is_some() {
                                return Some(fqn);
                            }
                        }
                        if let Some(fqn) = ctx.ws.resolve_name_at(id, head) {
                            if ctx.ws.find_by_name(&fqn).is_some() {
                                return Some(fqn);
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// If `type_fqn` is an interface and an impl class exists, return the impl class's FQN: Spring injects the impl class,
/// and the DB call is in the impl class's method body; otherwise return as-is.
///
/// Not resolving to the impl class breaks the chain: route → service interface (`@Autowired` field type) → impl-class method
/// (where `mapper.xxx()` really is) → Mapper → table.
fn resolve_impl(ctx: &PipelineContext, type_fqn: &str) -> String {
    if let Some(id) = ctx.ws.find_by_name(type_fqn) {
        if ctx
            .ws
            .node(id)
            .map(|n| n.kind.is("Interface"))
            .unwrap_or(false)
        {
            if let Some(rec) = ctx
                .ws
                .inherits
                .iter()
                .find(|r| r.base == type_fqn && r.kind.as_str() == "Implements")
            {
                return rec.child_fqn.clone();
            }
        }
    }
    type_fqn.to_string()
}

/// From a "method FQN" take the owning class FQN: Java `pkg.Class.method` → `pkg.Class`,
/// PHP `Class::method` → `Class`. Which separator separates them is this language's, never assumed.
fn owner_class_of(owner_fqn: &str, member_sep: &str) -> String {
    owner_fqn
        .rsplit_once(member_sep)
        .map(|(c, _)| c.to_string())
        .unwrap_or_else(|| owner_fqn.to_string())
}

/// Restore a short name to an FQN using **the `use` table of the caller's file**.
///
/// Return `Some(fqn)` means this file really did import this short name — the caller must recognize **only** this FQN,
/// even if it's not in the graph (a framework class), and must not fall back to the global short-name index to guess.
/// Return `None` means this file didn't import it (`new Foo`, global-namespace style, etc.), leave to later fallbacks.
fn file_import_of(ctx: &PipelineContext, owner: NodeId, short: &str) -> Option<String> {
    let file_id = ctx.ws.node(owner)?.file_id?;
    ctx.ws
        .imports_of_file(file_id.get())
        .and_then(|m| m.get(&short.to_ascii_lowercase()))
        .cloned()
}

fn resolve_call_target(ctx: &PipelineContext, call: &CallRecord) -> Option<NodeId> {
    // Notation comes from this call site's language: PHP `Class::method` + `\App\X`, Java `pkg.Class.method`.
    let policy = ctx.lang_policy_for_sub(call.sub);
    let member_sep = policy.member_separator.clone();
    let ns = policy.ns_separators.clone();
    // 1) `Class<member_sep>method` (static / facade): receiver is a class name (not a variable)
    if let Some(recv) = &call.receiver {
        if !recv.starts_with('$') {
            let m = call.method.as_deref().unwrap_or("");
            if m.is_empty() {
                return None;
            }
            let recv = recv.trim_start_matches(|c: char| ns.contains(&c));
            let recv = if recv == "parent" {
                let owner_class = call.owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
                match ctx.ws.parents_of(owner_class).into_iter().next() {
                    Some(p) => p,
                    None => return None,
                }
            } else if recv == "self" || recv == "static" {
                let owner_class = call.owner_fqn.split(member_sep.as_str()).next().unwrap_or("");
                if owner_class.is_empty() {
                    return None;
                }
                owner_class.to_string()
            } else {
                recv.to_string()
            };
            if let Some(id) = ctx.ws.find_by_name(&format!("{recv}::{m}")) {
                return Some(id);
            }
            let recv_fqn = if let Some(fqn) = file_import_of(ctx, call.owner, &recv) {
                fqn
            } else if let Some(fqn) = ctx.ws.resolve_name_at(call.owner, &recv) {
                fqn
            } else {
                recv.clone()
            };
            {
                let mut cur = Some(recv_fqn.clone());
                let mut seen: HashSet<String> = HashSet::new();
                let mut depth = 0usize;
                while let Some(c) = cur {
                    if depth > 20 || !seen.insert(c.clone()) {
                        break;
                    }
                    if let Some(id) = ctx.ws.find_by_name(&format!("{c}::{m}")) {
                        return Some(id);
                    }
                    cur = ctx.ws.parents_of(&c).into_iter().next();
                    depth += 1;
                }
            }
            // Method not in graph (framework method like `User::where`) → connect to **the class itself**,
            // so class-level semantic edges like `Model --MapsTo--> Table` surface along the call chain.
            if let Some(id) = ctx.ws.find_by_name(&recv_fqn) {
                return Some(id);
            }
        }
        return None;
    }
    let callee = call.callee.trim();
    if callee.is_empty() || callee.starts_with("new ") {
        return None;
    }
    if let Some(id) = ctx.ws.find_by_name(callee) {
        if is_function_node(ctx, id) {
            return Some(id);
        }
    }
    let id = ctx
        .ws
        .resolve_name_at(call.owner, callee)
        .and_then(|f| ctx.ws.find_by_name(&f))?;
    if is_function_node(ctx, id) {
        return Some(id);
    }
    None
}

/// Whether it's a free-function node (decides whether a free-function call can connect to it).
fn is_function_node(ctx: &PipelineContext, id: NodeId) -> bool {
    ctx.ws
        .node(id)
        .map(|n| n.kind.as_str() == NodeKind::FUNCTION)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_adapter_fkb::YamlKnowledgeBase;
    use gt_domain::model::{
        DbVerbsSpec, Language, MagicDelegationSpec, NewEdge, NewNode, Project, ProjectConfig,
        ProjectId, ProjectStatus, Span,
    };

    fn new_ctx() -> PipelineContext {
        PipelineContext::new(Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: std::path::PathBuf::from("/t"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        })
    }

    fn add_node(ctx: &mut PipelineContext, kind: &str, name: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(kind.to_string()),
            name: name.to_string(),
            fqn: Some(name.to_string()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("CfAst".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn maps_to(ctx: &mut PipelineContext, class: NodeId, table: NodeId) {
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind(EdgeKind::MAPS_TO.to_string()),
            from_id: class,
            to_id: table,
            phase: Phase("Synthesize".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        });
    }

    fn has_edge(ctx: &PipelineContext, from: NodeId, to: NodeId, kind: &str) -> bool {
        ctx.ws
            .edges()
            .iter()
            .any(|e| e.from_id == from && e.to_id == to && e.kind.as_str() == kind)
    }

    fn locator(owner: NodeId, call_node: NodeId, owner_fqn: &str) -> Locator {
        Locator {
            owner,
            call_node,
            owner_fqn: owner_fqn.to_string(),
            strategy: ResolveStrategy::VariableType,
            raw: String::new(),
            receiver: None,
            method: None,
            consumer: None,
            file: "app/x.php".to_string(),
            line: 10,
            sub: None,
        }
    }

    // ------------------------------------------------------- A. pure helpers

    /// Which separator splits controller from method is **FKB's** call (ThinkPHP `/`, Laravel `@` / `::`,
    /// Rails `#`), tried in the declared order.
    #[test]
    fn split_handler_tries_the_declared_separators_in_order() {
        assert_eq!(
            split_handler("Login/appleLogin", &["/".to_string(), "@".to_string()]),
            ("Login".to_string(), "appleLogin".to_string())
        );
        assert_eq!(
            split_handler("Login@appleLogin", &["@".to_string()]),
            ("Login".to_string(), "appleLogin".to_string())
        );
        // An empty separator must be skipped — splitting on "" would cut at position 0.
        assert_eq!(
            split_handler("Login", &["".to_string(), "/".to_string()]),
            ("Login".to_string(), String::new()),
            "空分隔符应跳过；都不命中时返回 (raw, \"\")"
        );
    }

    fn method_ref(anchor: Option<&str>, fallback: &str) -> MethodRefSpec {
        MethodRefSpec {
            method_separators: vec!["/".to_string()],
            hierarchy_separators: vec![".".to_string()],
            app_segments: Vec::new(),
            root_namespaces: Vec::new(),
            controller_layer_depth: 1,
            app_anchor_dir: anchor.map(|s| s.to_string()),
            app_fallback: fallback.to_string(),
        }
    }

    /// `{app}` comes from the route file path: the directory **one level above** the anchor.
    #[test]
    fn route_app_segment_takes_the_dir_above_the_anchor() {
        assert_eq!(
            route_app_segment("app/api/route/pc.php", &method_ref(Some("route"), "")),
            "api"
        );
        // Anchor at the very front has nothing above it -> fallback.
        assert_eq!(
            route_app_segment("route/pc.php", &method_ref(Some("route"), "admin")),
            "admin"
        );
        // No anchor declared -> fallback.
        assert_eq!(
            route_app_segment("app/api/pc.php", &method_ref(None, "admin")),
            "admin"
        );
        // Neither anchor nor fallback -> the first path segment.
        assert_eq!(
            route_app_segment("app/api/pc.php", &method_ref(None, "")),
            "app"
        );
    }

    #[test]
    fn owner_class_fqn_strips_the_last_member_separator() {
        assert_eq!(owner_class_fqn(r"app\Service::run", "::"), r"app\Service");
        assert_eq!(owner_class_fqn("org.x.Service.run", "."), "org.x.Service");
        assert_eq!(
            owner_class_fqn("plain", "::"),
            "plain",
            "没有分隔符时原样返回"
        );
    }

    // ------------------------------------------------------- B. DB verb vocabulary

    /// The read / write vocabulary comes from FKB (`db_verbs`), is matched case-insensitively, and a stack
    /// that declares nothing is simply not judged.
    #[test]
    fn db_verbs_come_from_fkb_and_are_case_insensitive() {
        let mut ctx = new_ctx();
        ctx.db_verbs_default = Some(DbVerbsSpec {
            read: vec!["select".to_string(), "find".to_string()],
            write: vec!["insert".to_string(), "save".to_string()],
        });
        let owner = add_node(&mut ctx, "Method", r"app\M::run");
        let call = add_node(&mut ctx, "CallSite", "select");
        let loc = locator(owner, call, r"app\M::run");

        assert_eq!(
            is_db_verb(&ctx, &loc, "FiNd").map(|(k, _)| k.as_str().to_string()),
            Some(EdgeKind::READS_DB.to_string())
        );
        assert_eq!(
            is_db_verb(&ctx, &loc, "SAVE").map(|(k, _)| k.as_str().to_string()),
            Some(EdgeKind::WRITES_DB.to_string())
        );
        assert!(
            is_db_verb(&ctx, &loc, "delete").is_none(),
            "未声明的动词不算 DB 动作"
        );

        ctx.db_verbs_default = Some(DbVerbsSpec {
            read: Vec::new(),
            write: Vec::new(),
        });
        assert!(
            is_db_verb(&ctx, &loc, "select").is_none(),
            "空声明不应判定任何东西"
        );
    }

    // ------------------------------------------------------- C. laying the DB action edge

    /// A read verb on a type that `MapsTo` a table lays `ReadsDb`; a method that is not a verb lays nothing.
    #[test]
    fn classify_db_action_lays_the_edge_on_the_mapped_type() {
        let mut ctx = new_ctx();
        ctx.db_verbs_default = Some(DbVerbsSpec {
            read: vec!["select".to_string()],
            write: Vec::new(),
        });
        let table = add_node(&mut ctx, "Table", "user");
        let model = add_node(&mut ctx, "Class", r"app\model\User");
        maps_to(&mut ctx, model, table);

        let owner = add_node(&mut ctx, "Method", r"app\model\User::select");
        let call = add_node(&mut ctx, "CallSite", "select");
        let loc = locator(owner, call, r"app\model\User::select");
        classify_db_action(&mut ctx, &loc, r"app\model\User", "select");
        assert!(
            has_edge(&ctx, owner, table, EdgeKind::READS_DB),
            "映射到表的类型上出现读动词应落 ReadsDb"
        );

        // Not a declared verb -> nothing.
        let mut plain = new_ctx();
        plain.db_verbs_default = Some(DbVerbsSpec {
            read: vec!["select".to_string()],
            write: Vec::new(),
        });
        let t2 = add_node(&mut plain, "Table", "user");
        let m2 = add_node(&mut plain, "Class", r"app\model\User");
        maps_to(&mut plain, m2, t2);
        let o2 = add_node(&mut plain, "Method", r"app\model\User::toArray");
        let c2 = add_node(&mut plain, "CallSite", "toArray");
        let loc2 = locator(o2, c2, r"app\model\User::toArray");
        classify_db_action(&mut plain, &loc2, r"app\model\User", "toArray");
        assert!(
            !has_edge(&plain, o2, t2, EdgeKind::READS_DB),
            "非 DB 动词不应落边"
        );
    }

    // ------------------------------------------------------- D. magic-method delegation

    /// `UserServices` has **no** `MapsTo` of its own; `getList` is an `@method` forwarded by `__call` to
    /// `$this->dao` (a `UserDao`, which does map to the table). Without the FKB-declared
    /// `magic_delegation.property` the "… -> Services -> Dao -> table" chain breaks at the Services hop.
    ///
    /// This declaration lives **only** in `fkb/projects/crmeb.yaml`, whose sole test (`crmeb_pipeline.rs`)
    /// is sample-gated — the real FKB is loaded here so the path has always-on coverage.
    fn delegation_ctx(delegation: Option<MagicDelegationSpec>) -> (PipelineContext, NodeId, NodeId, NodeId) {
        let mut ctx = new_ctx();
        ctx.db_verbs_default = Some(DbVerbsSpec {
            read: vec!["getList".to_string()],
            write: Vec::new(),
        });
        ctx.magic_delegation_default = delegation;
        let table = add_node(&mut ctx, "Table", "user");
        let dao = add_node(&mut ctx, "Class", r"app\dao\UserDao");
        maps_to(&mut ctx, dao, table);
        // The Services class's `dao` property is typed (constructor injection).
        ctx.ws
            .set_prop_type(r"app\services\UserServices", "dao", r"app\dao\UserDao");
        let owner = add_node(&mut ctx, "Method", r"app\services\UserServices::getList");
        let call = add_node(&mut ctx, "CallSite", "getList");
        (ctx, owner, call, table)
    }

    #[test]
    fn magic_delegation_bridges_the_services_hop_to_the_dao_table() {
        let fkb_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fkb/projects/crmeb.yaml");
        assert!(fkb_path.is_file(), "真实 FKB 应存在: {}", fkb_path.display());
        let real_fk = YamlKnowledgeBase::load_file(&fkb_path)
            .unwrap_or_else(|e| panic!("真实 FKB 解析失败: {}: {e}", fkb_path.display()));
        let spec = real_fk
            .magic_delegation
            .clone()
            .expect("crmeb FKB 应声明 magic_delegation");

        let (mut ctx, owner, call, table) = delegation_ctx(Some(spec));
        let loc = locator(owner, call, r"app\services\UserServices::getList");
        classify_db_action(&mut ctx, &loc, r"app\services\UserServices", "getList");
        assert!(
            has_edge(&ctx, owner, table, EdgeKind::READS_DB),
            "经 magic_delegation 应通过 dao 的类型找到表（否则 Services 这一跳断链）"
        );
    }

    /// Control for the one above: without the declaration, the Services hop has no table of its own and
    /// nothing is laid — so the delegation, not some other path, is what produced the edge.
    #[test]
    fn without_the_declaration_the_services_hop_finds_no_table() {
        let (mut ctx, owner, call, table) = delegation_ctx(None);
        let loc = locator(owner, call, r"app\services\UserServices::getList");
        classify_db_action(&mut ctx, &loc, r"app\services\UserServices", "getList");
        assert!(
            !has_edge(&ctx, owner, table, EdgeKind::READS_DB),
            "没有 magic_delegation 声明时不该落边"
        );
    }

    // ------------------------------------------------------- E. handler resolution

    /// A fully-qualified handler hits its node directly (after trimming this language's namespace
    /// separators — trimming a hard-coded `\` would miss it on every other stack).
    #[test]
    fn resolve_handler_target_hits_a_known_name_and_misses_an_unknown_one() {
        let mut ctx = new_ctx();
        let cls = add_node(&mut ctx, "Class", r"app\api\controller\Login");

        assert_eq!(
            resolve_handler_target(
                &ctx,
                r"app\api\controller\Login",
                "app/api/route/pc.php",
                None,
                None
            )
            .map(|(id, _)| id),
            Some(cls),
            "已知名字应直接命中"
        );
        assert!(
            resolve_handler_target(&ctx, "NoSuch/Nope", "app/api/route/pc.php", None, None).is_none(),
            "解析不到时应返回 None（不猜）"
        );
    }
}
