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
    AggregateView, Candidate, Cluster, EdgeEvidence, EdgeKind, EdgeView, GroupBy, HiddenInfo,
    MatrixView, NodeId, NodeKind, NodeLocations, NodeView, ObjectView, PerspectiveSpec, ProjectId,
    SourceLocation, UnresolvedInfo, ViewRegistry, ViaNode,
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
        // 无搜索词时取全量候选参与"价值排序"（要选出全局最高价值的对象），
        // 有搜索词时只按名称过滤、不做昂贵打分。
        let scoring = name_contains.is_none();
        let nodes = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(em_domain::model::NodeKind(kind.clone())),
            name_contains: name_contains.map(|s| s.to_string()),
            limit: Some(if scoring { 5000 } else { limit }),
            offset: Some(0),
        })?;
        // 候选按"语义依赖价值"降序：值越高越值得作为默认打开的对象，
        // 前端据此默认打开第一个（价值最高）的对象。
        let mut scored: Vec<(usize, Candidate)> = nodes
            .into_iter()
            .map(|n| {
                let fan = self
                    .store
                    .edges_of(n.id, EdgeDirection::Incoming)
                    .map(|e| e.len())
                    .unwrap_or(0);
                let value = self.semantic_value(n.id);
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
        scored.truncate(limit as usize);
        Ok(scored.into_iter().map(|(_, c)| c).collect())
    }

    /// 候选对象的"价值"：其调用链可达的语义节点数（表 / 配置 / 缓存 / 事件…）。
    /// 值越高，承载的业务依赖越丰富，越适合作为默认打开的对象。
    fn semantic_value(&self, center: NodeId) -> usize {
        const MAX_DEPTH: u32 = 3;
        const MAX_NODES: usize = 400;
        let mut seen: HashSet<i64> = HashSet::new();
        seen.insert(center.get());
        let mut semantic: HashSet<i64> = HashSet::new();
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
        queue.push_back((center, 0));
        // 与视图一致：把"中心的入向链邻居"也作为第 1 环（表 / 外部系统视角需要）
        if let Ok(incoming) = self.store.edges_of(center, EdgeDirection::Incoming) {
            for e in incoming {
                if is_chain_edge(e.kind.as_str()) && seen.insert(e.from_id.get()) {
                    queue.push_back((e.from_id, 1));
                }
            }
        }
        while let Some((id, r)) = queue.pop_front() {
            if r >= MAX_DEPTH || seen.len() >= MAX_NODES {
                continue;
            }
            let Ok(edges) = self.store.edges_of(id, EdgeDirection::Outgoing) else {
                continue;
            };
            for e in edges {
                if !is_chain_edge(e.kind.as_str()) {
                    continue;
                }
                let to = e.to_id;
                let is_sem = self
                    .store
                    .get_node(to)
                    .ok()
                    .flatten()
                    .map(|n| node_is_semantic(&n))
                    .unwrap_or(false);
                if is_sem {
                    semantic.insert(to.get());
                }
                if seen.insert(to.get()) {
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
        let chain_depth = if collapse { 12 } else { depth };
        let mut ring_of: HashMap<i64, u32> = HashMap::new();
        let mut parent_of: HashMap<i64, i64> = HashMap::new();
        let mut kind_of: HashMap<i64, String> = HashMap::new();
        let mut name_of: HashMap<i64, String> = HashMap::new();
        let mut semantic_of: HashMap<i64, bool> = HashMap::new();
        // 反向模式：从中心走到该节点途中遇到的**语义边种类**（即"资源被怎样访问"）。
        let mut path_kind: HashMap<i64, String> = HashMap::new();
        kind_of.insert(center_id.get(), center_node.kind.to_string());
        name_of.insert(center_id.get(), center_node.name.clone());
        semantic_of.insert(center_id.get(), node_is_semantic(&center_node));
        ring_of.insert(center_id.get(), 0);
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
        queue.push_back((center_id, 0));
        let library_check = !library_subs.is_empty();
        let mut sub_cache: HashMap<i64, i64> = HashMap::new();

        // 正向模式：把"中心的入向链邻居"也作为第 1 环（表 / 外部系统视角需要）。
        // 反向模式本身就是走入边，无需再播种。
        if collapse && !reverse {
            for e in self.store.edges_of(center_id, EdgeDirection::Incoming)? {
                if !is_chain_edge(e.kind.as_str()) {
                    continue;
                }
                let other = e.from_id;
                if ring_of.contains_key(&other.get()) {
                    continue;
                }
                ring_of.insert(other.get(), 1);
                parent_of.insert(other.get(), center_id.get());
                queue.push_back((other, 1));
            }
        }

        while let Some((id, r)) = queue.pop_front() {
            // 记录并缓存 kind / 语义性（语义终点判定要用）
            if !kind_of.contains_key(&id.get()) {
                if let Some(n) = self.store.get_node(id)? {
                    kind_of.insert(id.get(), n.kind.to_string());
                    name_of.insert(id.get(), n.name.clone());
                    semantic_of.insert(id.get(), node_is_semantic(&n));
                }
            }
            let k = kind_of.get(&id.get()).cloned().unwrap_or_default();
            // 折叠模式：语义节点是终点，不再向外穿透
            if collapse
                && id != center_id
                && semantic_of.get(&id.get()).copied().unwrap_or(false)
            {
                continue;
            }
            // 折叠模式：把方法的"声明类"也纳入链路，让**类级语义边**（Dao→Model、Model→Table）浮现。
            // 安全：`Declares` 不在链边集合里，所以从类节点出发不会再展开出它的一堆方法。
            if collapse && k == "Method" {
                if let Ok(inc) = self.store.edges_of(id, EdgeDirection::Incoming) {
                    for e in inc {
                        if e.kind.as_str() != "Declares" || ring_of.contains_key(&e.from_id.get()) {
                            continue;
                        }
                        // 沿用路径上的语义边种类，避免后续被 `HandledBy` 之类的边覆盖
                        let inherited = path_kind.get(&id.get()).cloned().unwrap_or_default();
                        path_kind.entry(e.from_id.get()).or_insert(inherited);
                        ring_of.insert(e.from_id.get(), r + 1);
                        parent_of.insert(e.from_id.get(), id.get());
                        queue.push_back((e.from_id, r + 1));
                    }
                }
            }
            if r >= chain_depth {
                continue;
            }
            if library_check {
                let sub = *sub_cache.entry(id.get()).or_insert_with(|| {
                    self.store
                        .get_node(id)
                        .ok()
                        .flatten()
                        .and_then(|n| n.sub_project_id)
                        .map(|s| s.get())
                        .unwrap_or(0)
                });
                if library_subs.contains(&sub) {
                    continue;
                }
            }
            let dir = if !collapse {
                EdgeDirection::Both
            } else if reverse {
                EdgeDirection::Incoming
            } else {
                EdgeDirection::Outgoing
            };
            for e in self.store.edges_of(id, dir)? {
                if collapse && !is_chain_edge(e.kind.as_str()) {
                    continue;
                }
                let other = if e.from_id == id { e.to_id } else { e.from_id };
                if ring_of.contains_key(&other.get()) {
                    continue;
                }
                // 记录"资源被怎样访问"：用访问方式标签的**优先级**而非"先到先得"，
                // 否则 `Model→Table` 的 `MapsTo`（结构映射）会盖过真正的读/写访问。
                //
                // 优先级：ReadsDb/WritesDb(3) > ReadsConfig/ReadsCache/PublishesTo/Triggers(2)
                //         > MapsTo(1) > 其它(0)。
                // 关键：`MapsTo` 的语义是「Model → Table」的**结构映射**（如 `StoreCategory` 模型
                // 对应 `store_category` 表），它**不是**"路由如何访问这张表"的标签；真正的读/写
                // 访问应以 `ReadsDb`/`WritesDb`（由 DAO/方法到表）为准，必须压过 `MapsTo`。
                let access_rank = |k: &str| -> u8 {
                    match k {
                        "ReadsDb" | "WritesDb" => 3,
                        "ReadsConfig" | "ReadsCache" | "PublishesTo" | "Triggers" => 2,
                        "MapsTo" => 1,
                        _ => 0,
                    }
                };
                let inherited_rank = path_kind.get(&id.get()).map(|k| access_rank(k)).unwrap_or(0);
                let new_rank = access_rank(e.kind.as_str());
                let pk = if new_rank > inherited_rank {
                    e.kind.to_string()
                } else if inherited_rank > 0 {
                    path_kind.get(&id.get()).cloned().unwrap()
                } else if is_semantic_edge(e.kind.as_str()) {
                    e.kind.to_string()
                } else {
                    String::new()
                };
                path_kind.entry(other.get()).or_insert(pk);
                ring_of.insert(other.get(), r + 1);
                parent_of.insert(other.get(), id.get());
                queue.push_back((other, r + 1));
            }
        }

        // 补齐所有可达节点的 kind / 语义性（边展示阶段要用）
        for &id in ring_of.keys() {
            if !kind_of.contains_key(&id) {
                if let Some(n) = self.store.get_node(NodeId(id))? {
                    kind_of.insert(id, n.kind.to_string());
                    semantic_of.insert(id, node_is_semantic(&n));
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
            semantic_of.get(&id).copied().unwrap_or(false)
        };

        let visible_nodes: Vec<i64> = ring_of.keys().copied().filter(|&id| is_visible(id)).collect();

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
        let push_edge = |from: i64, to: i64, kind: &str, src: i64, mut via: Vec<ViaNode>,
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
                v.call_site = self.call_site_between(prev, v.id.get());
                prev = v.id.get();
            }
            let to_call_site = self.call_site_between(prev, t);
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
                resolved: agg_rank(kind.as_str()) > 0,
                confidence: 0.8,
                hops,
                via,
                to_call_site,
            });
        };

        if !collapse {
            // ---- 原逻辑：逐环保留边 ----
            let all_nodes: Vec<NodeId> = ring_of.keys().map(|k| NodeId(*k)).collect();
            for node in &all_nodes {
                for e in self.store.edges_of(*node, EdgeDirection::Both)? {
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
                    push_edge(*id, center_id.get(), &kind, 0, via, &mut shown_keys, &mut shown_edges);
                }
                for id in users.iter().skip(MAX_USERS) {
                    hidden_total += 1;
                    if let Some(k) = kind_of.get(id) {
                        *hidden_by_kind.entry(k.clone()).or_insert(0) += 1;
                    }
                }
            } else {
                let all_nodes: Vec<NodeId> = ring_of.keys().map(|k| NodeId(*k)).collect();
                for node in &all_nodes {
                    for e in self.store.edges_of(*node, EdgeDirection::Outgoing)? {
                        // 只画语义边，且目标必须是语义节点（ReadsConfig→ConfigKey、MapsTo→Table…）
                        if !is_semantic_edge(e.kind.as_str()) {
                            continue;
                        }
                        let to = e.to_id.get();
                        if !ring_of.contains_key(&to) {
                            continue;
                        }
                        if !semantic_of.get(&to).copied().unwrap_or(false) {
                            continue;
                        }
                        let a = *lift.get(&e.from_id.get()).unwrap_or(&e.from_id.get());
                        if a == to {
                            continue;
                        }
                        // 被折叠掉的中间节点：从"提拉到的语义祖先"一路到"真正持有这条语义边的节点"
                        let via = chain_to(e.from_id.get(), a);
                        // 带上真实来源边 id：点击这条提拉边时能查到原始语义边的证据
                        push_edge(a, to, e.kind.as_str(), e.id.get(), via, &mut shown_keys, &mut shown_edges);
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
        let display_depth = if collapse { chain_depth } else { depth };
        let mut visible_rings: Vec<Vec<NodeId>> = vec![vec![]; display_depth as usize + 1];
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
            .build_node_view(center_id, 0)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {center_id}")))?;
        let ring_views: Vec<Vec<NodeView>> = visible_rings
            .iter()
            .enumerate()
            .skip(1)
            .map(|(r, slot)| {
                slot.iter()
                    .filter_map(|id| self.build_node_view(*id, r as u32).ok().flatten())
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
            candidates: self.candidates(project_id, perspective, 300, None)?,
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
                    .filter_map(|n| self.build_node_view(n.id, 1).ok().flatten())
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
        let resolved = authoritative || has_tier;
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
    fn call_site_between(&self, from: i64, to: i64) -> Option<SourceLocation> {
        // 1) 精确：直接读 `from → to` 的 Calls 边上记录的 CallSite 节点 id
        if let Some(outs) = self.store.edges_of(NodeId(from), EdgeDirection::Outgoing).ok() {
            for e in &outs {
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
        for e in self.store.edges_of(NodeId(from), EdgeDirection::Outgoing).ok()? {
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

    fn build_node_view(&self, id: NodeId, ring: u32) -> Result<Option<NodeView>> {
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
        let fan_in = self
            .store
            .edges_of(id, EdgeDirection::Incoming)
            .map(|e| e.len())
            .unwrap_or(0);
        let fan_out = self
            .store
            .edges_of(id, EdgeDirection::Outgoing)
            .map(|e| e.len())
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
fn is_chain_edge(kind: &str) -> bool {
    matches!(
        kind,
        "HandledBy"
            | "Calls"
            | "HasCallSite"
            | "ReadsConfig"
            | "ReadsCache"
            | "ReadsDb"
            | "WritesDb"
            | "MapsTo"
            | "Triggers"
            | "PublishesTo"
            | "CallsHttp"
            | "ResolvesTo"
    )
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
