//! 视图用例：把图**按视角切片**成"当前这一个对象"的链路子图。
//!
//! 核心诚实性约束：
//! * 对象类视角**只画中心对象的链路**；其它对象的链路边直接不画（不是变暗）
//! * 被省略的部分以 [`HiddenInfo`] 计数 + [`UnresolvedInfo`] 记账呈现
//! * 合成节点的位置**永远是多位置列表**，绝不编造单一位置

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use em_domain::error::{DomainError, Result};
use em_domain::model::{
    graph::NodeSummary, AggregateView, Candidate, Cluster, EdgeEvidence, EdgeKind, EdgeView,
    GroupBy, HiddenInfo, MatrixView, NodeId, NodeKind, NodeLocationEntry, NodeLocations, NodeView,
    ObjectView, PerspectiveSpec, ProjectId, SourceLocation, UnresolvedInfo, ViewRegistry, ViaNode,
};
use em_domain::port::{EdgeDirection, NodeFilter, Persistence, ViewRegistryProvider};
use serde_json::{json, Value};

/// 视角服务。
pub struct ViewService {
    store: Arc<dyn Persistence>,
    views: Arc<dyn ViewRegistryProvider>,
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
                (em_domain::model::ViewMode::Object, Some(kind)) => {
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
    ) -> Result<Vec<Candidate>> {
        let spec = self
            .views
            .registry()
            .by_id(perspective)
            .ok_or_else(|| DomainError::NotFound(format!("视角 {perspective}")))?;
        let Some(kind) = &spec.node_kind else {
            return Ok(Vec::new());
        };
        let nodes = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(em_domain::model::NodeKind(kind.clone())),
            name_contains: name_contains.map(|s| s.to_string()),
            // 无搜索词时多取一些参与"价值排序"（要选出全局最高价值的对象）；
            // 有搜索词时只按名称过滤、不做昂贵打分。
            limit: Some(if name_contains.is_none() { 5000 } else { limit }),
            offset: Some(0),
        })?;

        // 有搜索词：直接按名称命中，不做全量打分（下拉按需搜索，求快）。
        if name_contains.is_some() {
            return Ok(nodes
                .into_iter()
                .map(|n| Candidate {
                    id: n.id,
                    name: n.name,
                    badge: None,
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
        nodes: Vec<em_domain::model::Node>,
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
    /// `expand = Some(true)` 时不做折叠，展示全部语法节点。
    pub fn object_view(
        &self,
        project_id: ProjectId,
        perspective: &str,
        center_id: NodeId,
        depth: Option<u32>,
        expand: Option<bool>,
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
        // 默认折叠：只展示"对人类有意义的语义节点"（Table / HttpContract / ConfigKey /
        // I18nKey / Event / Queue / Cache / Topic）与语义边；语法节点（Method / CallSite / Class…）属于实现细节，
        // 折叠起来、点击可展开（`expand = Some(true)` 展开全部语法节点）。
        let collapse = !expand.unwrap_or(false);
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
        let reverse = collapse && center_node.kind.as_str() != NodeKind::HTTP_CONTRACT;
        let chain_depth = depth;

        // 一次性预加载全工程节点概要 + 链边邻接，供整段视图复用：
        // BFS 的语义性判定、候选打分（末尾 `rank_candidates`）、链路回溯都用这一份，
        // 不再重复加载（此前每次请求加载了 3 遍节点概要 + 2 遍链边邻接，是 ~3.5s 卡顿主因）。
        let summary = self.store.nodes_summary(project_id)?;
        let (out, inc) = self.store.chain_adjacency(project_id)?;

        // 语义性按需由 kind 计算，避免为「是否语义节点」反复查库。
        let sem_of = |id: i64| -> bool {
            summary
                .get(&id)
                .map(|m| NodeKind(m.kind.clone()).is_semantic())
                .unwrap_or(false)
        };
        let sub_of = |id: i64| -> i64 {
            summary.get(&id).and_then(|m| m.sub_project_id).unwrap_or(0)
        };

        let mut ring_of: HashMap<i64, u32> = HashMap::new();
        let mut parent_of: HashMap<i64, i64> = HashMap::new();
        let mut kind_of: HashMap<i64, String> = HashMap::new();
        let mut name_of: HashMap<i64, String> = HashMap::new();
        let mut semantic_of: HashMap<i64, bool> = HashMap::new();
        // 边缓存：BFS 与后续「提拉 / 原样保留边」共享，避免对同一节点反复 `edges_of` 往返。
        let mut out_edges: HashMap<i64, Vec<em_domain::model::Edge>> = HashMap::new();
        let mut in_edges: HashMap<i64, Vec<em_domain::model::Edge>> = HashMap::new();

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
        // 每个节点在本层择优时胜出的**边 id**：用于「边 id 小者优先」的确定性兜底比较。
        let mut best_edge_id_of: HashMap<i64, i64> = HashMap::new();
        kind_of.insert(center_id.get(), center_node.kind.to_string());
        name_of.insert(center_id.get(), center_node.name.clone());
        semantic_of.insert(center_id.get(), node_is_semantic(&center_node));
        ring_of.insert(center_id.get(), 0);

        // 按层批量取边（分块 `IN` 查询），写入边缓存。
        let fetch_edges = |ids: &[i64],
                           out: &mut HashMap<i64, Vec<em_domain::model::Edge>>,
                           inc: &mut HashMap<i64, Vec<em_domain::model::Edge>>|
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
                let other = if e.from_id.get() == center_id.get() {
                    e.to_id.get()
                } else {
                    e.from_id.get()
                };
                if other == center_id.get() {
                    continue;
                }
                path_kind.entry(other).or_insert(e.kind.to_string());
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
        if collapse && !reverse {
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
                // 折叠模式：语义节点是终点，不再向外穿透
                if collapse && id != center_id.get() && sem_of(id) {
                    continue;
                }
                // 折叠模式：把方法的"声明类"也纳入链路，让**类级语义边**（Dao→Model、Model→Table）浮现。
                // 安全：`Declares` 不在链边集合里，所以从类节点出发不会再展开出它的一堆方法。
                if collapse && k == "Method" {
                    if let Some(inc) = in_edges.get(&id) {
                        for e in inc {
                            if e.kind.as_str() != "Declares"
                                || ring_of.contains_key(&e.from_id.get())
                            {
                                continue;
                            }
                            // 沿用路径上的语义边种类，避免后续被 `HandledBy` 之类的边覆盖
                            let inherited =
                                path_kind.get(&id).cloned().unwrap_or_default();
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
                let dir_edges: Vec<&em_domain::model::Edge> = match (collapse, reverse) {
                    (false, _) => out_edges
                        .get(&id)
                        .into_iter()
                        .chain(in_edges.get(&id).into_iter())
                        .flatten()
                        .collect(),
                    (true, false) => out_edges.get(&id).into_iter().flatten().collect(),
                    (true, true) => in_edges.get(&id).into_iter().flatten().collect(),
                };
                for e in dir_edges {
                    if collapse && !is_chain_edge(e.kind.as_str()) {
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
        if !collapse {
            let miss_in: Vec<i64> = ring_of
                .keys()
                .copied()
                .filter(|id| !in_edges.contains_key(id))
                .collect();
            if !miss_in.is_empty() {
                let nids: Vec<NodeId> = miss_in.iter().map(|i| NodeId(*i)).collect();
                if let Ok(m) = self.store.edges_incoming(&nids) {
                    for (k, v) in m {
                        in_edges.entry(k).or_insert(v);
                    }
                }
            }
        }

        let is_visible = |id: i64| -> bool {
            if id == center_id.get() {
                return true;
            }
            if !collapse {
                return true;
            }
            semantic_of.get(&id).copied().unwrap_or(false) || force_visible.contains(&id)
        };

        // 语义边优先级（用于折叠路径上的边提拉）
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
        let mut shown_edges: Vec<EdgeView> = Vec::new();
        let mut shown_keys: HashSet<(String, i64, i64)> = HashSet::new();
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
                    kind: kind_of.get(&cur).cloned().unwrap_or_default(),
                    name: name_of.get(&cur).cloned().unwrap_or_default(),
                    call_site: None,
                });
                match parent_of.get(&cur) {
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
        let call_site = |from: i64, to: i64| -> Option<SourceLocation> {
            if let Some(outs) = out_edges.get(&from) {
                for e in outs {
                    if e.kind.as_str() == "Calls" && e.to_id.get() == to {
                        if let Some(cs_id) = e.properties.get("call_site").and_then(|v| v.as_i64()) {
                            if let Some(loc) = self.node_source_location(NodeId(cs_id)) {
                                return Some(loc);
                            }
                        }
                    }
                }
            }
            self.call_site_between(from, to, &out_edges)
        };
        // `indirect`：该语义边是否由 P8 沿调用链传播得来（起点自身并未执行该动作）。
        let push_edge = |from: i64, to: i64, kind: &str, src: i64, mut via: Vec<ViaNode>,
                             indirect: bool,
                             seen: &mut HashSet<(String, i64, i64)>,
                             out: &mut Vec<EdgeView>| {
            if from == to {
                return;
            }
            let (f, t) = match (ring_of.get(&from), ring_of.get(&to)) {
                (Some(a), Some(b)) if a > b => (to, from),
                _ => (from, to),
            };
            // 修正浮动/汇总边的标签：`MapsTo` 的语义是「Model → Table」的**结构映射**，
            // 不应充当"路由访问表"的标签。反向（资源视角）下 `src == 0` 的浮动边是把调用链
            // 回溯合成的"谁在访问这张表"，其真实语义是读/写库 —— 按 HTTP 动词启发区分读/写。
            let mut kind = kind.to_string();
            if kind == "MapsTo" && src == 0 {
                let table_at_to = kind_of.get(&t).map(|s| s.as_str()) == Some("Table");
                let table_at_from = kind_of.get(&f).map(|s| s.as_str()) == Some("Table");
                if table_at_to || table_at_from {
                    let route_end = if table_at_to { f } else { t };
                    let route_name = name_of.get(&route_end).cloned().unwrap_or_default();
                    kind = if is_write_http_method(&route_name) {
                        "WritesDb".to_string()
                    } else {
                        "ReadsDb".to_string()
                    };
                }
            }
            let key = (kind.clone(), f, t);
            if !seen.insert(key.clone()) {
                return;
            }
            // 若为了"由内向外"把边调转了方向，中间链也要跟着反过来
            if f != from {
                via.reverse();
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
                // 间接边（传播得来）只说明"上游某处发生过"，属于待验证假设 → 画虚线。
                resolved: agg_rank(kind.as_str()) > 0 && !indirect,
                confidence: if indirect { 0.5 } else { 0.8 },
                hops,
                via,
                to_call_site,
                indirect,
                // 由"内联链路位置"步骤统一填充（见 object_view 末尾）。
                node_locations: Vec::new(),
            });
        };

        if !collapse {
            // ---- 原逻辑：逐环保留边 ----（边直接来自缓存，不再逐节点查库）
            let all_nodes: Vec<i64> = ring_of.keys().copied().collect();
            for node in &all_nodes {
                let outs = out_edges.get(node).into_iter().flatten();
                let ins = in_edges.get(node).into_iter().flatten();
                for e in outs.chain(ins).cloned() {
                    let r_from = ring_of.get(&e.from_id.get()).copied();
                    let r_to = ring_of.get(&e.to_id.get()).copied();
                    let involves_center = e.from_id == center_id || e.to_id == center_id;
                    let on_chain = match (r_from, r_to) {
                        (Some(a), Some(b)) => a.abs_diff(b) == 1,
                        _ => false,
                    };
                    if !(involves_center || on_chain) {
                        hidden_total += 1;
                        *hidden_by_kind.entry(e.kind.to_string()).or_insert(0) += 1;
                        continue;
                    }
                    let key = (e.kind.to_string(), e.from_id.get(), e.to_id.get());
                    if !shown_keys.insert(key) {
                        continue;
                    }
                    shown_edges.push(self.to_edge_view(e));
                }
            }
        } else {
            // ---- 折叠：只保留"语义节点之间的语义边"；语法节点作为透传 ----
            // lift[x] = 沿"从中心出发的发现树"回退到最早的语义祖先，从而把挂在
            // Method / CallSite 上的语义边（ReadsConfig → ConfigKey、MapsTo → Table…）
            // 提拉到"发起它的语义节点"（如路由）上。
            let mut nodes_by_dist: Vec<(u32, i64)> =
                ring_of.iter().map(|(&id, &r)| (r, id)).collect();
            nodes_by_dist.sort_by(|a, b| a.0.cmp(&b.0));
            let mut lift: HashMap<i64, i64> = HashMap::new();
            for (_d, id) in &nodes_by_dist {
                if is_visible(*id) {
                    lift.insert(*id, *id);
                    continue;
                }
                let l = parent_of
                    .get(id)
                    .and_then(|p| lift.get(p))
                    .copied()
                    .unwrap_or(*id);
                lift.insert(*id, l);
            }

            if reverse {
                // 反向：把"谁在用这个资源"直接画成 `使用者 --语义边--> 资源`。
                // 语义边种类取回溯途中记录的访问方式标签（优先 ReadsDb/WritesDb，其次 ReadsConfig/ReadsCache，MapsTo 仅作结构兜底）。
                //
                // 共享资源（如 Cache）可能有上千个使用者：按环序取前 N 个画出来，
                // 其余计入 `hidden` —— 保证图可读，同时诚实记账。
                const MAX_USERS: usize = 80;
                let mut users: Vec<i64> = ring_of
                    .keys()
                    .copied()
                    .filter(|id| {
                        *id != center_id.get() && semantic_of.get(id).copied().unwrap_or(false)
                    })
                    .collect();
                // 事件 / 队列 / 缓存等中介节点的直接生产/消费方已在发现阶段前种入
                // 第 1 环并 `force_visible`，这里把它们也作为「用户」画出（其 `path_kind`
                // 即 `Triggers` / `HandledBy` / `PublishesTo`）。
                if matches!(center_node.kind.as_str(), "Event" | "Queue" | "Topic") {
                    for id in ring_of.keys().copied() {
                        if force_visible.contains(&id) && !users.contains(&id) {
                            users.push(id);
                        }
                    }
                }
                users.sort_by_key(|id| ring_of.get(id).copied().unwrap_or(0));
                for id in users.iter().take(MAX_USERS) {
                    let kind = path_kind
                        .get(id)
                        .cloned()
                        .filter(|k| !k.is_empty())
                        .unwrap_or_else(|| "Reads".to_string());
                    let mut via = chain_to(*id, center_id.get());
                    // reverse 模式下 *id 本身是语义节点，最终会成为提拉边的端点
                    // （端点已由 EdgeView.from/.to 渲染）；但 chain_to 会把 *id 一并收进
                    // via 的末节点，导致抽屉里的折叠链出现「端点既在 via 又在首尾」的闭环
                    // （如 GET /products 同时出现在首尾、store_category 反而被埋进中间）。
                    // 去掉末节点即可让端点只由 from/to 表达。
                    if via.last().map(|v| v.id.get()) == Some(*id) {
                        via.pop();
                    }
                    push_edge(
                        *id,
                        center_id.get(),
                        &kind,
                        0,
                        via,
                        path_indirect.get(id).copied().unwrap_or(false),
                        &mut shown_keys,
                        &mut shown_edges,
                    );
                }
                for id in users.iter().skip(MAX_USERS) {
                    hidden_total += 1;
                    if let Some(k) = kind_of.get(id) {
                        *hidden_by_kind.entry(k.clone()).or_insert(0) += 1;
                    }
                }
            } else {
                // 环外语义目标先暂存，等 `push_edge`/`chain_to`（捕获了这些 map）用完后
                // 再写回 `ring_of`/`parent_of`，避免与它们的不可变借用冲突。
                let mut pending: Vec<(i64, i64, u32)> = Vec::new();
                // 按「(kind, 提拉祖先 a, 目标 to)」分组，每组取**边 id 最小**的原始边作为代表。
                // 否则同一视图边可能由不同原始边「先到先得」地决定 `indirect` / `via` / 证据，
                // 而原始边的选取依赖 HashMap / 边查询的遍历顺序 → 每次运行结果不同。
                let mut groups: HashMap<(String, i64, i64), Vec<em_domain::model::Edge>> =
                    HashMap::new();
                let all_nodes: Vec<i64> = ring_of.keys().copied().collect();
                for node in &all_nodes {
                    let outs = out_edges.get(node).into_iter().flatten().cloned();
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
                        if !ring_of.contains_key(&to) {
                            let from_ring =
                                ring_of.get(&e.from_id.get()).copied().unwrap_or(0);
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
                for ((kind, a, to), mut es) in groups {
                    // 边 id 全图唯一，取最小者即确定；该代表边决定 `via` 与证据查询 id。
                    es.sort_by_key(|e| e.id.get());
                    let best = &es[0];
                    // `indirect` 同时考虑两点：① 原始边本身就是传播边（P8 沿调用链复刻）；
                    // ② 该边是从子孙节点**提拉**到祖先 `a` 的——祖先并不直接执行该动作
                    // （如 route 经调用链读到 config，route 本身不读）。两者任一成立即为间接。
                    let via = chain_to(best.from_id.get(), a);
                    let indirect = a != best.from_id.get() || is_indirect_edge(best);
                    push_edge(
                        a,
                        to,
                        &kind,
                        best.id.get(),
                        via,
                        indirect,
                        &mut shown_keys,
                        &mut shown_edges,
                    );
                }
                for (to, from, r) in pending {
                    ring_of.entry(to).or_insert(r);
                    parent_of.entry(to).or_insert(from);
                }
            }

            // 提拉阶段可能把环外的语义目标登记进了 `ring_of`，这里补齐它们的
            // kind / name / 语义性（直接读预加载的节点概要，不查库）。必须在 `chain_to` 等
            // 捕获了这些 map 的闭包使用完毕后再做，避免借用冲突。
            for &id in ring_of.keys() {
                if !kind_of.contains_key(&id) {
                    if let Some(m) = summary.get(&id) {
                        kind_of.insert(id, m.kind.clone());
                        name_of.insert(id, m.name.clone());
                        semantic_of.insert(id, NodeKind(m.kind.clone()).is_semantic());
                    }
                }
            }

            // 被折叠的语法节点按 kind 记账
            for (id, k) in &kind_of {
                if !semantic_of.get(id).copied().unwrap_or(false) {
                    *hidden_by_kind.entry(k.clone()).or_insert(0) += 1;
                    hidden_total += 1;
                }
            }
        }

        // ---- 内联链路节点的位置 ----
        // 折叠视图的"链路"是**临时提拉**的结果，中间跳只存在于当次响应里，
        // 按边 id 重查拿不到 —— 以往前端只能对每个节点单独请求
        // `/nodes/{id}/locations`（N+1 次），且那些位置与"这条边"不同源。
        // 这里一次性查好、按链路顺序（起点 → 各跳 → 终点）内联进 EdgeView。
        // 非折叠视图边量大且无提拉，保持原行为（前端回退到原接口）。
        if collapse {
            let mut need: Vec<i64> = Vec::new();
            for e in &shown_edges {
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
            for id in need {
                if let Ok(nl) = self.node_locations(NodeId(id)) {
                    synth.insert(id, nl.synthetic);
                    locs.insert(id, nl.locations);
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
                    let mut best: Option<(i64, SourceLocation)> = None;
                    // 优先命中 BFS 阶段已批量预取的入边缓存；缓存未覆盖（极少数漏预取的节点）
                    // 时再回退到单点查询。避免对每条 shown 边各发一次 DB 往返（N+1）。
                    let cached_ins = in_edges.get(&e.to.get()).cloned();
                    let store_ins = if cached_ins.is_some() {
                        None
                    } else {
                        self.store.edges_of(e.to, EdgeDirection::Incoming).ok()
                    };
                    if let Some(ins) = cached_ins.as_ref().or(store_ins.as_ref()) {
                        for raw in ins {
                            if raw.kind.as_str() != e.kind {
                                continue;
                            }
                            if !ring_of.contains_key(&raw.from_id.get()) {
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
                            match &best {
                                Some((f, _)) if *f <= raw.from_id.get() => {}
                                _ => best = Some((raw.from_id.get(), cand)),
                            }
                        }
                    }
                    if let Some((_, loc)) = best {
                        e.to_call_site = Some(loc);
                    }
                }
            }
        }

        // 可见节点：环内"语义节点 / 强制可见节点"。提拉阶段可能把环外的语义目标登记进环，
        // 所以放在提拉之后计算。
        let visible_nodes: Vec<i64> = ring_of
            .keys()
            .copied()
            .filter(|&id| {
                semantic_of.get(&id).copied().unwrap_or(false) || force_visible.contains(&id)
            })
            .collect();

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

        // ---- 只保留与中心真正连通的可见节点（丢弃孤立叶子，守住"单链路"诚实性）----
        let mut touched: HashSet<i64> = HashSet::new();
        touched.insert(center_id.get());
        for e in &shown_edges {
            touched.insert(e.from.get());
            touched.insert(e.to.get());
        }

        // ---- 组装可见节点的环与节点视图 ----
        let max_ring = ring_of.values().copied().max().unwrap_or(0);
        let mut visible_rings: Vec<Vec<NodeId>> = vec![vec![]; max_ring as usize + 1];
        for &id in &visible_nodes {
            if !touched.contains(&id) {
                continue;
            }
            if let Some(r) = ring_of.get(&id) {
                if let Some(slot) = visible_rings.get_mut(*r as usize) {
                    slot.push(NodeId(id));
                }
            }
        }
        for slot in visible_rings.iter_mut() {
            slot.sort_unstable();
        }

        let center_view = self
            .build_node_view(center_id, 0, &in_edges, &out_edges)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {center_id}")))?;
        let ring_views: Vec<Vec<NodeView>> = visible_rings
            .iter()
            .enumerate()
            .skip(1)
            .map(|(r, slot)| {
                slot.iter()
                    .filter_map(|id| {
                        self.build_node_view(*id, r as u32, &in_edges, &out_edges)
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
            note: if collapse {
                format!(
                    "当前视图已折叠语法节点（Method / CallSite 等），只保留语义节点与它们之间的依赖边：\
                     已画 {} 条边，另有 {} 个语法节点被折叠。切换上方「显示语法节点」可展开完整链路。",
                    shown_edges.len(),
                    hidden_total
                )
            } else {
                format!(
                    "当前视图只包含「{}」这一条链路：已画 {} 条边，另有 {} 条属于其它对象的链路边被刻意省略。\
                     切换上方的二级对象即可查看它们。",
                    center_node.name,
                    shown_edges.len(),
                    hidden_total
                )
            },
        };

        let unresolved = self.unresolved_for(project_id, &center_node.name);
        let conclusions = self.conclusions_for(project_id, spec, &center_node.name, &center_view);

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
            // 复用本视图已经加载的整图概要 / 链边邻接，只额外取一次候选节点，
            // 不再重跑 `nodes_summary` + `chain_adjacency` + 全量打分（这是此前 ~3.5s 主因）。
            candidates: {
                let cand_nodes = self.store.query_nodes(&NodeFilter {
                    project_id,
                    kind: spec.node_kind.clone().map(NodeKind),
                    name_contains: None,
                    limit: Some(5000),
                    offset: Some(0),
                })?;
                let r = self.rank_candidates(cand_nodes, &summary, &out, &inc, 300);
                r
            },
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
                Some(em_domain::model::NodeKind(target_kind.clone()))
            },
            name_contains: None,
            limit: Some(2000),
            offset: Some(0),
        })?;

        if spec.layout == em_domain::model::LayoutMode::Matrix {
            return Ok(self.matrix_view(project_id, spec, nodes, sample_limit));
        }

        // Compound / ER：按 group_by 分组
        let mut groups: BTreeMap<String, Vec<em_domain::model::Node>> = BTreeMap::new();
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
                        self.build_node_view(
                            n.id,
                            1,
                            &HashMap::new(),
                            &HashMap::new(),
                        )
                        .ok()
                        .flatten()
                    })
                    .collect();
                Cluster { label: key.clone(), key, count, members: samples }
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
        nodes: Vec<em_domain::model::Node>,
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
            matrix: Some(MatrixView { rows, cols, cells, row_totals, col_totals }),
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

    fn group_key(&self, spec: &PerspectiveSpec, node: &em_domain::model::Node) -> String {
        self.group_key_with(spec.group_by.as_ref(), node, &HashMap::new())
    }

    fn group_key_with(
        &self,
        by: Option<&GroupBy>,
        node: &em_domain::model::Node,
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
        let synthetic = node.identity.is_some();
        let mut locations: Vec<SourceLocation> = Vec::new();

        // ① 语法节点：文件 + 行号 + 符号
        if let Some(file_id) = node.file_id {
            if let Some(path) = self.file_path(file_id) {
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
        if let Some(name) = node.identity.as_ref().map(|i| i.value.clone()).or_else(|| node.fqn.clone())
        {
            for table in ["schema", "route_list", "event_listeners", "container_bindings"] {
                if let Some(entry) = self.store.get_symbol(node.project_id, table, &name)? {
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

        if !view.resolved {
            reason = Some(format!(
                "该边为推断边（{}），尚未被权威源交叉验证；请按上面的位置逐跳确认后再采信。",
                edge.kind
            ));
        }

        Ok(Some(EdgeEvidence { edge: view, reason, locations, via }))
    }

    // ---------------------------------------------------------------- 内部

    fn to_edge_view(&self, e: em_domain::model::Edge) -> EdgeView {
        // 实边判定：来自权威解析（P7 解析结果）或语法确定的继承/实现/声明
        let authoritative = matches!(
            e.kind.as_str(),
            "Extends" | "Implements" | "UsesTrait" | "Declares" | "Contains" | "HasCallSite"
        );
        let has_tier = e.properties.get("tier").is_some();
        // 传播边（P8 沿调用链复刻）只说明"上游某处发生过"，不是本节点的直接动作 → 虚线。
        let indirect = is_indirect_edge(&e);
        let resolved = (authoritative || has_tier) && !indirect;
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
    fn node_source_location(&self, id: NodeId) -> Option<SourceLocation> {
        let node = self.store.get_node(id).ok()??;
        let file_id = node.file_id?;
        let path = self.file_path(file_id)?;
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

    fn node_kind(&self, id: i64) -> String {
        self.store
            .get_node(NodeId(id))
            .ok()
            .flatten()
            .map(|n| n.kind.to_string())
            .unwrap_or_default()
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
        out_cache: &HashMap<i64, Vec<em_domain::model::Edge>>,
    ) -> Option<SourceLocation> {
        // 复用 BFS 阶段批量预取的出边缓存，避免对每条折叠边各发一次 DB 往返（N+1）。
        // 缓存未覆盖（极少数漏预取的节点）时回退到单点查询。
        let outs = out_cache
            .get(&from)
            .cloned()
            .or_else(|| self.store.edges_of(NodeId(from), EdgeDirection::Outgoing).ok());
        // 1) 精确：直接读 `from → to` 的 Calls 边上记录的 CallSite 节点 id
        if let Some(outs) = outs.as_ref() {
            for e in outs {
                if e.kind.as_str() != "Calls" || e.to_id.get() != to {
                    continue;
                }
                if let Some(cs_id) = e
                    .properties
                    .get("call_site")
                    .and_then(|v| v.as_i64())
                {
                    if let Some(loc) = self.node_source_location(NodeId(cs_id)) {
                        return Some(loc);
                    }
                }
            }
        }
        // 2) 回退：from 的 HasCallSite 调用点里，被调名与 to 匹配的那一个
        let to_name = self.short_name_of(to);
        if to_name.is_empty() {
            return None;
        }
        for e in outs.as_ref()? {
            if e.kind.as_str() != "HasCallSite" {
                continue;
            }
            let cs = e.to_id.get();
            if self.node_kind(cs) != "CallSite" {
                continue;
            }
            if let Some(cs_node) = self.store.get_node(NodeId(cs)).ok().flatten() {
                if let Some(fqn) = &cs_node.fqn {
                    // fqn 形如 "Owner::callee:line" / "Owner->callee:line" / "new Klass"
                    let callee = fqn.split('#').nth(1).unwrap_or("");
                    let callee_method = callee.split([':', '-', '>']).next().unwrap_or("").trim();
                    let callee_method = callee_method.strip_prefix("new ").unwrap_or(callee_method);
                    if !callee_method.is_empty() && self.short_name_str(callee_method) == to_name {
                        return self.node_source_location(NodeId(cs));
                    }
                }
            }
        }
        None
    }

    /// 取节点名（方法名 / 类名）的"短名"：去掉命名空间与前缀，仅保留最后一段。
    fn short_name_of(&self, id: i64) -> String {
        if let Some(n) = self.store.get_node(NodeId(id)).ok().flatten() {
            let raw = n.fqn.as_deref().filter(|s| !s.is_empty()).unwrap_or(&n.name);
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
        in_cache: &HashMap<i64, Vec<em_domain::model::Edge>>,
        out_cache: &HashMap<i64, Vec<em_domain::model::Edge>>,
    ) -> Result<Option<NodeView>> {
        let Some(n) = self.store.get_node(id)? else {
            return Ok(None);
        };
        let registry = self.views.registry();
        let own_view = registry.view_for_kind(n.kind.as_str()).map(|s| s.id.clone());
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
            .or_else(|| self.store.edges_of(id, EdgeDirection::Incoming).ok().map(|e| e.len()))
            .unwrap_or(0);
        let fan_out = out_cache
            .get(&id.get())
            .map(|e| e.len())
            .or_else(|| self.store.edges_of(id, EdgeDirection::Outgoing).ok().map(|e| e.len()))
            .unwrap_or(0);
        let mut locations = Vec::new();
        if let Some(Value::Array(arr)) = n.properties.get("locations") {
            for item in arr.iter().take(8) {
                if let Ok(l) = serde_json::from_value::<SourceLocation>(item.clone()) {
                    locations.push(l);
                }
            }
        }
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
    ) -> Value {
        let mut out = serde_json::Map::new();
        out.insert("视角".into(), json!(spec.label));
        out.insert("对象".into(), json!(name));
        out.insert("入边".into(), json!(center.metrics.get("fan_in").cloned().unwrap_or(json!(0))));
        out.insert("出边".into(), json!(center.metrics.get("fan_out").cloned().unwrap_or(json!(0))));
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
            out.insert("路由表登记".into(), entry.get("handler").cloned().unwrap_or(json!(null)));
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
                    || d.location.as_deref().map(|l| l.contains(name)).unwrap_or(false)
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

    fn find_edge(&self, edge_id: i64) -> Result<Option<em_domain::model::Edge>> {
        // `edges_of` 只能按节点查；这里借助 nodes 表反查代价过高，
        // 因此直接从图统计遍历是不现实的 —— 用 SQL 侧提供的查询端口替代。
        let _ = edge_id;
        Ok(self.store.find_edge(em_domain::model::EdgeId(edge_id))?)
    }

    fn file_path(&self, file_id: em_domain::model::FileId) -> Option<String> {
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
/// 权威定义在 `em_domain::model::kinds::is_chain_edge`。
fn is_chain_edge(kind: &str) -> bool {
    em_domain::model::kinds::is_chain_edge(kind)
}

/// 语义节点判定：第一类语义 kind（`kinds.rs` 的 `SYNTHESIZED`：`Table` / `HttpContract` /
/// `ConfigKey` / `I18nKey` / `Event` / `Queue` / `Cache` / `Topic`…）。
/// `Event` / `Queue` / `Cache` / `Topic` 现在是具体种类，不再依赖 `category` 伞。
/// 分类权威来自 `kinds.rs`。
fn node_is_semantic(n: &em_domain::model::Node) -> bool {
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
fn is_indirect_edge(e: &em_domain::model::Edge) -> bool {
    e.properties
        .get("indirect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || e.properties.get("via").and_then(|v| v.as_str()) == Some("propagate")
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
    let verb = route_name.split_whitespace().next().unwrap_or("").to_uppercase();
    matches!(verb.as_str(), "POST" | "PUT" | "DELETE" | "PATCH")
}
