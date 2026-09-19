//! 视图用例：把图**按视角切片**成"当前这一个对象"的链路子图。
//!
//! 核心诚实性约束：
//! * 对象类视角**只画中心对象的链路**；其它对象的链路边直接不画（不是变暗）
//! * 被省略的部分以 [`HiddenInfo`] 计数 + [`UnresolvedInfo`] 记账呈现
//! * 合成节点的位置**永远是多位置列表**，绝不编造单一位置

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    graph::NodeSummary, AggregateView, Candidate, Cluster, EdgeEvidence, EdgeKind, EdgeView,
    GroupBy, HiddenInfo, MatrixView, NodeId, NodeKind, NodeLocationEntry, NodeLocations, NodeView,
    ObjectView, PerspectiveSpec, ProjectId, SourceLocation, SubProjectId, UnresolvedInfo, ViaNode,
    ViewRegistry,
};
use gt_domain::port::{EdgeDirection, NodeFilter, Persistence, ViewRegistryProvider};
use serde_json::{json, Value};

/// 视角服务。
pub struct ViewService {
    store: Arc<dyn Persistence>,
    views: Arc<dyn ViewRegistryProvider>,
}

/// 折叠视图的**发现结果**：以中心为起点的分层 BFS 产物（环 / 父指针 / 类型名缓存 /
/// 边缓存）以及发现途中记录的"访问方式"标签。
///
/// 之所以单独成结构：`object_view` 原本在一个函数体里持有十余个相互引用的 HashMap，
/// 只能靠 `{ }` 作用域与 `pending` 暂存来绕过借用检查 —— 这正是那段近千行逻辑
/// 拆不动、也读不懂的根因。抽出来之后：
/// * `discover()` 一次性把状态建好并返回；
/// * 后续阶段整体**不可变**借用 `&Discovery`，`chain_to` / `via_from_ids` /
///   `push_edge` 等闭包不再与"补齐 kind / name"之类的写操作打架（NLL 即可处理）。
/// 折叠提拉阶段的产物。
struct FoldResult {
    /// 画出来的边（已折叠、已提拉、已内联 via）。
    edges: Vec<EdgeView>,
    /// 被折叠 / 被上限截断而未画出的条目数（诚实记账）。
    hidden_total: usize,
    hidden_by_kind: BTreeMap<String, usize>,
}

struct Discovery {
    /// 环号（跳数）。
    ring_of: HashMap<i64, u32>,
    /// 发现树上的父节点，供 `chain_to` 回溯被折叠掉的中间跳。
    parent_of: HashMap<i64, i64>,
    kind_of: HashMap<i64, String>,
    name_of: HashMap<i64, String>,
    semantic_of: HashMap<i64, bool>,
    /// 反向视角（资源类中心）沿入边回溯用的边缓存。
    in_edges: HashMap<i64, Vec<gt_domain::model::Edge>>,
    /// 正向视角（路由）沿出边下钻用的边缓存。
    out_edges: HashMap<i64, Vec<gt_domain::model::Edge>>,
    /// 事件 / 队列等中介视角下"必须画出来"的直接生产 / 消费方。
    force_visible: HashSet<i64>,
    /// 走到某节点途中遇到的语义边种类（"资源被怎样访问"）。
    path_kind: HashMap<i64, String>,
    /// 与 `path_kind` 平行：该语义边是否由 P8 传播得来。
    path_indirect: HashMap<i64, bool>,
    /// 与 `path_kind` 平行：该原始边是**中心 → 该节点**（中心为出边的一侧）。
    /// 反向模式默认画「使用者 --语义边--> 中心」，但 `HandledBy` 的主语是中心自己，
    /// 必须按原始边方向画成「中心 --由…处理--> 监听器」。
    path_from_center: HashSet<i64>,
    /// 是否反向（资源类中心沿入边回溯）。
    reverse: bool,
}


impl ViewService {
    pub fn new(store: Arc<dyn Persistence>, views: Arc<dyn ViewRegistryProvider>) -> Self {
        Self { store, views }
    }

    pub fn registry(&self) -> &ViewRegistry {
        self.views.registry()
    }

    /// 一级筛选器：各视角及其可用候选数量。
    pub fn perspectives(&self, project_id: ProjectId) -> Result<Vec<Value>> {
        let stats = self.store.stats(project_id)?;
        let mut out = Vec::new();
        for spec in &self.views.registry().perspectives {
            let available = match (&spec.mode, &spec.node_kind) {
                (gt_domain::model::ViewMode::Object, Some(kind)) => {
                    // 先按 kind 数；为 0 时回退到 category（兼容以 category 分组的视角）。
                    let n = stats.by_kind.get(kind).copied().unwrap_or(0);
                    if n > 0 {
                        n
                    } else {
                        stats.by_category.get(kind).copied().unwrap_or(0)
                    }
                }
                _ => stats.nodes,
            };
            out.push(json!({
                "id": spec.id,
                "label": spec.label,
                "mode": spec.mode,
                "layout": spec.layout,
                "depth": spec.depth,
                "description": spec.description,
                "available": available,
            }));
        }
        Ok(out)
    }

    /// 二级筛选器：某视角下的候选对象。
    pub fn candidates(
        &self,
        project_id: ProjectId,
        perspective: &str,
        limit: u32,
        name_contains: Option<&str>,
        sub_project_id: Option<SubProjectId>,
    ) -> Result<Vec<Candidate>> {
        let spec = self
            .views
            .registry()
            .by_id(perspective)
            .ok_or_else(|| DomainError::NotFound(format!("视角 {perspective}")))?;
        let Some(kind) = &spec.node_kind else {
            return Ok(Vec::new());
        };
        let mut nodes = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(gt_domain::model::NodeKind(kind.clone())),
            name_contains: name_contains.map(|s| s.to_string()),
            // 无搜索词时多取一些参与"价值排序"（要选出全局最高价值的对象）；
            // 有搜索词时只按名称过滤、不做昂贵打分。
            limit: Some(if name_contains.is_none() { 5000 } else { limit }),
            offset: Some(0),
        })?;

        // 按子工程收敛候选：选中单一子工程时，下拉只列出该子工程内的对象，
        // 让「子项目作为上层维度」切到该子项目后，默认对象也落在它内部。
        if let Some(sid) = sub_project_id {
            nodes.retain(|n| n.sub_project_id == Some(sid));
        }

        // 有搜索词：直接按名称命中，不做全量打分（下拉按需搜索，求快）。
        if name_contains.is_some() {
            return Ok(nodes
                .into_iter()
                .map(|n| Candidate {
                    id: n.id,
                    name: n.name,
                    badge: None,
                    sub_project_id: n.sub_project_id,
                })
                .collect());
        }

        // 无搜索词：按"语义依赖价值"降序，选出默认打开的对象。
        // 一次性预加载整图概要 + 链边邻接（`chain_adjacency` 只取整数邻接、不含 `properties`），
        // 全部 BFS 在内存里跑，避免逐节点查库、也避开拉取整张边表的沉重传输
        // （原先是 5000 节点 × 每节点多次 `edges_of` / `get_node` 往返，很慢）。
        let summary = self.store.nodes_summary(project_id)?;
        let (out, inc) = self.store.chain_adjacency(project_id)?;

        Ok(self.rank_candidates(nodes, &summary, &out, &inc, limit as usize))
    }

    /// 候选对象打分核心：把 `query_nodes` 取回的节点按"语义价值"排序并截断。
    ///
    /// `summary` / `out` / `inc` 来自 `nodes_summary` + `chain_adjacency`，
    /// **由调用方负责加载一次后复用**——`object_view` 与 `candidates` 共享同一份，
    /// 避免每次请求重复加载整图概要 + 链边邻接（工程越大越贵）。
    fn rank_candidates(
        &self,
        nodes: Vec<gt_domain::model::Node>,
        summary: &HashMap<i64, NodeSummary>,
        out: &HashMap<i64, Vec<i64>>,
        inc: &HashMap<i64, Vec<i64>>,
        limit: usize,
    ) -> Vec<Candidate> {
        let mut scored: Vec<(usize, Candidate)> = nodes
            .into_iter()
            .map(|n| {
                let fan = inc.get(&n.id.get()).map(|e| e.len()).unwrap_or(0);
                let value = self.semantic_value(n.id, summary, out, inc);
                (
                    value,
                    Candidate {
                        id: n.id,
                        name: n.name.clone(),
                        badge: Some(format!("语义依赖 {value} · 入边 {fan}")),
                        sub_project_id: n.sub_project_id,
                    },
                )
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored.truncate(limit);
        scored.into_iter().map(|(_, c)| c).collect()
    }

    /// 候选对象的"价值"：其调用链（chain 边）可达的语义节点数（表 / 配置 / 缓存 / 事件…）。
    /// 值越高，承载的业务依赖越丰富，越适合作为默认打开的对象。
    /// 全部在内存里完成：`summary` 提供节点语义性、`out`/`inc` 为链边整数邻接，不再回查数据库。
    fn semantic_value(
        &self,
        center: NodeId,
        summary: &HashMap<i64, NodeSummary>,
        out: &HashMap<i64, Vec<i64>>,
        inc: &HashMap<i64, Vec<i64>>,
    ) -> usize {
        const MAX_DEPTH: u32 = 3;
        const MAX_NODES: usize = 400;
        let mut seen: HashSet<i64> = HashSet::new();
        seen.insert(center.get());
        let mut semantic: HashSet<i64> = HashSet::new();
        let mut queue: VecDeque<(i64, u32)> = VecDeque::new();
        queue.push_back((center.get(), 0));
        // 与视图一致：把"中心的入向链邻居"也作为第 1 环（表 / 外部系统视角需要）。
        // `inc` 已只含链边，无需再判 `is_chain_edge`。
        if let Some(incoming) = inc.get(&center.get()) {
            for &from in incoming {
                if seen.insert(from) {
                    queue.push_back((from, 1));
                }
            }
        }
        while let Some((id, r)) = queue.pop_front() {
            if r >= MAX_DEPTH || seen.len() >= MAX_NODES {
                continue;
            }
            let Some(edges) = out.get(&id) else {
                continue;
            };
            // `out` 已只含链边，直接遍历目标即可。
            for &to in edges {
                if let Some(s) = summary.get(&to) {
                    if NodeKind(s.kind.clone()).is_semantic() {
                        semantic.insert(to);
                    }
                }
                if seen.insert(to) {
                    queue.push_back((to, r + 1));
                }
            }
        }
        semantic.len()
    }

    /// 对象类视角：以**一个**对象为中心的链路子图（环 = 跳数）。
    ///
    /// 恒为**折叠**视图：只画语义节点（Table / HttpContract / ConfigKey / I18nKey / Event /
    /// Queue / Cache / Topic）与它们之间的语义边；语法节点（Method / CallSite / Class…）
    /// 被收进边的 `via` 链，并内联每一跳的调用处 —— 单击边即可逐跳核对。
    ///
    /// 曾有过 `expand` 参数可展开全部语法节点，已移除：那条分支不做提拉、不生成 `via`、
    /// 不内联调用处 —— **画了更多节点却少了证据**，还会把图撑成多层单行、要横向滚好几屏。
    pub fn object_view(
        &self,
        project_id: ProjectId,
        perspective: &str,
        center_id: NodeId,
        depth: Option<u32>,
    ) -> Result<ObjectView> {
        let registry = self.views.registry();
        let spec = registry
            .by_id(perspective)
            .ok_or_else(|| DomainError::NotFound(format!("视角 {perspective}")))?;
        let center_node = self
            .store
            .get_node(center_id)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {center_id}")))?;
        let depth = depth.unwrap_or(spec.depth).clamp(1, 6);
        // 第三方 / 库子工程（role == "library"）节点作为终点，不向外展开。
        let library_subs: HashSet<i64> = self
            .store
            .list_sub_projects(project_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|s| s.role == "library")
            .map(|s| s.id.get())
            .collect();

        // ---- 发现阶段（环 = 跳数）----
        // 非折叠：双向 BFS（要展示完整语法链路）。
        // 折叠：**沿调用链正向发现** —— HttpContract →HandledBy→ handler →Calls→ 服务方法
        //       →Reads*/MapsTo/Triggers…→ 语义节点；并先把"中心的入向链邻居"作为第 1 环，
        //       让"表 / 外部系统"这类视角能看到"谁在读写它"。
        //       **语义节点是终点**：展示但不再向外穿透——否则会经由事件 / 监听器 / 共享配置
        //       把整库都拉进来（实测可把一个路由的可达集从 ~350 膨胀到 ~2 万）。
        // **资源类中心**（Table / ConfigKey / Cache / Event…）的关系方向是反向的：
        // 语义边由"使用者"指向资源（`X --ReadsDb/WritesDb--> 表`、`X --ReadsConfig--> 配置键`），
        // 所以要回答"谁在用它"必须沿**入边回溯调用链**；路由（HttpContract）才是正向展开依赖。
        //
        // **入口类中心**（HttpContract = HTTP 入口、Schedule = 定时任务入口）与资源相反：
        // 它们**没有入边**，依赖全在出边（`Schedule --HandledBy--> handler →Calls→ …→ ReadsCache`）。
        // 曾把 Schedule 归进"资源类"沿入边回溯 → 一条边也走不到，画布上只剩孤零零一个中心节点
        // （环全空、`hidden.total = 0`），而候选列表的徽标却按 3 跳评分显示"语义依赖 1" —— 自相矛盾。
        // 判定"往哪走"的依据不是"是不是语义节点"，而是**依赖在出边还是入边**：入口在出边，资源在入边。
        let reverse = !matches!(
            center_node.kind.as_str(),
            NodeKind::HTTP_CONTRACT | NodeKind::SCHEDULE
        );
        let chain_depth = depth;

        // 一次性预加载全工程节点概要 + 链边邻接，供整段视图复用：
        // BFS 的语义性判定、链路回溯都用这一份，不再重复加载
        // （此前每次请求加载了 3 遍节点概要 + 2 遍链边邻接，是 ~3.5s 卡顿主因）。
        let summary = self.store.nodes_summary(project_id)?;
        // `inc` 只服务于候选打分（已移出对象视图，见 `ObjectView` 注释），这里不再需要。
        let (out, _inc) = self.store.chain_adjacency(project_id)?;

        // 语义性按需由 kind 计算，避免为「是否语义节点」反复查库。
        let sem_of = |id: i64| -> bool {
            summary
                .get(&id)
                .map(|m| NodeKind(m.kind.clone()).is_semantic())
                .unwrap_or(false)
        };

        let mut d = self.discover(
            center_id,
            &center_node,
            chain_depth,
            reverse,
            &library_subs,
            &summary,
            &sem_of,
        )?;
        let fold = self.fold_and_collect(
            &mut d,
            center_id,
            &center_node,
            &summary,
            &out,
        )?;
        let mut shown_edges = fold.edges;
        let hidden_total = fold.hidden_total;
        let hidden_by_kind = fold.hidden_by_kind;
        self.inline_chain_locations(&mut shown_edges, &d, &summary);
        // 可见节点：环内"语义节点 / 强制可见节点"。提拉阶段可能把环外的语义目标登记进环，
        // 所以放在提拉之后计算。
        let visible_nodes: Vec<i64> = d
            .ring_of
            .keys()
            .copied()
            .filter(|&id| {
                d.semantic_of.get(&id).copied().unwrap_or(false) || d.force_visible.contains(&id)
            })
            .collect();
        // ---- 只保留与中心真正连通的可见节点（丢弃孤立叶子，守住"单链路"诚实性）----
        let mut touched: HashSet<i64> = HashSet::new();
        touched.insert(center_id.get());
        for e in &shown_edges {
            touched.insert(e.from.get());
            touched.insert(e.to.get());
        }

        // ---- 组装可见节点的环与节点视图 ----
        let max_ring = d.ring_of.values().copied().max().unwrap_or(0);
        let mut visible_rings: Vec<Vec<NodeId>> = vec![vec![]; max_ring as usize + 1];
        for &id in &visible_nodes {
            if !touched.contains(&id) {
                continue;
            }
            if let Some(r) = d.ring_of.get(&id) {
                if let Some(slot) = visible_rings.get_mut(*r as usize) {
                    slot.push(NodeId(id));
                }
            }
        }
        for slot in visible_rings.iter_mut() {
            slot.sort_unstable();
        }

        let center_view = self
            .build_node_view(center_id, 0, &d.in_edges, &d.out_edges)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {center_id}")))?;
        let ring_views: Vec<Vec<NodeView>> = visible_rings
            .iter()
            .enumerate()
            .skip(1)
            .map(|(r, slot)| {
                slot.iter()
                    .filter_map(|id| {
                        self.build_node_view(*id, r as u32, &d.in_edges, &d.out_edges)
                            .ok()
                            .flatten()
                    })
                    .collect()
            })
            .collect();

        let hidden = HiddenInfo {
            total: hidden_total + shown_edges.len(),
            shown: shown_edges.len(),
            by_kind: hidden_by_kind,
            note: format!(
                "当前视图已折叠语法节点（Method / CallSite 等），只保留语义节点与它们之间的依赖边：\
                 已画 {} 条边，另有 {} 个语法节点被折叠。单击任意一条边可查看它经由的每一跳及调用处。",
                shown_edges.len(),
                hidden_total
            ),
        };

        let unresolved = self.unresolved_for(project_id, &center_node.name);
        let conclusions = self.conclusions_for(
            project_id,
            spec,
            &center_node.name,
            &center_view,
            shown_edges.len(),
        );

        Ok(ObjectView {
            project_id,
            perspective: spec.id.clone(),
            layout: spec.layout,
            center: center_view,
            rings: ring_views,
            edges: shown_edges,
            hidden,
            unresolved,
            conclusions,
        })
    }

    /// 发现阶段：以中心为起点沿**调用链**做分层 BFS，产出折叠视图所需的全部状态。
    ///
    /// 见 `Discovery` 的文档：这一阶段结束后状态即冻结，后续只读。
    #[allow(clippy::too_many_arguments)]
    fn discover(
        &self,
        center_id: NodeId,
        center_node: &gt_domain::model::Node,
        chain_depth: u32,
        reverse: bool,
        library_subs: &HashSet<i64>,
        summary: &HashMap<i64, NodeSummary>,
        sem_of: &dyn Fn(i64) -> bool,
    ) -> Result<Discovery> {
        let sub_of =
            |id: i64| -> i64 { summary.get(&id).and_then(|m| m.sub_project_id).unwrap_or(0) };
        let mut ring_of: HashMap<i64, u32> = HashMap::new();
        let mut parent_of: HashMap<i64, i64> = HashMap::new();
        let mut kind_of: HashMap<i64, String> = HashMap::new();
        let mut name_of: HashMap<i64, String> = HashMap::new();
        let mut semantic_of: HashMap<i64, bool> = HashMap::new();
        // 边缓存：BFS 与后续「提拉 / 原样保留边」共享，避免对同一节点反复 `edges_of` 往返。
        let mut out_edges: HashMap<i64, Vec<gt_domain::model::Edge>> = HashMap::new();
        let mut in_edges: HashMap<i64, Vec<gt_domain::model::Edge>> = HashMap::new();

        // 事件 / 队列 / 缓存等中介节点：其直接生产方（`Triggers` 入边）与消费方
        // （`HandledBy` / `PublishesTo` 出边）通常不是第一类语义节点（而是
        // `event('x')` 调用点或被 `listener` 标注的类），但正是该视角关心的对象。
        // 反向模式原本只画语义「使用者」，会把它们漏掉、让中介节点看起来孤立。
        // 这里在发现阶段前先把它们种入第 1 环并强制可见。
        let mut force_visible: HashSet<i64> = HashSet::new();
        // 反向模式：从中心走到该节点途中遇到的**语义边种类**（即"资源被怎样访问"）。
        let mut path_kind: HashMap<i64, String> = HashMap::new();
        // 与 `path_kind` 平行：走到该节点途中遇到的语义边**是否为传播得来的间接边**。
        // 资源视角（"谁在读这个配置"）同样要能区分直接读者与"上游读过"的间接入口。
        let mut path_indirect: HashMap<i64, bool> = HashMap::new();
        // 与 `path_kind` 平行：这条原始边是**中心 → 该节点**（中心为出边的一侧）。
        // 反向模式默认画「使用者 --语义边--> 中心」，这对主语在使用者的边（Triggers /
        // ReadsDb …）是对的；但 `HandledBy` 的主语是**中心自己**（`事件 --由…处理--> 监听器`、
        // `契约 --由…处理--> handler`），它是中心的出边，照默认方向画就颠倒了谓语。
        let mut path_from_center: HashSet<i64> = HashSet::new();
        // 每个节点在本层择优时胜出的**边 id**：用于「边 id 小者优先」的确定性兜底比较。
        let mut best_edge_id_of: HashMap<i64, i64> = HashMap::new();
        kind_of.insert(center_id.get(), center_node.kind.to_string());
        name_of.insert(center_id.get(), center_node.name.clone());
        semantic_of.insert(center_id.get(), node_is_semantic(&center_node));
        ring_of.insert(center_id.get(), 0);

        // 按层批量取边（分块 `IN` 查询），写入边缓存。
        let fetch_edges = |ids: &[i64],
                           out: &mut HashMap<i64, Vec<gt_domain::model::Edge>>,
                           inc: &mut HashMap<i64, Vec<gt_domain::model::Edge>>|
         -> Result<()> {
            if ids.is_empty() {
                return Ok(());
            }
            let nids: Vec<NodeId> = ids.iter().map(|i| NodeId(*i)).collect();
            let outs = self.store.edges_outgoing(&nids)?;
            let ins = self.store.edges_incoming(&nids)?;
            for (k, v) in outs {
                out.entry(k).or_insert(v);
            }
            for (k, v) in ins {
                inc.entry(k).or_insert(v);
            }
            Ok(())
        };

        // 预取中心节点的双向边（Event 播种 / 正向播种 / 第 0 层展开都要用）。
        fetch_edges(&[center_id.get()], &mut out_edges, &mut in_edges)?;

        if matches!(center_node.kind.as_str(), "Event" | "Queue" | "Topic") {
            let outs = out_edges.get(&center_id.get()).cloned().unwrap_or_default();
            let ins = in_edges.get(&center_id.get()).cloned().unwrap_or_default();
            for e in outs.iter().chain(ins.iter()) {
                if !matches!(e.kind.as_str(), "Triggers" | "HandledBy" | "PublishesTo") {
                    continue;
                }
                let center_side = e.from_id.get() == center_id.get();
                let other = if center_side {
                    e.to_id.get()
                } else {
                    e.from_id.get()
                };
                if other == center_id.get() {
                    continue;
                }
                path_kind.entry(other).or_insert(e.kind.to_string());
                // 中心的**出边**（如 `事件 --HandledBy--> 监听器`）：画边时不能反过来。
                if center_side {
                    path_from_center.insert(other);
                }
                if !ring_of.contains_key(&other) {
                    ring_of.insert(other, 1);
                    parent_of.insert(other, center_id.get());
                    semantic_of.insert(other, false);
                }
                force_visible.insert(other);
            }
        }

        // 正向模式：把"中心的入向链邻居"也作为第 1 环（表 / 外部系统视角需要）。
        // 这些第 1 环节点要在第 1 层与中心的孩子一起展开。
        let mut seed_ring1: Vec<i64> = Vec::new();
        if !reverse {
            if let Some(inc) = in_edges.get(&center_id.get()) {
                for e in inc {
                    if !is_chain_edge(e.kind.as_str()) {
                        continue;
                    }
                    let other = e.from_id.get();
                    if other == center_id.get() || ring_of.contains_key(&other) {
                        continue;
                    }
                    ring_of.insert(other, 1);
                    parent_of.insert(other, center_id.get());
                    seed_ring1.push(other);
                }
            }
        }

        // 分层 BFS：每层一次性批量取边，把原来「每节点一次 edges_of 往返」压成几次分块查询；
        // 语义节点是终点（折叠模式不再向外穿透），第三方库子工程节点也不展开。
        let library_check = !library_subs.is_empty();
        let mut current: Vec<i64> = vec![center_id.get()];
        let mut ring: u32 = 0;
        loop {
            if current.is_empty() {
                break;
            }
            fetch_edges(&current, &mut out_edges, &mut in_edges)?;
            let mut next: Vec<i64> = Vec::new();
            for &id in &current {
                if !kind_of.contains_key(&id) {
                    if let Some(m) = summary.get(&id) {
                        kind_of.insert(id, m.kind.clone());
                        name_of.insert(id, m.name.clone());
                        semantic_of.insert(id, NodeKind(m.kind.clone()).is_semantic());
                    }
                }
                let k = kind_of.get(&id).cloned().unwrap_or_default();
                // 语义节点是终点，不再向外穿透
                if id != center_id.get() && sem_of(id) {
                    continue;
                }
                // 折叠模式：把方法的"声明类"也纳入链路，让**类级语义边**（Dao→Model、Model→Table）浮现。
                // 安全：`Declares` 不在链边集合里，所以从类节点出发不会再展开出它的一堆方法。
                if k == "Method" {
                    if let Some(inc) = in_edges.get(&id) {
                        for e in inc {
                            if e.kind.as_str() != "Declares"
                                || ring_of.contains_key(&e.from_id.get())
                            {
                                continue;
                            }
                            // 沿用路径上的语义边种类，避免后续被 `HandledBy` 之类的边覆盖
                            let inherited = path_kind.get(&id).cloned().unwrap_or_default();
                            path_kind.entry(e.from_id.get()).or_insert(inherited);
                            ring_of.insert(e.from_id.get(), ring + 1);
                            parent_of.insert(e.from_id.get(), id);
                            next.push(e.from_id.get());
                        }
                    }
                }
                if ring >= chain_depth {
                    continue;
                }
                if library_check && library_subs.contains(&sub_of(id)) {
                    continue;
                }
                // 折叠视图沿调用链单向发现：正向（路由）沿出边下钻，反向（资源）沿入边回溯。
                let dir_edges: Vec<&gt_domain::model::Edge> = if reverse {
                    in_edges.get(&id).into_iter().flatten().collect()
                } else {
                    out_edges.get(&id).into_iter().flatten().collect()
                };
                for e in dir_edges {
                    if !is_chain_edge(e.kind.as_str()) {
                        continue;
                    }
                    let other = if e.from_id.get() == id {
                        e.to_id.get()
                    } else {
                        e.from_id.get()
                    };
                    // 同一节点 `other` 可能被本层多条边（不同父、不同 kind、直接或传播边）
                    // 同时发现。以前用 `or_insert`（先到先得），而"先到"取决于 `HashMap` / 边查询
                    // 的遍历顺序，导致每次运行给同一节点打上不同的「访问方式」标签，甚至把**直接读者
                    // 误判为间接**（影响前端虚线渲染）。改为按「直接优先 → 高 rank 优先 → 边 id
                    // 小者优先」的确定顺序择优，结果不再依赖任何非确定遍历顺序。
                    let already = ring_of.get(&other).copied();
                    if let Some(r) = already {
                        if r != ring + 1 {
                            // 已在更近的层确定路径（或异常更远），不再处理。
                            continue;
                        }
                        // r == ring + 1：本层重见，按比较器决定是否升级。
                    }
                    // 访问方式标签优先级：ReadsDb/WritesDb(3) > ReadsConfig/ReadsCache/
                    // PublishesTo/Triggers(2) > MapsTo(1) > 其它(0)。`MapsTo` 是结构映射
                    // （Model→Table），不是"如何访问"，必须被真正的读/写访问压过。
                    let access_rank = |k: &str| -> u8 {
                        match k {
                            "ReadsDb" | "WritesDb" => 3,
                            "ReadsConfig" | "ReadsCache" | "PublishesTo" | "Triggers" => 2,
                            "MapsTo" => 1,
                            _ => 0,
                        }
                    };
                    let indirect = is_indirect_edge(e);
                    let rank = access_rank(e.kind.as_str());
                    let inherited_rank = path_kind.get(&id).map(|k| access_rank(k)).unwrap_or(0);
                    let pk = if rank > inherited_rank {
                        e.kind.to_string()
                    } else if inherited_rank > 0 {
                        path_kind.get(&id).cloned().unwrap()
                    } else if is_semantic_edge(e.kind.as_str()) {
                        e.kind.to_string()
                    } else {
                        String::new()
                    };
                    let first_time = already.is_none();
                    let upgrade = if first_time {
                        true
                    } else {
                        let old_ind = path_indirect.get(&other).copied().unwrap_or(false);
                        let old_rank = path_kind.get(&other).map(|k| access_rank(k)).unwrap_or(0);
                        let old_eid = best_edge_id_of.get(&other).copied().unwrap_or(0);
                        better_path(indirect, rank, e.id.get(), old_ind, old_rank, old_eid)
                    };
                    if upgrade {
                        path_kind.insert(other, pk);
                        path_indirect.insert(other, indirect);
                        ring_of.insert(other, ring + 1);
                        parent_of.insert(other, id);
                        best_edge_id_of.insert(other, e.id.get());
                    }
                    if first_time {
                        next.push(other);
                    }
                }
            }
            // 把"正向播种的第 1 环"并入第 1 层一起展开。
            if ring == 0 {
                for s in &seed_ring1 {
                    if !next.contains(s) {
                        next.push(*s);
                    }
                }
            }
            current = next;
            ring += 1;
        }

        // 补齐所有可达节点的 kind / 语义性（边展示阶段要用）—— 直接读预加载的节点概要，
        // 不再逐个 `get_node` 往返。
        for &id in ring_of.keys() {
            if !kind_of.contains_key(&id) {
                if let Some(m) = summary.get(&id) {
                    kind_of.insert(id, m.kind.clone());
                    name_of.insert(id, m.name.clone());
                    semantic_of.insert(id, NodeKind(m.kind.clone()).is_semantic());
                }
            }
        }

        // 补齐环内节点的边缓存（原逻辑在提拉 / 原样保留边时会对每个环内节点再 `edges_of` 一次；
        // 这里一次性批量补齐，等价于原行为但把 N 次往返压成几次分块查询）。
        let miss_out: Vec<i64> = ring_of
            .keys()
            .copied()
            .filter(|id| !out_edges.contains_key(id))
            .collect();
        if !miss_out.is_empty() {
            let nids: Vec<NodeId> = miss_out.iter().map(|i| NodeId(*i)).collect();
            if let Ok(m) = self.store.edges_outgoing(&nids) {
                for (k, v) in m {
                    out_edges.entry(k).or_insert(v);
                }
            }
        }

        Ok(Discovery {
            ring_of,
            parent_of,
            kind_of,
            name_of,
            semantic_of,
            in_edges,
            out_edges,
            force_visible,
            path_kind,
            path_indirect,
            path_from_center,
            reverse,
        })
    }

    /// 内联链路节点的位置。
    ///
    /// 折叠视图的"链路"是**临时提拉**的结果，中间跳只存在于当次响应里，按边 id 重查
    /// 拿不到 —— 这里一次性查好、按链路顺序（起点 → 各跳 → 终点）内联进 `EdgeView`，
    /// 省掉前端对每个节点单独请求 `/nodes/{id}/locations` 的 N+1 次往返，
    /// 且这些位置与"这条边"同源。
    fn inline_chain_locations(
        &self,
        shown_edges: &mut Vec<EdgeView>,
        d: &Discovery,
        summary: &HashMap<i64, NodeSummary>,
    ) {
        // ---- 补回 P8 传播边漏掉的"真实接触点" ----
        //
        // 折叠出的语义边，其 `via` 末端必须是**真实接触点**：自己持有指向该资源的直接语义边
        // （P5 命中、带 `evidence`）。反向（资源视角）会沿 P8 **传播边**这条"捷径"回溯，
        // 于是 `via` 停在上游调用者（如 `SystemGroupData::set_status`），漏掉真正访问资源的
        // `CacheService::clear`；随后填 `to_call_site` 时接触点无 `evidence` 可依，就退到
        // 环路里**任取一个同资源读者** —— 表现为"本链路访问缓存的位置"指向无关文件
        // （实测 `PUT /setting/seckill_data/set_status` 的缓存边被标到
        // `DataMigrationServices.php:53` 的 `Cache::get(self::MIGRATION_STATUS_PREFIX . $name)`，
        // 而该路由其实经 `CacheService::clear()` → `Cache::tag('crmeb')->clear()` 触达缓存）。
        //
        // 传播边的 `seed_source(s)` 正是那条原始直接边的起点（见 `propagate.rs`）：现在传播边
        // 记录升序、确定的 `seed_sources` 数组（兼容 `seed_source = 最小 seed`），把它补回
        // `via` 末端：既补齐缺的一跳，也让接触点重新锚定到有证据的直接边上，且不再随建图
        // 遍历顺序而变（修掉同一路由画出边数在 29/30 间跳的非确定问题）。
        // 只对**接触点是传播边**的情形生效 —— 直接边已有 `evidence`，不受影响。
        for e in shown_edges.iter_mut() {
            let contact = e.via.last().map(|v| v.id.get()).unwrap_or(e.from.get());
            let Some(ins) = d.in_edges.get(&e.to.get()) else {
                continue;
            };
            let Some(raw) = ins
                .iter()
                .find(|r| r.kind.as_str() == e.kind && r.from_id.get() == contact)
            else {
                continue;
            };
            if raw.properties.get("evidence").is_some() {
                continue;
            }
            // 接触点是传播边：没有 `evidence`，真实接触点在 `seed_source(s)`（原始直接边起点）。
            // 取 `seed_sources` 数组（升序、确定）；优先选其中**确实持有指向本资源、带 evidence
            // 直接边**的那个，否则按 id 升序取第一个 —— 即便同一条传播边由多个根因汇聚，
            // 也始终是同一个接触点，不再随建图遍历顺序而变。
            let seeds = match raw.properties.get("seed_sources").and_then(|v| v.as_array()) {
                Some(arr) => arr.iter().filter_map(|x| x.as_i64()).collect::<Vec<i64>>(),
                None => raw
                    .properties
                    .get("seed_source")
                    .and_then(|v| v.as_i64())
                    .map(|s| vec![s])
                    .unwrap_or_default(),
            };
            if seeds.is_empty() {
                continue;
            }
            let mut seed = *seeds.iter().min().unwrap();
            // 优先：能从出边缓存里找到「带 evidence 的同 kind 直接边」的 seed（环路内基本都命中）。
            for s in &seeds {
                let Some(outs) = d.out_edges.get(s) else {
                    continue;
                };
                if outs.iter().any(|r| {
                    r.kind.as_str() == e.kind
                        && r.to_id.get() == e.to.get()
                        && r.properties.get("evidence").is_some()
                }) {
                    seed = *s;
                    break;
                }
            }
            if seed == e.to.get() || seed == contact || e.via.iter().any(|v| v.id.get() == seed) {
                continue;
            }
            // kind / 短名取自本视图已经预加载的 `summary`，别再为它俩各 `get_node` 一次。
            let (kind, name) = summary
                .get(&seed)
                .map(|m| {
                    let raw = m.fqn.as_deref().filter(|s| !s.is_empty()).unwrap_or(&m.name);
                    (m.kind.clone(), self.short_name_str(raw))
                })
                .unwrap_or_default();
            e.via.push(ViaNode {
                id: NodeId(seed),
                kind,
                name,
                call_site: self.call_site_between(contact, seed, &d.out_edges, &NodeCache::default()),
            });
            e.hops = Some(e.via.len() as u32);
        }

        // ---- 内联链路节点的位置 ----
        // 折叠视图的"链路"是**临时提拉**的结果，中间跳只存在于当次响应里，
        // 按边 id 重查拿不到 —— 以往前端只能对每个节点单独请求
        // `/nodes/{id}/locations`（N+1 次），且那些位置与"这条边"不同源。
        // 这里一次性查好、按链路顺序（起点 → 各跳 → 终点）内联进 EdgeView。
        {
            // 同一终点会被多条 shown 边反复问到（平行路径），入边缓存住，别每次都查库。
            let ins_cache =
                std::cell::RefCell::new(HashMap::<i64, Vec<gt_domain::model::Edge>>::new());
            let mut need: Vec<i64> = Vec::new();
            for e in shown_edges.iter() {
                need.push(e.from.get());
                for v in &e.via {
                    need.push(v.id.get());
                }
                need.push(e.to.get());
            }
            need.sort_unstable();
            need.dedup();
            let mut locs: HashMap<i64, Vec<SourceLocation>> = HashMap::new();
            let mut synth: HashMap<i64, bool> = HashMap::new();
            // 一次取回链路上全部节点（`id IN (...)` 分块）：原先逐个 `node_locations`
            // 是 N+1 往返（每个节点还要额外算 `reference_count`），一条 29 边的图要
            // 几百次查询，实测 420ms —— 占对象视图的大头。
            let nids: Vec<NodeId> = need.iter().map(|i| NodeId(*i)).collect();
            let nodes = self.store.get_nodes(&nids).unwrap_or_default();
            let loc_cache = NodeCache::default();
            for id in need {
                if let Some(n) = nodes.get(&id) {
                    if let Ok((s, l)) = self.locations_of_node(n, &loc_cache) {
                        synth.insert(id, s);
                        locs.insert(id, l);
                    }
                }
            }
            for e in shown_edges.iter_mut() {
                let mut ids: Vec<i64> = vec![e.from.get()];
                ids.extend(e.via.iter().map(|v| v.id.get()));
                ids.push(e.to.get());
                e.node_locations = ids
                    .into_iter()
                    .filter_map(|id| {
                        locs.get(&id).map(|l| NodeLocationEntry {
                            id: NodeId(id),
                            synthetic: *synth.get(&id).unwrap_or(&false),
                            locations: l.clone(),
                        })
                    })
                    .collect();

                // 终点那跳的"调用处"：语义边（`ReadsConfig` / `ReadsDb` …）在
                // `properties.evidence.location` 里记着"这条边是在哪读到的"，
                // 那正是**本链路访问该资源的位置**。
                //
                // 必须在这里补：`call_site_between` 对它无能为力 —— 其回退逻辑按
                // **被调函数名**匹配（`sys_config`），而终点的名字是**资源名**
                // （`offline_pay_status`），永远对不上，导致终点只剩"全部出处"列表，
                // 看起来像链路串到了无关文件。
                //
                // **不能靠提拉边的 `e.id` 反查**：折叠时多条 raw 边（P5 直接边 +
                // P8 传播边）会提拉到同一个 (from, to)，`push_edge` 留下哪条的 id
                // 取决于 `HashMap` 遍历顺序（非确定），而传播边**没有** `evidence`
                // —— 那样会时有时无。改为从**链路节点**（起点 + 各跳）出发，
                // 找指向终点的、带 `evidence` 的直接语义边。
                if e.to_call_site.is_none() {
                    // 在**终点的入边**里找：同 kind、带 `evidence`、且**起点在本视图环内**
                    // （即属于这条链路）的那条。取 from 最小者，保证结果确定。
                    //
                    // 不依赖提拉边的 id / via：二者会随 `HashMap` 遍历顺序变化
                    // （同一 (from,to) 上 P5 直接边与 P8 传播边竞争，而传播边无 evidence）。
                    // 这条链的**接触点**：`via` 末端（未经折叠时就是起点自己）。
                    let contact_id = e.via.last().map(|v| v.id.get()).unwrap_or(e.from.get());
                    let mut best: Option<(bool, i64, SourceLocation)> = None;
                    // 优先命中 BFS 阶段已批量预取的入边缓存；缓存未覆盖（极少数漏预取的节点）
                    // 时再回退到单点查询。避免对每条 shown 边各发一次 DB 往返（N+1）。
                    let cached_ins = d.in_edges.get(&e.to.get()).cloned();
                    let store_ins = if cached_ins.is_some() {
                        None
                    } else {
                        // 同一终点会被多条 shown 边反复问到（平行路径），缓存住，别每次都查库。
                        Some(
                            ins_cache
                                .borrow_mut()
                                .entry(e.to.get())
                                .or_insert_with(|| {
                                    self.store
                                        .edges_of(e.to, EdgeDirection::Incoming)
                                        .unwrap_or_default()
                                })
                                .clone(),
                        )
                    };
                    if let Some(ins) = cached_ins.as_ref().or(store_ins.as_ref()) {
                        for raw in ins {
                            if raw.kind.as_str() != e.kind {
                                continue;
                            }
                            // "属于这条链路" = 在本视图环内，或是这条 `via` 上的一跳。
                            // 后者正是传播边回溯出的接触点：它不在发现深度内（不在 `d.ring_of`），
                            // 但却是这条链真正访问资源的地方，它的 `evidence` 必须被采纳。
                            if !d.ring_of.contains_key(&raw.from_id.get())
                                && !e.via.iter().any(|v| v.id.get() == raw.from_id.get())
                            {
                                continue;
                            }
                            let Some(ev) = raw.properties.get("evidence") else {
                                continue;
                            };
                            let Some(loc) = ev.get("location").and_then(|v| v.as_str()) else {
                                continue;
                            };
                            let (file, line) = split_file_line(loc);
                            let cand = SourceLocation {
                                file,
                                line,
                                symbol: None,
                                note: Some("本链路访问该资源的位置".to_string()),
                                snippet: ev
                                    .get("snippet")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            };
                            // **接触点的证据优先**：`via` 末端才是这条链真正访问资源的地方。
                            // 只按"from 最小者"选，会挑中链路**上游**某处的同名读取
                            // （如 `detail` 与 `getQRCodePath` 都读 `site_url`，却总是显示 `detail` 的行号）。
                            let new = (raw.from_id.get() == contact_id, raw.from_id.get(), cand);
                            let better = match &best {
                                None => true,
                                Some(old) => (new.0 && !old.0) || (new.0 == old.0 && new.1 < old.1),
                            };
                            if better {
                                best = Some(new);
                            }
                        }
                    }
                    if let Some((_, _, loc)) = best {
                        e.to_call_site = Some(loc);
                    }
                }
            }
        }

        // 合成边（反向汇总产生，无真实行）的 id 仍是 0：统一改成**唯一负数 id**。
        // 否则前端 `<g key={id}>` 会撞 key，按 id 查找 EdgeView 也永远只命中第一条
        // （表现为"所有边都显示成同一种类"、悬浮一条高亮全部）。负号同时保留"合成边"语义。
        {
            let mut next: i64 = -1;
            for e in shown_edges.iter_mut() {
                if e.id == 0 {
                    e.id = next;
                    next -= 1;
                }
            }
        }

    }

    /// 折叠提拉 + 语义边收集：把挂在语法节点（Method / CallSite）上的语义边提拉到
    /// "发起它的语义节点"上，并把被折叠掉的中间跳记进 `via`。
    ///
    /// 见 `Discovery` 的文档：本方法接受 `&mut Discovery`（提拉阶段会把环外的语义
    /// 目标补登记进环），闭包则整体不可变借用，靠 NLL 在最后一次使用后让出借用。
    #[allow(clippy::too_many_arguments)]
    fn fold_and_collect(
        &self,
        d: &mut Discovery,
        center_id: NodeId,
        center_node: &gt_domain::model::Node,
        summary: &HashMap<i64, NodeSummary>,
        out: &HashMap<i64, Vec<i64>>,
    ) -> Result<FoldResult> {
        // `sem_of` 只依赖 `summary`，在本方法内重建即可（无需穿过函数边界传递闭包）。
        let sem_of = |id: i64| -> bool {
            summary
                .get(&id)
                .map(|m| NodeKind(m.kind.clone()).is_semantic())
                .unwrap_or(false)
        };
        // **入口类中心的入向语义边：塌缩兜底**（必须在 `is_visible` / `lift` 之前算好）。
        //
        // 前端 `Function --CallsHttp--> HttpContract` 与后端 `Method --HandledBy--> 契约`
        // 本是同一件事的两半——谓语的主语是那个**语法节点**。照常规提拉会把它的语义边挂到
        // 中心自己身上，`a == to` 使整条边被丢弃：画布上"前端调用了契约"这一事实彻底消失，
        // 而候选徽标（按原始边计数）仍写着「入边 N」——列表说有、图里没有。
        // 与反向（资源）视角的塌缩兜底同构：调用方是语法节点时把它本身点亮，如实画出
        // `调用方 --CallsHttp--> 中心`，中间的语法跳留进 `via`（drawer 逐跳可查）。
        if !d.reverse {
            let center = center_id.get();
            let mut callers: Vec<i64> = d
                .ring_of
                .keys()
                .copied()
                .filter(|id| {
                    *id != center
                        && !d.semantic_of.get(id).copied().unwrap_or(false)
                        && d
                            .out_edges
                            .get(id)
                            .into_iter()
                            .flatten()
                            .any(|e| e.to_id.get() == center && is_semantic_edge(e.kind.as_str()))
                })
                .collect();
            callers.sort_unstable();
            // 传播边的**接触点**（`seed_source`）才是真正发出调用的那个函数：它已随上游
            // 调用方的 `via` 链被如实收录（见后面 ② 的路径补全），再单独点亮就成了同一
            // 事实的第二条边（`onDelete → 契约` 与 `deleteItem → 契约` 重复）。
            let mut contacts: HashSet<i64> = HashSet::new();
            for id in &callers {
                let Some(es) = d.out_edges.get(id) else {
                    continue;
                };
                for e in es {
                    if e.to_id.get() != center || !is_semantic_edge(e.kind.as_str()) {
                        continue;
                    }
                    if let Some(seed) = seed_source_of(e) {
                        if seed != *id && callers.contains(&seed) {
                            contacts.insert(seed);
                        }
                    }
                }
            }
            for id in callers.iter().filter(|id| !contacts.contains(id)) {
                d.force_visible.insert(*id);
            }
        }

        let is_visible = |id: i64| -> bool {
            if id == center_id.get() {
                return true;
            }
            d.semantic_of.get(&id).copied().unwrap_or(false) || d.force_visible.contains(&id)
        };

        // 语义边优先级（用于折叠路径上的边提拉）：见 `Self::agg_rank`。
        let mut shown_edges: Vec<EdgeView> = Vec::new();
        // 去重键带上**路径**（via 的节点序列）：同一对端点的不同路径要都保留。
        // 只按 (kind, from, to) 去重时，多条路径只剩一条代表 —— 分叉/汇聚信息就丢了，
        // 而"展开语法"正是靠画出分叉才显得有用；现在折叠视图必须自己给出全部路径。
        let mut shown_keys: HashSet<(String, i64, i64, Vec<i64>)> = HashSet::new();
        let mut hidden_by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut hidden_total = 0usize;

        // 沿 BFS 父链收集"被折叠掉的中间节点"，顺序为**从祖先到目标**（便于前端直接串成链）。
        // `child` 是真正持有语义边的节点，`ancestor` 是提拉后的语义节点；两者相邻时返回空。
        let chain_to = |child: i64, ancestor: i64| -> Vec<ViaNode> {
            let mut path: Vec<ViaNode> = Vec::new();
            let mut cur = child;
            for _ in 0..64 {
                if cur == ancestor {
                    break;
                }
                path.push(ViaNode {
                    id: NodeId(cur),
                    kind: d.kind_of.get(&cur).cloned().unwrap_or_default(),
                    name: d.name_of.get(&cur).cloned().unwrap_or_default(),
                    call_site: None,
                });
                match d.parent_of.get(&cur) {
                    Some(p) => cur = *p,
                    None => break,
                }
            }
            path.reverse();
            path
        };

        // 去重 + 按环定向后加入一条提拉边
        // `src`：该边的**真实来源边 id**（提拉时带上，点击可查到原始证据）；反向汇总出的
        // 合成边没有来源，传 0，随后统一改成唯一负数 id。
        // `via`：被折叠掉的中间节点 —— 让"看起来直连"的边如实说明自己跨了几跳。
        // 调用处定位：优先用已缓存的出边（BFS 阶段已批量取过）做精确 `Calls` 命中，
        // 仅在精确边缺失时回退到原 `call_site_between`（含 HasCallSite 启发式）。
        // 折叠链上每个"调用处"都要查节点 / 文件 / 符号表，同一批节点被反复查 —— 请求级缓存。
        let cs_cache = NodeCache::default();
        let outs_cache = std::cell::RefCell::new(HashMap::<i64, Vec<gt_domain::model::Edge>>::new());
        let call_site = |from: i64, to: i64| -> Option<SourceLocation> {
            // 优先用已缓存的出边（BFS 阶段已批量取过）做精确 `Calls` 命中。
            // 传播边回溯出的完整链路会经过**发现深度之外**的方法节点，缓存里没有它们
            // —— 这里补一次单点查询，否则抽屉里这些跳全部没有调用处，看起来仍像断链。
            let outs = match d.out_edges.get(&from) {
                Some(v) => v.clone(),
                None => {
                    outs_cache
                        .borrow_mut()
                        .entry(from)
                        .or_insert_with(|| {
                            self.store
                                .edges_outgoing(&[NodeId(from)])
                                .ok()
                                .and_then(|m| m.get(&from).cloned())
                                .unwrap_or_default()
                        })
                        .clone()
                }
            };
            {
                for e in outs {
                    if e.kind.as_str() == "Calls" && e.to_id.get() == to {
                        if let Some(cs_id) = e.properties.get("call_site").and_then(|v| v.as_i64())
                        {
                            if let Some(loc) = self.node_source_location_cached(NodeId(cs_id), &cs_cache)
                            {
                                return Some(loc);
                            }
                        }
                    }
                }
            }
            self.call_site_between(from, to, &d.out_edges, &cs_cache)
        };
        // 把节点 id 序列还原成 `ViaNode` 链。
        //
        // `chain_to` 只能沿**发现树的父链**回溯，而传播边回溯出的完整链路会经过发现深度
        // 之外的节点 —— 它们既不在 `d.ring_of` 也不在 `d.kind_of` / `d.name_of` 里，所以要退回
        // 全工程节点概要 `summary`（本次请求已整份预加载，无额外 IO）。
        let via_from_ids = |ids: Vec<i64>| -> Vec<ViaNode> {
            ids.into_iter()
                .map(|id| ViaNode {
                    id: NodeId(id),
                    kind: d.kind_of
                        .get(&id)
                        .cloned()
                        .or_else(|| summary.get(&id).map(|m| m.kind.clone()))
                        .unwrap_or_default(),
                    name: d.name_of
                        .get(&id)
                        .cloned()
                        .or_else(|| summary.get(&id).map(|m| m.name.clone()))
                        .unwrap_or_default(),
                    call_site: None,
                })
                .collect()
        };
        // `indirect`：该语义边是否由 P8 沿调用链传播得来（起点自身并未执行该动作）。
        let push_edge = |from: i64,
                         to: i64,
                         kind: &str,
                         src: i64,
                         mut via: Vec<ViaNode>,
                         indirect: bool,
                         seen: &mut HashSet<(String, i64, i64, Vec<i64>)>,
                         out: &mut Vec<EdgeView>| {
            if from == to {
                return;
            }
            // 边的方向**恒为语义方向**：`from --kind--> to`。
            //
            // 语义边的种类是**谓语**，主语就是 `from`（`X --ReadsDb/ReadsCache/ReadsConfig--> 资源`、
            // `Model --MapsTo--> Table`）。曾按"由内向外"归一化（`d.ring_of[from] > d.ring_of[to]` 就把
            // 两端对调），路由视角看不出问题（中心就是发起方，天然由内向外）；但**资源视角的每一条边
            // 都会被翻过来**：反向模式收的就是 `(使用者, 中心资源)`，而使用者必在环 ≥1、资源在环 0，
            // 于是 `表·cache --读库--> GET /verify_code` —— 箭头、hover 卡的 `from → to`、
            // Inspector 的「起点 / 终点」全都与挂在边上的谓语（`读库` 的主语应是那个使用者）相反。
            let (f, t) = (from, to);
            // 修正浮动/汇总边的标签：`MapsTo` 的语义是「Model → Table」的**结构映射**，
            // 不应充当"路由访问表"的标签。反向（资源视角）下 `src == 0` 的浮动边是把调用链
            // 回溯合成的"谁在访问这张表"，其真实语义是读/写库 —— 按 HTTP 动词启发区分读/写。
            let mut kind = kind.to_string();
            if kind == "MapsTo" && src == 0 {
                let table_at_to = d.kind_of.get(&t).map(|s| s.as_str()) == Some("Table");
                let table_at_from = d.kind_of.get(&f).map(|s| s.as_str()) == Some("Table");
                if table_at_to || table_at_from {
                    let route_end = if table_at_to { f } else { t };
                    let route_name = d.name_of.get(&route_end).cloned().unwrap_or_default();
                    kind = if is_write_http_method(&route_name) {
                        "WritesDb".to_string()
                    } else {
                        "ReadsDb".to_string()
                    };
                }
            }
            // `via`（`chain_to` 的产物）在**祖先**一侧收尾：正向模式锚在 `from`（如路由），
            // 反向模式锚在 `to`（= 资源中心，因为链是"从中心沿入边回溯到使用者"长出来的）。
            // 锚在 `to` 时链路顺序与 `from → … → to` 恰好相反，必须翻一次；
            // 翻错了不只是顺序难看 —— 后面按 `via` 顺序逐跳取"调用处"，会整体错位成
            // "中心调用使用者"这种不存在的调用关系，每跳 call_site 全落空。
            if d.reverse {
                via.reverse();
            }
            let path: Vec<i64> = via.iter().map(|v| v.id.get()).collect();
            let key = (kind.clone(), f, t, path);
            if !seen.insert(key) {
                return;
            }
            // 给每一跳补上"调用处"：prev(起点/上一跳) → 本跳 的 CallSite 位置。
            let mut prev = f;
            for v in via.iter_mut() {
                v.call_site = call_site(prev, v.id.get());
                prev = v.id.get();
            }
            let to_call_site = call_site(prev, t);
            let hops = if via.is_empty() {
                None
            } else {
                Some(via.len() as u32)
            };
            out.push(EdgeView {
                id: src,
                kind: kind.clone(),
                from: NodeId(f),
                to: NodeId(t),
                // `resolved` 与置信度只反映"是否为可追溯的真实语义依赖"，
                // 与"直接/间接（经调用链提拉）"无关——间接边同样可以有实打实的调用链与接触点证据。
                // 直接性仅通过 `indirect`（画布虚线 + 金色「间接」标签）表达，不再降权为"待验证假设"。
                resolved: Self::agg_rank(kind.as_str()) > 0,
                confidence: if Self::agg_rank(kind.as_str()) > 0 { 0.8 } else { 0.5 },
                hops,
                via,
                to_call_site,
                indirect,
                // 由"内联链路位置"步骤统一填充（见 object_view 末尾）。
                node_locations: Vec::new(),
            });
        };

        {
            // ---- 折叠：只保留"语义节点之间的语义边"；语法节点作为透传 ----
            // lift[x] = 沿"从中心出发的发现树"回退到最早的语义祖先，从而把挂在
            // Method / CallSite 上的语义边（ReadsConfig → ConfigKey、MapsTo → Table…）
            // 提拉到"发起它的语义节点"（如路由）上。
            let mut nodes_by_dist: Vec<(u32, i64)> =
                d.ring_of.iter().map(|(&id, &r)| (r, id)).collect();
            nodes_by_dist.sort_by(|a, b| a.0.cmp(&b.0));
            let mut lift: HashMap<i64, i64> = HashMap::new();
            for (_d, id) in &nodes_by_dist {
                if is_visible(*id) {
                    lift.insert(*id, *id);
                    continue;
                }
                let l = d.parent_of
                    .get(id)
                    .and_then(|p| lift.get(p))
                    .copied()
                    .unwrap_or(*id);
                lift.insert(*id, l);
            }

            if d.reverse {
                // 反向：把"谁在用这个资源"直接画成 `使用者 --语义边--> 资源`。
                // 语义边种类取回溯途中记录的访问方式标签（优先 ReadsDb/WritesDb，其次 ReadsConfig/ReadsCache，MapsTo 仅作结构兜底）。
                //
                // 共享资源（如 Cache）可能有上千个使用者：按环序取前 N 个画出来，
                // 其余计入 `hidden` —— 保证图可读，同时诚实记账。
                const MAX_USERS: usize = 80;
                let mut users: Vec<i64> = d.ring_of
                    .keys()
                    .copied()
                    .filter(|id| {
                        *id != center_id.get() && d.semantic_of.get(id).copied().unwrap_or(false)
                    })
                    .collect();
                // 事件 / 队列 / 缓存等中介节点的直接生产/消费方已在发现阶段前种入
                // 第 1 环并 `d.force_visible`，这里把它们也作为「用户」画出（其 `d.path_kind`
                // 即 `Triggers` / `HandledBy` / `PublishesTo`）。
                if matches!(center_node.kind.as_str(), "Event" | "Queue" | "Topic") {
                    for id in d.ring_of.keys().copied() {
                        if d.force_visible.contains(&id) && !users.contains(&id) {
                            users.push(id);
                        }
                    }
                }
                // 折叠塌缩兜底：资源的**直接**访问方（第 1 环、父即中心）是语法节点、
                // 且其发现树路径上不存在任何语义用户（上游没有路由 / 契约等语义发起者
                // —— Seeder / DataGrid / 迁移脚本 / Console 命令是常态）时，它既不会被
                // 画成用户，也永远不会出现在任何提拉边的 via 链里 —— 画布只剩孤零零的
                // 中心节点，而候选徽标却按原始边计数显示「入边 N」，自相矛盾。
                // 此时把该访问方本身强制可见，直接画 `使用者 --语义边--> 资源`
                // （via 为空 = 直连，证据在边上）。反之，路径上已有语义用户时，
                // 其 via 链已如实收录这些语法跳，维持折叠视图「只画语义节点」的约束。
                {
                    let mut covered: HashSet<i64> = HashSet::new();
                    let mut sem_ids: Vec<i64> = users.clone();
                    sem_ids.sort_unstable();
                    for s in sem_ids {
                        let mut cur = s;
                        for _ in 0..64 {
                            if !covered.insert(cur) {
                                break;
                            }
                            match d.parent_of.get(&cur) {
                                Some(p) => cur = *p,
                                None => break,
                            }
                        }
                    }
                    // 上游可达语义用户判定：从候选方法沿「调用方(Calls) / 被路由处理(HandledBy)
                    // / 声明类(Declares)」等上游链边上溯，若途中经过语义用户（路由）或已覆盖节点，
                    // 则该方法的资源访问应归因于那个语义用户、折叠进其 `via`，而非作为独立直连
                    // 读者被 `collapsed` 点亮。
                    //
                    // 背景：P8 沿调用链把 `ReadsDb/WritesDb` 复刻到了每个中间调用方
                    // （controller / service / mapper），使它们都成了表的直连读者；但语义归属
                    // 应落在最上游的路由上（见 `propagate.rs` 与 `inline_chain_locations` 的
                    // seed_source 补回）。否则资源视角会画出一堆方法节点、而非其上游语义入口。
                    let upstream_reaches_semantic = |start: i64| -> bool {
                        // 多分支 BFS：从候选方法沿「调用方(Calls) / 被路由处理(HandledBy) /
                        // 声明类(Declares)」等上游链边上溯，任一分支途中遇到语义用户（路由）或
                        // 已覆盖节点即命中。必须穷举所有上游分支——只跟第一条会漏掉并行分支
                        // （如 controller 类同时被 `HandledBy→路由` 与 `Declares→文件` 连接，
                        // 或 mapper 同时被 `Calls→service` 与 `Declares→接口` 连接）。
                        let mut frontier: Vec<i64> = vec![start];
                        let mut seen: HashSet<i64> = HashSet::new();
                        for _ in 0..64 {
                            if frontier.is_empty() {
                                break;
                            }
                            let mut nxt: Vec<i64> = Vec::new();
                            for cur in frontier {
                                if !seen.insert(cur) {
                                    continue;
                                }
                                if users.contains(&cur)
                                    || covered.contains(&cur)
                                    || d.semantic_of.get(&cur).copied().unwrap_or(false)
                                    || summary
                                        .get(&cur)
                                        .map_or(false, |m| NodeKind(m.kind.clone()).is_semantic())
                                {
                                    return true;
                                }
                                if let Some(ins) = d.in_edges.get(&cur) {
                                    for e in ins {
                                        if matches!(
                                            e.kind.as_str(),
                                            "Calls" | "HandledBy" | "Declares"
                                        ) {
                                            nxt.push(e.from_id.get());
                                        }
                                    }
                                }
                            }
                            frontier = nxt;
                        }
                        false
                    };
                    let mut collapsed: Vec<i64> = d.ring_of
                        .keys()
                        .copied()
                        .filter(|id| {
                            *id != center_id.get()
                                && d.parent_of.get(id) == Some(&center_id.get())
                                && !d.semantic_of.get(id).copied().unwrap_or(false)
                                && !covered.contains(id)
                                && !upstream_reaches_semantic(*id)
                                && d.path_kind.get(id).map_or(false, |k| !k.is_empty())
                        })
                        .collect();
                    collapsed.sort_unstable();
                    for id in &collapsed {
                        d.force_visible.insert(*id);
                    }
                    users.extend(collapsed);
                }
                users.sort_by_key(|id| d.ring_of.get(id).copied().unwrap_or(0));
                for id in users.iter().take(MAX_USERS) {
                    let kind = d.path_kind
                        .get(id)
                        .cloned()
                        .filter(|k| !k.is_empty())
                        .unwrap_or_else(|| "Reads".to_string());
                    let mut via = chain_to(*id, center_id.get());
                    // d.reverse 模式下 *id 本身是语义节点，最终会成为提拉边的端点
                    // （端点已由 EdgeView.from/.to 渲染）；但 chain_to 会把 *id 一并收进
                    // via 的末节点，导致抽屉里的折叠链出现「端点既在 via 又在首尾」的闭环
                    // （如 GET /products 同时出现在首尾、store_category 反而被埋进中间）。
                    // 去掉末节点即可让端点只由 from/to 表达。
                    if via.last().map(|v| v.id.get()) == Some(*id) {
                        via.pop();
                    }
                    // 边的方向**必须跟着原始边**：这里默认画「使用者 --语义边--> 中心」，
                    // 对主语在使用者的边（Triggers / ReadsDb / PublishesTo …）是对的；
                    // 但 `HandledBy` 是「中心 --由…处理--> 监听器 / handler」，主语是中心
                    // （它是播种时中心的**出边**），照默认方向画会读成"监听器由…处理事件"。
                    let from_center = d.path_from_center.contains(id);
                    // 端点对调后，链路顺序也要跟着翻一次：`push_edge` 在 reverse 下还会再翻，
                    // 两次翻转正好还原为「中心 → … → 对方」的阅读顺序。
                    if from_center {
                        via.reverse();
                    }
                    push_edge(
                        if from_center { center_id.get() } else { *id },
                        if from_center { *id } else { center_id.get() },
                        &kind,
                        0,
                        via,
                        d.path_indirect.get(id).copied().unwrap_or(false),
                        &mut shown_keys,
                        &mut shown_edges,
                    );
                }
                for id in users.iter().skip(MAX_USERS) {
                    hidden_total += 1;
                    if let Some(k) = d.kind_of.get(id) {
                        *hidden_by_kind.entry(k.clone()).or_insert(0) += 1;
                    }
                }
            } else {
                // 环外语义目标先暂存，等 `push_edge`/`chain_to`（捕获了这些 map）用完后
                // 再写回 `d.ring_of`/`d.parent_of`，避免与它们的不可变借用冲突。
                let mut pending: Vec<(i64, i64, u32)> = Vec::new();
                // 按「(kind, 提拉祖先 a, 目标 to)」分组，每组取**边 id 最小**的原始边作为代表。
                // 否则同一视图边可能由不同原始边「先到先得」地决定 `indirect` / `via` / 证据，
                // 而原始边的选取依赖 HashMap / 边查询的遍历顺序 → 每次运行结果不同。
                let mut groups: HashMap<(String, i64, i64), Vec<gt_domain::model::Edge>> =
                    HashMap::new();
                // **必须排序**：`d.ring_of` 是 HashMap，直接取 keys 会让 `groups` 的插入顺序
                // 与 `pending` 的登记顺序随运行变化 —— 而 `pending` 用 `or_insert` 决定
                // 环外语义目标的 `d.ring_of` / `d.parent_of`，进而改变 `chain_to` 回溯出的 via，
                // 最终让**画出的边数都逐次不同**（实测同一路由 29 / 30 条来回跳）。
                let mut all_nodes: Vec<i64> = d.ring_of.keys().copied().collect();
                all_nodes.sort_unstable();
                for node in &all_nodes {
                    let outs = d.out_edges.get(node).into_iter().flatten().cloned();
                    for e in outs {
                        // 只画语义边，且目标必须是语义节点（ReadsConfig→ConfigKey、MapsTo→Table…）
                        if !is_semantic_edge(e.kind.as_str()) {
                            continue;
                        }
                        let to = e.to_id.get();
                        // 目标可以是"被语法链挡在环外的语义节点"：把它登记进环，
                        // 这样即便 depth 较小，折叠视图仍能把语义目标渲染出来。
                        let to_is_sem = summary
                            .get(&to)
                            .map(|m| NodeKind(m.kind.clone()).is_semantic())
                            .unwrap_or(false);
                        if !to_is_sem {
                            continue;
                        }
                        if !d.ring_of.contains_key(&to) {
                            let from_ring = d.ring_of.get(&e.from_id.get()).copied().unwrap_or(0);
                            pending.push((to, e.from_id.get(), from_ring + 1));
                        }
                        let a = *lift.get(&e.from_id.get()).unwrap_or(&e.from_id.get());
                        if a == to {
                            continue;
                        }
                        groups
                            .entry((e.kind.as_str().to_string(), a, to))
                            .or_default()
                            .push(e);
                    }
                }
                // 同一对 `(kind, a, to)` 上最多画几条**不同路径**：再多会糊成一片，
                // 超出部分计入 `hidden` 诚实记账。
                const MAX_PATHS: usize = 4;
                // 传播边回溯到接触点时允许的最大跳数。`seed_source` 通常在发现深度之外，
                // 但离中心并不远（CRMEB 实测 4~5 跳）；超过即放弃回溯、退回原行为。
                const MAX_SEED_HOPS: usize = 8;
                        for ((kind, a, to), mut es) in groups {
                    es.sort_by_key(|e| e.id.get());
                    // **路径起点必须是真实接触点**。
                    //
                    // P5 直接边带 `evidence`，是"动作在此发生"的证据；P8 传播边只陈述
                    // "上游可达该资源"，是**摘要而不是路径**。若拿它当起点，`via` 就只能
                    // 走到发现深度为止，画出"路由自己读了缓存"这种断尾伪路径 ——
                    // 与边的种类（读库 / 读缓存 / 投递）无关，一律适用。
                    let mut by_origin: BTreeMap<i64, &gt_domain::model::Edge> = BTreeMap::new();
                    for e in &es {
                        if !is_indirect_edge(e) {
                            by_origin.entry(e.from_id.get()).or_insert(e);
                        }
                    }
                    // 候选路径：(接触点, 完整 via 的节点 id 序列, 证据边 id, 是否间接)
                    let mut drawn: Vec<(i64, Vec<i64>, i64, bool)> = Vec::new();
                    // ① 接触点边：`via` 由发现树回溯，末端就是发出动作的那个方法。
                    for (o, e) in &by_origin {
                        // `indirect` 同时考虑两点：① 原始边本身就是传播边；② 该边是从子孙
                        // 节点**提拉**到祖先 `a` 的——祖先并不直接执行该动作。任一成立即间接。
                        let ids: Vec<i64> =
                            chain_to(*o, a).iter().map(|v| v.id.get()).collect();
                        drawn.push((*o, ids, e.id.get(), a != *o || is_indirect_edge(e)));
                    }
                    // ② 传播边：按 `seed_source` 回溯到接触点，补全**完整**到达路径。
                    //    同一 seed 的多条传播边（沿途每个栈帧各一条）在这里并成同一批路径，
                    //    无需任何"前缀剔除"之类的启发式——它们本来就是同一事实的投影。
                    let mut seen_seeds: HashSet<i64> = HashSet::new();
                    for e in &es {
                        let Some(seed) = seed_source_of(e) else {
                            continue;
                        };
                        if by_origin.contains_key(&seed) || !seen_seeds.insert(seed) {
                            continue;
                        }
                        let paths =
                            enumerate_chain_paths(&out, a, seed, MAX_SEED_HOPS, MAX_PATHS, &sem_of);
                        if paths.is_empty() {
                            // 接触点在本视图的链边邻接里不可达（跨子工程 / `Calls` 缺失）。
                            // 退回"传播边自身"的 via：宁可截断，也不能把这条依赖整个弄丢。
                            let ids: Vec<i64> = chain_to(e.from_id.get(), a)
                                .iter()
                                .map(|v| v.id.get())
                                .collect();
                            drawn.push((e.from_id.get(), ids, e.id.get(), true));
                            continue;
                        }
                        for p in paths {
                            if p.len() < 2 {
                                continue;
                            }
                            // 证据边优先取接触点**自己**的直接边（带 `evidence`）：
                            // 传播边没有证据，点开"边详情"会一片空白。
                            let ev_id = d.in_edges
                                .get(&to)
                                .into_iter()
                                .flatten()
                                .find(|r| {
                                    r.from_id.get() == seed
                                        && r.kind.as_str() == kind.as_str()
                                        && !is_indirect_edge(r)
                                })
                                .map(|r| r.id.get())
                                .unwrap_or_else(|| e.id.get());
                            // `p` = [a, …, seed]；`via` 去掉首端的中心、保留末端的接触点。
                            drawn.push((seed, p[1..].to_vec(), ev_id, true));
                        }
                    }
                    // ③ 同一 (接触点, 路径) 只画一条，超出 `MAX_PATHS` 计入 hidden 诚实记账。
                    let mut seen_path: HashSet<(i64, Vec<i64>)> = HashSet::new();
                    for (origin, via, id, indirect) in drawn {
                        if !seen_path.insert((origin, via.clone())) {
                            continue;
                        }
                        if seen_path.len() > MAX_PATHS {
                            hidden_total += 1;
                            continue;
                        }
                        push_edge(
                            a,
                            to,
                            &kind,
                            id,
                            via_from_ids(via),
                            indirect,
                            &mut shown_keys,
                            &mut shown_edges,
                        );
                    }
                }
                for (to, from, r) in pending {
                    d.ring_of.entry(to).or_insert(r);
                    d.parent_of.entry(to).or_insert(from);
                }
            }

            // 提拉阶段可能把环外的语义目标登记进了 `d.ring_of`，这里补齐它们的
            // kind / name / 语义性（直接读预加载的节点概要，不查库）。必须在 `chain_to` 等
            // 捕获了这些 map 的闭包使用完毕后再做，避免借用冲突。
            for &id in d.ring_of.keys() {
                if !d.kind_of.contains_key(&id) {
                    if let Some(m) = summary.get(&id) {
                        d.kind_of.insert(id, m.kind.clone());
                        d.name_of.insert(id, m.name.clone());
                        d.semantic_of.insert(id, NodeKind(m.kind.clone()).is_semantic());
                    }
                }
            }

            // 被折叠的语法节点按 kind 记账
            for (id, k) in &d.kind_of {
                if !d.semantic_of.get(id).copied().unwrap_or(false) {
                    *hidden_by_kind.entry(k.clone()).or_insert(0) += 1;
                    hidden_total += 1;
                }
            }
        }

        Ok(FoldResult {
            edges: shown_edges,
            hidden_total,
            hidden_by_kind,
        })
    }




    /// 聚合类视角：聚类框 / 矩阵（**不是单链路**）。
    pub fn aggregate_view(
        &self,
        project_id: ProjectId,
        perspective: &str,
        sample_limit: u32,
    ) -> Result<AggregateView> {
        let registry = self.views.registry();
        let spec = registry
            .by_id(perspective)
            .ok_or_else(|| DomainError::NotFound(format!("视角 {perspective}")))?;

        let target_kind = spec.node_kind.clone().unwrap_or_default();
        let nodes = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: if target_kind.is_empty() {
                None
            } else {
                Some(gt_domain::model::NodeKind(target_kind.clone()))
            },
            name_contains: None,
            limit: Some(2000),
            offset: Some(0),
        })?;

        if spec.layout == gt_domain::model::LayoutMode::Matrix {
            return Ok(self.matrix_view(project_id, spec, nodes, sample_limit));
        }

        // Compound / ER：按 group_by 分组
        let mut groups: BTreeMap<String, Vec<gt_domain::model::Node>> = BTreeMap::new();
        for n in nodes {
            let key = self.group_key(spec, &n);
            groups.entry(key).or_default().push(n);
        }

        let sub_names: HashMap<i64, String> = self
            .store
            .list_sub_projects(project_id)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.get(), s.name))
            .collect();
        let clusters: Vec<Cluster> = groups
            .into_iter()
            .map(|(key, members)| {
                let _ = &sub_names;
                let count = members.len();
                let samples = members
                    .into_iter()
                    .take(sample_limit as usize)
                    .filter_map(|n| {
                        self.build_node_view(n.id, 1, &HashMap::new(), &HashMap::new())
                            .ok()
                            .flatten()
                    })
                    .collect();
                Cluster {
                    label: key.clone(),
                    key,
                    count,
                    members: samples,
                }
            })
            .collect();
        let cluster_count_holder = clusters.clone();

        let total: usize = cluster_count_holder.iter().map(|c| c.count).sum();
        let notice = if cluster_count_holder.is_empty() {
            Some(format!(
                "图里还没有可用于「{}」聚合的节点，因此这里不给任何聚类结论。",
                spec.label
            ))
        } else {
            None
        };

        Ok(AggregateView {
            project_id,
            perspective: spec.id.clone(),
            layout: spec.layout,
            clusters: cluster_count_holder.clone(),
            matrix: None,
            hidden: HiddenInfo {
                total,
                shown: clusters.len(),
                by_kind: BTreeMap::new(),
                note: "聚合视图按分组只展示样例成员，完整成员数以计数呈现。".into(),
            },
            unresolved: self.unresolved_for(project_id, ""),
            conclusions: json!({ "clusters": clusters.len() }),
            notice,
        })
    }

    fn matrix_view(
        &self,
        project_id: ProjectId,
        spec: &PerspectiveSpec,
        nodes: Vec<gt_domain::model::Node>,
        _sample_limit: u32,
    ) -> AggregateView {
        let sub_names: HashMap<i64, String> = self
            .store
            .list_sub_projects(project_id)
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id.get(), s.name))
            .collect();
        let mut rows: Vec<String> = Vec::new();
        let mut cols: Vec<String> = Vec::new();
        let mut row_idx: HashMap<String, usize> = HashMap::new();
        let mut col_idx: HashMap<String, usize> = HashMap::new();
        let mut cells: Vec<Vec<u32>> = Vec::new();

        for n in &nodes {
            let row = self.group_key_with(spec.row_from.as_ref(), n, &sub_names);
            let col = self.group_key_with(spec.col_from.as_ref(), n, &sub_names);
            let r = match row_idx.get(&row) {
                Some(i) => *i,
                None => {
                    row_idx.insert(row.clone(), rows.len());
                    rows.push(row.clone());
                    cells.push(vec![0; cols.len()]);
                    rows.len() - 1
                }
            };
            let c = match col_idx.get(&col) {
                Some(i) => *i,
                None => {
                    col_idx.insert(col.clone(), cols.len());
                    cols.push(col.clone());
                    for row_cells in cells.iter_mut() {
                        row_cells.push(0);
                    }
                    cols.len() - 1
                }
            };
            cells[r][c] += 1;
        }

        let row_totals: Vec<u32> = cells.iter().map(|r| r.iter().sum()).collect();
        let mut col_totals = vec![0u32; cols.len()];
        for r in &cells {
            for (i, v) in r.iter().enumerate() {
                col_totals[i] += *v;
            }
        }

        AggregateView {
            project_id,
            perspective: spec.id.clone(),
            layout: spec.layout,
            clusters: Vec::new(),
            matrix: Some(MatrixView {
                rows,
                cols,
                cells,
                row_totals,
                col_totals,
            }),
            hidden: HiddenInfo {
                total: nodes.len(),
                shown: nodes.len(),
                by_kind: BTreeMap::new(),
                note: "矩阵单元格为两维度下的节点数量（0 表示该组合确实没有产出）。".into(),
            },
            unresolved: Vec::new(),
            conclusions: json!({ "cells": nodes.len() }),
            notice: None,
        }
    }

    fn group_key(&self, spec: &PerspectiveSpec, node: &gt_domain::model::Node) -> String {
        self.group_key_with(spec.group_by.as_ref(), node, &HashMap::new())
    }

    fn group_key_with(
        &self,
        by: Option<&GroupBy>,
        node: &gt_domain::model::Node,
        sub_names: &HashMap<i64, String>,
    ) -> String {
        match by {
            Some(GroupBy::NodeKind) => node.kind.to_string(),
            Some(GroupBy::SubProject) => node
                .sub_project_id
                .and_then(|s| sub_names.get(&s.get()).cloned())
                .unwrap_or_else(|| {
                    node.sub_project_id
                        .map(|s| format!("子工程 #{}", s))
                        .unwrap_or_else(|| "未归属".into())
                }),
            Some(GroupBy::Property(name)) => node
                .properties
                .get(name)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(无)".to_string()),
            None => node.kind.to_string(),
        }
    }

    /// 节点的定义位置（合成节点返回**多位置列表**）。
    pub fn node_locations(&self, node_id: NodeId) -> Result<NodeLocations> {
        let node = self
            .store
            .get_node(node_id)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {node_id}")))?;
        let (synthetic, locations) = self.locations_of_node(&node, &NodeCache::default())?;
        let reference_count = self
            .store
            .edges_of(node_id, EdgeDirection::Incoming)
            .map(|e| e.len())
            .unwrap_or(0);

        Ok(NodeLocations {
            id: node_id,
            kind: node.kind.to_string(),
            name: node.name.clone(),
            synthetic,
            locations,
            reference_count,
        })
    }

    /// 从**已在内存里的**节点算出位置列表（不再查库取节点）。
    ///
    /// `files` 是文件路径缓存：一条链路上的多个节点常落在同一批文件里，
    /// 逐个 `file_path()` 会重复往返。
    fn locations_of_node(
        &self,
        node: &gt_domain::model::Node,
        c: &NodeCache,
    ) -> Result<(bool, Vec<SourceLocation>)> {
        let synthetic = node.identity.is_some();
        let mut locations: Vec<SourceLocation> = Vec::new();

        // ① 语法节点：文件 + 行号 + 符号
        if let Some(file_id) = node.file_id {
            if let Some(path) = self.cached_file(c, file_id) {
                locations.push(SourceLocation {
                    file: path,
                    line: node.span.start_line,
                    symbol: node.fqn.clone(),
                    note: Some(format!("{} 定义", node.kind)),
                    snippet: node
                        .properties
                        .get("snippet")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }

        // ② 合成节点：各处共现位置
        if let Some(Value::Array(arr)) = node.properties.get("locations") {
            for item in arr {
                if let Ok(loc) = serde_json::from_value::<SourceLocation>(item.clone()) {
                    locations.push(loc);
                }
            }
        }

        // ③ 权威符号表里的出处（如 schema 的 SQL 文件、route_list 的路由文件）
        if let Some(name) = node
            .identity
            .as_ref()
            .map(|i| i.value.clone())
            .or_else(|| node.fqn.clone())
        {
            for table in [
                "schema",
                "route_list",
                "event_listeners",
                "container_bindings",
            ] {
                if let Some(entry) = self.cached_symbol(c, node.project_id, table, &name) {
                    if let Some(file) = entry.get("file").and_then(|v| v.as_str()) {
                        locations.push(SourceLocation {
                            file: file.to_string(),
                            line: entry.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                            symbol: Some(name.clone()),
                            note: Some(format!("权威源 {table}")),
                            snippet: None,
                        });
                    }
                    if let Some(Value::Array(srcs)) = entry.get("sources") {
                        for s in srcs.iter().take(20) {
                            if let Some(s) = s.as_str() {
                                let (file, line) = match s.split_once(':') {
                                    Some((f, l)) => (f.to_string(), l.parse::<u32>().unwrap_or(0)),
                                    None => (s.to_string(), 0),
                                };
                                locations.push(SourceLocation {
                                    file,
                                    line,
                                    symbol: Some(name.clone()),
                                    note: Some(format!("{table} 来源")),
                                    snippet: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok((synthetic, locations))
    }

    /// 边的证据链：实边单点跳，虚线边展开途经的 CallSite 位置。
    pub fn edge_evidence(&self, edge_id: i64) -> Result<Option<EdgeEvidence>> {
        let Some(edge) = self.find_edge(edge_id)? else {
            return Ok(None);
        };
        let view = self.to_edge_view(edge.clone());
        let mut locations: Vec<SourceLocation> = Vec::new();
        let mut via: Vec<String> = Vec::new();
        let mut reason: Option<String> = None;

        // 证据一：边上自带的 evidence
        if let Some(Value::Object(ev)) = edge.properties.get("evidence") {
            if let Some(loc) = ev.get("location").and_then(|v| v.as_str()) {
                let (file, line) = split_file_line(loc);
                locations.push(SourceLocation {
                    file,
                    line,
                    symbol: None,
                    note: Some("边建立时的证据位置".into()),
                    // P5 建边时一并存入的调用语句原文。
                    snippet: ev
                        .get("snippet")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
            if let Some(s) = ev.get("evidence").and_then(|v| v.as_str()) {
                via.push(s.to_string());
            }
        }

        // 证据二：起点方法内的 CallSite（虚线边要能逐跳验证）
        if let Some(from) = self.store.get_node(edge.from_id)? {
            if let Some(file_id) = from.file_id {
                if let Some(path) = self.file_path(file_id) {
                    locations.push(SourceLocation {
                        file: path,
                        line: from.span.start_line,
                        symbol: from.fqn.clone(),
                        note: Some("起点定义".into()),
                        snippet: from
                            .properties
                            .get("snippet")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }
        if let Some(to) = self.store.get_node(edge.to_id)? {
            if let Some(file_id) = to.file_id {
                if let Some(path) = self.file_path(file_id) {
                    locations.push(SourceLocation {
                        file: path,
                        line: to.span.start_line,
                        symbol: to.fqn.clone(),
                        note: Some("终点定义".into()),
                        snippet: to
                            .properties
                            .get("snippet")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }

        // 仅在「未解析且无任何可定位证据」时提示：有真实调用点/证据位置的边不应被冤枉为"推断边"。
        if !view.resolved && locations.is_empty() {
            reason = Some(format!(
                "该边（{}）没有可定位的证据位置，可能来自规则补全，请谨慎采信。",
                edge.kind
            ));
        }

        Ok(Some(EdgeEvidence {
            edge: view,
            reason,
            locations,
            via,
        }))
    }

    // ---------------------------------------------------------------- 内部

    /// 语义边优先级：用于折叠路径上的边提拉，以及 `to_edge_view` 判定是否"已解析"。
    /// 返回 >0 即视为已解析的语义边（来自真实调用点/FKB 规则合成，而非无证据的推断）。
    fn agg_rank(kind: &str) -> u8 {
        match kind {
            "ReadsDb" | "WritesDb" => 9,
            "ReadsConfig" => 8,
            "ReadsCache" => 7,
            "PublishesTo" => 6,
            "MapsTo" => 5,
            "Triggers" => 4,
            "CallsHttp" => 3,
            "HandledBy" => 2,
            "ResolvesTo" => 1,
            _ => 0,
        }
    }

    fn to_edge_view(&self, e: gt_domain::model::Edge) -> EdgeView {
        // 实边判定：来自权威解析（P7 解析结果）或语法确定的继承/实现/声明
        let authoritative = matches!(
            e.kind.as_str(),
            "Extends" | "Implements" | "UsesTrait" | "Declares" | "Contains" | "HasCallSite"
        );
        let has_tier = e.properties.get("tier").is_some();
        // `indirect`（P8 沿调用链传播）只决定画布虚线与「间接」标签，不再降权为"待验证"。
        let indirect = is_indirect_edge(&e);
        // 已解析 = 语法/结构边（authoritative）或 权威解析边（has_tier）或 语义边（agg_rank>0）。
        let resolved = (authoritative || has_tier) || Self::agg_rank(e.kind.as_str()) > 0;
        let hops = e
            .properties
            .get("hops")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        EdgeView {
            id: e.id.get(),
            kind: e.kind.to_string(),
            from: e.from_id,
            via: Vec::new(),
            to: e.to_id,
            resolved,
            confidence: e.confidence,
            hops,
            to_call_site: None,
            indirect,
            node_locations: Vec::new(),
        }
    }

    /// 单个节点的"定义位置"（无 file/span 时返回 None）。
    fn node_source_location_cached(&self, id: NodeId, c: &NodeCache) -> Option<SourceLocation> {
        let node = self.cached_node(c, id.get())?;
        let file_id = node.file_id?;
        let path = self.cached_file(c, file_id)?;
        Some(SourceLocation {
            file: path,
            line: node.span.start_line,
            symbol: node.fqn.clone(),
            note: None,
            // CallSite 节点带"调用语句"原文，显示在链路的每一跳下便于核验。
            snippet: node
                .properties
                .get("snippet")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }

    /// 请求级缓存取节点：同一节点在折叠 / 内联链路里会被查很多次，
    /// 缓存让它最多查一次库（见 [`NodeCache`]）。
    fn cached_node(&self, c: &NodeCache, id: i64) -> Option<gt_domain::model::Node> {
        if let Some(hit) = c.nodes.borrow().get(&id) {
            return hit.clone();
        }
        let v = self.store.get_node(NodeId(id)).ok().flatten();
        c.nodes.borrow_mut().insert(id, v.clone());
        v
    }

    fn cached_file(&self, c: &NodeCache, id: gt_domain::model::FileId) -> Option<String> {
        if let Some(hit) = c.files.borrow().get(&id.get()) {
            return hit.clone();
        }
        let v = self.file_path(id);
        c.files.borrow_mut().insert(id.get(), v.clone());
        v
    }

    fn cached_symbol(
        &self,
        c: &NodeCache,
        project_id: gt_domain::model::ProjectId,
        table: &str,
        key: &str,
    ) -> Option<Value> {
        let k = (project_id.get(), table.to_string(), key.to_string());
        if let Some(hit) = c.symbols.borrow().get(&k) {
            return hit.clone();
        }
        let v = self.store.get_symbol(project_id, table, key).ok().flatten();
        c.symbols.borrow_mut().insert(k, v.clone());
        v
    }

    /// 两个节点之间"调用处"的位置。
    ///
    /// **精确路径**：`from --Calls--> to` 这条边在解析阶段就显式记录了对应的 `CallSite` 节点 id
    /// （见 `resolve_calls`），直接取它即可，无需任何启发式。仅当这条精确边不存在（例如
    /// `route → handler`、`method → table` 这类语义边，而非 `Calls` 边）时，才回退到
    /// `HasCallSite` 调用点按被调名匹配。
    fn call_site_between(
        &self,
        from: i64,
        to: i64,
        out_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
        c: &NodeCache,
    ) -> Option<SourceLocation> {
        // 复用 BFS 阶段批量预取的出边缓存，避免对每条折叠边各发一次 DB 往返（N+1）。
        // 缓存未覆盖（极少数漏预取的节点）时回退到单点查询。
        let outs = out_cache.get(&from).cloned().or_else(|| {
            self.store
                .edges_of(NodeId(from), EdgeDirection::Outgoing)
                .ok()
        });
        // 1) 精确：直接读 `from → to` 的 Calls 边上记录的 CallSite 节点 id
        if let Some(outs) = outs.as_ref() {
            for e in outs {
                if e.kind.as_str() != "Calls" || e.to_id.get() != to {
                    continue;
                }
                if let Some(cs_id) = e.properties.get("call_site").and_then(|v| v.as_i64()) {
                    if let Some(loc) = self.node_source_location_cached(NodeId(cs_id), c) {
                        return Some(loc);
                    }
                }
            }
        }
        // 3) route → handler（HttpContract ─HandledBy→ handler）不是 Calls 边，没有 CallSite；
        //    但它的「调用处」就是路由注册处。HttpContract 是合成节点（无 file_id/span），
        //    `node_source_location` 会返回 None，故改用 `node_locations` 取它汇聚的路由文件+行号
        //    （来自 properties.locations 或 route_list 符号表），避免折叠链在起点这一跳莫名缺「调用语句」。
        let from_node = self.cached_node(c, from);
        let from_kind = from_node
            .as_ref()
            .map(|n| n.kind.to_string())
            .unwrap_or_default();
        if matches!(from_kind.as_str(), "HttpContract" | "Route" | "Endpoint") {
            if let Some(n) = from_node.as_ref() {
                if let Ok((_, locs)) = self.locations_of_node(n, c) {
                    if let Some(loc) = locs.into_iter().next() {
                        return Some(loc);
                    }
                }
            }
        }
        // 2) 回退：from 的 HasCallSite 调用点里，被调名与 to 匹配的那一个
        let to_name = match self.cached_node(c, to) {
            Some(n) => {
                let raw = n.fqn.as_deref().filter(|s| !s.is_empty()).unwrap_or(&n.name);
                self.short_name_str(raw)
            }
            None => String::new(),
        };
        if to_name.is_empty() {
            return None;
        }
        for e in outs.as_ref()? {
            if e.kind.as_str() != "HasCallSite" {
                continue;
            }
            let cs = e.to_id.get();
            if self
                .cached_node(c, cs)
                .map(|n| n.kind.to_string())
                .unwrap_or_default()
                != "CallSite"
            {
                continue;
            }
            if let Some(cs_node) = self.cached_node(c, cs) {
                if let Some(fqn) = &cs_node.fqn {
                    // fqn 形如 "<owner>#<callee_text>:<line>"（见 cf_ast.rs）：
                    //   "Owner::callee:line" / "Owner->callee:line" / "new Klass:line"
                    // 取 '#' 之后、末尾 ":line" 之前的部分作为被调标识，再交给 `short_name_str`
                    // 取末段方法名，与 `to` 的短名比较。
                    // 旧实现 `split([':', '-', '>']).next()` 在第一个 ':' 就切断 ——
                    // "Class::method" 被切成 "Class"、"this->method" 切成 "this"，永远对不上方法名，
                    // 导致所有变量接收者调用（`resolve_calls` 故意不为它们建 Calls 边）的回退全部失效。
                    if let Some(callee_full) = fqn.split('#').nth(1) {
                        let callee = callee_full
                            .trim_end_matches(|c: char| c.is_ascii_digit())
                            .trim_end_matches(':')
                            .trim_end();
                        let callee_method = callee.strip_prefix("new ").unwrap_or(callee).trim();
                        if !callee_method.is_empty()
                            && self.short_name_str(callee_method) == to_name
                        {
                            return self.node_source_location_cached(NodeId(cs), c);
                        }
                    }
                }
            }
        }
        None
    }

    /// 取节点名（方法名 / 类名）的"短名"：去掉命名空间与前缀，仅保留最后一段。
    fn short_name_of(&self, id: i64) -> String {
        if let Some(n) = self.store.get_node(NodeId(id)).ok().flatten() {
            let raw = n
                .fqn
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(&n.name);
            return self.short_name_str(raw);
        }
        String::new()
    }

    /// 从 "App\X\Foo::bar" / "obj->bar" / "new Foo" 中提取 "bar" / "Foo"。
    fn short_name_str(&self, s: &str) -> String {
        let s = s.rsplit("::").next().unwrap_or(s);
        let s = s.rsplitn(2, "->").next().unwrap_or(s);
        s.strip_prefix("new ").unwrap_or(s).trim().to_string()
    }

    fn build_node_view(
        &self,
        id: NodeId,
        ring: u32,
        in_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
        out_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    ) -> Result<Option<NodeView>> {
        let Some(n) = self.store.get_node(id)? else {
            return Ok(None);
        };
        let registry = self.views.registry();
        let own_view = registry
            .view_for_kind(n.kind.as_str())
            .map(|s| s.id.clone());
        let has_own_view = own_view.is_some();
        let annotations: Vec<String> = self
            .store
            .annotations_of(id)?
            .into_iter()
            .map(|a| match a.subkind {
                Some(s) => format!("{}:{}", a.kind, s),
                None => a.kind,
            })
            .collect();
        // 优先命中 BFS 已批量预取的边缓存，避免对每个节点各发 2 次 DB 往返（N+1）。
        let fan_in = in_cache
            .get(&id.get())
            .map(|e| e.len())
            .or_else(|| {
                self.store
                    .edges_of(id, EdgeDirection::Incoming)
                    .ok()
                    .map(|e| e.len())
            })
            .unwrap_or(0);
        let fan_out = out_cache
            .get(&id.get())
            .map(|e| e.len())
            .or_else(|| {
                self.store
                    .edges_of(id, EdgeDirection::Outgoing)
                    .ok()
                    .map(|e| e.len())
            })
            .unwrap_or(0);
        let mut locations = Vec::new();
        if let Some(Value::Array(arr)) = n.properties.get("locations") {
            for item in arr.iter().take(8) {
                if let Ok(l) = serde_json::from_value::<SourceLocation>(item.clone()) {
                    locations.push(l);
                }
            }
        }
        // 节点所属「端」：FKB 在语义节点上标注的 `side`（`frontend` / `backend`）。
        // 透传给前端，用于图上区分前后端子工程。
        let side = n
            .properties
            .get("side")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(Some(NodeView {
            id,
            kind: n.kind.to_string(),
            category: n
                .properties
                .get("category")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            name: n.name.clone(),
            fqn: n.fqn.clone(),
            ring,
            sub_project_id: n.sub_project_id,
            has_own_view,
            own_view,
            side,
            locations,
            annotations,
            metrics: json!({ "fan_in": fan_in, "fan_out": fan_out }),
        }))
    }

    fn conclusions_for(
        &self,
        project_id: ProjectId,
        spec: &PerspectiveSpec,
        name: &str,
        center: &NodeView,
        drawn_edges: usize,
    ) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("视角".into(), json!(spec.label));
        out.insert("对象".into(), json!(name));
        out.insert(
            "入边".into(),
            json!(center.metrics.get("fan_in").cloned().unwrap_or(json!(0))),
        );
        out.insert(
            "出边".into(),
            json!(center.metrics.get("fan_out").cloned().unwrap_or(json!(0))),
        );
        if !center.annotations.is_empty() {
            out.insert("标注".into(), json!(center.annotations));
        }
        // 权威源补充（能补就补，补不了就不写，绝不编造）
        if let Ok(Some(entry)) = self.store.get_symbol(project_id, "schema", name) {
            let cols = entry
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            out.insert("schema 列数".into(), json!(cols));
        }
        if let Ok(Some(entry)) = self.store.get_symbol(project_id, "route_list", name) {
            out.insert(
                "路由表登记".into(),
                entry.get("handler").cloned().unwrap_or(json!(null)),
            );
        }
        // 入口类视角（路由 / 定时任务）若一条语义边都没画出来，画布会只剩孤零零一个中心节点，
        // 容易被误以为"视图坏了"。多半是真实情况（crontab 路由没解析到 handler，
        // 或 handler 没接触任何语义资源）。给一行提示，避免与二级候选徽标自相矛盾。
        if drawn_edges == 0
            && matches!(
                center.kind.as_str(),
                NodeKind::HTTP_CONTRACT | NodeKind::SCHEDULE
            )
        {
            let hint = if center.kind.as_str() == NodeKind::SCHEDULE {
                "该定时任务暂无可展开的语义依赖：crontab 路由未解析到 handler，或 handler 未接触任何语义资源（表 / 配置 / 缓存 / 事件）。这通常是真实情况，并非视图缺失。"
            } else {
                "该路由暂无可展开的语义依赖：路由未解析到 handler，或 handler 未接触任何语义资源。这通常是真实情况，并非视图缺失。"
            };
            out.insert("提示".into(), json!(hint));
        }
        Value::Object(out)
    }

    fn unresolved_for(&self, project_id: ProjectId, name: &str) -> Vec<UnresolvedInfo> {
        let Ok(all) = self.store.list_diagnostics(project_id, 500) else {
            return Vec::new();
        };
        let mut out: Vec<UnresolvedInfo> = all
            .into_iter()
            .filter(|d| {
                if name.is_empty() {
                    return true;
                }
                d.message.contains(name)
                    || d.location
                        .as_deref()
                        .map(|l| l.contains(name))
                        .unwrap_or(false)
            })
            .take(20)
            .map(|d| UnresolvedInfo {
                code: d.code,
                message: d.message,
                location: d.location,
            })
            .collect();
        out.truncate(20);
        out
    }

    fn find_edge(&self, edge_id: i64) -> Result<Option<gt_domain::model::Edge>> {
        // `edges_of` 只能按节点查；这里借助 nodes 表反查代价过高，
        // 因此直接从图统计遍历是不现实的 —— 用 SQL 侧提供的查询端口替代。
        let _ = edge_id;
        Ok(self.store.find_edge(gt_domain::model::EdgeId(edge_id))?)
    }

    fn file_path(&self, file_id: gt_domain::model::FileId) -> Option<String> {
        self.store.file_path(file_id).ok().flatten()
    }
}

fn split_file_line(s: &str) -> (String, u32) {
    match s.rsplit_once(':') {
        Some((f, l)) => (f.to_string(), l.parse::<u32>().unwrap_or(0)),
        None => (s.to_string(), 0),
    }
}

// ---------------------------------------------------------------- 语义 / 调用链判定

/// 调用链边：折叠视图沿这些边做"正向发现"，把语法节点当透传。
/// 权威定义在 `gt_domain::model::kinds::is_chain_edge`。
fn is_chain_edge(kind: &str) -> bool {
    gt_domain::model::kinds::is_chain_edge(kind)
}

/// 语义节点判定：第一类语义 kind（`kinds.rs` 的 `SYNTHESIZED`：`Table` / `HttpContract` /
/// `ConfigKey` / `I18nKey` / `Event` / `Queue` / `Cache` / `Topic`…）。
/// `Event` / `Queue` / `Cache` / `Topic` 现在是具体种类，不再依赖 `category` 伞。
/// 分类权威来自 `kinds.rs`。
fn node_is_semantic(n: &gt_domain::model::Node) -> bool {
    NodeKind(n.kind.to_string()).is_semantic()
}

/// 语义边判定（权威来源：`kinds.rs` 的语义边集合）。
fn is_semantic_edge(kind: &str) -> bool {
    EdgeKind(kind.to_string()).is_semantic()
}

/// 是否为**传播得来**的间接边（P8 沿 `Calls` 调用链复刻，见 `propagate.rs`）。
///
/// 直接由 FKB 规则命中的边只带 `evidence`；传播边带 `via: "propagate"`，
/// 其中环境读取类（`ReadsConfig` / `ReadsCache`）还会额外带 `indirect: true`。
///
/// 必须区分二者：否则任何读了某配置的共享方法，都会让所有途经它的入口
/// 在图上显示为"直接读取该配置"，看起来像是真实依赖。
fn is_indirect_edge(e: &gt_domain::model::Edge) -> bool {
    e.properties
        .get("indirect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || e.properties.get("via").and_then(|v| v.as_str()) == Some("propagate")
}

/// 传播边的**根因接触点**：P8 记录的 `seed_source`，即真正执行该动作的方法节点。
///
/// 注意 `is_indirect_edge` 与"是传播边"等价：直接边只带 `evidence`，传播边必带
/// `via: "propagate"`（环境读取类还额外带 `indirect: true`）。
fn seed_source_of(e: &gt_domain::model::Edge) -> Option<i64> {
    e.properties.get("seed_source").and_then(|v| v.as_i64())
}

/// 一次视图请求内的节点缓存。
///
/// 折叠与内联链路会对**同一批节点反复查库**：`get_node` / `file_path` / `get_symbol`
/// 各自一次往返，一条 29 边的图要几百次查询 —— 实测"调用处定位"一项就占
/// 对象视图 ~670ms（占 2/3）。缓存让每个节点 / 文件 / 符号表项最多查一次。
///
/// 请求级而非全局：图会被重建，缓存跨请求会读到陈旧数据。
#[derive(Default)]
pub(crate) struct NodeCache {
    nodes: std::cell::RefCell<HashMap<i64, Option<gt_domain::model::Node>>>,
    files: std::cell::RefCell<HashMap<i64, Option<String>>>,
    symbols: std::cell::RefCell<HashMap<(i64, String, String), Option<serde_json::Value>>>,
}

/// 在链边邻接上枚举 `from → to` 的完整**简单路径**（DFS，最多 `limit` 条，深度上限 `max_hops`）。
///
/// 为什么需要它：传播边只陈述"上游可达该资源"，它自己**不是路径**。若直接拿它当 `via`
/// 的末端，`via` 就断在发现深度上，画出"路由自己读了缓存"这种伪路径（实际接触点在几跳之外）。
/// 这里从中心一路走到 `seed_source` 接触点，把被折叠掉的整条链补全 —— 折叠视图的 `via`
/// 必须一直落到接触点，否则前端抽屉里看到的是断链。
///
/// * `avoid`：中间跳的排除谓词（传"是否语义节点"；语义节点是终点，不该被穿过）;
/// * 探索节点数有硬预算，防御稠密子图上的组合爆炸。
fn enumerate_chain_paths(
    out: &HashMap<i64, Vec<i64>>,
    from: i64,
    to: i64,
    max_hops: usize,
    limit: usize,
    avoid: &dyn Fn(i64) -> bool,
) -> Vec<Vec<i64>> {
    if from == to || limit == 0 || max_hops == 0 {
        return Vec::new();
    }

    fn dfs(
        out: &HashMap<i64, Vec<i64>>,
        from: i64,
        cur: i64,
        to: i64,
        max_hops: usize,
        limit: usize,
        avoid: &dyn Fn(i64) -> bool,
        path: &mut Vec<i64>,
        on_path: &mut HashSet<i64>,
        acc: &mut Vec<Vec<i64>>,
        budget: &mut usize,
    ) {
        if acc.len() >= limit || *budget == 0 || path.len() > max_hops {
            return;
        }
        *budget -= 1;
        let Some(nexts) = out.get(&cur) else {
            return;
        };
        for &n in nexts {
            if n == to {
                let mut full = path.clone();
                full.push(n);
                acc.push(full);
                if acc.len() >= limit {
                    return;
                }
                continue;
            }
            // 简单路径：不回到起点、不走重复节点；不穿过被排除的节点（语义节点是终点）。
            if n == from || on_path.contains(&n) || avoid(n) {
                continue;
            }
            on_path.insert(n);
            path.push(n);
            dfs(out, from, n, to, max_hops, limit, avoid, path, on_path, acc, budget);
            path.pop();
            on_path.remove(&n);
            if acc.len() >= limit {
                return;
            }
        }
    }

    let mut acc: Vec<Vec<i64>> = Vec::new();
    let mut path = vec![from];
    let mut on_path: HashSet<i64> = HashSet::new();
    on_path.insert(from);
    let mut budget = 20_000usize;
    dfs(
        out, from, from, to, max_hops, limit, avoid, &mut path, &mut on_path, &mut acc, &mut budget,
    );
    acc
}

/// 比较两个「节点访问方式」候选，返回 `new` 是否优于 `old`。
///
/// 优先级：① 直接(`false`) 优于 间接(`true`)；② 访问 rank 高者优；③ 边 id 小者优（确定性兜底）。
///
/// 边 id 全图唯一，故同一 BFS 层内给定节点的所有候选里**优胜者唯一**——
/// 无论 `HashMap` / 边查询以何种顺序遍历，最终选中的边都相同，结果确定。
fn better_path(
    new_indirect: bool,
    new_rank: u8,
    new_eid: i64,
    old_indirect: bool,
    old_rank: u8,
    old_eid: i64,
) -> bool {
    if new_indirect != old_indirect {
        return !new_indirect; // 新的更直接 → 更优
    }
    if new_rank != old_rank {
        return new_rank > old_rank;
    }
    new_eid < old_eid
}

/// 按 HTTP 契约名（如 `POST /product`、`GET /products`）首词判定读/写，
/// 作为"路由访问表"是读库还是写库的启发式。
///
/// 静态分析无法总判定某条调用链最终执行的是 `SELECT` 还是 `INSERT/UPDATE/DELETE`；
/// 这里用温水动词兜底：写类动词（`POST`/`PUT`/`DELETE`/`PATCH`）判为写库（`WritesDb`），
/// 其余（含 `GET`/`HEAD`/`OPTIONS` 与未知）判为读库（`ReadsDb`）。
fn is_write_http_method(route_name: &str) -> bool {
    let verb = route_name
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_uppercase();
    matches!(verb.as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
}
