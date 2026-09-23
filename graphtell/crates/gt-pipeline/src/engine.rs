//! FKB 规则引擎：选择器匹配 + 绑定动作执行。
//!
//! 内核不认识任何框架 —— 它只认识"选择器"与"绑定"这两个抽象概念，
//! 具体语义全部来自 FKB YAML（依赖倒置 + 开闭原则）。

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

// ---------------------------------------------------------------- 命名空间

/// 已知命名空间分隔符的并集：PHP `\`、Java/JS `.`、路径式 `/`、C++/Ruby `:`。
///
/// 内核做 FQN 归一与短名提取时按**并集**处理，不假设某一门语言用哪个符号。
/// "本语言分隔符"由 [`gt_domain::port::LanguageParser::namespace_separator()`]
/// 提供，并经 [`gt_domain::model::NamespacePolicy`] 随流水线传递到每个子工程
/// （见 [`crate::context::PipelineContext::lang_policy_for_sub`]），拼接与解析
/// 都按该语言的分隔符取值 —— 内核不认识任何具体语言。
pub const NS_SEPARATORS: [char; 4] = ['\\', '.', '/', ':'];

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
    // 单冒号形式：`*:dispatch` 表示「任意接收者、方法 dispatch」。
    if let Some(pos) = alt.find(':') {
        let (r, m) = (&alt[..pos], &alt[pos + 1..]);
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

/// 在匹配前，把调用点的 `receiver` / `callee` 经由 **import 别名** 还原（通用，不针对任何框架）。
///
/// 例：`use think\facade\Queue as QueueThink;` 后写 `QueueThink::push()`，receiver `QueueThink`
/// 被还原成 `think\facade\Queue`，从而能命中 FKB 里 `Queue::push` 这类「以伞名结尾」的模式
/// （`recv_matches` 的尾部 `\Queue` 匹配）。别名查的是 P2 写进 `imports` 符号表的全局索引。
///
/// 若没有对应别名，调用点原样匹配，行为与此前完全一致（无回归）。
pub fn aliased_callee_matches(ws: &GraphWorkspace, pattern: &str, rec: &CallRecord) -> bool {
    let (recv, method, callee) = resolve_aliased_call(ws, rec);
    callee_matches(pattern, &callee, recv.as_deref(), method.as_deref())
}

/// 把调用点的 receiver / callee 还原成别名对应的 FQN。
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
    // 前导分隔符折叠：Java 的 `com.x.X` 与 PHP 的 `\X` 统一去掉前缀分隔符。
    let r = r.trim_start_matches(|c| NS_SEPARATORS.contains(&c));
    if r.eq_ignore_ascii_case(pattern) {
        return true;
    }
    // 尾部匹配（伞名）：`Queue` 匹配 `think\facade\Queue`，`Service` 匹配 `com.x.Service`。
    // 分隔符取已知语言的并集，不假设某门语言用哪个符号（内核零语言知识）。
    if r.len() > pattern.len() {
        for sep in NS_SEPARATORS {
            let prefix = format!("{}{}", sep, pattern);
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

/// 调用点是否匹配选择器。
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

/// 配置条目是否匹配选择器（支持 `file` 中含 `{locale}` 的占位与 `*` 通配）。
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
        Predicate::EntryArityGte(n) => match mctx {
            // 只放行「值是数组且元素数 >= n」的配置条目：
            // 既排除数组展开出的标量叶子条目（`listen.evt.0`），也排除空数组条目。
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
            Some(n) => n.properties.get(name).and_then(|v| v.as_str()) == Some(value.as_str()),
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
        // P6 的选择器作用在**图节点**上：此时"出处"是节点自己的声明位置，而不是
        // identity / FQN 字符串（那会让 `locations[].file` 塞进一个 FQN，前端跳转失效）。
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

/// 展开表：**一条调用 → N 个语义节点**（如 REST 资源路由）。
///
/// 顺序执行每个「被允许」的变体，各自用同一份 `identity` / `fields` / `link`
/// 合成一个节点；变体带来的差异(method / 路径后缀 / 入口方法)由
/// `{ expand_method: true }` / `{ expand_entry: true }` 注入。
/// 返回**最后一个**变体的节点（供同一规则内后续 binding 的 `@self` 引用）。
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

/// 展开表里**实际生效**的动作名集合；返回 `None` 表示「未声明过滤，全部生效」。
///
/// 过滤条件来自**同一语句行**上的链式调用 —— `Route::resource(...)->except(['read'])`
/// 被解析成同一行内方法名为 `except` 的另一个调用点，其首个实参即动作名数组。
/// 这样内核不需要认识任何框架的 `only` / `except` 语义（名字由 FKB 给出）。
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



/// 变体感知的求值器：`Synthesize.expand` 逐条执行时，把当前变体注入求值上下文，
/// 使 `{ expand_method: true }` / `{ expand_entry: true }` 取到该变体的值。
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
            format!("规则 {} 未能算出 identity", rule.id),
            location_of(ctx, mctx),
        );
        return matched;
    };

    // 把 `side`（frontend / backend）收进身份作用域，避免同名 key 前后端合并。
    // 展示名 `value` 保持干净（仍是 key 原文，如 `token`），只在幂等合并键上区分。
    // 适用：缓存 / 配置 / 事件 / 队列 / Topic 等「进程外中介」与资产类节点
    // （ConfigKey / Event / Queue / Topic / Cache …），它们各自属于某一端，
    // 前端 `uni.setStorageSync('token')` 与后端 `Cache::get('token')` 因此是
    // 两个独立节点，而非共用一个。
    //
    // **唯一例外是 HttpContract（API 契约）**：前端调用与后端路由的同名契约本就该
    // 合并（那是前后端链路打通的关键），绝不能按端拆分。契约身份用 `CONTRACT_ID`
    // 标记，这里据此放行。
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

    // 子类型提升为"种类"：`node: ExternalSystem, subtype: Cache` → kind = `Cache`。
    // 不再写 `ExternalSystem` 伞类别：每个语义节点都以其**具体种类**（Event / Queue /
    // Cache / Topic / Table / ConfigKey…）作为 kind，视角直接按 kind 切换；`category`
    // 仅保留为"等于 kind"的冗余标签（第一类语义节点本就等同于 kind）。
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
    // 记录来源，便于冲突诊断
    props["sources"] = json!([rule.id]);
    new_node.properties = props.clone();

    // 调用语句原文（CallSite 由 P2 从 span 提取）。让边的证据与合成节点的出处
    // 都能直接显示"这条边 / 这个配置是怎么读出来的"，不必自己打开文件核对。
    let snippet = ctx
        .ws
        .node(matched)
        .and_then(|n| n.properties.get("snippet"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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
            snippet.clone(),
        );
    }

    if !created {
        // 幂等合并：**只补 `sources`**，绝不整份回写 properties。
        //
        // 内存里的 `node.properties` 已被 `append_location` 累积了 `locations` 数组；
        // 若把它整份作为 property patch 推给落库，而 `location_patches` 又会逐条追加，
        // 同一份 locations 就会被写两遍 —— 落库时 `merge_into` 是浅合并，
        // 整份 patch 里的 `locations` 会先覆盖、随后又被逐条追加
        // （实测 7 个位置变成 14 条，"整批重复"）。
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
            ctx.ws.patch_properties(node_id, json!({ f.name.clone(): v }));
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
                // 记录传播种子：owner 是「动作发出方」，其调用方沿 CALLS 链也应被识别为同一语义动作的发出方。
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
                // 目标方法名：数组式 handler 的 `[Ctrl::class, 'method']` 在这里给出。
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
                // 主来源里「可当作字符串目标」的项（跳过数组/空，正是队列 `arg:0` 是数组的场景）。
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
                    // 主来源解析不出（如队列 `arg:0` 是数组/动作名而非类），
                    // 整体退回 `to_fallback`（如 `receiver_class`）继续找消费方。
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
                            // 交给 P7：路由 handler 可能需要按约定拼装后才知道是否存在
                            ctx.ws.pending_links.push(crate::workspace::PendingLink {
                                from: node_id,
                                kind: link.kind.clone(),
                                raw: resolved.clone(),
                                // 带上 FKB 显式给出的入口方法（资源路由的 `expand_entry`），
                                // 否则 P7 只有类名可用，会退化到「类级回退」。
                                method: method.clone(),
                                // `resolve` 既可以写在 link 上，也可以写在 to 的值来源里
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

    // alias 注册
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

/// 全限定名的候选形态。
///
/// 源码里指涉一个类有两种写法：**标识符引用**（`app\common\X`、`com.example.X`）
/// 与**字符串字面量**（`'app\\common\\X'`）。后者取到的是源码原文，命名空间分隔符
/// 常被转义成连续两个字符，与图里已经建好的 FQN 对不上，边就连不上。
///
/// 这里做的是**语言无关**的归一：折叠连续重复的命名空间分隔符、去掉前导分隔符
/// （全局命名空间写法）。内核不假设某门语言用哪个符号 —— 分隔符取已知语言的并集。
/// 顺序上先试原值，因此不会改变任何原本就能匹配的情形。
fn fqn_variants(fqn: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !s.is_empty() && !out.contains(&s) {
            out.push(s);
        }
    };
    push(fqn.to_string());

    // 折叠连续重复的分隔符：`app\\common\\X` ≡ `app\common\X`
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

    // 去前导分隔符：`\app\X` ≡ `app\X`
    for cand in [fqn.to_string(), collapsed] {
        push(cand.trim_start_matches(|c| NS_SEPARATORS.contains(&c)).to_string());
    }
    out
}

/// 取某子工程（或全局兜底）的消费入口方法名候选（FKB `entry_methods`）。
fn entry_methods_for(ctx: &PipelineContext, sub: Option<SubProjectId>) -> Vec<String> {
    if let Some(s) = sub {
        if let Some(v) = ctx.entry_methods.get(&s.get()) {
            return v.clone();
        }
    }
    ctx.entry_methods_default.clone()
}

/// 解析边目标：类 → 优先其 `handle`/`fire`/`doJob` 方法，否则类本身。
///
/// `doJob` 是 ThinkPHP/CRMEB 队列 Job 类的通用入口（`QueueTrait` 约定），
/// 与 Laravel 的 `handle`、Symfony 的 `__invoke` 并列纳入。
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
    // FKB 显式指定了方法名（如数组式 handler 的 `[Ctrl::class, 'method']`）—— 优先于约定入口
    if let Some(m) = method.filter(|m| !m.is_empty()) {
        let method_fqn = policy.join_member(&fqn_of(ctx, class_id), m);
        if let Some(id) = ctx.ws.find_by_name(&method_fqn) {
            return Some(id);
        }
    }
    // 消费入口方法：优先 FKB 的 `entry_methods`（框架知识），未声明时回退到
    // 跨框架常见入口名默认集。新框架只需在 YAML 里声明，不必改内核。
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

/// `ToTarget` 边生成：把一组值来源解析为「目标节点」并建边（查不到则降级为 PendingLink）。
///
/// 与内联逻辑一致，供主 `to` 解析失败后的 `to_fallback` 复用。
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
    // 目标方法名（与主路径一致，供 `to_fallback` 场景复用）
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
        // 先解析（临时 Evaluator，用后即弃，避免与下方 `find_target_node(ctx, …)` 的整结构借用冲突）。
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
        // 补齐 `Route::group('v2', ...)` 的路由组前缀。
        // 不补的话 `Route::group('v2', fn(){ Route::get('order/x') })` 会建成
        // `GET /order/x`，与真实请求路径 `/v2/order/x` 不符，也无法与前端契约汇聚。
        //
        // 注意：`path` 已由 `ValueSource.normalize`（含 `leading_slash`）处理过，
        // 而 `IdentitySpec.normalize` 通常为空 —— 所以这里必须**自己保住前导斜杠**，
        // 把组前缀插到斜杠之后，而不是简单前置（否则会产出 `GET v2/order/x`）。
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
        // 展开变体的路径后缀（`/create` / `/:id` / `/:id/edit`）：必须接在
        // **组前缀之后**，否则 `Route::group('cms')` + `resource('cms')` 的
        // `create` 会算成 `/create/cms` 而不是 `/cms/cms/create`。
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
            span: Span::default(),
            file: String::new(),
            sub: None,
            language: Language::default(),
        }
    }

    #[test]
    fn alias_resolution_makes_facade_pattern_match() {
        let mut ws = GraphWorkspace::new(ProjectId(1));
        // 模拟 P2 写进的 import 符号表：`use think\facade\Queue as QueueThink;`
        ws.put_symbol(
            ProjectId(1),
            "imports",
            "queuethink",
            serde_json::json!({ "fqn": "think\\facade\\Queue" }),
        );
        let rec = call_record(Some("QueueThink"), Some("push"), "QueueThink::push");

        // 未做别名还原时，原始匹配失败（receiver 是 QueueThink，不是 \Queue 结尾）。
        assert!(!callee_matches(
            "Queue::push",
            &rec.callee,
            rec.receiver.as_deref(),
            rec.method.as_deref()
        ));
        // 经 import 别名还原后应命中 FKB 的 `Queue::push` 伞名模式。
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
    fn alias_resolution_is_global_heuristic_safe() {
        // 不存在的别名不应改变匹配结果（无回归）。
        let ws = GraphWorkspace::new(ProjectId(1));
        let rec = call_record(Some("QueueThink"), Some("push"), "QueueThink::push");
        // 没有写 imports 表，QueueThink 解析不到 → 仍不匹配。
        assert!(!aliased_callee_matches(&ws, "Queue::push", &rec));
        // 但裸方法模式（`*:dispatch`）不受 receiver 影响。
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
        // `owner_fqn` 是「调用方方法」的完全限定名，去掉末尾 ::method 即所属类。
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
        // `$action` 不是代码库里的类 → class_const + require_class 应返回 None。
        let rec = call_record(Some("QueueTrait"), Some("dispatch"), "QueueTrait::dispatch");
        let ev = Evaluator::new(&ws, MatchCtx::Call(&rec));
        let vs = ValueSource {
            arg: Some(0),
            resolve: Some(ResolveAs::ClassConst),
            require_class: Some(true),
            literal: None,
            ..Default::default()
        };
        // CallRecord 没有真实实参，这里直接验证 owner_class 兜底路径可用。
        let fb = ValueSource {
            owner_class: Some(true),
            ..Default::default()
        };
        let mut rec2 = rec.clone();
        rec2.owner_fqn = "app\\services\\Foo::bar".to_string();
        let ev2 = Evaluator::new(&ws, MatchCtx::Call(&rec2));
        // 主源（arg0，无实参）取不到，回退到 owner_class。
        let value = ev.string(&vs).or_else(|| ev2.string(&fb));
        assert_eq!(value, Some("app\\services\\Foo".to_string()));
    }
}

