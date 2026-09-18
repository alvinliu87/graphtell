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

use gt_domain::model::{
    EdgeKind, FactValue, HandlerSpec, Language, NewEdge, NodeId, NodeKind, Phase, ResolveStrategy,
    ResolveTier, Resolution, Severity, SubProjectId,
};
use gt_domain::port::KnowledgeProvider;
use serde_json::Value;
use tracing::info;

use crate::context::PipelineContext;
use crate::workspace::CallRecord;

/// 一次动态解析请求（去重后参与不动点迭代）。
#[derive(Debug, Clone)]
struct Locator {
    owner: NodeId,
    /// 所属方法的 FQN（供按"变量类型"解析时查参数 / 属性类型）。
    owner_fqn: String,
    strategy: ResolveStrategy,
    raw: String,
    /// 变量类型策略用：接收者（`$services` / `$this->services`）。
    receiver: Option<String>,
    /// 变量类型策略用：方法名。
    method: Option<String>,
    /// `EventListen` 策略用：监听器类（arg1，如 `OrderListener::class`）。
    consumer: Option<String>,
    file: String,
    line: u32,
    /// 所属子工程：用于取该子工程的路由 handler 解析规则（FKB 声明）。
    sub: Option<SubProjectId>,
}

/// 执行 Resolve。
pub fn run(ctx: &mut PipelineContext, kb: &dyn KnowledgeProvider) {
    let phase = Phase(Phase::RESOLVE.to_string());

    // ① 先解析 P5 遗留的待定链接（路由 handler 等）
    resolve_pending_links(ctx, &phase);

    // ①.5 通用调用解析：建立 `Calls` 边，把「路由 → handler → 服务方法 → 语义节点」
    //      的调用链真正连通（否则语义边孤立在被调方法上，视图只能靠结构边绕路）。
    resolve_calls(ctx, &phase);

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
            if !crate::engine::aliased_callee_matches(&ctx.ws, pattern, call) {
                continue;
            }
            // 变量类型：按「所属方法 + 接收者 + 方法」建定位点，不依赖参数。
            if *strategy == ResolveStrategy::VariableType {
                let (Some(recv), Some(method)) =
                    (call.receiver.as_deref(), call.method.as_deref())
                else {
                    continue;
                };
                // PHP：接收者须为 `$var` / `$this->prop`（静态 `Foo::m()` 交给 Facade）；
                // Java：字段是裸标识符，直接按字段声明类型解析。
                let is_java = call.language.as_str() == Language::JAVA;
                if (!recv.starts_with('$') && !is_java) || method.is_empty() {
                    continue;
                }
                let raw = format!("{}::{recv}::{method}", call.owner_fqn);
                if keys.insert(format!("{strategy:?}:{raw}")) {
                    uniques.push(Locator {
                        owner: call.owner,
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
            // `EventListen` 需要 arg1（监听器类）作为边的另一端。
            let consumer = if *strategy == ResolveStrategy::EventListen {
                call.args.get(1).map(fact_to_string)
            } else {
                None
            };
            let key = format!("{strategy:?}:{raw}");
            if keys.insert(key) {
                uniques.push(Locator {
                    owner: call.owner,
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
        ResolveStrategy::EventListen => resolve_event_listen(ctx, loc),
        ResolveStrategy::Facade => resolve_facade(ctx, loc),
        ResolveStrategy::Accessor => resolve_accessor(ctx, loc),
        ResolveStrategy::Handler => resolve_handler(ctx, loc),
        ResolveStrategy::VariableType => resolve_variable_type(ctx, loc),
    }
}

/// 变量类型解析：`$svc->method()` / `$this->prop->method()` → `Type::method`。
///
/// 类型来源（由 P2 记录）：
/// * `$var` —— 所在方法的**参数类型提示**（ThinkPHP 控制器/服务常见 DI 写法）；
/// * `$this->prop` —— 构造器注入（`__construct(T $x){ $this->p = $x; }`）。
///
/// 找不到类型就返回 unknown —— **宁可缺边，不可错边**。
fn resolve_variable_type(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let Some(recv) = loc.receiver.as_deref() else {
        return Resolution::unknown("无接收者".to_string());
    };
    let method = loc.method.as_deref().unwrap_or("");
    if method.is_empty() {
        return Resolution::unknown("无方法名".to_string());
    }
    let type_fqn = receiver_type_fqn(ctx, &loc.owner_fqn, recv);
    let Some(type_fqn) = type_fqn else {
        return Resolution::unknown(format!("接收者 {recv} 的类型未知"));
    };

    // 目标方法：先本类，再沿继承链回溯（方法常继承自基类）。
    // 成员分隔符语言相关：PHP `Class::method`、Java `Class.method` —— 两者都试。
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
    Resolution::unknown(format!("{type_fqn}::{method} 未找到"))
}

/// 容器解析：L1 字面 → L2 注册表 → L4 约定 → L6 与类全集求交。
fn resolve_container(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    let raw = loc.raw.trim_start_matches('\\');

    // L1：字面 FQN
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Resolution::resolved(ResolveTier::Exact, id, format!("字面 FQN {raw}"));
    }
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
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
                .or_else(|| {
                    ctx.ws
                        .resolve_name_in_file(Some(&loc.file), &target)
                        .and_then(|f| ctx.ws.find_by_name(&f))
                })
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
    if let Some(fqn) = ctx.ws.resolve_name_in_file(Some(&loc.file), raw) {
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

/// 运行时事件监听注册：`Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`。
///
/// arg0（事件名）复用 `event_name` 别名索引解析到事件节点（与 `event('x')` 同一套），
/// 解析结果在 `apply_resolution` 里用于把 `HandledBy` 边从事件节点指向 arg1 监听器类。
fn resolve_event_listen(ctx: &mut PipelineContext, loc: &Locator) -> Resolution {
    match ctx.ws.find_by_alias("event_name", &loc.raw, None) {
        Some(id) => Resolution::resolved(ResolveTier::Alias, id, format!("事件别名 {}", loc.raw)),
        None => {
            ctx.ws.diagnose(
                &Phase(Phase::RESOLVE.to_string()),
                "EventListenUnresolved",
                Severity::Warning,
                format!(
                    "Event::listen('{}', …) 未找到对应事件节点 —— 该事件未在 event.php 注册，\
                     监听器仍可经 `listener` 标注识别，但无法精确挂到具体事件",
                    loc.raw
                ),
                Some(format!("{}:{}", loc.file, loc.line)),
            );
            Resolution::unknown(format!("事件 {} 未注册", loc.raw))
        }
    }
}

/// 把 `Foo::class` / `\App\X` 这类字面解析成图内的类节点。
fn resolve_class_node(ctx: &PipelineContext, raw: &str, file: Option<&str>) -> Option<NodeId> {
    let mut s = raw.trim_start_matches('\\').to_string();
    if let Some(stripped) = s.strip_suffix("::class") {
        s = stripped.to_string();
    }
    ctx.ws.find_by_name(&s).or_else(|| {
        ctx.ws
            .resolve_name_in_file(file, &s)
            .and_then(|f| ctx.ws.find_by_name(&f))
    })
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
    match resolve_handler_target(ctx, &loc.raw, &loc.file, loc.sub) {
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
    sub: Option<gt_domain::model::SubProjectId>,
) -> Option<(NodeId, &'static str)> {
    // handler 的形态（分隔符 / 类名模板 / 应用段）全部由 FKB 声明 —— 内核不认识任何框架。
    let spec = sub
        .and_then(|s| ctx.handler_specs.get(&s.get()).cloned())
        .or_else(|| ctx.handler_spec_default.clone())
        .unwrap_or_default();
    // 命名空间 / 成员分隔符来自语言策略（PHP `\` + `::`，Java `.` + `.`）
    let policy = ctx.lang_policy_for_sub(sub).clone();

    let raw = raw.trim_start_matches('\\');
    if let Some(id) = ctx.ws.find_by_name(raw) {
        return Some((id, "完全限定名直接命中"));
    }

    // ① 按 FKB 声明的分隔符拆 controller / method
    let (controller_part, method) = split_handler(raw, &spec.method_separators);

    // ② controller 内部的层级字符 → 命名空间分隔符
    //    例：ThinkPHP 的 `v1.agent.AgentManage` → `v1\agent\AgentManage`
    let mut controller = controller_part.to_string();
    let ns_sep = policy.ns_separator.to_string();
    for sep in &spec.hierarchy_separators {
        if !sep.is_empty() {
            controller = controller.replace(sep.as_str(), &ns_sep);
        }
    }

    // ③ 候选类名 = 模板 × 应用段
    let app_seg = route_app_segment(file, &spec);
    let mut app_segs: Vec<String> = vec![app_seg];
    app_segs.extend(spec.app_segments.iter().cloned());
    let mut bases: Vec<String> = Vec::new();
    for tpl in &spec.class_templates {
        for seg in &app_segs {
            let b = tpl
                .replace("{app}", seg)
                .replace("{controller}", &controller);
            if !bases.contains(&b) {
                bases.push(b);
            }
        }
    }
    // 框架没声明模板时，controller 本身即候选（全限定名或短名，由 ⑤ 兜底）
    if bases.is_empty() {
        bases.push(controller.clone());
    }

    // ④ 方法优先，其次回退到类（控制器方法常继承自基类，要求方法存在会断链）
    if !method.is_empty() {
        for base in &bases {
            if let Some(id) = ctx.ws.find_by_name(&policy.join_member(base, &method)) {
                return Some((id, "方法精确命中"));
            }
        }
    }
    for base in &bases {
        if let Some(id) = ctx.ws.find_by_name(base) {
            return Some((id, "类命中（方法未在图内，可能继承自基类）"));
        }
    }

    // ⑤ 短名兜底：按路由文件里的 import 还原
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

/// 按 FKB 声明的**顺序**尝试分隔符，拆出 controller 与 method。
///
/// ThinkPHP 用 `/`、Laravel 与 Symfony 用 `@` 或 `::`、Rails 用 `#` —— 都由此支持。
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

/// 从路由文件路径推断应用段：`app/api/route/pc.php` + 锚点 `route` → `api`。
fn route_app_segment(file: &str, spec: &HandlerSpec) -> String {
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

/// 把一次动态解析落成**可定位**的 `evidence` 对象（与 `engine.rs` 里 FKB 规则边同一形状）。
///
/// 关键在于 `location`：视图层靠 `properties.evidence.location` 取"这条边是在哪一行建立的"
/// （`view_service::inline_chain_locations` 补 `to_call_site`、`edge_evidence` 取证据位置，
/// 两处都读它）。此前这里只存 `res.evidence` 这**一个字符串**，形状对不上，于是
/// `Triggers` / `HandledBy` 这类语义边永远取不到位置 —— 抽屉里只剩"没有逐跳证据可查"，
/// 其实那一行（如 `event('X')` 的调用处）本来就在图里。
fn evidence_of(loc: &Locator, res: &Resolution) -> serde_json::Value {
    serde_json::json!({
        "rule": format!("{:?}", loc.strategy),
        "evidence": res.evidence.clone(),
        "location": format!("{}:{}", loc.file, loc.line),
    })
}

fn apply_resolution(ctx: &mut PipelineContext, loc: &Locator, res: &Resolution, phase: &Phase) {
    // 运行时事件监听注册：把 arg0 解析出的事件节点，用 `HandledBy` 边指向 arg1 监听器类。
    // 注意：这不是"触发"，故不走默认的 owner→target（Triggers）边。
    if loc.strategy == ResolveStrategy::EventListen {
        if let Some(event_id) = res.candidates.first() {
            match loc.consumer.as_deref() {
                Some(consumer) => {
                    if let Some(listener_id) = resolve_class_node(ctx, consumer, Some(&loc.file)) {
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
        // 实例方法调用：调用者 → 目标方法，是一条真正的「调用」边。
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

/// 解析 P5 遗留的待定链接（路由 handler / 事件监听器等）。
fn resolve_pending_links(ctx: &mut PipelineContext, phase: &Phase) {
    let pending = std::mem::take(&mut ctx.ws.pending_links);
    for link in &pending {
        let target = match link.resolve {
            gt_domain::model::ResolveAs::HandlerPattern => {
                let (file, _) = link.file.split_once(':').unwrap_or((link.file.as_str(), "0"));
                resolve_handler_target(ctx, &link.raw, file, link.sub).map(|(id, _)| id)
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

/// 通用调用解析：为可静态确定的调用建立 `Calls` 边（调用者方法 → 目标方法 / 函数）。
///
/// 为什么必须做：`ReadsConfig` / `ReadsCache` / `MapsTo` 等语义边挂在**被调方法的节点**上，
/// 若调用者与被调者之间没有边，整条「路由 → 缓存 / 配置 / 表」的链路在图上就是断的，
/// 视图只能靠 `Declares`/`HasCallSite` 之类的结构边绕路，最终把无关节点全拉进来。
///
/// 只解析能静态确定的调用，避免误连：
/// * `Class::method`（静态 / 门面调用）：receiver 是类名 → 解析类短名后拼 `Fqn::method`；
/// * 自由函数 `foo()`（排除 `new Foo`）→ 按名 / 短名解析；
/// * 变量接收者（`$this->x`、`$obj->y`）需要类型推断，**不解析**（宁可缺边，不可错边）。
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
                    confidence: 0.7,
                    properties: serde_json::json!({
                        "callee": call.callee,
                        // 精确记录这次调用对应的 CallSite 节点 id，视图层据此直接取到"调用处"，
                        // 无需再用被调名做启发式匹配。
                        "call_site": call.node.get(),
                    }),
                });
                added += 1;
            }
        }
        // 调用者 → **接收者类型类**：让类级语义边（`Dao → Model`、`Model → Table`）沿调用链浮现。
        // 例：`$this->dao->value()` 解析到的是继承来的 `BaseDao::value`（声明类没有 Model 链接），
        // 但接收者类型 `WechatUserDao` 才是 `setModel()` 指向 Model 的那一层。
        if let Some(recv) = call.receiver.as_deref() {
            if let Some(type_fqn) = receiver_type_fqn(ctx, &call.owner_fqn, recv) {
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
    info!("P7 调用链解析：{} 条 Calls 边", added);
}

/// 解析调用接收者的类型 FQN：
/// * `$this->prop` → 属性类型（沿继承链回溯，`WechatServices` 找不到就看父类）；
/// * `$var` → 所在方法的参数类型。
fn receiver_type_fqn(ctx: &PipelineContext, owner_fqn: &str, recv: &str) -> Option<String> {
    if let Some(prop) = recv.strip_prefix("$this->") {
        let class_fqn = owner_fqn.split("::").next().unwrap_or("");
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
        ctx.ws
            .param_type(owner_fqn, var)
            .map(|s| resolve_impl(ctx, s))
    } else {
        // Java 字段（裸标识符）：从所属类（含父类）的字段类型推断
        // `service.mapper.findX()` 中 `mapper` 是 `@Autowired` 注入字段。
        let class_fqn = owner_class_of(owner_fqn);
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
        None
    }
}

/// 若 `type_fqn` 是接口且存在实现类，返回实现类的 FQN：Spring 注入的是实现类，
/// DB 调用也在实现类的方法体里；否则原样返回。
///
/// 不解析到实现类会断链：路由 → 服务接口（`@Autowired` 字段类型）→ 实现类方法
/// （真正 `mapper.xxx()` 的地方）→ Mapper → 表。
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

/// 从「方法 FQN」取所属类 FQN：Java `pkg.Class.method` → `pkg.Class`，
/// PHP `Class::method` → `Class`。
fn owner_class_of(owner_fqn: &str) -> String {
    owner_fqn
        .rsplit_once('.')
        .or_else(|| owner_fqn.rsplit_once("::"))
        .map(|(c, _)| c.to_string())
        .unwrap_or_else(|| owner_fqn.to_string())
}

/// 用**调用方所在文件的 `use` 表**把短名还原成 FQN。
///
/// 返回 `Some(fqn)` 表示该文件确实导入了这个短名 —— 调用方必须**只**认这个 FQN，
/// 即使它不在图里（框架类）也不能再退回全局短名索引猜测。
/// 返回 `None` 表示该文件没有导入它（`new Foo`、全局命名空间写法等），交由后续兜底。
fn file_import_of(ctx: &PipelineContext, owner: NodeId, short: &str) -> Option<String> {
    let file_id = ctx.ws.node(owner)?.file_id?;
    ctx.ws
        .imports_of_file(file_id.get())
        .and_then(|m| m.get(&short.to_ascii_lowercase()))
        .cloned()
}

fn resolve_call_target(ctx: &PipelineContext, call: &CallRecord) -> Option<NodeId> {
    // 1) `Class::method`（静态 / 门面）：receiver 是类名（非变量）
    if let Some(recv) = &call.receiver {
        if !recv.starts_with('$') {
            let m = call.method.as_deref().unwrap_or("");
            if m.is_empty() {
                return None;
            }
            let recv = recv.trim_start_matches('\\');
            if let Some(id) = ctx.ws.find_by_name(&format!("{recv}::{m}")) {
                return Some(id);
            }
            // 先按**本文件的 `use`** 还原 FQN —— 这才是 PHP 的真实解析规则。
            //
            // 关键：若文件导入的这个类**不在图里**（框架类，如 `think\facade\Cache`，
            // vendor 已被 P0 排除），就必须**放弃解析**，绝不能退回全局短名索引去猜
            // 一个同名的项目类。`Cache` 会被猜成 `app\model\other\Cache`（一个 Model），
            // 于是每次 `Cache::xxx()` 都凭空多出一条 `Calls` 边，并把
            // `Model --MapsTo--> Table(cache)` 这类类级语义边拖到路由上。
            if let Some(fqn) = file_import_of(ctx, call.owner, recv) {
                if let Some(id) = ctx.ws.find_by_name(&format!("{fqn}::{m}")) {
                    return Some(id);
                }
                return ctx.ws.find_by_name(&fqn);
            }
            if let Some(fqn) = ctx.ws.resolve_name_at(call.owner, recv) {
                if let Some(id) = ctx.ws.find_by_name(&format!("{fqn}::{m}")) {
                    return Some(id);
                }
                // 方法不在图内（框架方法如 `User::where`）→ 连到**类本身**，
                // 从而让 Model --MapsTo--> Table 这类"类级语义边"能沿调用链浮现。
                if let Some(id) = ctx.ws.find_by_name(&fqn) {
                    return Some(id);
                }
            }
        }
        return None;
    }
    // 2) 自由函数 `foo()`（排除 `new Foo`）
    //
    // **只接受解析到 `Function` 节点的结果**：`find_by_name` 是按 FQN 查的，而全局
    // helper 的名字会与命名空间 / 类撞名 —— `app()` 命中 Namespace `app`、
    // `config()` 命中同名类方法。此前不校验种类，凭空连出大量错误 Calls 边。
    // 宁可缺边，不可错边。
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

/// 是否为自由函数节点（决定自由函数调用能否连到它）。
fn is_function_node(ctx: &PipelineContext, id: NodeId) -> bool {
    ctx.ws
        .node(id)
        .map(|n| n.kind.as_str() == NodeKind::FUNCTION)
        .unwrap_or(false)
}
