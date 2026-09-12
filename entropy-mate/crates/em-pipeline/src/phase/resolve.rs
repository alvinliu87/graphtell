//! P7 Resolve：建立动态边。
//!
//! 核心是**解析漏斗 + 不动点迭代**：
//!
//! | 层级 | 来源                                  | 置信度 |
//! | ---- | ----------------------------------- | --- |
//! | L1   | 字面 FQN `make(StoreOrderServices::class)` | 1.0 |
//! | L2   | 容器注册表 `provider.php`                | 0.95 |
//! | L3   | 别名索引（Facade / 事件名 / 获取器）           | 0.85 |
//! | L4   | 约定（命名空间拼接、类名推导）                    | 0.8  |
//! | L5   | 常量传播                                | 0.6  |
//! | L6   | 与有限全集求交（schema 的 203 张表）            | 0.7  |
//! | L7   | 完全未知                                | 0.3  |
//!
//! 迭代判据：**候选集变大才继续下一轮**，直到收敛；收敛后再统一建边。

use std::collections::{HashMap, HashSet};

use em_domain::model::{
    EdgeKind, FactValue, NewEdge, NodeId, Phase, ResolveStrategy, ResolveTier, Resolution, Severity,
};
use em_domain::port::KnowledgeProvider;
use serde_json::Value;
use tracing::info;

use crate::context::PipelineContext;
use crate::engine::callee_matches;

/// 一次动态解析请求（去重后参与不动点迭代）。
#[derive(Debug, Clone)]
struct Locator {
    owner: NodeId,
    strategy: ResolveStrategy,
    raw: String,
    file: String,
    line: u32,
}

/// 执行 Resolve。
pub fn run(ctx: &mut PipelineContext, kb: &dyn KnowledgeProvider) {
    let phase = Phase(Phase::RESOLVE.to_string());

    // ① 先解析 P5 遗留的待定链接（路由 handler 等）
    resolve_pending_links(ctx, &phase);

    // ② 收集 FKB 声明的动态调用
    let resolvers: Vec<(String, ResolveStrategy)> = ctx
        .frameworks
        .values()
        .flatten()
        .filter_map(|id| kb.by_id(id))
        .flat_map(|fk| fk.resolvers.iter())
        .map(|r| (r.call.clone().unwrap_or_default(), r.strategy))
        .filter(|(call, _)| !call.is_empty())
        .collect();

    // 按 (strategy, raw) 去重：同一个字符串只需解析一次
    let mut uniques: Vec<Locator> = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();
    for call in ctx.ws.calls.iter() {
        for (pattern, strategy) in &resolvers {
            if !callee_matches(pattern, &call.callee, call.receiver.as_deref(), call.method.as_deref())
            {
                continue;
            }
            let raw = call.args.first().map(fact_to_string).unwrap_or_default();
            if raw.is_empty() || raw.len() > 256 {
                continue;
            }
            let key = format!("{strategy:?}:{raw}");
            if keys.insert(key) {
                uniques.push(Locator {
                    owner: call.owner,
                    strategy: *strategy,
                    raw,
                    file: call.file.clone(),
                    line: call.span.start_line,
                });
            }
        }
    }

    // ③ 不动点迭代：直到没有 locator 的候选集再变大
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

    // ④ 收敛后统一建边与记账
    for loc in &uniques {
        let key = format!("{:?}:{}", loc.strategy, loc.raw);
        if let Some(res) = resolutions.get(&key) {
            apply_resolution(ctx, loc, res, &phase);
        }
    }

    info!("P7 解析完成：{} 个解析点，{} 轮迭代", uniques.len(), rounds);
}

fn resolve_once(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match loc.strategy {
        ResolveStrategy::Container => resolve_container(ctx, loc),
        ResolveStrategy::Event => resolve_event(ctx, loc),
        ResolveStrategy::Facade => resolve_facade(ctx, loc),
        ResolveStrategy::Accessor => resolve_accessor(ctx, loc),
        ResolveStrategy::Handler => resolve_handler(ctx, loc),
    }
}

/// 容器解析：L1 字面 → L2 注册表 → L4 约定 → L6 与类全集求交。
fn resolve_container(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let raw = loc.raw.trim_start_matches('\\');

    // L1：字面 FQN
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Resolution::resolved(ResolveTier::Exact, id, format!("字面 FQN {raw}"));
    }
    if let Some(fqn) = ctx.ws.resolve_short_name(raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Exact, id, format!("短名解析 {raw} → {fqn}"));
        }
    }

    // L2：容器注册表（链式引用靠外层不动点收敛）
    if let Some(Value::Object(entry)) = ctx.ws.get_symbol("container_bindings", raw).cloned() {
        let target = entry
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim_start_matches('\\')
            .to_string();
        if !target.is_empty() {
            if let Some(id) = ctx
                .ws
                .find_by_name(&target)
                .or_else(|| ctx.ws.resolve_short_name(&target).and_then(|f| ctx.ws.find_by_name(&f)))
            {
                return Resolution::resolved(
                    ResolveTier::Registry,
                    id,
                    format!("provider.php 绑定 {raw} → {target}"),
                );
            }
            return Resolution::unknown(format!("间接绑定 {raw} → {target}"));
        }
        return Resolution::unknown(format!("闭包绑定 {raw}"));
    }

    // L4：约定（FQN 字符串）
    if raw.contains('\\') {
        if let Some(id) = ctx.ws.find_by_name(raw) {
            return Resolution::resolved(ResolveTier::Convention, id, format!("约定 {raw}"));
        }
    }

    // L6：与类短名全集求交
    if let Some(fqn) = ctx.ws.resolve_short_name(raw) {
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Resolution::resolved(ResolveTier::Intersection, id, format!("与类全集求交 {raw}"));
        }
    }

    Resolution::unknown(format!("容器无法解析 {raw}"))
}

/// 事件触发：查 P6 注册的 `by_alias(event_name, x)`。
fn resolve_event(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventTriggerUnresolved",
                Severity::Warning,
                format!(
                    "event('{}') 未找到注册的事件节点 —— 若 event.php 里该键为空数组，\
                     监听器会被整体误判为死代码",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// Facade：查 P3 装载的 FacadeMap。
fn resolve_facade(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let receiver = loc.raw.trim_start_matches('\\');
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
            // vendor 被 P0 排除，目标不在图中是**预期**的，不能当错误报
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

/// 获取器：`$order->status_text` → `getStatusTextAttr`（复合键，避免撞名）。
fn resolve_accessor(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("accessor", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("获取器 {}", loc.raw)),
        None => Resolution::unknown(format!("获取器 {} 未注册", loc.raw)),
    }
}

/// 路由 handler：`'Login/appleLogin'` → `app\api\controller\Login::appleLogin`。
fn resolve_handler(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match resolve_handler_target(ctx, &loc.raw, &loc.file) {
        Some((id, note)) => Resolution::resolved(
            ResolveTier::Convention,
            id,
            format!("handler {}（{}）", loc.raw, note),
        ),
        None => Resolution::unknown(format!("无法解析 handler {}", loc.raw)),
    }
}

/// 解析路由 handler，返回 (目标节点, 命中说明)。
///
/// 关键：**方法不存在时回退到类**。控制器方法常常继承自基类，
/// 要求 `Controller::method` 一定存在会让整条路由链断掉。
pub fn resolve_handler_target(
    ctx: &PipelineContext,
    raw: &str,
    file: &str,
) -> Option<(NodeId, &'static str)> {
    let raw = raw.trim_start_matches('\\');
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Some((id, "完全限定名直接命中"));
    }

    let (controller_part, method) = match raw.split_once('/') {
        Some((c, m)) => (c.to_string(), m.to_string()),
        None => (raw.to_string(), String::new()),
    };

    let app_seg = route_app_segment(file);
    // CRMEB 用点号表示控制器层级：`v1.agent.AgentManage` → `v1\agent\AgentManage`
    let controller = controller_part.replace('.', "\\");
    let mut bases = vec![
        format!("app\\{}\\controller\\{}", app_seg, controller),
        format!("app\\{}\\controller\\v1\\{}", app_seg, controller),
    ];
    for seg in ["api", "adminapi", "outapi", "kefuapi", "index"] {
        bases.push(format!("app\\{}\\controller\\{}", seg, controller));
    }

    if !method.is_empty() {
        for base in &bases {
            if let Some(id) = ctx.ws.find_by_name(&format!("{}::{}", base, method)) {
                return Some((id, "方法精确命中"));
            }
        }
    }
    for base in &bases {
        if let Some(id) = ctx.ws.find_by_name(base) {
            return Some((id, "类命中（方法未在图内，可能继承自基类）"));
        }
    }

    let short = controller.rsplit('\\').next().unwrap_or(&controller);
    if let Some(fqn) = ctx.ws.resolve_short_name(short) {
        if !method.is_empty() {
            if let Some(id) = ctx.ws.find_by_name(&format!("{}::{}", fqn, method)) {
                return Some((id, "短名方法命中"));
            }
        }
        if let Some(id) = ctx.ws.find_by_name(&fqn) {
            return Some((id, "短名类命中"));
        }
    }
    None
}

/// 从路由文件路径推断应用段：`app/api/route/pc.php` → `api`。
fn route_app_segment(file: &str) -> String {
    let parts: Vec<&str> = file.split('/').collect();
    if let Some(pos) = parts.iter().position(|p| *p == "route") {
        if pos > 0 {
            return parts[pos - 1].to_string();
        }
    }
    parts
        .first()
        .copied()
        .unwrap_or("api")
        .to_string()
}

fn apply_resolution(ctx: &mut PipelineContext, loc: &Locator, res: &Resolution, phase: &Phase) {
    let kind = match loc.strategy {
        ResolveStrategy::Event => EdgeKind(EdgeKind::TRIGGERS.to_string()),
        ResolveStrategy::Handler => EdgeKind(EdgeKind::HANDLED_BY.to_string()),
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
                "evidence": res.evidence,
            }),
        });
    }
    if res.candidates.is_empty() && loc.strategy != ResolveStrategy::Facade {
        ctx.ws.diagnose(
            phase,
            "UnresolvedLink",
            Severity::Info,
            format!("{:?} 解析失败: {}", loc.strategy, res.evidence),
            Some(format!("{}:{}", loc.file, loc.line)),
        );
    }
}

/// 解析 P5 遗留的待定链接（路由 handler / 事件监听器等）。
fn resolve_pending_links(ctx: &mut PipelineContext, phase: &Phase) {
    let pending = std::mem::take(&mut ctx.ws.pending_links);
    for link in &pending {
        let target = match link.resolve {
            em_domain::model::ResolveAs::HandlerPattern => {
                let (file, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                resolve_handler_target(ctx, &link.raw, file).map(|(id, _)| id)
                    .or_else(|| {
                        ctx.ws
                            .resolve_short_name(&link.raw)
                            .and_then(|f| ctx.ws.find_by_name(&f))
                    })
            }
            _ => ctx
                .ws
                .find_by_name(&link.raw)
                .or_else(|| ctx.ws.resolve_short_name(&link.raw).and_then(|f| ctx.ws.find_by_name(&f))),
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
