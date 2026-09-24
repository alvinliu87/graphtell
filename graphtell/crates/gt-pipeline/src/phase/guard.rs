//! P14 Guard：把「这条路由过了哪些中间件」落成图上的边。
//!
//! # 为什么是**桥边**，而不是语义边
//!
//! `HttpContract --PassesThrough--> <中间件>` 两端性质不同：起点是语义节点，终点是一个
//! `Class`（语法节点），中间件自身并不需要"多处引用汇聚才存在"。与 `HandledBy`
//! （契约 → handler）、`HasColumn`（表 → 列）同族：**可沿链遍历，但不计入「语义入边 /
//! 出边 N」，也不当画布边画出来** —— 否则一条路由挂 3 个中间件，它的"出边 N"就凭空 +3，
//! 与旁边"读写了几张表"混进同一个口径。
//!
//! # 为什么先造边、不先晋升节点
//!
//! 中间件将来要不要在折叠视图里当邻居节点出现，是可以后验的选择；而"这条关系是否真的存在、
//! 命中多少、组合有没有区分度"必须先有数据。边先落图，晋升与否就变成一个**可以量化决定**
//! 的问题，而不是口味之争。
//!
//! # 已知边界
//!
//! * 只接**路由级 / 路由组级**挂载（与 P3 的取舍一致）：全局中间件对每个端点都成立，是环境
//!   常量；给它连边等于每条契约都加上同样几条 —— 数字涨了，信息没涨。
//! * 中间件类不在图里时（`vendor` 里的类、尚未还原的动态别名）**跳过，不建悬空边也不猜**。

use gt_domain::model::{
    AnnotationChannel, EdgeKind, MergeStrategy, NewAnnotation, NewEdge, NewNode, NodeId, NodeKind,
    Phase, Span,
};
use serde_json::{json, Value};

use crate::context::PipelineContext;

/// 语义边种类：`HttpContract --PassesThrough--> 中间件`。
///
/// 名字取中性（"经过"而不是"守卫"）：中间件里既有会拒绝请求的守卫，也有只加响应头 /
/// 记日志的旁路，统一叫"守卫"等于替后者过度声明。鉴不鉴权由 `Capability` 标注回答。
const PASSES_THROUGH: &str = "PassesThrough";

/// 「挂了鉴权中间件、但实参显式写着可选」的标注（未登录也能进，不等于没挂）。
const OPTIONAL_AUTH: &str = "auth.optional";

/// 契约名 → 节点（`HttpContract.name` 与 `route_list` 的键同源）。
fn contract_index(ctx: &PipelineContext) -> std::collections::HashMap<String, NodeId> {
    let mut out = std::collections::HashMap::new();
    for id in ctx.ws.nodes_of_kind(NodeKind::HTTP_CONTRACT) {
        if let Some(node) = ctx.ws.node(id) {
            out.insert(node.name.clone(), id);
        }
    }
    out
}

/// 每行守卫的形状：`[(中间件短名, 挂载实参)]`。
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

/// P5.5：把中间件带来的能力标到契约上（`Capability` 通道）。
///
/// # 为什么必须在 P6 之前
///
/// `crmeb-public-endpoint` 这类 P6 规则用 `none_of_capability` 判定"公开端点"。
/// 反向判据只在正面证据**确实存在过**时才成立：能力若等 P14 才标注，
/// P6 早已把每个契约都标成 `auth.public`（实测 1603 个里 1529 个），
/// 之后再补也纠正不回来（标注已落库）。
///
/// # 为什么不算"推断"
///
/// 锚点是中间件的**确凿身份**（类名，由 FKB 声明哪个名字对应哪种能力），不是
/// "这个端点看起来要不要登录"。挂载实参明确写着可选时（`AuthToken::class, false`
/// == 未登录也能进）**不打能力** —— 宁可缺，不可把可选的当成强制的。
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
            // 全部命中都是"可选"时，不能宣称这个端点被该能力守卫。
            let mandatory = hits
                .iter()
                .any(|(_, arg)| arg.as_deref().map(|a| a != "false").unwrap_or(true));
            if !mandatory {
                // 不打能力，但**记录这个确凿事实**：确实挂了鉴权中间件、且实参显式写着可选
                // （`AuthTokenMiddleware::class, false` == 未登录也能进）。
                // 这是"可选"而不是"没有"——把两者混为一谈就是编造，故另起一个标注，
                // 供 `write-endpoint-with-optional-auth` 这类**正向**规则命中。
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
        "P5.5 守卫能力完成：{} 条能力标注（{} 处挂载显式写着可选，按「宁可缺不可猜」不打能力）",
        stamped,
        skipped_optional
    );
}

pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase("Guard".to_string());

    // 先攒计划再改图：`add_edge` 要 &mut ws，不能一边遍历一边改。
    // ① 契约名 → 节点：HttpContract 的 `name` 与 `route_list` 的键同源
    //    （见 `prepare::load_routes`：键 = "METHOD /prefix/path"）。
    let mut contracts: std::collections::HashMap<String, NodeId> = std::collections::HashMap::new();
    for id in ctx.ws.nodes_of_kind(NodeKind::HTTP_CONTRACT) {
        if let Some(node) = ctx.ws.node(id) {
            contracts.insert(node.name.clone(), id);
        }
    }
    if contracts.is_empty() {
        return;
    }

    // ② 计划：契约节点 + 它身上的中间件类名列表
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

    // ②b 声明式中间件（文件登记，非路由调用链）：由 prepare 阶段的 `declared_middleware`
    // 符号表给出。prepare 时 HttpContract 节点尚未合成（自动路由项目尤甚），故这里在节点
    // 已存在后按作用域挂链——内核零框架字符串，作用域完全来自 FKB 声明。
    // * `global`：挂到全部契约节点；
    // * `per_app`：只挂到名字含 `prefix`（如 `/adminapi`）的契约节点。
    if let Some(entries) = ctx.ws.symbols.get("declared_middleware") {
        for entry in entries.values() {
            let scope = entry.get("scope").and_then(Value::as_str).unwrap_or("global");
            let prefix = entry.get("prefix").and_then(Value::as_str);
            let classes: Vec<String> = entry
                .get("classes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str())
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if classes.is_empty() {
                continue;
            }
            let targets: Vec<NodeId> = if scope == "per_app" {
                contracts
                    .iter()
                    .filter(|(name, _)| prefix.map_or(true, |p| name.contains(p)))
                    .map(|(_, id)| *id)
                    .collect()
            } else {
                contracts.values().cloned().collect()
            };
            for tid in targets {
                plan.push((tid, classes.clone()));
            }
        }
    }

    // ③ 落边，并把被挂上的那个东西晋升 / 建成为 `Middleware` 语义节点
    let mut created = 0usize;
    let mut promoted = 0usize;
    let mut synthesized = 0usize;
    let mut unresolved = 0usize;
    // FKB 授权「查不到也建节点」？见 `route_guards.synthesize_unresolved`。
    // JS / Python 的中间件是**函数值**（`const loginLimiter = rateLimit({...})`），解析器
    // 不会为它建语法节点，但"这条路由挂了一个叫 X 的中间件"是源码里的确凿事实 ——
    // 授权后建一个同名 `Middleware` 语义节点，是如实记录而不是猜测。
    let allow_synthesize = ctx.ws.synthesize_unresolved_guards;
    // FKB 预声明的**已知中间件**（框架 / 库自带、源码不在图里）：`middleware_classes` 表，
    // 由 FKB 的 `inline` 装载器写入（`{ class, capability }`），无需解析器改动。
    // 按全名与短名两种键都可命中。
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
            // 精确 FQN 优先；落空时（Laravel 常写裸短名 `NoCacheMiddleware`、
            // ThinkPHP 也可能被写成不带命名空间的短名）按**短名**兜底 ——
            // `resolve_short_name` 对歧义短名一律拒绝，不会连错类。
            let target = ctx
                .ws
                .find_by_name(&class)
                .or_else(|| {
                    ctx.ws
                        .resolve_short_name(&class)
                        .and_then(|fqn| ctx.ws.find_by_name(&fqn))
                });
            let Some(target) = target else {
                // 图里没有这个类 —— 但**没节点不等于没语义**：
                // 框架 / 库自带的中间件常住在 `vendor/` / `node_modules/`（不进图），
                // FKB 若已声明过它（`middleware_classes` 表），就按声明落成节点 ——
                // 这不是猜测，与"靠名字猜"是两回事，故不受 `synthesize_unresolved` 限制。
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
                // 显示用**短名**（与图里其它中间件节点一致），FQN 仍留在 `fqn` 字段备查。
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
                // 建一个 `Middleware` 语义节点（FKB 已声明 / 已授权）。
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
                    // 语言随所属契约（新建的中间件节点没有自己的文件位置）。
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
            // 晋升：改 kind 而**不新建节点**（`patch_kind` 的注释里写了为何必须如此）。
            // * `Class`：PHP 的中间件恒为类，无条件晋升；
            // * `Function` / `Method`：JS / Python 的中间件是函数，仅当 FKB 授权
            //   （`synthesize_unresolved`）时才晋升 —— 未授权时对别的东西一律不动，
            //   万一某个 FKB 把不相干的东西当成中间件，也不会连带改坏。
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
        "P14 路由守卫完成：PassesThrough 边 {} 条 / {} 个晋升为 Middleware / {} 个按名建成 Middleware（中间件不在图里且未授权 {} 处，跳过不猜）",
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
    use gt_domain::model::{IdentityKey, NewNode, ProjectId, ProjectStatus};

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

    /// 契约节点：`name` 必须与 `route_list` 的键同形，否则查不到。
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

    /// 中间件类节点：`by_fqn` 按 FQN 索引，故必须写 `fqn`。
    fn add_class(ctx: &mut PipelineContext, fqn: &str) -> NodeId {
        let mut n = NewNode::new(
            ProjectId::new(1),
            NodeKind(NodeKind::CLASS.to_string()),
            fqn,
        );
        n.fqn = Some(fqn.into());
        ctx.ws.add_node(n)
    }

    /// 内存：往 `route_list` 符号表塞一行带守卫的路由。
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
        // 键对不上（`route_list` 里没有的契约）不该凭空长边
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

    /// 被挂上的类要**晋升**成 `Middleware`（改 kind，不新建节点）。
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

        let node = ctx.ws.node(mw).expect("中间件类节点应仍然存在");
        assert_eq!(node.kind.as_str(), NodeKind::MIDDLEWARE, "kind 应被改成 Middleware");
        assert_eq!(node.fqn.as_deref(), Some(r"app\api\middleware\AuthTokenMiddleware"));
        // 关键：晋升不产生第二个节点 —— 图里仍然只有这一个，fqn 仍可查到同一个 id
        assert_eq!(ctx.ws.find_by_name(r"app\api\middleware\AuthTokenMiddleware"), Some(mw));
        assert!(NodeKind(NodeKind::MIDDLEWARE.to_string()).is_semantic());
        // 晋升要真的落库（delta 里带 kind 补丁），否则重跑就没了
        assert!(
            ctx.ws.take_delta().kind_patches.iter().any(|(id, _)| *id == mw),
            "晋升必须写进 delta，否则持久化层收不到"
        );
    }

    /// 中间件不在图里（vendor / 别名未还原）：跳过，不建悬空边。
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
        assert!(guarded_by(&ctx).is_empty(), "类不在图里就不该有边");
    }
}
