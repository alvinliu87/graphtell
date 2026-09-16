//! P8 Propagate：把合成阶段建立的「方法 → 语义节点」动作边，沿 `Calls` 调用链向上传播。
//!
//! # 动机
//!
//! FKB 规则只在**字面调用点**命中（如 `Queue::push`）。如果一个高层方法经由若干封装层
//! 最终调到该调用点，只有最里层方法会被连到语义节点，外层调用方全部丢失 —— 这正是
//! `crmeb\utils\Queue::push → QueueThink::push` 这类「框架封装」被吞掉的根因。
//!
//! 这违背一条原则：**一个功能最终调用了 FKB 认得的东西，就该被正确解析，无论多深。**
//!
//! # 设计（通用，不绑定任何框架）
//!
//! * 只依赖已经建好的 `Calls` 边（P7 已解析方法→方法的调用，含别名 / 短名 / 接收者类型）。
//! * 只搬运「动作发出方方法 → 语义节点」这一事实：当 `M --kind--> S` 存在，且 `C` 调用 `M`，
//!   则复刻 `C --kind--> S`，并沿调用链递归到不动点（循环安全）。
//! * 只沿「方法 / 函数」节点向上；类节点（如 `MapsTo` 的源）不参与，避免把语义边误挂到类上。
//! * 与具体边种类无关：`PublishesTo` / `ReadsDb` / `ReadsCache` / `ReadsConfig` 等一律同等对待。

use std::collections::{BTreeSet, HashMap, HashSet};

use em_domain::model::{EdgeKind, NewEdge, NodeId, Phase};
use serde_json::json;

use crate::context::{PipelineContext, PropSeed};

/// 执行传播。必须在 P7 之后运行（依赖其建好的 `Calls` 边；种子在 P5 收集，已暂存于 ctx）。
pub fn run(ctx: &mut PipelineContext) {
    let phase = Phase(Phase::PROPAGATE.to_string());

    // 反向调用索引：被调方法 → 它的所有「方法 / 函数」调用方（构建时已过滤非方法节点）。
    let mut callers: HashMap<i64, Vec<i64>> = HashMap::new();
    for e in ctx.ws.edges() {
        if e.kind.as_str() != EdgeKind::CALLS {
            continue;
        }
        let Some(from) = ctx.ws.node(e.from_id) else {
            continue;
        };
        if !is_action_site(from.kind.as_str()) {
            continue;
        }
        callers.entry(e.to_id.get()).or_default().push(e.from_id.get());
    }

    let seeds = std::mem::take(&mut ctx.propagation_seeds);
    if seeds.is_empty() {
        return;
    }

    // 按 source 分组：同一 source 的可达调用方集合只需算一次。
    let mut by_source: HashMap<i64, Vec<PropSeed>> = HashMap::new();
    for s in seeds {
        by_source.entry(s.source.get()).or_default().push(s);
    }

    // **必须按 source 排序后遍历**，保证 `add_edge` 的提交顺序可复现。
    //
    // **确定性根因**：同一个 `(kind, from, to)` 往往能被多个 seed 传播到（如 `detail`
    // 既经由 `getQRCodePath`、又经由 `getQRCodePath → init` 读到同一个配置键）。
    // 之前「`add_edge` 按 (kind, from, to) 先到者胜出」让胜出者写下唯一 `seed_source`，
    // 而 HashMap 遍历顺序随进程随机，于是同一条传播边每次建图拿到不同的 `seed_source`
    // —— 视图据此回溯出的接触点与完整路径随之改变（实测同一路由画出的边数在 29/30 间跳）。
    //
    // 现在：按 `(kind, from, to)` 聚合**全部**根因进 `seed_sources` 数组（升序、确定），
    // 并保留 `seed_source = 最小 seed` 作为兼容字段。视图即可确定性地选到真正接触点，
    // 不再因遍历顺序而变。（"取 id 最小的 seed"仍是并列时的确定性打破方式，非语义选择；
    // 要为每个根因各画一条路径是另一件事，与"一条路径一条边"同源。）
    let mut sources: Vec<i64> = by_source.keys().copied().collect();
    sources.sort_unstable();

    // `(kind, from, to)` → 全部根因（直接边起点，升序）；及其置信度 / 间接标记。
    let mut edge_seeds: HashMap<(EdgeKind, i64, i64), BTreeSet<i64>> = HashMap::new();
    let mut edge_meta: HashMap<(EdgeKind, i64, i64), (f32, bool)> = HashMap::new();
    for src in &sources {
        let src_seeds = &by_source[src];
        let reach = transitive_callers(*src, &callers, ctx);
        for s in src_seeds {
            let (confidence, indirect) = propagated(&s.kind, s.confidence);
            for c in &reach {
                // `from` 是沿调用链上行到达的调用方，`to` 是语义目标；
                // `src` 才是真正执行该动作的方法节点（根因）。
                let key = (EdgeKind(s.kind.clone()), *c, s.target.get());
                edge_seeds.entry(key.clone()).or_default().insert(*src);
                edge_meta.entry(key.clone()).or_insert((confidence, indirect));
            }
        }
    }
    let mut keys: Vec<(EdgeKind, i64, i64)> = edge_seeds.keys().cloned().collect();
    keys.sort_by(|a, b| (a.0.as_str(), a.1, a.2).cmp(&(b.0.as_str(), b.1, b.2)));

    let mut added = 0usize;
    for key in keys {
        let seeds: Vec<i64> = edge_seeds[&key].iter().copied().collect();
        let (confidence, indirect) = edge_meta[&key];
        let mut props = json!({
            "via": "propagate",
            "seed_source": seeds[0],
            "seed_sources": seeds,
        });
        if indirect {
            props["indirect"] = json!(true);
        }
        ctx.ws.add_edge(NewEdge {
            project_id: ctx.project.id,
            kind: key.0,
            from_id: NodeId(key.1),
            to_id: NodeId(key.2),
            phase: phase.clone(),
            confidence,
            properties: props,
        });
        added += 1;
    }
    tracing::info!(
        "P8 传播完成：{} 个 source，{} 条传播边（共 {} 个根因）",
        by_source.len(),
        added,
        edge_seeds.values().map(|s| s.len()).sum::<usize>()
    );
}

/// 从 `src` 出发，沿 `callers` 索引向上收集所有可达的「方法 / 函数」调用方（不含 src 自身）。
///
/// `seen` 保证循环安全；`is_action_site` 保证只经过方法 / 函数节点。
fn transitive_callers(
    src: i64,
    callers: &HashMap<i64, Vec<i64>>,
    ctx: &PipelineContext,
) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut stack = vec![src];
    while let Some(cur) = stack.pop() {
        let Some(next) = callers.get(&cur) else {
            continue;
        };
        for c in next {
            if !seen.insert(*c) {
                continue;
            }
            // 仅当调用方是方法 / 函数时才纳入并继续向上；其余节点（类 / 文件）不进入传播链。
            let is_site = ctx
                .ws
                .node(NodeId(*c))
                .map(|n| is_action_site(n.kind.as_str()))
                .unwrap_or(false);
            if is_site {
                out.push(*c);
                stack.push(*c);
            }
        }
    }
    out
}

/// 是否为「动作发出方」节点：只有方法 / 函数能作为语义动作的源头被传播。
fn is_action_site(kind: &str) -> bool {
    kind == NODE_KIND_METHOD || kind == NODE_KIND_FUNCTION
}

/// 传播时会**退化**的语义边种类。
///
/// `ReadsDb` / `WritesDb` / `PublishesTo` 这类是**真实发生的动作**：调用方调用了它，
/// 该动作就确实在这次调用中发生了，传播到调用方不算失真。
///
/// `ReadsConfig` / `ReadsCache` 则不同：它们是**环境读取**，传播后含义从
/// 「此处读取了该配置」退化为「上游某处读过，本入口可能受影响」——事实强度明显更弱。
/// 若不加以区分，任何一个读了 `member_func_status` 的共享方法，都会把所有途经它的
/// 入口全标成「读取该配置」，产生大量假正。
const DECAYED_KINDS: &[&str] = &[EdgeKind::READS_CONFIG, "ReadsCache"];

/// 退化系数：环境读取类语义边沿调用链每向上传播一次所保留的置信度。
const DECAY_FACTOR: f32 = 0.6;

/// 计算传播边的置信度与「是否间接」标记。
fn propagated(kind: &str, base: f32) -> (f32, bool) {
    if DECAYED_KINDS.contains(&kind) {
        (base * DECAY_FACTOR, true)
    } else {
        (base, false)
    }
}

const NODE_KIND_METHOD: &str = "Method";
const NODE_KIND_FUNCTION: &str = "Function";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::PropSeed;
    use em_domain::model::{
        EdgeKind, Language, NewEdge, NewNode, NodeId, NodeKind, Phase, Project, ProjectConfig,
        ProjectId, ProjectStatus, Span,
    };
    use std::path::PathBuf;

    fn test_ctx() -> PipelineContext {
        let project = Project {
            id: ProjectId(1),
            name: "t".into(),
            root_path: PathBuf::from("/t"),
            description: None,
            config: ProjectConfig::default(),
            status: ProjectStatus::Created,
            created_at: 0,
            updated_at: 0,
        };
        PipelineContext::new(project)
    }

    fn add_method(ctx: &mut PipelineContext, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(NODE_KIND_METHOD.to_string()),
            name: fqn.into(),
            fqn: Some(fqn.into()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("CfAst".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn add_node(ctx: &mut PipelineContext, kind: &str, fqn: &str) -> NodeId {
        ctx.ws.add_node(NewNode {
            id: None,
            project_id: ProjectId(1),
            sub_project_id: None,
            kind: NodeKind(kind.to_string()),
            name: fqn.into(),
            fqn: Some(fqn.into()),
            identity: None,
            file_id: None,
            span: Span::default(),
            language: Language::default(),
            phase: Phase("CfAst".into()),
            confidence: 1.0,
            properties: serde_json::Value::Null,
        })
    }

    fn calls(ctx: &mut PipelineContext, from: NodeId, to: NodeId) {
        ctx.ws.add_edge(NewEdge {
            project_id: ProjectId(1),
            kind: EdgeKind("Calls".to_string()),
            from_id: from,
            to_id: to,
            phase: Phase("Resolve".into()),
            confidence: 0.7,
            properties: serde_json::Value::Null,
        });
    }

    fn has_edge(ctx: &PipelineContext, from: NodeId, to: NodeId, kind: &str) -> bool {
        ctx.ws.edges().iter().any(|e| {
            e.from_id == from && e.to_id == to && e.kind.as_str() == kind
        })
    }

    #[test]
    fn propagates_action_edge_up_call_chain() {
        let mut ctx = test_ctx();
        let leaf = add_method(&mut ctx, "app\\Leaf::run");
        let mid = add_method(&mut ctx, "app\\Mid::run");
        let top = add_method(&mut ctx, "app\\Top::run");
        let queue = add_node(&mut ctx, "Queue", "Queue:default");

        // 调用链：top → mid → leaf
        calls(&mut ctx, top, mid);
        calls(&mut ctx, mid, leaf);

        // 种子：leaf 作为动作发出方，投递到队列。
        ctx.propagation_seeds.push(PropSeed {
            source: leaf,
            target: queue,
            kind: "PublishesTo".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        super::run(&mut ctx);

        // 调用方都应被识别为「投递到队列」的发出方，无论多深。
        assert!(has_edge(&ctx, mid, queue, EdgeKind::PUBLISHES_TO));
        assert!(has_edge(&ctx, top, queue, EdgeKind::PUBLISHES_TO));
        // 种子源自身在本测试里没有基边（基边由合成阶段另外建立），传播只针对调用方。
    }

    #[test]
    fn propagation_is_cycle_safe_and_deduped() {
        let mut ctx = test_ctx();
        let a = add_method(&mut ctx, "app\\A::run");
        let b = add_method(&mut ctx, "app\\B::run");
        let c = add_method(&mut ctx, "app\\C::run");
        let q = add_node(&mut ctx, "Queue", "Queue:default");

        // 环：a ↔ b，且 c → a（c 经两条路径可达 a 的调用方 b）。
        calls(&mut ctx, b, a);
        calls(&mut ctx, a, b);
        calls(&mut ctx, c, a);

        ctx.propagation_seeds.push(PropSeed {
            source: a,
            target: q,
            kind: "PublishesTo".to_string(),
            confidence: 0.85,
            sub: None,
            phase: Phase("Synthesize".into()),
        });

        // 不应死循环；且 b 只出现一次。
        super::run(&mut ctx);
        let b_count = ctx
            .ws
            .edges()
            .iter()
            .filter(|e| e.from_id == b && e.to_id == q && e.kind.as_str() == EdgeKind::PUBLISHES_TO)
            .count();
        assert_eq!(b_count, 1);
        assert!(has_edge(&ctx, c, q, EdgeKind::PUBLISHES_TO));
    }
}

