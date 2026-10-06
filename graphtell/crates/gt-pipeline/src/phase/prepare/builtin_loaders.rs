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
pub(crate) fn run_builtin(
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
        // Neutral ids: neither loader knows a language — the SQL install script is SQL, and "which receivers
        // / accessors name a table or a config key" is declared by FKB `params`. Every other id falls through
        // to the tech-stack adapter below, so the kernel owns **no** language-specific loader name.
        "db_schema" => load_schema(ctx, params, sub, project_root, phase),
        "config_keys" => load_config_keys(ctx, params, sub),
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
pub(crate) fn run_adapter_loader(
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
pub(crate) fn apply_adapter_fact(ctx: &mut PipelineContext, fact: AdapterFact) {
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

pub(crate) fn load_schema(
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
pub(crate) fn merge_schema_columns(
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

/// Collect config keys (`config_keys`).
///
/// Which callables read configuration is **stack knowledge** and comes from `params`; there is no
/// built-in default list. That used to be different: the default spelled out one product's own helpers
/// (`sys_config` / `sys_config_all`), so every stack that merely forgot to declare `accessors` silently
/// inherited them. "No knowledge declared" now means "no keys collected", which is honest.
///
/// The same applies to `suffixes`: a blanket `::get` matched `Route::get('api/goods/detail')` — measured
/// on CRMEB, 791 route registrations against 141 real `Config::get` / `Env::get` reads, i.e. the config
/// table filled up with URL paths.
pub(crate) fn load_config_keys(ctx: &mut PipelineContext, params: &Value, _sub: &gt_domain::model::SubProject) {
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
pub(crate) fn short_callee(callee: &str) -> &str {
    callee.rsplit(['\\', '.']).next().unwrap_or(callee)
}

/// Whether a call's `receiver` matches the `receiver` field of some `route_calls` pattern.
pub(crate) fn receiver_matches(rc: &RouteCallSpec, recv: &str) -> bool {
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
pub(crate) fn guard_arg_name(v: &FactValue, accept_identifier: bool) -> Option<String> {
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
pub(crate) fn match_route_call(call: &CallRecord, rc: &RouteCallSpec) -> Option<(String, String, Option<String>)> {
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
pub(crate) fn collect_route_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
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
pub(crate) fn extract_chain_guards(
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
pub(crate) fn extract_positional_guards(spec: &RouteGuardSpec, calls: &[CallRecord]) -> Vec<RouteGuardScope> {
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
pub(crate) fn extract_consumer_guards(
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
pub(crate) fn parse_for_route_target(a: &FactValue) -> Option<(String, Option<String>)> {
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
pub(crate) fn request_method_verb(v: &FactValue) -> Option<String> {
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
pub(crate) fn extract_decorator_guards(
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
pub(crate) fn load_middleware_aliases(
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
pub(crate) fn resolve_guard_class(
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
pub(crate) fn guard_arg_text(v: &FactValue) -> Option<String> {
    match v {
        FactValue::String(s) | FactValue::ClassConst(s) => Some(s.clone()),
        FactValue::Bool(b) => Some(b.to_string()),
        FactValue::Int(i) => Some(i.to_string()),
        FactValue::Float(f) => Some(f.to_string()),
        _ => None,
    }
}

pub(crate) fn load_routes(
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

pub(crate) fn load_nginx(
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
pub(crate) fn load_declared_middleware(
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
            "P3 declarative middleware: {} classes (scope {:?}{})",
            classes.len(),
            scope,
            prefix.map(|p| format!(", prefix {}", p)).unwrap_or_default()
        );
    }
}


