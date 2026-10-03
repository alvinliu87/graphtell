//! The FKB rules engine: selector matching + bound-action execution.
//!
//! The kernel knows no framework — it only knows the two abstractions "selector" and "binding";
//! all concrete semantics come from the FKB YAML (dependency inversion + open-closed principle).

use std::collections::HashSet;

use gt_domain::model::{
    Action, AliasEntry, AnnotateTarget, AnnotationChannel, EdgeKind, ExpandSpec, ExpandVariant,
    FactValue, IdentityKey,
    Language, MergeStrategy, NamespacePolicy, NewAnnotation, NewEdge, NodeId, NodeKind, Phase,
    Predicate, ResolveTier, Rule, Selector, Severity, SubProjectId, SubkindSource, SynthesizedKind,
};
use serde_json::{json, Value};

use crate::eval::{Evaluator, MatchCtx};
use crate::normalize::{apply_normalize, apply_table_prefix_steps};
use crate::workspace::{CallRecord, ConfigRecord, GraphWorkspace, InheritRecord};
use crate::context::PipelineContext;

// ---------------------------------------------------------------- Namespace

/// The union of known namespace separators: PHP `\`, Java/JS `.`, path-style `/`, C++/Ruby `:`.
///
/// The kernel normalises FQNs and extracts short names by the **union**, assuming no specific language uses which symbol.
/// "This language's separator" is provided by [`gt_domain::port::LanguageParser::namespace_separator()`]
/// and passed to every sub-project along the pipeline via [`gt_domain::model::NamespacePolicy`]
/// (see [`crate::context::PipelineContext::lang_policy_for_sub`]); concatenation and parsing both take that language's separator —
/// the kernel knows no concrete language.
pub const NS_SEPARATORS: [char; 4] = ['\\', '.', '/', ':'];

// ---------------------------------------------------------------- Selector

/// Callee pattern matching.
///
/// Supports:
/// * `|`-separated alternatives
/// * `A::b` / `A->b` forms (`A` supports `*` and tail matching, e.g. `Db::name` matches `think\facade\Db::name`)
/// * `A::{b,c}` method lists
/// * bare method names (match `method`)
pub fn callee_matches(
    pattern: &str,
    callee: &str,
    receiver: Option<&str>,
    method: Option<&str>,
) -> bool {
    pattern
        .split('|')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .any(|alt| alt_matches(alt, callee, receiver, method))
}

fn alt_matches(alt: &str, callee: &str, receiver: Option<&str>, method: Option<&str>) -> bool {
    // Method list: A::{b,c}
    if let Some(open) = alt.find("::{") {
        if let Some(close) = alt[open..].find('}') {
            let recv = &alt[..open];
            let list = &alt[open + 3..open + close];
            let methods: Vec<&str> = list.split(',').map(str::trim).collect();
            if !recv_matches(recv, receiver, callee) {
                return false;
            }
            return match method {
                Some(m) => methods.iter().any(|x| x.eq_ignore_ascii_case(m)),
                None => false,
            };
        }
    }
    if let Some(pos) = alt.find("::") {
        let (r, m) = (&alt[..pos], &alt[pos + 2..]);
        return recv_matches(r, receiver, callee) && meth_matches(m, method);
    }
    // Single-colon form: `*:dispatch` means "any receiver, method dispatch".
    if let Some(pos) = alt.find(':') {
        let (r, m) = (&alt[..pos], &alt[pos + 1..]);
        return recv_matches(r, receiver, callee) && meth_matches(m, method);
    }
    if let Some(pos) = alt.find("->") {
        let (r, m) = (&alt[..pos], &alt[pos + 2..]);
        return recv_matches(r, receiver, callee) && meth_matches(m, method);
    }
    // Bare pattern: match by method name first, then by full callee
    if let Some(m) = method {
        if m.eq_ignore_ascii_case(alt) {
            return true;
        }
    }
    callee.eq_ignore_ascii_case(alt) || callee.ends_with(&format!("::{}", alt))
}

/// Before matching, restore the call site's `receiver` / `callee` via the **import alias** (generic, framework-agnostic).
///
/// Example: after `use think\facade\Queue as QueueThink;` writing `QueueThink::push()`, the receiver `QueueThink`
/// is restored to `think\facade\Queue`, so it can hit FKB patterns like `Queue::push` that "end in an umbrella name"
/// (the tail `\Queue` match of `recv_matches`). The alias looks up the global index written into the `imports` symbol table by P2.
///
/// If there is no matching alias, the call site matches as-is, behaviour identical to before (no regression).
pub fn aliased_callee_matches(ws: &GraphWorkspace, pattern: &str, rec: &CallRecord) -> bool {
    let (recv, method, callee) = resolve_aliased_call(ws, rec);
    callee_matches(pattern, &callee, recv.as_deref(), method.as_deref())
}

/// Restore a call site's receiver / callee into the alias's FQN.
fn resolve_aliased_call(ws: &GraphWorkspace, rec: &CallRecord) -> (Option<String>, Option<String>, String) {
    let method = rec.method.clone();
    let mut recv = rec.receiver.clone();
    let mut callee = rec.callee.clone();
    if let Some(r) = &recv {
        if let Some(fqn) = ws.resolve_import_alias(r) {
            if let Some(m) = &method {
                callee = format!("{}::{}", fqn, m);
            }
            recv = Some(fqn);
        }
    } else if let Some((head, tail)) = rec.callee.split_once("::") {
        if let Some(fqn) = ws.resolve_import_alias(head) {
            recv = Some(fqn.clone());
            callee = format!("{}::{}", fqn, tail);
        }
    }
    (recv, method, callee)
}

fn recv_matches(pattern: &str, receiver: Option<&str>, callee: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let Some(r) = receiver else { return false };
    // Leading-separator folding: Java's `com.x.X` and PHP's `\X` both drop the prefix separator.
    let r = r.trim_start_matches(|c| NS_SEPARATORS.contains(&c));
    let pat = pattern.strip_prefix('*').unwrap_or(pattern);
    if r.eq_ignore_ascii_case(pat) {
        return true;
    }
    // Tail matching (umbrella name): `Queue` matches `think\facade\Queue`, `Service` matches `com.x.Service`.
    // The separator takes the union of known languages, assuming no specific language uses which symbol (kernel has zero language knowledge).
    if r.len() > pat.len() {
        for sep in NS_SEPARATORS {
            let prefix = format!("{}{}", sep, pat);
            if r.len() >= prefix.len()
                && r.as_bytes()[r.len() - prefix.len()..].eq_ignore_ascii_case(prefix.as_bytes())
            {
                return true;
            }
        }
    }
    callee.eq_ignore_ascii_case(pattern)
}

fn meth_matches(pattern: &str, method: Option<&str>) -> bool {
    if pattern == "*" {
        return true;
    }
    match method {
        Some(m) => m.eq_ignore_ascii_case(pattern),
        None => false,
    }
}

/// Whether a call site matches the selector.
pub fn matches_call(sel: &Selector, rec: &CallRecord, ws: &GraphWorkspace) -> bool {
    let (callee_pat, preds) = match sel {
        Selector::Call { callee, r#where } => (callee, r#where),
        _ => return false,
    };
    if let Some(pat) = callee_pat {
        if !aliased_callee_matches(ws, pat, rec) {
            return false;
        }
    }
    let ev = Evaluator::new(ws, MatchCtx::Call(rec));
    preds
        .iter()
        .all(|p| eval_predicate(p, rec.node, Some(MatchCtx::Call(rec)), ws, &ev))
}

/// Whether a config entry matches the selector (supports `{locale}` placeholders and `*` wildcards in `file`).
pub fn matches_config(
    sel: &Selector,
    rec: &ConfigRecord,
    node: NodeId,
    ws: &GraphWorkspace,
) -> bool {
    let (file_pat, key_pat, preds) = match sel {
        Selector::ConfigEntry { file, key_path, r#where } => (file, key_path, r#where),
        _ => return false,
    };
    if let Some(pat) = file_pat {
        if !path_matches(pat, &rec.file) {
            return false;
        }
    }
    if let Some(pat) = key_pat {
        if !wildcard_matches(pat, &rec.key_path) {
            return false;
        }
    }
    if !preds.is_empty() {
        let ev = Evaluator::new(ws, MatchCtx::Config(rec));
        if !preds
            .iter()
            .all(|p| eval_predicate(p, node, Some(MatchCtx::Config(rec)), ws, &ev))
        {
            return false;
        }
    }
    true
}

/// Path matching: supports `*` wildcards and `{locale}` placeholders (the placeholder content does not participate in matching).
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let normalized = pattern.replace("{locale}", "*");
    wildcard_matches(&normalized, path)
}

/// Extract the fragment corresponding to the `{locale}` placeholder from a path.
pub fn capture_locale(pattern: &str, path: &str) -> Option<String> {
    let idx = pattern.find("{locale}")?;
    let prefix = &pattern[..idx];
    let suffix = &pattern[idx + "{locale}".len()..];
    if !path.starts_with(prefix) {
        return None;
    }
    let rest = &path[prefix.len()..];
    let end = if suffix.is_empty() {
        rest.find('/').unwrap_or(rest.len())
    } else {
        rest.find(suffix)?
    };
    Some(rest[..end].to_string())
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern.is_empty() {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == value || value.ends_with(pattern.trim_start_matches('/'));
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match value[pos..].find(part) {
            Some(idx) => {
                if i == 0 && idx != 0 && !pattern.starts_with('*') {
                    return false;
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }
    parts.last().map(|l| l.is_empty() || value.ends_with(l)).unwrap_or(true)
}

/// Whether inheritance matches the selector.
pub fn matches_inherit(sel: &Selector, rec: &InheritRecord, ws: &GraphWorkspace) -> bool {
    let (base, with_property) = match sel {
        Selector::Inheritance { base, with_property } => (base, with_property),
        _ => return false,
    };
    if let Some(pat) = base {
        // Direct base-class match, or **transitive inheritance** match (`X extends BaseModel extends Model`)
        let direct = pat
            .split('|')
            .map(str::trim)
            .any(|p| recv_matches(p, Some(&rec.base), &rec.base));
        if !direct {
            // Transitive inheritance is judged by the **name chain**, tolerating missing intermediate/leaf classes in vendor
            let transitive = pat
                .split('|')
                .map(str::trim)
                .any(|p| ws.has_supertype(&rec.child_fqn, p));
            if !transitive {
                return false;
            }
        }
    }
    if let Some(prop) = with_property {
        if ws.property_of(rec.child, prop).is_none() {
            return false;
        }
    }
    true
}

/// Whether a graph node matches the selector.
pub fn matches_node(sel: &Selector, id: NodeId, ws: &GraphWorkspace) -> bool {
    let (kind, preds) = match sel {
        Selector::Node { node_kind, r#where } => (node_kind, r#where),
        _ => return false,
    };
    let Some(node) = ws.node(id) else { return false };
    if let Some(k) = kind {
        if node.kind != *k {
            return false;
        }
    }
    let ev = Evaluator::new(ws, MatchCtx::Node(id));
    preds
        .iter()
        .all(|p| eval_predicate(p, id, Some(MatchCtx::Node(id)), ws, &ev))
}

/// Turn a node property value into a string for `property_is` comparison.
///
/// Supports both string and boolean: the parser writes `in_loop` as a JSON boolean (`true` / `false`),
/// while the rule writes the string `"true"`. Using only `as_str()` would return `None` for a boolean,
/// making `property_is: { name: in_loop, value: "true" }` never hold.
fn node_prop_str(v: Option<&serde_json::Value>) -> Option<&str> {
    match v {
        Some(serde_json::Value::String(s)) => Some(s.as_str()),
        Some(serde_json::Value::Bool(b)) => Some(if *b { "true" } else { "false" }),
        _ => None,
    }
}

/// Predicate evaluation.
pub fn eval_predicate(
    pred: &Predicate,
    node: NodeId,
    mctx: Option<MatchCtx>,
    ws: &GraphWorkspace,
    ev: &Evaluator,
) -> bool {
    match pred {
        Predicate::HasProperty(name) => ws.property_of(node, name).is_some(),
        Predicate::InSymbolTable { table, key_of } => match ev.string(key_of) {
            Some(key) => {
                ws.get_symbol(table, &key).is_some()
                    || ws.get_symbol(table, &ws.strip_table_prefix(&key)).is_some()
            }
            None => false,
        },
        Predicate::ColumnsMatch { table, names } => {
            let Some(node) = ws.node(node) else { return false };
            let Some(tname) = node.identity.as_ref().map(|i| i.value.clone()).or_else(|| node.fqn.clone()) else {
                return false;
            };
            schema_columns(ws, table, &tname)
                .map(|cols| names.iter().any(|n| cols.iter().any(|c| column_matches(c, n))))
                .unwrap_or(false)
        }
        Predicate::HasAnnotation { kind } => ws.has_annotation(node, kind),
        Predicate::NoneOfCapability(caps) => !ws
            .annotations_of(node)
            .iter()
            .any(|a| a.channel.as_str() == AnnotationChannel::CAPABILITY && caps.iter().any(|c| c == &a.kind)),
        Predicate::HasMissing(want) => {
            let Some(node) = ws.node(node) else { return false };
            let present = node
                .properties
                .get("texts")
                .and_then(|t| t.as_object())
                .map(|o| o.len())
                .unwrap_or(0);
            let required = node
                .properties
                .get("required_locales")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            (present < required) == *want
        }
        Predicate::FanInGte(n) => ws.fan_in(node) as u64 >= *n,
        Predicate::EntryArityGte(n) => match mctx {
            // Only pass config entries where "the value is an array with element count >= n":
            // excludes both scalar leaf entries expanded from an array (`listen.evt.0`) and empty-array entries.
            Some(MatchCtx::Config(c)) => {
                matches!(&c.value, FactValue::Array(_)) && c.value.array_len() >= *n
            }
            _ => false,
        },
        Predicate::NameMatches(sub) => {
            let Some(n) = ws.node(node) else { return false };
            let hay = n.identity.as_ref().map(|i| i.value.clone()).unwrap_or_else(|| n.name.clone());
            hay.to_ascii_lowercase().contains(&sub.to_ascii_lowercase())
        }
        Predicate::PropertyIs { name, value } => match ws.node(node) {
            Some(n) => node_prop_str(n.properties.get(name)) == Some(value.as_str()),
            None => false,
        },
        Predicate::FqnMatches(sub) => {
            let Some(n) = ws.node(node) else { return false };
            let hay = n.fqn.clone().unwrap_or_else(|| n.name.clone());
            hay.to_ascii_lowercase().contains(&sub.to_ascii_lowercase())
        }
        Predicate::NameNotIn(names) => match ws.node(node) {
            Some(n) => !names.iter().any(|x| x.eq_ignore_ascii_case(&n.name)),
            None => false,
        },
        Predicate::NotClaimedBy(kind) => !ws.claimed_by(node, kind),
        // Evidence-based counterpart of the "marker property" habits: the edge is the witness, a property
        // several parties all patch is not.
        Predicate::HasIncoming(kind) => ws.has_incoming_edge(node, kind),
        Predicate::ArgCount(n) => match mctx {
            Some(MatchCtx::Call(c)) => c.args.len() == *n,
            _ => false,
        },
        Predicate::ArgStartsWith { arg, prefix } => match mctx {
            Some(MatchCtx::Call(c)) => c
                .args
                .get(*arg)
                .and_then(|v| match v {
                    FactValue::String(s) => Some(s),
                    _ => None,
                })
                .map(|s| s.starts_with(prefix))
                .unwrap_or(false),
            _ => false,
        },
    }
}

fn column_matches(column: &str, wanted: &str) -> bool {
    let c = column.to_ascii_lowercase();
    let w = wanted.to_ascii_lowercase();
    c == w || c.ends_with(&format!("_{}", w))
}

/// Look up columns of the schema table (compatible with prefixed / unprefixed table names).
pub fn schema_columns(ws: &GraphWorkspace, table: &str, name: &str) -> Option<Vec<String>> {
    let stripped = ws.strip_table_prefix(name);
    if stripped != name {
        if let Some(v) = ws.get_symbol(table, &stripped) {
            return Some(columns_of(v));
        }
    }
    if let Some(v) = ws.get_symbol(table, name) {
        return Some(columns_of(v));
    }
    None
}

fn columns_of(v: &Value) -> Vec<String> {
    v.get("columns")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- Binding execution

/// Execute all actions of one rule.
#[allow(clippy::too_many_arguments)]
pub fn exec_binding(
    ctx: &mut PipelineContext,
    rule: &Rule,
    mctx: MatchCtx,
    matched: NodeId,
    owner: NodeId,
    sub: Option<gt_domain::model::SubProjectId>,
    phase: &Phase,
    last_synth: &mut Option<NodeId>,
) {
    for action in &rule.binding {
        match action {
            Action::Annotate(a) => {
                if let Some(p) = &a.phase {
                    let want = Phase(format!("{}{}", phase_prefix(phase), suffix_of(p)));
                    if want != *phase {
                        continue;
                    }
                }
                let targets = resolve_annotate_targets(ctx, &a.target, mctx, matched, last_synth, rule);
                if targets.is_empty() {
                    ctx.ws.diagnose(
                        phase,
                        "AnnotateTargetMissing",
                        Severity::Warning,
                        format!("rule {}: annotation target could not be resolved", rule.id),
                        location_of(ctx, mctx),
                    );
                    continue;
                }
                for node in targets {
                    for spec in &a.annotations {
                        let mut confidence = spec.confidence * rule.confidence;
                        if let Some(scale) = a.confidence_scale {
                            confidence *= scale;
                        }
                        let subkind = resolve_subkind(ctx, &spec.subkind, node, mctx);
                        ctx.ws.annotate(NewAnnotation {
                            node_id: node,
                            channel: spec
                                .channel
                                .clone()
                                .unwrap_or_else(|| a.channel.clone()),
                            kind: spec.kind.clone(),
                            subkind,
                            confidence,
                            evidence: json!({
                                "hook": rule.id,
                                "location": location_of(ctx, mctx),
                                "severity": spec.severity,
                            }),
                            phase: phase.clone(),
                            merge: a.merge,
                        });
                    }
                }
            }
            Action::Synthesize(s) => {
                let id = exec_synthesize(ctx, rule, s, mctx, matched, owner, sub, phase);
                *last_synth = Some(id);
            }
            Action::Link(l) => {
                let ev = Evaluator::new(&ctx.ws, mctx);
                let from = l.from.as_ref().and_then(|s| ev.string(s)).and_then(|n| ctx.ws.find_by_name(&n));
                let to = l.to.as_ref().and_then(|s| ev.string(s)).and_then(|n| ctx.ws.find_by_name(&n));
                if let (Some(f), Some(t)) = (from, to) {
                    ctx.ws.add_edge(NewEdge {
                        project_id: ctx.project.id,
                        kind: l.kind.clone(),
                        from_id: f,
                        to_id: t,
                        phase: phase.clone(),
                        confidence: l.confidence.unwrap_or(rule.confidence),
                        properties: Value::Null,
                    });
                }
            }
            Action::Project(p) => {
                exec_project(ctx, rule, p, matched, phase);
            }
        }
    }
}

/// Edge projection: for each matching node's `along` out-edge, walk each end along its own edge-kind chain to the landing point,
/// and build a `kind` edge between the two landing points.
///
/// **One-to-many** is why it exists: an entity class with several `@ManyToOne` produces several foreign-key edges,
/// while `Link`'s two ends can each take only one name, and multiple relations would necessarily build only one (silently dropping edges).
/// When a landing point cannot be reached (e.g. that entity has no corresponding `Table`), skip that one — better missing than guessed.
fn exec_project(
    ctx: &mut PipelineContext,
    rule: &Rule,
    p: &gt_domain::model::ProjectAction,
    matched: NodeId,
    phase: &Phase,
) {
    // Copy the adjacency list first: `add_edge` needs &mut, so it cannot simultaneously borrow the `out_edges_of` slice.
    let outs: Vec<(String, i64)> = ctx.ws.out_edges_of(matched).to_vec();
    for (kind, to) in outs {
        if kind != p.along.as_str() {
            continue;
        }
        let from_end = ctx.ws.follow(matched, &p.from);
        let to_end = ctx.ws.follow(NodeId(to), &p.to);
        let (Some(f), Some(t)) = (from_end, to_end) else {
            continue;
        };
        if f == t {
            continue; // a self-loop (references itself) carries no information
        }
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: p.kind.clone(),
            from_id: f,
            to_id: t,
            phase: phase.clone(),
            confidence: p.confidence.unwrap_or(rule.confidence),
            properties: Value::Null,
        });
    }
}

/// `Annotate.phase` only writes `Pre` / `Post`; needs to be assembled into a full phase name.
fn suffix_of(p: &Phase) -> &str {
    if p.as_str().ends_with("Post") {
        "Post"
    } else {
        "Pre"
    }
}

fn phase_prefix(phase: &Phase) -> &'static str {
    if phase.as_str() == Phase::ANNOTATE_POST {
        "Annotate"
    } else {
        "Annotate"
    }
}

fn location_of(ctx: &PipelineContext, m: MatchCtx) -> Option<String> {
    match m {
        MatchCtx::Call(c) => Some(format!("{}:{}", c.file, c.span.start_line)),
        MatchCtx::Config(c) => Some(format!("{}:{}", c.file, c.span.start_line)),
        MatchCtx::Inherit(i) => Some(format!("{}:{}", i.file, i.span.start_line)),
        // P6's selector acts on **graph nodes**: here the "origin" is the node's own declaration location, not the
        // identity / FQN string (that would stuff an FQN into `locations[].file` and break the frontend jump).
        MatchCtx::Node(id) => {
            let n = ctx.ws.node(id)?;
            let file = n.file_id.and_then(|fid| ctx.ws.source_path_of(fid.get()))?;
            Some(format!("{}:{}", file, n.span.start_line))
        }
    }
}

fn resolve_annotate_targets(
    ctx: &mut PipelineContext,
    target: &AnnotateTarget,
    mctx: MatchCtx,
    matched: NodeId,
    last_synth: &Option<NodeId>,
    rule: &Rule,
) -> Vec<NodeId> {
    match target {
        AnnotateTarget::Matched => vec![matched],
        AnnotateTarget::SynthesizedRef(r) => {
            if r == "@self" || r == "@last" {
                return last_synth.as_ref().copied().into_iter().collect();
            }
            let id = IdentityKey::named(r.clone());
            ctx.ws
                .node_ids()
                .into_iter()
                .find(|n| ctx.ws.node(*n).and_then(|x| x.identity.clone()).map(|i| i.key()) == Some(id.key()))
                .into_iter()
                .collect()
        }
        AnnotateTarget::FromField { source, resolve } => {
            let ev = Evaluator::new(&ctx.ws, mctx);
            let items = ev.list(source);
            let mut out = Vec::new();
            for item in items {
                let name = match item {
                    FactValue::String(s) | FactValue::ClassConst(s) => s,
                    _ => continue,
                };
                let resolved = match resolve {
                    Some(_) => ev.resolve_name(&name),
                    None => name,
                };
                if let Some(id) = ctx.ws.find_by_name(&resolved) {
                    out.push(id);
                } else if let Some(short) = ctx.ws.resolve_short_name(&resolved) {
                    if let Some(id) = ctx.ws.find_by_name(&short) {
                        out.push(id);
                    }
                }
            }
            if out.is_empty() {
                tracing::debug!("rule {}: from_field matched no node", rule.id);
            }
            out
        }
    }
}

fn resolve_subkind(
    ctx: &PipelineContext,
    src: &Option<SubkindSource>,
    node: NodeId,
    mctx: MatchCtx,
) -> Option<String> {
    let src = src.as_ref()?;
    match src {
        SubkindSource::Literal(s) => Some(s.clone()),
        SubkindSource::Computed(kind) => {
            if kind == "missing_locales" {
                let n = ctx.ws.node(node)?;
                let present: std::collections::BTreeSet<String> = n
                    .properties
                    .get("texts")
                    .and_then(|t| t.as_object())
                    .map(|o| o.keys().cloned().collect())
                    .unwrap_or_default();
                let required: Vec<String> = n
                    .properties
                    .get("required_locales")
                    .and_then(|r| serde_json::from_value(r.clone()).ok())
                    .unwrap_or_else(|| ctx.project.config.required_locales.clone());
                let missing: Vec<String> =
                    required.into_iter().filter(|r| !present.contains(r)).collect();
                if missing.is_empty() {
                    None
                } else {
                    Some(missing.join(","))
                }
            } else {
                None
            }
        }
        SubkindSource::FromSymbolTable { table, field, of } => {
            let key = match of {
                Some(src) => Evaluator::new(&ctx.ws, mctx).string(src)?,
                None => ctx
                    .ws
                    .node(node)?
                    .identity
                    .as_ref()
                    .map(|i| i.value.clone())
                    .or_else(|| ctx.ws.node(node)?.fqn.clone())?,
            };
            let value = ctx.ws.get_symbol(table, &key)?;
            value.get(field).and_then(|v| v.as_str()).map(|s| s.to_string())
        }
        SubkindSource::FromFanIn { thresholds } => {
            let fan = ctx.ws.fan_in(node) as u64;
            let label = if fan >= thresholds.high {
                thresholds.high_label.clone().unwrap_or_else(|| "high".into())
            } else if fan >= thresholds.medium {
                thresholds.medium_label.clone().unwrap_or_else(|| "medium".into())
            } else {
                thresholds.low_label.clone().unwrap_or_else(|| "low".into())
            };
            Some(label)
        }
    }
}

// ---------------------------------------------------------------- Synthesize

/// Expansion table: **one call -> N semantic nodes** (e.g. a REST resource route).
///
/// Execute each "allowed" variant in order, each synthesising one node with the same `identity` / `fields` / `link`;
/// the variant's differences (method / path suffix / entry method) are injected via `{ expand_method: true }` / `{ expand_entry: true }`.
/// Returns the **last** variant's node (for `@self` references of later bindings in the same rule).
fn exec_synthesize(
    ctx: &mut PipelineContext,
    rule: &Rule,
    s: &gt_domain::model::SynthesizeAction,
    mctx: MatchCtx,
    matched: NodeId,
    owner: NodeId,
    sub: Option<gt_domain::model::SubProjectId>,
    phase: &Phase,
) -> NodeId {
    let Some(expand) = &s.expand else {
        return exec_synthesize_one(ctx, rule, s, mctx, matched, owner, sub, phase, None);
    };
    if expand.variants.is_empty() {
        return exec_synthesize_one(ctx, rule, s, mctx, matched, owner, sub, phase, None);
    }
    let allowed = expanded_actions(ctx, expand, mctx);
    let mut last = matched;
    for v in &expand.variants {
        if let Some(set) = &allowed {
            if !set.contains(&v.name) {
                continue;
            }
        }
        last = exec_synthesize_one(ctx, rule, s, mctx, matched, owner, sub, phase, Some(v));
    }
    last
}

/// The actually-effective action-name set in the expansion table; `None` means "no filter declared, all take effect".
///
/// The filter comes from a chained call on the **same statement line** — `Route::resource(...)->except(['read'])`
/// is parsed into another call site on the same line whose method name is `except`, with its first argument being the action-name array.
/// This way the kernel need not know any framework's `only` / `except` semantics (the names come from FKB).
fn expanded_actions(
    ctx: &PipelineContext,
    spec: &ExpandSpec,
    mctx: MatchCtx,
) -> Option<HashSet<String>> {
    let call = match mctx {
        crate::eval::MatchCtx::Call(c) => c,
        _ => return None,
    };
    let mut selected: Option<HashSet<String>> = None;
    if let Some(name) = &spec.only {
        let vals = ctx
            .ws
            .chained_strings(&call.file, call.span.start_line, name);
        if !vals.is_empty() {
            selected = Some(vals.into_iter().collect());
        }
    }
    if let Some(name) = &spec.except {
        let vals = ctx
            .ws
            .chained_strings(&call.file, call.span.start_line, name);
        if !vals.is_empty() {
            let excluded: HashSet<String> = vals.into_iter().collect();
            match &mut selected {
                Some(set) => set.retain(|n| !excluded.contains(n)),
                None => {
                    selected = Some(
                        spec.variants
                            .iter()
                            .map(|v| v.name.clone())
                            .filter(|n| !excluded.contains(n))
                            .collect(),
                    );
                }
            }
        }
    }
    selected
}



/// A variant-aware evaluator: as `Synthesize.expand` runs each variant, it injects the current variant into the evaluation context,
/// so `{ expand_method: true }` / `{ expand_entry: true }` take that variant's value.
fn ev_of<'a>(
    ws: &'a GraphWorkspace,
    mctx: MatchCtx<'a>,
    variant: Option<&ExpandVariant>,
) -> Evaluator<'a> {
    Evaluator::new(ws, mctx).with_variant(variant.cloned())
}

fn exec_synthesize_one(
    ctx: &mut PipelineContext,
    rule: &Rule,
    s: &gt_domain::model::SynthesizeAction,
    mctx: MatchCtx,
    matched: NodeId,
    owner: NodeId,
    sub: Option<gt_domain::model::SubProjectId>,
    phase: &Phase,
    variant: Option<&ExpandVariant>,
) -> NodeId {
    let ev = Evaluator::new(&ctx.ws, mctx).with_variant(variant.cloned());
    let identity = compute_identity(&ev, &s.identity, variant);
    let Some(mut identity) = identity else {
        ctx.ws.diagnose(
            phase,
            "IdentityUnresolved",
            Severity::Warning,
            format!("rule {}: could not compute an identity", rule.id),
            location_of(ctx, mctx),
        );
        return matched;
    };

    if identity.kind.as_str() != SynthesizedKind::CONTRACT_ID {
        if let Some(side) = s
            .fields
            .iter()
            .find(|f| f.name == "side")
            .and_then(|f| f.value.as_ref())
            .and_then(|vs| ev.string(vs))
        {
            identity = identity.with_scope(side);
        }
    }

    let kind = match &s.subtype {
        Some(sub) if !sub.is_empty() => NodeKind(sub.clone()),
        _ => s.node.clone(),
    };
    let mut props = json!({ "category": kind.as_str() });

    let mut new_node = crate::workspace::synthesized_node(
        ctx.project.id,
        kind.as_str(),
        identity.clone(),
        sub,
        phase,
        s.confidence * rule.confidence,
        &ctx.language_of(sub),
        gt_domain::model::Span::default(),
    );
    // Record the source, for conflict diagnosis
    props["sources"] = json!([rule.id]);
    new_node.properties = props.clone();

    // The call statement source (CallSite is extracted from the span by P2). Let both an edge's evidence and a synthesised node's origin
    // directly show "how this edge / this config was read", without opening the file to check.
    let snippet = ctx
        .ws
        .node(matched)
        .and_then(|n| n.properties.get("snippet"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let (node_id, created) = ctx.ws.get_or_create_synthesized(new_node);

    // Record "where this semantic object comes from in source", for the frontend to give a **multi-location** jump list
    if let Some(loc) = location_of(ctx, mctx) {
        let (file, line) = match loc.split_once(':') {
            Some((f, l)) => (f.to_string(), l.parse::<u32>().unwrap_or(0)),
            None => (loc.clone(), 0),
        };
        ctx.ws.append_location(
            node_id,
            file,
            line,
            Some(identity.value.clone()),
            Some(format!("rule {}", rule.id)),
            snippet.clone(),
        );
    }

    if !created {
        if let Some(node) = ctx.ws.node(node_id) {
            let mut sources = node
                .properties
                .get("sources")
                .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
                .unwrap_or_default();
            if !sources.contains(&rule.id) {
                sources.push(rule.id.clone());
            }
            ctx.ws.patch_properties(node_id, json!({ "sources": sources }));
        }
    }

    // fields
    for f in &s.fields {
        if let Some(acc) = &f.accumulate {
            let key = ev_of(&ctx.ws, mctx, variant).string(&acc.key).unwrap_or_default();
            let value = ev_of(&ctx.ws, mctx, variant).string(&acc.value).unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            ctx.ws
                .patch_properties(node_id, json!({ f.name.clone(): { key: value } }));
            continue;
        }
        let value: Option<Value> = if let Some(sym) = &f.from_symbol_table {
            let key = match &sym.of {
                Some(src) => ev_of(&ctx.ws, mctx, variant).string(src),
                None => Some(identity.value.clone()),
            };
            key.and_then(|k| ctx.ws.get_symbol(&sym.table, &k).cloned())
                .and_then(|v| v.get(&sym.field).cloned())
        } else if let Some(src) = &f.value {
            match ev_of(&ctx.ws, mctx, variant).string(src) {
                Some(v) => Some(json!(v)),
                None => None,
            }
        } else {
            None
        };
        if let Some(v) = value {
            // `side` is **evidence about who has touched this node**, not "the owner": The same synthesised node
            // may be written by several parties (a contract bridge is written by both the front end and the back
            // end onto one node). Patching a scalar here made the result depend on **who patches last** — an
            // order-dependent lie. So it is accumulated into a sorted set `sides` (order-independent by
            // construction) and `side` becomes merely the derived display label: one party ⇒ that party, several
            // ⇒ `bridge`. `sides` must never be folded into `IdentityKey::scope` (it is not part of "what is
            // this node", it answers "who knows about it").
            if f.name == "side" {
                if let Some(side) = v.as_str() {
                    ctx.ws.record_side(node_id, side);
                }
            } else {
                ctx.ws.patch_properties(node_id, json!({ f.name.clone(): v }));
            }
        }
    }

    // link
    if let Some(link) = &s.link {
        let confidence = link.confidence.unwrap_or(s.confidence * rule.confidence);
        match link.direction {
            gt_domain::model::Direction::Incoming => {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: link.kind.clone(),
                    from_id: owner,
                    to_id: node_id,
                    phase: phase.clone(),
                    confidence,
                    properties: serde_json::json!({
                        "evidence": {
                            "rule": rule.id,
                            "location": location_of(ctx, mctx),
                            "snippet": snippet.clone(),
                        },
                    }),
                });
                // Record a propagation seed: owner is the "action emitter", and its callers along the CALLS chain should also be recognised as emitters of the same semantic action.
                ctx.propagation_seeds.push(crate::context::PropSeed {
                    source: owner,
                    target: node_id,
                    kind: link.kind.0.clone(),
                    confidence,
                    sub,
                    phase: phase.clone(),
                });
            }
            gt_domain::model::Direction::Outgoing => {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: link.kind.clone(),
                    from_id: node_id,
                    to_id: owner,
                    phase: phase.clone(),
                    confidence,
                    properties: Value::Null,
                });
            }
            gt_domain::model::Direction::ToTarget => {
                // The target method name: an array-style handler's `[Ctrl::class, 'method']` is given here.
                let method = link
                    .to_method
                    .as_ref()
                    .and_then(|src| ev_of(&ctx.ws, mctx, variant).string(src));
                let entry = entry_methods_for(ctx, sub);
                let policy = ctx.lang_policy_for_sub(sub).clone();
                let items = link
                    .to
                    .as_ref()
                    .map(|src| ev_of(&ctx.ws, mctx, variant).list(src))
                    .unwrap_or_default();
                let fb_items = link
                    .to_fallback
                    .as_ref()
                    .map(|src| ev_of(&ctx.ws, mctx, variant).list(src))
                    .unwrap_or_default();
                // Items in the main source that "can be treated as a string target" (skip arrays / empty — exactly the queue `arg:0` is-an-array case).
                let primary_strings: Vec<String> = items
                    .iter()
                    .filter_map(|item| match item {
                        FactValue::String(s) | FactValue::ClassConst(s) if !s.is_empty() => {
                            Some(s.clone())
                        }
                        _ => None,
                    })
                    .collect();
                if primary_strings.is_empty() && !fb_items.is_empty() {
                    // The main source cannot resolve (e.g. queue `arg:0` is an array / action name rather than a class),
                    // fall back entirely to `to_fallback` (e.g. `receiver_class`) to keep finding the consumer.
                    push_to_target_edges(
                        ctx, mctx, rule, link, node_id, phase, confidence, sub, &fb_items,
                    );
                } else {
                    for raw in primary_strings {
                        let resolved = ev_of(&ctx.ws, mctx, variant).resolve_name(&raw);
                        if let Some(target) = find_target_node(
                            ctx,
                            &resolved,
                            &link.kind,
                            method.as_deref(),
                            &entry,
                            &policy,
                        )
                        .or_else(|| ctx.ws.find_by_name(&resolved))
                        {
                            ctx.ws.add_edge(NewEdge {
                                project_id: ctx.project.id,
                                kind: link.kind.clone(),
                                from_id: node_id,
                                to_id: target,
                                phase: phase.clone(),
                                confidence,
                                properties: serde_json::json!({
                                    "evidence": { "rule": rule.id, "target": resolved },
                                }),
                            });
                        } else {
                            // Hand to P7: a route handler may need convention-based assembly before it is known to exist
                            ctx.ws.pending_links.push(crate::workspace::PendingLink {
                                from: node_id,
                                kind: link.kind.clone(),
                                raw: resolved.clone(),
                                // Carry the entry method explicitly given by FKB (the `expand_entry` of a resource route),
                                // otherwise P7 only has the class name and degrades to "class-level fallback".
                                method: method.clone(),
                                // `resolve` can be written on either a link or on the `to` value source
                                resolve: link
                                    .resolve
                                    .clone()
                                    .or_else(|| link.to.as_ref().and_then(|t| t.resolve))
                                    .unwrap_or(gt_domain::model::ResolveAs::ClassConst),
                                confidence,
                                sub,
                                file: location_of(ctx, mctx).unwrap_or_default(),
                                line: 0,
                            });
                        }
                    }
                }
            }
        }
    }

    // alias registration
    if let Some(alias) = &s.alias {
        if let Some(key) = ev_of(&ctx.ws, mctx, variant).string(&alias.key) {
            let qualifier = alias
                .qualifier
                .as_ref()
                .and_then(|q| ev_of(&ctx.ws, mctx, variant).string(q));
            ctx.ws.put_alias(AliasEntry {
                project_id: ctx.project.id,
                namespace: alias.namespace.clone(),
                key,
                qualifier,
                node_id,
                confidence: s.confidence,
                evidence: json!({ "hook": rule.id }),
            });
        }
    }
    node_id
}

/// Candidate forms of a fully qualified name.
///
/// There are two ways to refer to a class in source: an **identifier reference** (`app\common\X`, `com.example.X`)
/// and a **string literal** (`'app\\common\\X'`). The latter takes the source verbatim, where the namespace separator
/// is often escaped into two consecutive characters, mismatching the FQN already built in the graph, so the edge fails to connect.
///
/// What is done here is **language-agnostic** normalisation: fold consecutive repeated namespace separators, drop the leading separator
/// (global-namespace form). The kernel assumes no specific language uses which symbol — the separator takes the union of known languages.
/// In order, the original value is tried first, so nothing that already matched is changed.
fn fqn_variants(fqn: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    push(fqn.to_string());

    // Fold consecutive repeated separators: `app\\common\\X` == `app\common\X`
    let mut collapsed = String::with_capacity(fqn.len());
    let mut prev: Option<char> = None;
    for ch in fqn.chars() {
        if NS_SEPARATORS.contains(&ch) && prev == Some(ch) {
            continue;
        }
        collapsed.push(ch);
        prev = Some(ch);
    }
    push(collapsed.clone());

    // Drop leading separator: `\app\X` == `app\X`
    for cand in [fqn.to_string(), collapsed] {
        push(cand.trim_start_matches(|c| NS_SEPARATORS.contains(&c)).to_string());
    }
    out
}

/// Take a sub-project's (or the global fallback's) candidate consumer-entry method names (FKB `entry_methods`).
fn entry_methods_for(ctx: &PipelineContext, sub: Option<SubProjectId>) -> Vec<String> {
    if let Some(s) = sub {
        if let Some(v) = ctx.entry_methods.get(&s.get()) {
            return v.clone();
        }
    }
    ctx.entry_methods_default.clone()
}

/// Resolve an edge target: class -> prefer its `handle`/`fire`/`doJob` method, otherwise the class itself.
///
/// `doJob` is the common entry of ThinkPHP/CRMEB queue Job classes (the `QueueTrait` convention),
/// listed alongside Laravel's `handle` and Symfony's `__invoke`.
fn find_target_node(
    ctx: &PipelineContext,
    fqn: &str,
    _kind: &EdgeKind,
    method: Option<&str>,
    entry_methods: &[String],
    policy: &NamespacePolicy,
) -> Option<NodeId> {
    let class_id = fqn_variants(fqn).iter().find_map(|cand| {
        ctx.ws
            .find_by_name(cand)
            .or_else(|| ctx.ws.resolve_short_name(cand).and_then(|f| ctx.ws.find_by_name(&f)))
    })?;
    // FKB explicitly gave a method name (e.g. an array-style handler's `[Ctrl::class, 'method']`) — precedence over the convention entry
    if let Some(m) = method.filter(|m| !m.is_empty()) {
        let method_fqn = policy.join_member(&fqn_of(ctx, class_id), m);
        if let Some(id) = ctx.ws.find_by_name(&method_fqn) {
            return Some(id);
        }
    }
    // Consumer entry method: prefer FKB's `entry_methods` (framework knowledge); fall back to
    // the cross-framework common entry-name default set when undeclared. A new framework only declares it in YAML, no kernel change.
    const DEFAULT_ENTRY_METHODS: [&str; 5] = ["handle", "fire", "doJob", "__invoke", "run"];
    let list: Vec<&str> = if entry_methods.is_empty() {
        DEFAULT_ENTRY_METHODS.to_vec()
    } else {
        entry_methods.iter().map(|s| s.as_str()).collect()
    };
    for m in list {
        let method_fqn = policy.join_member(&fqn_of(ctx, class_id), m);
        if let Some(id) = ctx.ws.find_by_name(&method_fqn) {
            return Some(id);
        }
    }
    Some(class_id)
}

/// `ToTarget` edge generation: resolve a set of value sources into "target nodes" and build edges (degrade to a PendingLink if not found).
///
/// Consistent with the inline logic, reused by the `to_fallback` after the main `to` resolution fails.
fn push_to_target_edges(
    ctx: &mut PipelineContext,
    mctx: MatchCtx<'_>,
    rule: &gt_domain::model::Rule,
    link: &gt_domain::model::LinkSpec,
    node_id: gt_domain::model::NodeId,
    phase: &gt_domain::model::Phase,
    confidence: f32,
    sub: Option<gt_domain::model::SubProjectId>,
    items: &[gt_domain::model::FactValue],
) {
    // The target method name (consistent with the main path, reused in the `to_fallback` scenario)
    let method = link
        .to_method
        .as_ref()
        .and_then(|src| Evaluator::new(&ctx.ws, mctx).string(src));
    let entry = entry_methods_for(ctx, sub);
    let policy = ctx.lang_policy_for_sub(sub).clone();
    for item in items {
        let raw = match item {
            gt_domain::model::FactValue::String(s) | gt_domain::model::FactValue::ClassConst(s) => {
                s.clone()
            }
            _ => continue,
        };
        if raw.is_empty() {
            continue;
        }
        // Resolve first (a temporary Evaluator, discarded after use, to avoid a whole-structure borrow conflict with the `find_target_node(ctx, …)` below).
        let resolved = Evaluator::new(&ctx.ws, mctx).resolve_name(&raw);
        if let Some(target) =
            find_target_node(ctx, &resolved, &link.kind, method.as_deref(), &entry, &policy)
                .or_else(|| ctx.ws.find_by_name(&resolved))
        {
            ctx.ws.add_edge(NewEdge {
                project_id: ctx.project.id,
                kind: link.kind.clone(),
                from_id: node_id,
                to_id: target,
                phase: phase.clone(),
                confidence,
                properties: serde_json::json!({
                    "evidence": { "rule": rule.id, "target": resolved },
                }),
            });
        } else {
            ctx.ws.pending_links.push(crate::workspace::PendingLink {
                from: node_id,
                kind: link.kind.clone(),
                raw: resolved.clone(),
                method: method.clone(),
                resolve: link
                    .resolve
                    .clone()
                    .or_else(|| link.to.as_ref().and_then(|t| t.resolve))
                    .unwrap_or(gt_domain::model::ResolveAs::ClassConst),
                confidence,
                sub,
                file: location_of(ctx, mctx).unwrap_or_default(),
                line: 0,
            });
        }
    }
}

fn fqn_of(ctx: &PipelineContext, id: NodeId) -> String {
    ctx.ws
        .node(id)
        .and_then(|n| n.fqn.clone())
        .unwrap_or_default()
}

fn compute_identity(
    ev: &Evaluator,
    spec: &gt_domain::model::IdentitySpec,
    variant: Option<&ExpandVariant>,
) -> Option<IdentityKey> {
    let kind = spec.kind.clone();
    let prefixes = ev.ws().table_prefixes();
    if kind.as_str() == SynthesizedKind::CONTRACT_ID {
        let method = spec
            .method
            .as_ref()
            .and_then(|s| ev.string(s))
            .unwrap_or_else(|| "GET".to_string());
        let path = spec.path.as_ref().and_then(|s| ev.string(s))?;
        let path = match ev.ctx() {
            crate::eval::MatchCtx::Call(c) => {
                let prefix = ev.ws().route_group_prefix(&c.file, c.span.start_line);
                if prefix.is_empty() {
                    path
                } else {
                    format!("/{}/{}", prefix, path.trim_start_matches('/'))
                }
            }
            _ => path,
        };
        let path = match variant.and_then(|v| v.path_suffix.as_deref()) {
            Some(suffix) if !suffix.is_empty() => format!("{}{}", path, suffix),
            _ => path,
        };
        let path = apply_normalize(&path, &apply_table_prefix_steps(&spec.normalize, prefixes));
        return Some(IdentityKey::contract(&method, &path));
    }
    let raw = spec
        .value
        .as_ref()
        .and_then(|s| ev.string(s))
        .or_else(|| spec.value_fallback.as_ref().and_then(|s| ev.string(s)))?;
    let value = apply_normalize(&raw, &apply_table_prefix_steps(&spec.normalize, prefixes));
    if value.is_empty() {
        return None;
    }
    Some(IdentityKey { kind, value, scope: None })
}

/// A resolution-tier judgement reused externally (used by P7).
pub fn tier_of(t: ResolveTier) -> ResolveTier {
    t
}

/// Common merge-strategy default values.
pub fn default_merge() -> MergeStrategy {
    MergeStrategy::MaxByKind
}

/// Language placeholder (for compile-time validation).
pub fn _assert_language(_l: &Language) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::GraphWorkspace;
    use gt_domain::model::{Language, NodeId, ProjectId, Span};

    fn call_record(receiver: Option<&str>, method: Option<&str>, callee: &str) -> CallRecord {
        CallRecord {
            node: NodeId(0),
            owner: NodeId(0),
            owner_fqn: String::new(),
            owner_class: None,
            callee: callee.to_string(),
            receiver: receiver.map(|s| s.to_string()),
            method: method.map(|s| s.to_string()),
            args: vec![],
            db_table: None,
            in_loop: false,
            entity: None,
            span: Span::default(),
            file: String::new(),
            sub: None,
            language: Language::default(),
        }
    }

    #[test]
    fn alias_resolution_makes_facade_pattern_match() {
        let mut ws = GraphWorkspace::new(ProjectId(1));
        // Simulate the import symbol table written by P2: `use think\facade\Queue as QueueThink;`
        ws.put_symbol(
            ProjectId(1),
            "imports",
            "queuethink",
            serde_json::json!({ "fqn": "think\\facade\\Queue" }),
        );
        let rec = call_record(Some("QueueThink"), Some("push"), "QueueThink::push");

        // Without alias restoration, the original match fails (receiver is QueueThink, not ending in \Queue).
        assert!(!callee_matches(
            "Queue::push",
            &rec.callee,
            rec.receiver.as_deref(),
            rec.method.as_deref()
        ));
        // After import-alias restoration it should hit FKB's `Queue::push` umbrella-name pattern.
        assert!(aliased_callee_matches(&ws, "Queue::push", &rec));
    }

    #[test]
    fn alias_absent_keeps_original_behavior() {
        let ws = GraphWorkspace::new(ProjectId(1));
        let rec = call_record(Some("Db"), Some("name"), "Db::name");
        assert!(aliased_callee_matches(&ws, "Db::name", &rec));

        let rec2 = call_record(Some("UnknownThing"), Some("push"), "UnknownThing::push");
        assert!(!aliased_callee_matches(&ws, "Queue::push", &rec2));
    }

    #[test]
    fn leading_wildcard_matches_class_suffix() {
        // The capability interface `wrapper` pattern (`*CacheService`) should match class FQNs of "any prefix + that suffix",
        // i.e. a framework cache facade, after magic dispatch, can only know the convention "a wrapper method named `*CacheService::get`".
        let ws = GraphWorkspace::new(ProjectId(1));
        let rec = call_record(
            Some("crmeb\\services\\CacheService"),
            Some("get"),
            "crmeb\\services\\CacheService::get",
        );
        assert!(aliased_callee_matches(&ws, "*CacheService::get", &rec));
        // The synonymous short-name receiver (no import alias) should also hit (tail umbrella-name match).
        let rec2 = call_record(Some("CacheService"), Some("get"), "CacheService::get");
        assert!(aliased_callee_matches(&ws, "*CacheService::get", &rec2));
        // A class without that suffix (e.g. the framework facade `Cache`) should not hit.
        let rec3 = call_record(Some("Cache"), Some("get"), "Cache::get");
        assert!(!aliased_callee_matches(&ws, "*CacheService::get", &rec3));
        // The existing `*Cache` umbrella still matches the same-named model `app\model\other\Cache` (no regression).
        let rec4 = call_record(
            Some("app\\model\\other\\Cache"),
            Some("get"),
            "app\\model\\other\\Cache::get",
        );
        assert!(aliased_callee_matches(&ws, "*Cache::get", &rec4));
    }

    #[test]
    fn alias_resolution_is_global_heuristic_safe() {
        // A non-existent alias should not change the match result (no regression).
        let ws = GraphWorkspace::new(ProjectId(1));
        let rec = call_record(Some("QueueThink"), Some("push"), "QueueThink::push");
        // With no imports table written, QueueThink cannot be resolved -> still no match.
        assert!(!aliased_callee_matches(&ws, "Queue::push", &rec));
        // But the bare-method pattern (`*:dispatch`) is unaffected by the receiver.
        assert!(aliased_callee_matches(
            &ws,
            "*:dispatch",
            &call_record(None, Some("dispatch"), "->dispatch")
        ));
    }

    #[test]
    fn owner_class_extracted_from_call_context() {
        use crate::eval::{Evaluator, MatchCtx};
        use gt_domain::model::ValueSource;
        let ws = GraphWorkspace::new(ProjectId(1));
        let mut rec = call_record(Some("QueueTrait"), Some("dispatch"), "QueueTrait::dispatch");
        // `owner_fqn` is the call site's method's fully qualified name; dropping the trailing ::method gives the owning class.
        rec.owner_fqn = "app\\services\\order\\StoreOrderServices::createOrder".to_string();
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let vs = ValueSource {
            owner_class: Some(true),
            ..Default::default()
        };
        assert_eq!(
            ev.string(&vs),
            Some("app\\services\\order\\StoreOrderServices".to_string())
        );
    }

    #[test]
    fn require_class_rejects_non_class_arg_then_fallback() {
        use crate::eval::{Evaluator, MatchCtx};
        use gt_domain::model::{ValueSource, ResolveAs};
        let ws = GraphWorkspace::new(ProjectId(1));
        // `$action` is not a class in the codebase -> class_const + require_class should return None.
        let rec = call_record(Some("QueueTrait"), Some("dispatch"), "QueueTrait::dispatch");
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let vs = ValueSource {
            arg: Some(0),
            resolve: Some(ResolveAs::ClassConst),
            require_class: Some(true),
            literal: None,
            ..Default::default()
        };
        // CallRecord has no real arguments; here directly verify the owner_class fallback path is usable.
        let fb = ValueSource {
            owner_class: Some(true),
            ..Default::default()
        };
        let mut rec2 = rec.clone();
        rec2.owner_fqn = "app\\services\\Foo::bar".to_string();
        let ev2 = Evaluator::new(&ws, MatchCtx::Call(&rec2));
        // The main source (arg0, no args) cannot be taken; fall back to owner_class.
        let value = ev.string(&vs).or_else(|| ev2.string(&fb));
        assert_eq!(value, Some("app\\services\\Foo".to_string()));
    }
}

