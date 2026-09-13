//! FKB 规则引擎：选择器匹配 + 绑定动作执行。
//!
//! 内核不认识任何框架 —— 它只认识"选择器"与"绑定"这两个抽象概念，
//! 具体语义全部来自 FKB YAML（依赖倒置 + 开闭原则）。

use em_domain::model::{
    Action, AliasEntry, AnnotateTarget, AnnotationChannel, EdgeKind, FactValue, IdentityKey,
    Language, MergeStrategy, NewAnnotation, NewEdge, NodeId, NodeKind, Phase, Predicate,
    ResolveTier, Rule, Selector, Severity, SubkindSource, SynthesizedKind,
};
use serde_json::{json, Value};

use crate::eval::{Evaluator, MatchCtx};
use crate::normalize::apply_normalize;
use crate::workspace::{CallRecord, ConfigRecord, GraphWorkspace, InheritRecord};
use crate::context::PipelineContext;

// ---------------------------------------------------------------- 选择器

/// callee 模式匹配。
///
/// 支持：
/// * `|` 分隔多个备选
/// * `A::b` / `A->b` 形式（`A` 支持 `*` 与尾部匹配，如 `Db::name` 可匹配 `think\facade\Db::name`）
/// * `A::{b,c}` 方法列表
/// * 裸方法名（匹配 `method`）
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
    // 方法列表：A::{b,c}
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
    if let Some(pos) = alt.find("->") {
        let (r, m) = (&alt[..pos], &alt[pos + 2..]);
        return recv_matches(r, receiver, callee) && meth_matches(m, method);
    }
    // 裸模式：优先按方法名匹配，其次按完整 callee
    if let Some(m) = method {
        if m.eq_ignore_ascii_case(alt) {
            return true;
        }
    }
    callee.eq_ignore_ascii_case(alt) || callee.ends_with(&format!("::{}", alt))
}

fn recv_matches(pattern: &str, receiver: Option<&str>, callee: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let Some(r) = receiver else { return false };
    let r = r.trim_start_matches('\\');
    if r.eq_ignore_ascii_case(pattern) {
        return true;
    }
    // 尾部匹配：`Db` 匹配 `think\facade\Db`
    r.len() > pattern.len()
        && r[1..].eq_ignore_ascii_case(pattern)
        && r.as_bytes()[r.len() - pattern.len() - 1] == b'\\'
        || callee.eq_ignore_ascii_case(pattern)
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

/// 调用点是否匹配选择器。
pub fn matches_call(sel: &Selector, rec: &CallRecord, ws: &GraphWorkspace) -> bool {
    let (callee_pat, preds) = match sel {
        Selector::Call { callee, r#where } => (callee, r#where),
        _ => return false,
    };
    if let Some(pat) = callee_pat {
        if !callee_matches(pat, &rec.callee, rec.receiver.as_deref(), rec.method.as_deref()) {
            return false;
        }
    }
    let ev = Evaluator::new(ws, MatchCtx::Call(rec));
    preds
        .iter()
        .all(|p| eval_predicate(p, rec.node, Some(MatchCtx::Call(rec)), ws, &ev))
}

/// 配置条目是否匹配选择器（支持 `file` 中含 `{locale}` 的占位与 `*` 通配）。
pub fn matches_config(sel: &Selector, rec: &ConfigRecord) -> bool {
    let (file_pat, key_pat) = match sel {
        Selector::ConfigEntry { file, key_path } => (file, key_path),
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
    true
}

/// 路径匹配：支持 `*` 通配与 `{locale}` 占位（占位内容不参与匹配）。
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let normalized = pattern.replace("{locale}", "*");
    wildcard_matches(&normalized, path)
}

/// 从路径中抽取 `{locale}` 占位对应的片段。
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

/// 继承是否匹配选择器。
pub fn matches_inherit(sel: &Selector, rec: &InheritRecord, ws: &GraphWorkspace) -> bool {
    let (base, with_property) = match sel {
        Selector::Inheritance { base, with_property } => (base, with_property),
        _ => return false,
    };
    if let Some(pat) = base {
        // 直接基类匹配，或**传递继承**匹配（`X extends BaseModel extends Model`）
        let direct = pat
            .split('|')
            .map(str::trim)
            .any(|p| recv_matches(p, Some(&rec.base), &rec.base));
        if !direct {
            // 传递继承按**名字链**判定，容忍 vendor 里的中间/末端类缺失
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

/// 图节点是否匹配选择器。
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

/// 谓词求值。
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
        Predicate::NameMatches(sub) => {
            let Some(n) = ws.node(node) else { return false };
            let hay = n.identity.as_ref().map(|i| i.value.clone()).unwrap_or_else(|| n.name.clone());
            hay.to_ascii_lowercase().contains(&sub.to_ascii_lowercase())
        }
        Predicate::PropertyIs { name, value } => match ws.node(node) {
            Some(n) => n.properties.get(name).and_then(|v| v.as_str()) == Some(value.as_str()),
            None => false,
        },
        Predicate::ArgCount(n) => match mctx {
            Some(MatchCtx::Call(c)) => c.args.len() == *n,
            _ => false,
        },
    }
}

fn column_matches(column: &str, wanted: &str) -> bool {
    let c = column.to_ascii_lowercase();
    let w = wanted.to_ascii_lowercase();
    c == w || c.ends_with(&format!("_{}", w))
}

/// 查 schema 表的列（兼容带/不带前缀的表名）。
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

// ---------------------------------------------------------------- 绑定执行

/// 执行一条规则的全部动作。
#[allow(clippy::too_many_arguments)]
pub fn exec_binding(
    ctx: &mut PipelineContext,
    rule: &Rule,
    mctx: MatchCtx,
    matched: NodeId,
    owner: NodeId,
    sub: Option<em_domain::model::SubProjectId>,
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
                        format!("规则 {} 的标注目标未能解析", rule.id),
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
        }
    }
}

/// `Annotate.phase` 只写 `Pre` / `Post`，需要拼成完整阶段名。
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
        MatchCtx::Node(id) => ctx.ws.node(id).and_then(|n| {
            n.identity
                .as_ref()
                .map(|i| i.value.clone())
                .or_else(|| n.fqn.clone())
        }),
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
                tracing::debug!("规则 {} 的 from_field 未命中任何节点", rule.id);
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

fn exec_synthesize(
    ctx: &mut PipelineContext,
    rule: &Rule,
    s: &em_domain::model::SynthesizeAction,
    mctx: MatchCtx,
    matched: NodeId,
    owner: NodeId,
    sub: Option<em_domain::model::SubProjectId>,
    phase: &Phase,
) -> NodeId {
    let ev = Evaluator::new(&ctx.ws, mctx);
    let identity = compute_identity(&ev, &s.identity);
    let Some(identity) = identity else {
        ctx.ws.diagnose(
            phase,
            "IdentityUnresolved",
            Severity::Warning,
            format!("规则 {} 未能算出 identity", rule.id),
            location_of(ctx, mctx),
        );
        return matched;
    };

    // 子类型提升为"种类"：`node: ExternalSystem, subtype: Cache` → kind = `Cache`；
    // 同时把伞形名（`ExternalSystem`）记入 `category`，供视角按**类别**分组 / 筛选。
    // 这样每个语义节点都有**具体种类**（Table / ConfigKey / Cache / Event / Queue…），
    // 命名粒度一致，不再出现"有的具体、有的笼统"。
    let kind = match &s.subtype {
        Some(sub) if !sub.is_empty() => NodeKind(sub.clone()),
        _ => s.node.clone(),
    };
    let mut props = json!({ "category": s.node.as_str() });

    let mut new_node = crate::workspace::synthesized_node(
        ctx.project.id,
        kind.as_str(),
        identity.clone(),
        sub,
        phase,
        s.confidence * rule.confidence,
        &ctx.language_of(sub),
        em_domain::model::Span::default(),
    );
    // 记录来源，便于冲突诊断
    props["sources"] = json!([rule.id]);
    new_node.properties = props.clone();

    let (node_id, created) = ctx.ws.get_or_create_synthesized(new_node);

    // 记录"这个语义对象在源码里的出处"，供前端给出**多位置列表**跳转
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
            Some(format!("规则 {}", rule.id)),
        );
    }

    if !created {
        // 幂等合并：追加来源
        if let Some(node) = ctx.ws.node(node_id) {
            let mut p = node.properties.clone();
            let mut sources = p
                .get("sources")
                .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
                .unwrap_or_default();
            if !sources.contains(&rule.id) {
                sources.push(rule.id.clone());
            }
            p["sources"] = json!(sources);
            ctx.ws.patch_properties(node_id, p);
        }
    }

    // fields
    for f in &s.fields {
        if let Some(acc) = &f.accumulate {
            let key = Evaluator::new(&ctx.ws, mctx).string(&acc.key).unwrap_or_default();
            let value = Evaluator::new(&ctx.ws, mctx).string(&acc.value).unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            ctx.ws
                .patch_properties(node_id, json!({ f.name.clone(): { key: value } }));
            continue;
        }
        let value: Option<Value> = if let Some(sym) = &f.from_symbol_table {
            let key = match &sym.of {
                Some(src) => Evaluator::new(&ctx.ws, mctx).string(src),
                None => Some(identity.value.clone()),
            };
            key.and_then(|k| ctx.ws.get_symbol(&sym.table, &k).cloned())
                .and_then(|v| v.get(&sym.field).cloned())
        } else if let Some(src) = &f.value {
            match Evaluator::new(&ctx.ws, mctx).string(src) {
                Some(v) => Some(json!(v)),
                None => None,
            }
        } else {
            None
        };
        if let Some(v) = value {
            ctx.ws.patch_properties(node_id, json!({ f.name.clone(): v }));
        }
    }

    // link
    if let Some(link) = &s.link {
        let confidence = link.confidence.unwrap_or(s.confidence * rule.confidence);
        match link.direction {
            em_domain::model::Direction::Incoming => {
                ctx.ws.add_edge(NewEdge {
                    project_id: ctx.project.id,
                    kind: link.kind.clone(),
                    from_id: owner,
                    to_id: node_id,
                    phase: phase.clone(),
                    confidence,
                    properties: serde_json::json!({
                        "evidence": { "rule": rule.id, "location": location_of(ctx, mctx) },
                    }),
                });
            }
            em_domain::model::Direction::Outgoing => {
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
            em_domain::model::Direction::ToTarget => {
                let items = link
                    .to
                    .as_ref()
                    .map(|src| Evaluator::new(&ctx.ws, mctx).list(src))
                    .unwrap_or_default();
                for item in items {
                    let raw = match item {
                        FactValue::String(s) | FactValue::ClassConst(s) => s,
                        _ => continue,
                    };
                    let resolved = Evaluator::new(&ctx.ws, mctx).resolve_name(&raw);
                    if let Some(target) = find_target_node(ctx, &resolved, &link.kind) {
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
                        // 交给 P7：路由 handler 可能需要按约定拼装后才知道是否存在
                        ctx.ws.pending_links.push(crate::workspace::PendingLink {
                            from: node_id,
                            kind: link.kind.clone(),
                            raw: resolved.clone(),
                            // `resolve` 既可以写在 link 上，也可以写在 to 的值来源里
                            resolve: link.resolve.clone().or_else(|| {
                                link.to.as_ref().and_then(|t| t.resolve)
                            }).unwrap_or(em_domain::model::ResolveAs::ClassConst),
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

    // alias 注册
    if let Some(alias) = &s.alias {
        if let Some(key) = Evaluator::new(&ctx.ws, mctx).string(&alias.key) {
            let qualifier = alias
                .qualifier
                .as_ref()
                .and_then(|q| Evaluator::new(&ctx.ws, mctx).string(q));
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

/// 解析边目标：类 → 优先其 `handle`/`fire` 方法，否则类本身。
fn find_target_node(ctx: &PipelineContext, fqn: &str, _kind: &EdgeKind) -> Option<NodeId> {
    let class_id = ctx
        .ws
        .find_by_name(fqn)
        .or_else(|| ctx.ws.resolve_short_name(fqn).and_then(|f| ctx.ws.find_by_name(&f)))?;
    // 找 handle / fire / __invoke 方法
    for m in ["handle", "fire", "__invoke", "run"] {
        let method_fqn = format!("{}::{}", fqn_of(ctx, class_id), m);
        if let Some(id) = ctx.ws.find_by_name(&method_fqn) {
            return Some(id);
        }
    }
    Some(class_id)
}

fn fqn_of(ctx: &PipelineContext, id: NodeId) -> String {
    ctx.ws
        .node(id)
        .and_then(|n| n.fqn.clone())
        .unwrap_or_default()
}

fn compute_identity(
    ev: &Evaluator,
    spec: &em_domain::model::IdentitySpec,
) -> Option<IdentityKey> {
    let kind = spec.kind.clone();
    if kind.as_str() == SynthesizedKind::CONTRACT_ID {
        let method = spec
            .method
            .as_ref()
            .and_then(|s| ev.string(s))
            .unwrap_or_else(|| "GET".to_string());
        let path = spec.path.as_ref().and_then(|s| ev.string(s))?;
        let path = apply_normalize(&path, &spec.normalize);
        return Some(IdentityKey::contract(&method, &path));
    }
    let raw = spec.value.as_ref().and_then(|s| ev.string(s))?;
    let value = apply_normalize(&raw, &spec.normalize);
    if value.is_empty() {
        return None;
    }
    Some(IdentityKey { kind, value })
}

/// 供外部复用的解析层级判定（P7 用）。
pub fn tier_of(t: ResolveTier) -> ResolveTier {
    t
}

/// 常用的合并策略默认值。
pub fn default_merge() -> MergeStrategy {
    MergeStrategy::MaxByKind
}

/// 语言占位（供编译期校验）。
pub fn _assert_language(_l: &Language) {}
