//! View use cases: slice the graph **by perspective** into the link subgraph of "this one object".
//!
//! Core honesty constraints:
//! * An object-kind perspective **only draws the center object's links**; links of other objects aren't drawn at all (not dimmed)
//! * Omitted parts are surfaced as [`HiddenInfo`] counts + [`UnresolvedInfo`] accounting
//! * A synthesized node's location is **always a multi-location list**, never a fabricated single location

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gt_domain::error::{DomainError, Result};
use gt_domain::model::{
    graph::NodeSummary, AggregateView, Candidate, Cluster, EdgeEvidence, EdgeView, GroupBy,
    HiddenInfo, MatrixView, NodeId, NodeKind, NodeLocationEntry, NodeLocations, NodeView,
    ObjectView, OrphanAccess, PerspectiveSpec, ProjectId, SourceLocation, SubProjectId,
    UnresolvedInfo, ViaNode, ViewRegistry,
};
use gt_domain::port::{EdgeDirection, NodeFilter, Persistence, ViewRegistryProvider};
use serde_json::{json, Value};

/// Perspective service.
pub struct ViewService {
    store: Arc<dyn Persistence>,
    views: Arc<dyn ViewRegistryProvider>,
}

/// A folded view's **discovery result**: the layered-BFS product rooted at the center (rings / parent pointers / kind-name cache /
/// edge cache) plus the "access mode" labels recorded along discovery.
///
/// Why it's a separate struct: `object_view` used to hold a dozen cross-referencing HashMaps inside one function body,
/// dodging the borrow checker only via `{ }` scopes and a `pending` staging buffer — that's exactly why that near-thousand-line logic
/// couldn't be split or read. Extracted:
/// * `discover()` builds the state once and returns it;
/// * later stages immutably borrow `&Discovery`; closures like `chain_to` / `via_from_ids` /
///   `push_edge` no longer fight "fill in kind / name" writes (NLL handles it).
struct FoldResult {
    /// Edges drawn (folded, lifted, with via inlined).
    edges: Vec<EdgeView>,
    /// Count of entries folded / truncated by the cap and not drawn (honest accounting).
    hidden_total: usize,
    hidden_by_kind: BTreeMap<String, usize>,
    /// Direct accesses (orphans) with no semantic entry: off the canvas, listed separately.
    orphans: Vec<OrphanAccess>,
}

struct Discovery {
    /// Ring number (hop count).
    ring_of: HashMap<i64, u32>,
    /// Parent node on the discovery tree, for `chain_to` to walk back the folded intermediate hops.
    parent_of: HashMap<i64, i64>,
    kind_of: HashMap<i64, String>,
    name_of: HashMap<i64, String>,
    semantic_of: HashMap<i64, bool>,
    /// Edge cache for the reverse perspective (resource-kind center) walking back along in-edges.
    in_edges: HashMap<i64, Vec<gt_domain::model::Edge>>,
    /// Edge cache for the forward perspective (route) drilling down along out-edges.
    out_edges: HashMap<i64, Vec<gt_domain::model::Edge>>,
    /// The direct producer / consumer that "must be drawn" under broker perspectives (event / queue).
    force_visible: HashSet<i64>,
    /// Semantic-edge kinds encountered reaching a node ("how the resource is accessed").
    path_kind: HashMap<i64, String>,
    /// Parallel to `path_kind`: whether this edge came from P8 propagation.
    path_indirect: HashMap<i64, bool>,
    /// Parallel to `path_kind`: whether the original edge is **center → node** (center on the out-edge side).
/// The reverse mode draws "user --semantic edge--> center" by default, but `HandledBy`'s subject is the center itself,
/// so it must be drawn by the original edge direction as "center --handled by…--> listener".
    path_from_center: HashSet<i64>,
    /// Whether reversed (resource-kind center walking back along in-edges).
    reverse: bool,
}

impl ViewService {
    pub fn new(store: Arc<dyn Persistence>, views: Arc<dyn ViewRegistryProvider>) -> Self {
        Self { store, views }
    }

    pub fn registry(&self) -> &ViewRegistry {
        self.views.registry()
    }

    /// First-level filter: each perspective and its available candidate count.
    pub fn perspectives(&self, project_id: ProjectId) -> Result<Vec<Value>> {
        let stats = self.store.stats(project_id)?;
        let mut out = Vec::new();
        for spec in &self.views.registry().perspectives {
            let available = match (&spec.mode, &spec.node_kind) {
                (gt_domain::model::ViewMode::Object, Some(kind)) => {
                    // Perspectives with `side`: count precisely by `kind + side` (frontend/backend cache split).
                    if let Some(side) = &spec.side {
                        self.store
                            .count_nodes(
                                project_id,
                                Some(&gt_domain::model::NodeKind(kind.clone())),
                                Some(side.as_str()),
                            )
                            .unwrap_or(0)
                    } else {
                        // Count by kind first; fall back to category when 0 (for category-grouped perspectives).
                        let n = stats.by_kind.get(kind).copied().unwrap_or(0);
                        if n > 0 {
                            n
                        } else {
                            stats.by_category.get(kind).copied().unwrap_or(0)
                        }
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

    /// Second-level filter: candidate objects under a perspective.
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
            .ok_or_else(|| DomainError::NotFound(format!("perspective {perspective}")))?;
        let Some(kind) = &spec.node_kind else {
            return Ok(Vec::new());
        };
        let mut nodes = self.store.query_nodes(&NodeFilter {
            project_id,
            kind: Some(gt_domain::model::NodeKind(kind.clone())),
            name_contains: name_contains.map(|s| s.to_string()),
            // With no search term, fetch more to participate in "value ranking" (pick the globally highest-value object);
            // with a term, filter by name only, no expensive scoring.
            limit: Some(if name_contains.is_none() { 5000 } else { limit }),
            offset: Some(0),
        })?;

        // Converge candidates by sub-project: selecting one sub-project lists only its objects in the dropdown,
        // so when "sub-project as top dimension" switches to it, the default object also lands inside it.
        if let Some(sid) = sub_project_id {
            nodes.retain(|n| n.sub_project_id == Some(sid));
        }

        // 按「端」过滤：把种类相同但端不同的节点（如前后端缓存）拆到各自视角。
        // 不满足 `side` 的节点（缺属性或与视角不符）直接剔除；`side` 为 `None` 的视角不过滤。
        if let Some(side) = &spec.side {
            nodes.retain(|n| {
                n.properties
                    .get("side")
                    .and_then(|v| v.as_str())
                    .map(|s| s == side.as_str())
                    .unwrap_or(false)
            });
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

        let summary = self.store.nodes_summary(project_id)?;
        let (out, inc, sem_inc) = self.store.chain_adjacency(project_id)?;

        Ok(self.rank_candidates(nodes, &summary, &out, &inc, &sem_inc, limit as usize))
    }

    /// 候选对象打分核心：把 `query_nodes` 取回的节点按"语义价值"排序并截断。
    ///
    /// `summary` / `out` / `inc` / `sem_inc` 来自 `nodes_summary` + `chain_adjacency`，
    /// **由调用方负责加载一次后复用**——`object_view` 与 `candidates` 共享同一份，
    /// 避免每次请求重复加载整图概要 + 链边邻接（工程越大越贵）。
    fn rank_candidates(
        &self,
        nodes: Vec<gt_domain::model::Node>,
        summary: &HashMap<i64, NodeSummary>,
        out: &HashMap<i64, Vec<i64>>,
        inc: &HashMap<i64, Vec<i64>>,
        sem_inc: &HashMap<i64, Vec<i64>>,
        limit: usize,
    ) -> Vec<Candidate> {
        let mut scored: Vec<(usize, Candidate)> = nodes
            .into_iter()
            .map(|n| {
                let fan = sem_inc
                    .get(&n.id.get())
                    .map(|v| v.iter().copied().collect::<HashSet<i64>>().len())
                    .unwrap_or(0);
                let value = self.semantic_value(n.id, summary, out, inc);
                (
                    value,
                    Candidate {
                        id: n.id,
                        name: n.name.clone(),
                        badge: Some(format!("semantic dependencies {value} · semantic in-edges {fan}")),
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
            .ok_or_else(|| DomainError::NotFound(format!("perspective {perspective}")))?;
        let center_node = self
            .store
            .get_node(center_id)?
            .ok_or_else(|| DomainError::NotFound(format!("node {center_id}")))?;
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

        let reverse = !matches!(
            center_node.kind.as_str(),
            NodeKind::HTTP_CONTRACT | NodeKind::SCHEDULE
        );
        let chain_depth = depth;

        let summary = self.store.nodes_summary(project_id)?;
        // `inc` / `sem_inc` 只服务于候选打分（已移出对象视图，见 `ObjectView` 注释），这里不再需要。
        let (out, _inc, _sem_inc) = self.store.chain_adjacency(project_id)?;

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
        let fold = self.fold_and_collect(&mut d, center_id, &center_node, &summary, &out)?;
        let mut shown_edges = fold.edges;
        let hidden_total = fold.hidden_total;
        let hidden_by_kind = fold.hidden_by_kind;
        let orphans = fold.orphans;
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
            .ok_or_else(|| DomainError::NotFound(format!("node {center_id}")))?;
        let trigger_methods: HashSet<i64> = shown_edges
            .iter()
            .filter(|e| e.kind == "Triggers")
            .map(|e| e.from.get())
            .collect();
        let consumer_classes: HashSet<i64> = shown_edges
            .iter()
            .filter(|e| e.kind == "HandledBy" || e.kind == "PublishesTo")
            .flat_map(|e| [e.from.get(), e.to.get()])
            .filter(|id| d.kind_of.get(id).map_or(false, |k| k == "Class"))
            .collect();
        let mut qname_cache: HashMap<i64, String> = HashMap::new();
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
                            .map(|mut nv| {
                                let idv = nv.id.get();
                                if consumer_classes.contains(&idv) {
                                    // 同族着色 + `category` 同步（前端按 category 决定端 / 点击切视角）。
                                    nv.kind = NodeKind::EVENT_HANDLER.to_string();
                                    nv.category = Some(NodeKind::EVENT_HANDLER.to_string());
                                }
                                if trigger_methods.contains(&idv) {
                                    let qn = qname_cache
                                        .entry(idv)
                                        .or_insert_with(|| {
                                            self.method_qualified_name(idv, &d, &summary)
                                        });
                                    nv.name = qn.clone();
                                }
                                nv
                            })
                    })
                    .collect()
            })
            .collect();

        // 孤儿访问并入"被折叠的条目"记账：它们同样是从画布上省掉的访问方，
        // 只是额外带接触点位置、可在前端逐条核对（见 `ObjectView.orphans`）。
        let hidden = HiddenInfo {
            total: hidden_total + orphans.len() + shown_edges.len(),
            shown: shown_edges.len(),
            by_kind: hidden_by_kind,
            note: format!(
                "The current view has folded syntax nodes (Method / CallSite, etc.) and keeps only semantic nodes and the dependency edges between them:\
                 {} edge(s) drawn, {} syntax node(s) folded. Click any edge to see every hop and call site it passes through.",
                shown_edges.len(),
                hidden_total
            ),
        };

        let unresolved = self.unresolved_for(project_id, &center_node.name);
        let mut conclusions = self.conclusions_for(
            project_id,
            spec,
            &center_node.name,
            &center_view,
            shown_edges.len(),
        );
        if !orphans.is_empty() {
            if let Some(obj) = conclusions.as_object_mut() {
                obj.insert("other_direct_access".into(), json!(orphans.len()));
            }
        }

        Ok(ObjectView {
            project_id,
            perspective: spec.id.clone(),
            layout: spec.layout,
            center: center_view,
            rings: ring_views,
            edges: shown_edges,
            hidden,
            orphans,
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

        let force_visible: HashSet<i64> = HashSet::new();
        // 反向模式：从中心走到该节点途中遇到的**语义边种类**（即"资源被怎样访问"）。
        let mut path_kind: HashMap<i64, String> = HashMap::new();
        // 与 `path_kind` 平行：走到该节点途中遇到的语义边**是否为传播得来的间接边**。
        // 资源视角（"谁在读这个配置"）同样要能区分直接读者与"上游读过"的间接入口。
        let mut path_indirect: HashMap<i64, bool> = HashMap::new();
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
                let is_consumer =
                    e.kind.as_str() == "HandledBy" || e.kind.as_str() == "PublishesTo";
                let mut node = other;
                if is_consumer && summary.get(&other).map_or(false, |m| m.kind == "Method") {
                    if let Ok(m) = self.store.edges_incoming(&[NodeId(other)]) {
                        if let Some(cls) = m.get(&other).and_then(|v| {
                            v.iter()
                                .find(|e| e.kind.as_str() == "Declares")
                                .map(|e| e.from_id.get())
                        }) {
                            node = cls;
                            ring_of.entry(other).or_insert(2);
                            parent_of.entry(other).or_insert(node);
                        }
                    }
                }
                path_kind.entry(node).or_insert(e.kind.to_string());
                // 中心的**出边**（如 `事件 --HandledBy--> 监听器`）：画边时不能反过来。
                if center_side {
                    path_from_center.insert(node);
                }
                if !ring_of.contains_key(&node) {
                    ring_of.insert(node, 1);
                    parent_of.insert(node, center_id.get());
                    semantic_of.insert(node, is_consumer);
                    // 关键：同时补齐 kind / name。否则 BFS 收尾的「补齐」步骤会因
                    // kind_of 缺失而按 kind 重算 semantic_of，把这里的提升覆盖回 false。
                    if let Some(m) = summary.get(&node) {
                        kind_of.insert(node, m.kind.clone());
                        name_of.insert(node, m.name.clone());
                    }
                }
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
                    let already = ring_of.get(&other).copied();
                    if let Some(r) = already {
                        if r != ring + 1 {
                            // 已在更近的层确定路径（或异常更远），不再处理。
                            continue;
                        }
                        // r == ring + 1：本层重见，按比较器决定是否升级。
                    }
                    let indirect = is_indirect_edge(e);
                    let rank = access_rank(e.kind.as_str());
                    let inherited_rank = path_kind.get(&id).map(|k| access_rank(k)).unwrap_or(0);
                    let pk = if rank > inherited_rank {
                        e.kind.to_string()
                    } else if inherited_rank > 0 {
                        path_kind.get(&id).cloned().unwrap()
                    } else if is_semantic_edge(e.kind.as_str()) || is_bridge_edge(e.kind.as_str()) {
                        e.kind.to_string()
                    } else {
                        String::new()
                    };
                    let first_time = already.is_none();
                    let upgrade = if first_time {
                        true
                    } else {
                        let old_ind = path_indirect.get(&other).copied().unwrap_or(false);
                        let old_kind = path_kind.get(&other).cloned().unwrap_or_default();
                        let old_eid = best_edge_id_of.get(&other).copied().unwrap_or(0);
                        better_path(
                            indirect,
                            e.kind.as_str(),
                            e.id.get(),
                            old_ind,
                            &old_kind,
                            old_eid,
                        )
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
            let seeds = match raw
                .properties
                .get("seed_sources")
                .and_then(|v| v.as_array())
            {
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
                    let raw = m
                        .fqn
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&m.name);
                    (m.kind.clone(), self.short_name_str(raw))
                })
                .unwrap_or_default();
            e.via.push(ViaNode {
                id: NodeId(seed),
                kind,
                name,
                call_site: self.call_site_between(
                    contact,
                    seed,
                    &d.out_edges,
                    &NodeCache::default(),
                ),
            });
            e.hops = Some(e.via.len() as u32);
        }

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

                let contact_id = e.via.last().map(|v| v.id.get()).unwrap_or(e.from.get());
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
                let ins = cached_ins.as_ref().or(store_ins.as_ref());
                // ---- 「读+写」：把被 `action_strength` 压掉的另一种访问方式记回边上 ----
                if let Some(ins) = ins {
                    let mut others: Vec<String> = ins
                        .iter()
                        .filter(|r| {
                            r.from_id.get() == contact_id
                                && r.kind.as_str() != e.kind
                                && counterpart_kinds(e.kind.as_str()).contains(&r.kind.as_str())
                        })
                        .map(|r| r.kind.to_string())
                        .collect();
                    others.sort();
                    others.dedup();
                    e.also_kinds = others;
                }
                if e.to_call_site.is_none() {
                    let mut best: Option<(bool, i64, SourceLocation)> = None;
                    if let Some(ins) = cached_ins.as_ref().or(store_ins.as_ref()) {
                        for raw in ins {
                            if raw.kind.as_str() != e.kind {
                                continue;
                            }
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
                                note: Some("location where this link accesses the resource".to_string()),
                                snippet: ev
                                    .get("snippet")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string()),
                            };
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
        _center_node: &gt_domain::model::Node,
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

        let is_visible = |id: i64| -> bool {
            if id == center_id.get() {
                return true;
            }
            d.semantic_of.get(&id).copied().unwrap_or(false) || d.force_visible.contains(&id)
        };

        // 语义边优先级（用于折叠路径上的边提拉）：见 `Self::agg_rank`。
        let mut shown_edges: Vec<EdgeView> = Vec::new();
        let mut shown_keys: HashSet<(String, i64, i64, Vec<i64>)> = HashSet::new();
        let mut hidden_by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut hidden_total = 0usize;
        // 无语义入口的直接访问（孤儿）：不进画布，只记账（见下方反向分支的收集处）。
        let mut orphans: Vec<OrphanAccess> = Vec::new();

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

        let cs_cache = NodeCache::default();
        let outs_cache =
            std::cell::RefCell::new(HashMap::<i64, Vec<gt_domain::model::Edge>>::new());
        let ins_cache = std::cell::RefCell::new(HashMap::<i64, Vec<gt_domain::model::Edge>>::new());
        let call_site = |from: i64, to: i64| -> Option<SourceLocation> {
            let outs = match d.out_edges.get(&from) {
                Some(v) => v.clone(),
                None => outs_cache
                    .borrow_mut()
                    .entry(from)
                    .or_insert_with(|| {
                        self.store
                            .edges_outgoing(&[NodeId(from)])
                            .ok()
                            .and_then(|m| m.get(&from).cloned())
                            .unwrap_or_default()
                    })
                    .clone(),
            };
            {
                for e in outs {
                    if e.kind.as_str() == "Calls" && e.to_id.get() == to {
                        if let Some(cs_id) = e.properties.get("call_site").and_then(|v| v.as_i64())
                        {
                            if let Some(loc) =
                                self.node_source_location_cached(NodeId(cs_id), &cs_cache)
                            {
                                return Some(loc);
                            }
                        }
                    }
                }
            }
            self.call_site_between(from, to, &d.out_edges, &cs_cache)
        };
        let via_from_ids = |ids: Vec<i64>| -> Vec<ViaNode> {
            ids.into_iter()
                .map(|id| ViaNode {
                    id: NodeId(id),
                    kind: d
                        .kind_of
                        .get(&id)
                        .cloned()
                        .or_else(|| summary.get(&id).map(|m| m.kind.clone()))
                        .unwrap_or_default(),
                    name: d
                        .name_of
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
            let (f, t) = (from, to);
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
                resolved: Self::agg_rank(kind.as_str()) > 0,
                confidence: if Self::agg_rank(kind.as_str()) > 0 {
                    0.8
                } else {
                    0.5
                },
                hops,
                via,
                to_call_site,
                indirect,
                also_kinds: Vec::new(),
                // 由"内联链路位置"步骤统一填充（见 object_view 末尾）。
                node_locations: Vec::new(),
            });
        };

        {
            let mut nodes_by_dist: Vec<(u32, i64)> =
                d.ring_of.iter().map(|(&id, &r)| (r, id)).collect();
            nodes_by_dist.sort_by(|a, b| a.0.cmp(&b.0));
            let mut lift: HashMap<i64, i64> = HashMap::new();
            for (_d, id) in &nodes_by_dist {
                if is_visible(*id) {
                    lift.insert(*id, *id);
                    continue;
                }
                let l = d
                    .parent_of
                    .get(id)
                    .and_then(|p| lift.get(p))
                    .copied()
                    .unwrap_or(*id);
                lift.insert(*id, l);
            }

            if d.reverse {
                const MAX_USERS: usize = 80;
                let mut pending_trigger_vis: Vec<i64> = Vec::new();
                let mut users: Vec<i64> = d
                    .ring_of
                    .keys()
                    .copied()
                    .filter(|id| {
                        *id != center_id.get() && d.semantic_of.get(id).copied().unwrap_or(false)
                    })
                    .collect();
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
                    let upstream_reaches_semantic = |start: i64| -> bool {
                        let mut frontier: Vec<i64> = vec![start];
                        let mut seen: HashSet<i64> = HashSet::new();
                        for _ in 0..64 {
                            if frontier.is_empty() {
                                break;
                            }
                            {
                                let mut miss: Vec<i64> = frontier
                                    .iter()
                                    .copied()
                                    .filter(|id| {
                                        !d.in_edges.contains_key(id)
                                            && !ins_cache.borrow().contains_key(id)
                                    })
                                    .collect();
                                miss.sort_unstable();
                                miss.dedup();
                                if !miss.is_empty() {
                                    let ids: Vec<NodeId> = miss.into_iter().map(NodeId).collect();
                                    if let Ok(m) = self.store.edges_incoming(&ids) {
                                        // 查不到（确无入边）也要记一个空占位，避免同一节点
                                        // 在后续层里被反复查询。
                                        let mut c = ins_cache.borrow_mut();
                                        for id in ids {
                                            let v = m.get(&id.get()).cloned().unwrap_or_default();
                                            c.insert(id.get(), v);
                                        }
                                    }
                                }
                            }
                            let mut nxt: Vec<i64> = Vec::new();
                            for cur in frontier {
                                if !seen.insert(cur) {
                                    continue;
                                }
                                if (users.contains(&cur)
                                    || covered.contains(&cur)
                                    || d.semantic_of.get(&cur).copied().unwrap_or(false)
                                    || summary
                                        .get(&cur)
                                        .map_or(false, |m| NodeKind(m.kind.clone()).is_semantic()))
                                    && cur != center_id.get()
                                {
                                    return true;
                                }
                                let ins = d
                                    .in_edges
                                    .get(&cur)
                                    .cloned()
                                    .or_else(|| ins_cache.borrow().get(&cur).cloned());
                                if let Some(ins) = ins {
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
                    let mut collapsed: Vec<i64> = d
                        .ring_of
                        .keys()
                        .copied()
                        .filter(|id| {
                            let is_producer = d
                                .path_kind
                                .get(id)
                                .map_or(false, |k| k == "Triggers");
                            *id != center_id.get()
                                && d.parent_of.get(id) == Some(&center_id.get())
                                && !d.semantic_of.get(id).copied().unwrap_or(false)
                                && d.path_kind.get(id).map_or(false, |k| !k.is_empty())
                                && (is_producer
                                    || (!covered.contains(id)
                                        && !upstream_reaches_semantic(*id)))
                        })
                        .collect();
                    collapsed.sort_unstable();
                    for id in collapsed {
                        let ek = d.path_kind.get(&id).cloned().unwrap_or_default();
                        if ek == "Triggers" {
                            if let Some(ev) =
                                self.triggers_edge_view(id, center_id.get(), d, &summary, &cs_cache)
                            {
                                let path: Vec<i64> = ev.via.iter().map(|v| v.id.get()).collect();
                                let key = (ev.kind.clone(), ev.from.get(), ev.to.get(), path);
                                if shown_keys.insert(key) {
                                    pending_trigger_vis.push(ev.from.get());
                                    shown_edges.push(ev);
                                }
                                // Already visible (drawn before with same start and same chain): no need to account.
                                continue;
                            }
                            // Enrichment failed (original-edge evidence missing, etc.): downgrade to orphans accounting, never silently omit.
                        }
                        orphans.push(OrphanAccess {
                            id: NodeId(id),
                            kind: d.kind_of.get(&id).cloned().unwrap_or_default(),
                            name: d.name_of.get(&id).cloned().unwrap_or_default(),
                            edge_kind: ek,
                            location: self.node_source_location_cached(NodeId(id), &cs_cache),
                            edge: None,
                        });
                    }
                }
                users.sort_by_key(|id| d.ring_of.get(id).copied().unwrap_or(0));
                for id in users.iter().take(MAX_USERS) {
                    let kind = d
                        .path_kind
                        .get(id)
                        .cloned()
                        .filter(|k| !k.is_empty())
                        .unwrap_or_else(|| "Reads".to_string());
                    let mut via = chain_to(*id, center_id.get());
                    if via.last().map(|v| v.id.get()) == Some(*id) {
                        via.pop();
                    }
                    let from_center = d.path_from_center.contains(id);
                    // After swapping endpoints, the chain order must flip once too: `push_edge` flips again under reverse,
                    // the two flips restore the "center → … → other" reading order.
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
                for from in pending_trigger_vis {
                    d.force_visible.insert(from);
                    let ring = d.ring_of.get(&from).copied().unwrap_or(1);
                    d.ring_of.entry(from).or_insert(ring);
                    d.parent_of.entry(from).or_insert(center_id.get());
                }
            } else {
                // Off-ring semantic targets staged first; after `push_edge`/`chain_to` (which capture these maps) finish,
                // write back to `d.ring_of`/`d.parent_of`, avoiding borrow conflicts with their immutable borrows.
                let mut pending: Vec<(i64, i64, u32)> = Vec::new();
                let mut groups: HashMap<(String, i64, i64), Vec<gt_domain::model::Edge>> =
                    HashMap::new();
                // Subject is a syntax node, and after lifting its ancestor is the end itself (`a == to`): don't draw, downgrade to `orphans`. Use `BTreeMap` for deterministic order.
                let mut collapsed_from: BTreeMap<i64, String> = BTreeMap::new();
                let mut all_nodes: Vec<i64> = d.ring_of.keys().copied().collect();
                all_nodes.sort_unstable();
                for node in &all_nodes {
                    let outs = d.out_edges.get(node).into_iter().flatten().cloned();
                    for e in outs {
                        if !is_semantic_edge(e.kind.as_str()) && !is_bridge_edge(e.kind.as_str()) {
                            continue;
                        }
                        let to = e.to_id.get();
                        // The target may be a "semantic node blocked off-ring by the syntax chain": register it into the ring,
                        // so even with a small depth, the folded view still renders the semantic target.
                        let to_is_sem = summary
                            .get(&to)
                            .map(|m| NodeKind(m.kind.clone()).is_semantic())
                            .unwrap_or(false)
                            || d.semantic_of.get(&to).copied().unwrap_or(false);
                        if !to_is_sem {
                            continue;
                        }
                        if !d.ring_of.contains_key(&to) {
                            let from_ring = d.ring_of.get(&e.from_id.get()).copied().unwrap_or(0);
                            pending.push((to, e.from_id.get(), from_ring + 1));
                        }
                        let a = *lift.get(&e.from_id.get()).unwrap_or(&e.from_id.get());
                        if a == to {
                            if !is_visible(e.from_id.get()) {
                                collapsed_from
                                    .entry(e.from_id.get())
                                    .or_insert_with(|| e.kind.to_string());
                            }
                            continue;
                        }
                        groups
                            .entry((e.kind.as_str().to_string(), a, to))
                            .or_default()
                            .push(e);
                    }
                }
                {
                    let mut action_pairs: HashSet<(i64, i64)> = HashSet::new();
                    for (k, a, to) in groups.keys() {
                        if k.as_str() == "ReadsDb" || k.as_str() == "WritesDb" {
                            action_pairs.insert((*a, *to));
                        }
                    }
                    groups.retain(|(k, a, to), _| {
                        !(k.as_str() == "MapsTo" && action_pairs.contains(&(*a, *to)))
                    });
                }
                {
                    let mut keys: Vec<(String, i64, i64)> = groups.keys().cloned().collect();
                    keys.sort_unstable();
                    let mut seen: HashSet<i64> = HashSet::new();
                    for (kind, a, _to) in keys {
                        if is_visible(a) || !seen.insert(a) {
                            continue;
                        }
                        orphans.push(OrphanAccess {
                            id: NodeId(a),
                            kind: d.kind_of.get(&a).cloned().unwrap_or_default(),
                            name: d.name_of.get(&a).cloned().unwrap_or_default(),
                            edge_kind: kind,
                            location: self.node_source_location_cached(NodeId(a), &cs_cache),
                            edge: None,
                        });
                    }
                    groups.retain(|(_k, a, _to), _| is_visible(*a));
                }
                // The `a == to` batch (syntax-node-subject direct semantic edges) also goes to accounting.
                for (id, kind) in collapsed_from {
                    let edge = if kind == "Triggers" {
                        self.triggers_edge_view(id, center_id.get(), d, &summary, &cs_cache)
                    } else {
                        None
                    };
                    orphans.push(OrphanAccess {
                        id: NodeId(id),
                        kind: d.kind_of.get(&id).cloned().unwrap_or_default(),
                        name: d.name_of.get(&id).cloned().unwrap_or_default(),
                        edge_kind: kind,
                        location: self.node_source_location_cached(NodeId(id), &cs_cache),
                        edge,
                    });
                }
                // At most a few **different paths** on the same `(kind, a, to)` pair: more would blur together,
                // the overflow counts into `hidden` honest accounting.
                const MAX_PATHS: usize = 4;
                // Max hops when walking a propagation edge back to the touch point. `seed_source` is usually beyond discovery depth,
                // but not far from the center (CRMEB measured 4~5 hops); beyond that, give up backtracking, fall back to original behavior.
                const MAX_SEED_HOPS: usize = 8;
                for ((kind, a, to), mut es) in groups {
                    es.sort_by_key(|e| e.id.get());
                    let mut by_origin: BTreeMap<i64, &gt_domain::model::Edge> = BTreeMap::new();
                    for e in &es {
                        if !is_indirect_edge(e) {
                            by_origin.entry(e.from_id.get()).or_insert(e);
                        }
                    }
                    // Candidate path: (touch point, full via node-id sequence, evidence-edge id, is-indirect)
                    let mut drawn: Vec<(i64, Vec<i64>, i64, bool)> = Vec::new();
                    // ① touch-point edge: `via` back-tracked from the discovery tree, the end is the method that emits the action.
                    for (o, e) in &by_origin {
                        // `indirect` considers two points: ① the original edge is itself a propagation edge; ② the edge was lifted from a descendant
                        // node to ancestor `a` — the ancestor doesn't directly perform the action. Either makes it indirect.
                        let ids: Vec<i64> = chain_to(*o, a).iter().map(|v| v.id.get()).collect();
                        drawn.push((*o, ids, e.id.get(), a != *o || is_indirect_edge(e)));
                    }
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
                            // Touch point unreachable in this view's chain-edge adjacency (cross-sub-project / `Calls` missing).
                            // Fall back to the propagation edge's own via: rather truncate than lose this dependency entirely.
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
                            // Evidence edge prefers the touch point's **own** direct edge (with `evidence`):
                            // a propagation edge has no evidence, opening "edge details" would be blank.
                            let ev_id = d
                                .in_edges
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
                            // `p` = [a, …, seed]; `via` drops the leading center, keeps the trailing touch point.
                            drawn.push((seed, p[1..].to_vec(), ev_id, true));
                        }
                    }
                    // ③ same (touch point, path) draws only one; beyond `MAX_PATHS` counts into hidden honest accounting.
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

            for &id in d.ring_of.keys() {
                if !d.kind_of.contains_key(&id) {
                    if let Some(m) = summary.get(&id) {
                        d.kind_of.insert(id, m.kind.clone());
                        d.name_of.insert(id, m.name.clone());
                        d.semantic_of
                            .insert(id, NodeKind(m.kind.clone()).is_semantic());
                    }
                }
            }

            // Folded syntax nodes accounted by kind
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
            orphans,
        })
    }

    /// Aggregate-kind perspective: cluster boxes / matrix (**not a single chain**).
    pub fn aggregate_view(
        &self,
        project_id: ProjectId,
        perspective: &str,
        sample_limit: u32,
    ) -> Result<AggregateView> {
        let registry = self.views.registry();
        let spec = registry
            .by_id(perspective)
            .ok_or_else(|| DomainError::NotFound(format!("perspective {perspective}")))?;

        let target_kind = spec.node_kind.clone().unwrap_or_default();
        let mut nodes = self.store.query_nodes(&NodeFilter {
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
        // Filter by "side" (frontend/backend cache split).
        if let Some(side) = &spec.side {
            nodes.retain(|n| {
                n.properties
                    .get("side")
                    .and_then(|v| v.as_str())
                    .map(|s| s == side.as_str())
                    .unwrap_or(false)
            });
        }

        if spec.layout == gt_domain::model::LayoutMode::Matrix {
            return Ok(self.matrix_view(project_id, spec, nodes, sample_limit));
        }

        // Compound / ER: group by `group_by`
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
                "The graph has no nodes that can be aggregated under \"{}\", so no clustering conclusion is given here.",
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
                note: "The aggregate view shows only sample members per group; full member counts are given as numbers.".into(),
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
                note: "A matrix cell holds the node count for the two dimensions (0 means that combination really has no output).".into(),
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
                        .map(|s| format!("sub-project #{}", s))
                        .unwrap_or_else(|| "unassigned".into())
                }),
            Some(GroupBy::Property(name)) => node
                .properties
                .get(name)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "(none)".to_string()),
            None => node.kind.to_string(),
        }
    }

    /// A node's definition location (a synthesized node returns a **multi-location list**).
    pub fn node_locations(&self, node_id: NodeId) -> Result<NodeLocations> {
        let node = self
            .store
            .get_node(node_id)?
            .ok_or_else(|| DomainError::NotFound(format!("node {node_id}")))?;
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

    /// Compute the location list from the **in-memory** nodes (no more DB round trips for nodes).
///
/// `files` is a file-path cache: many nodes on one chain often fall in the same batch of files;
/// per-node `file_path()` would repeat the round trip.
    fn locations_of_node(
        &self,
        node: &gt_domain::model::Node,
        c: &NodeCache,
    ) -> Result<(bool, Vec<SourceLocation>)> {
        let synthetic = node.identity.is_some();
        let mut locations: Vec<SourceLocation> = Vec::new();

        // ① syntax node: file + line + symbol
        if let Some(file_id) = node.file_id {
            if let Some(path) = self.cached_file(c, file_id) {
                locations.push(SourceLocation {
                    file: path,
                    line: node.span.start_line,
                    symbol: node.fqn.clone(),
                    note: Some(format!("{} definition", node.kind)),
                    snippet: node
                        .properties
                        .get("snippet")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string()),
                });
            }
        }

        // ② synthesized node: co-occurrence locations everywhere
        if let Some(Value::Array(arr)) = node.properties.get("locations") {
            for item in arr {
                if let Ok(loc) = serde_json::from_value::<SourceLocation>(item.clone()) {
                    locations.push(loc);
                }
            }
        }

        // ③ origin from the authoritative symbol table (e.g. the SQL file for schema, the route file for route_list)
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
                            note: Some(format!("authoritative source {table}")),
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
                                    note: Some(format!("{table} source")),
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

    /// An edge's evidence chain: a real edge is a single hop, a dashed edge expands the CallSite locations it passes through.
    pub fn edge_evidence(&self, edge_id: i64) -> Result<Option<EdgeEvidence>> {
        let Some(edge) = self.find_edge(edge_id)? else {
            return Ok(None);
        };
        let view = self.to_edge_view(edge.clone());
        let mut locations: Vec<SourceLocation> = Vec::new();
        let mut via: Vec<String> = Vec::new();
        let mut reason: Option<String> = None;

        // Evidence 1: the evidence carried on the edge itself
        if let Some(Value::Object(ev)) = edge.properties.get("evidence") {
            if let Some(loc) = ev.get("location").and_then(|v| v.as_str()) {
                let (file, line) = split_file_line(loc);
                locations.push(SourceLocation {
                    file,
                    line,
                    symbol: None,
                    note: Some("evidence location recorded when the edge was created".into()),
                    // The call-statement source text stored together when P5 built the edge.
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

        // Evidence 2: the CallSite inside the start method (a dashed edge must be verifiable hop by hop)
        if let Some(from) = self.store.get_node(edge.from_id)? {
            if let Some(file_id) = from.file_id {
                if let Some(path) = self.file_path(file_id) {
                    locations.push(SourceLocation {
                        file: path,
                        line: from.span.start_line,
                        symbol: from.fqn.clone(),
                        note: Some("start definition".into()),
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
                        note: Some("end definition".into()),
                        snippet: to
                            .properties
                            .get("snippet")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
            }
        }

        // Only hint when "unresolved and no locatable evidence": an edge with a real call site / evidence location shouldn't be wrongly called an "inferred edge".
        if !view.resolved && locations.is_empty() {
            reason = Some(format!(
                "This edge ({}) has no locatable evidence position; it may come from rule completion, so treat it with care.",
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

    // ---------------------------------------------------------------- internals

    /// Semantic-edge priority: for folding-path edge lifting, and `to_edge_view` judging "resolved".
/// Returns >0 to be treated as a resolved semantic edge (from a real call site / FKB-rule synthesis, not an evidence-less inference).
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
        // Real-edge judgment: from authoritative resolution (P7 result) or syntactically-determined inheritance/implementation/declaration
        let authoritative = matches!(
            e.kind.as_str(),
            "Extends" | "Implements" | "UsesTrait" | "Declares" | "Contains" | "HasCallSite"
        );
        let has_tier = e.properties.get("tier").is_some();
        // `indirect` (P8 propagation along the call chain) only decides dashed styling and the "indirect" label, no longer down-weights as "to-verify".
        let indirect = is_indirect_edge(&e);
        // Resolved = syntax/structural edge (authoritative) or authoritative-resolution edge (has_tier) or semantic edge (agg_rank>0).
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
            also_kinds: Vec::new(),
            node_locations: Vec::new(),
        }
    }

    /// A single node's "definition location" (None when no file/span).
    fn node_source_location_cached(&self, id: NodeId, c: &NodeCache) -> Option<SourceLocation> {
        let node = self.cached_node(c, id.get())?;
        let file_id = node.file_id?;
        let path = self.cached_file(c, file_id)?;
        Some(SourceLocation {
            file: path,
            line: node.span.start_line,
            symbol: node.fqn.clone(),
            note: None,
            // CallSite nodes carry the call-statement source text, shown under each hop of the chain for verification.
            snippet: node
                .properties
                .get("snippet")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        })
    }

    /// Build a **clickable-expandable semantic-edge** view for an event perspective's `Triggers` trigger point (isomorphic to the route perspective's click-to-expand drawer):
/// start = the method that triggered it, end = the center event, `via` = that method's call chain (upstream callers … touch point),
/// `to_call_site` = the dispatch call site.
///
/// The trigger is any business method (high fan-in); it's not promoted to a canvas node, only accounted as a "direct access"; but with this edge attached,
/// clicking the frontend orphan list lets you verify hop-by-hop "who dispatched this event, on which call chain".
    fn triggers_edge_view(
        &self,
        producer: i64,
        center: i64,
        d: &Discovery,
        summary: &HashMap<i64, NodeSummary>,
        c: &NodeCache,
    ) -> Option<EdgeView> {
        // Find the original `producer --Triggers--> center` edge, take its evidence (dispatch call site) and id.
        let trig = d
            .out_edges
            .get(&producer)
            .into_iter()
            .flatten()
            .chain(d.in_edges.get(&center).into_iter().flatten())
            .find(|e| {
                e.kind.as_str() == "Triggers"
                    && e.to_id.get() == center
                    && e.from_id.get() == producer
            })
            .or_else(|| {
                d.out_edges
                    .get(&producer)
                    .into_iter()
                    .flatten()
                    .find(|e| e.kind.as_str() == "Triggers" && e.to_id.get() == center)
            });
        let (src, to_call_site) = match trig {
            Some(e) => {
                let loc = e
                    .properties
                    .get("evidence")
                    .and_then(|v| v.get("location"))
                    .and_then(|l| serde_json::from_value::<SourceLocation>(l.clone()).ok())
                    .or_else(|| self.node_source_location_cached(NodeId(producer), c));
                (e.id.get(), loc)
            }
            None => (0, self.node_source_location_cached(NodeId(producer), c)),
        };

        let sem = |id: i64| -> bool {
            summary
                .get(&id)
                .map(|m| NodeKind(m.kind.clone()).is_semantic())
                .unwrap_or(false)
        };
        let mut callers: Vec<i64> = Vec::new(); // 由近及远：[直接调用方, …, 最远]
        let mut seen_up: HashSet<i64> = HashSet::new();
        seen_up.insert(producer);
        let mut cur = producer;
        for _ in 0..4 {
            let ins = match d.in_edges.get(&cur) {
                Some(v) => Some(v.clone()),
                None => self
                    .store
                    .edges_incoming(&[NodeId(cur)])
                    .ok()
                    .map(|m| m.get(&cur).cloned().unwrap_or_default()),
            };
            let Some(ins) = ins else { break };
            let next = ins
                .iter()
                .filter(|e| e.kind.as_str() == "Calls")
                .find(|e| seen_up.insert(e.from_id.get()))
                .map(|e| e.from_id.get());
            match next {
                Some(p) => {
                    callers.push(p);
                    if sem(p) {
                        break; // 到达语义入口（路由等），链路到此封顶
                    }
                    cur = p;
                }
                None => break,
            }
        }
        // start = the farthest caller (usually the semantic entry); via = [intermediate callers…, producer (touch point)].
        // When no caller exists: start = producer, via empty (only start↔end, two hops).
        let (from, via_ids): (i64, Vec<i64>) = if callers.is_empty() {
            (producer, Vec::new())
        } else {
            let top = *callers.last().unwrap();
            let mut v: Vec<i64> = callers[..callers.len() - 1].iter().rev().copied().collect();
            v.push(producer); // 接触点作为 via 末跳
            (top, v)
        };
        let via: Vec<ViaNode> = via_ids
            .iter()
            .map(|&vid| ViaNode {
                id: NodeId(vid),
                kind: d
                    .kind_of
                    .get(&vid)
                    .cloned()
                    .or_else(|| summary.get(&vid).map(|m| m.kind.clone()))
                    .unwrap_or_default(),
                name: d
                    .name_of
                    .get(&vid)
                    .cloned()
                    .or_else(|| summary.get(&vid).map(|m| m.name.clone()))
                    .unwrap_or_default(),
                call_site: None,
            })
            .collect();
        let hops = if via.is_empty() { None } else { Some(via.len() as u32) };
        Some(EdgeView {
            id: if src == 0 { -1 } else { src },
            kind: "Triggers".to_string(),
            from: NodeId(from),
            to: NodeId(center),
            resolved: true,
            confidence: 0.8,
            hops,
            via,
            to_call_site,
            indirect: false,
            also_kinds: Vec::new(),
            node_locations: Vec::new(),
        })
    }

    /// A method's "addressable full name": `owning class::method`. A bare method name can't be located once detached from its class (an early comment logged this pitfall),
/// so when the event perspective draws the trigger method on the canvas, wrapping it in its owning class makes it readable and clickable. The owning class is traced back via `Declares` in-edges (listener / service methods hang on some `Class`).
    fn method_qualified_name(
        &self,
        method: i64,
        d: &Discovery,
        summary: &HashMap<i64, NodeSummary>,
    ) -> String {
        let raw = d
            .name_of
            .get(&method)
            .cloned()
            .or_else(|| summary.get(&method).map(|m| m.name.clone()))
            .unwrap_or_default();
        let class = self
            .store
            .edges_incoming(&[NodeId(method)])
            .ok()
            .and_then(|m| m.get(&method).cloned())
            .and_then(|edges| {
                edges
                    .iter()
                    .find(|e| e.kind.as_str() == "Declares")
                    .map(|e| e.from_id.get())
            })
            .and_then(|cid| {
                d.name_of
                    .get(&cid)
                    .cloned()
                    .or_else(|| self.store.get_node(NodeId(cid)).ok().flatten().map(|n| n.name))
            });
        match class {
            Some(c) => format!("{}::{}", c, raw),
            None => raw,
        }
    }

    /// Request-level cached node lookup: the same node is queried many times in folding / inline chains;
/// caching makes it hit the DB at most once (see [`NodeCache`]).
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

    /// The "call site" location between two nodes.
///
/// **Precise path**: the `from --Calls--> to` edge explicitly records the corresponding `CallSite` node id during resolution (see `resolve_calls`),
/// just take it, no heuristic needed. Only when this precise edge is absent (e.g. `route → handler`, `method → table`, which are semantic edges, not `Calls` edges)
/// fall back to matching `HasCallSite` call sites by callee name.
    fn call_site_between(
        &self,
        from: i64,
        to: i64,
        out_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
        c: &NodeCache,
    ) -> Option<SourceLocation> {
        // Reuse the out-edge cache batch-prefetched in the BFS phase, avoid one DB round trip per folded edge (N+1).
        // When the cache misses (a few nodes not prefetched), fall back to a single query.
        let outs = out_cache.get(&from).cloned().or_else(|| {
            self.store
                .edges_of(NodeId(from), EdgeDirection::Outgoing)
                .ok()
        });
        // 1) precise: directly read the CallSite node id recorded on the `from → to` Calls edge
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
        // 2) fallback: among from's HasCallSite call sites, the one whose callee name matches to
        let to_name = self
            .cached_node(c, to)
            .map(|n| self.short_name_of(&n))
            .unwrap_or_default();
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

    /// A node name's (method name / class name) "short name": drop namespace and prefix, keep only the last segment.
    fn short_name_of(&self, n: &gt_domain::model::Node) -> String {
        let raw = n
            .fqn
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&n.name);
        self.short_name_str(raw)
    }

    /// Extract "bar" / "Foo" from "App\X\Foo::bar" / "obj->bar" / "new Foo".
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
        // A node's "side": the `side` (`frontend` / `backend`) annotated by FKB on semantic nodes.
        // Passed through to the frontend, and used to pick the right perspective by side when "click to switch".
        let side = n
            .properties
            .get("side")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let registry = self.views.registry();
        let own_view = registry
            .view_for_kind_and_side(n.kind.as_str(), side.as_deref())
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
        let fan_in = in_cache
            .get(&id.get())
            .cloned()
            .or_else(|| self.store.edges_of(id, EdgeDirection::Incoming).ok())
            .map(|es| {
                es.iter()
                    .filter(|e| is_semantic_edge(e.kind.as_str()))
                    .map(|e| e.from_id.get())
                    .collect::<HashSet<i64>>()
                    .len()
            })
            .unwrap_or(0);
        let fan_out = out_cache
            .get(&id.get())
            .cloned()
            .or_else(|| self.store.edges_of(id, EdgeDirection::Outgoing).ok())
            .map(|es| {
                es.iter()
                    .filter(|e| is_semantic_edge(e.kind.as_str()))
                    .map(|e| e.to_id.get())
                    .collect::<HashSet<i64>>()
                    .len()
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
        let columns = self.columns_of(id, in_cache, out_cache);
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
            columns: if columns.is_empty() { None } else { Some(columns) },
            metrics: json!({ "fan_in": fan_in, "fan_out": fan_out }),
        }))
    }

    /// A node's columns (**bare column name**, the `table.column` prefix in column identity is stripped here).
///
/// Columns aren't drawn as independent nodes in the folded view, but "open a table to see its fields" is a must-have,
/// so they're carried out here as node properties. Two source paths:
/// * **PHP**: `Table --HasColumn--> Column` (columns hang directly under the table, from authoritative schema);
/// * **Node / TypeORM**: `Table <--MapsTo-- entity class --HasColumn--> Column`
///   (`@Column` hangs on the entity class, the table must reverse-route one hop via `MapsTo`).
    fn columns_of(
        &self,
        id: NodeId,
        in_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
        out_cache: &HashMap<i64, Vec<gt_domain::model::Edge>>,
    ) -> Vec<String> {
        let mut cols: Vec<String> = Vec::new();
        let push = |raw: &str, cols: &mut Vec<String>| {
            // Column identity is `table.column` (scoped to avoid same-name collisions across tables), show only the column name
            let bare = raw.rsplit('.').next().unwrap_or(raw).to_string();
            if !bare.is_empty() && !cols.iter().any(|c| c == &bare) {
                cols.push(bare);
            }
        };

        // ① columns hang directly under this node (PHP's table)
        if let Some(es) = out_cache.get(&id.get()) {
            for e in es.iter().filter(|e| e.kind.as_str() == "HasColumn") {
                if let Ok(Some(n)) = self.store.get_node(e.to_id) {
                    push(&n.name, &mut cols);
                }
            }
        }
        if !cols.is_empty() {
            cols.sort();
            return cols;
        }

        // ② via `MapsTo` reverse-find the entity class, then take its columns (TypeORM: columns on the entity class)
        if let Some(es) = in_cache.get(&id.get()) {
            for e in es.iter().filter(|e| e.kind.as_str() == "MapsTo") {
                let Ok(outs) = self.store.edges_of(e.from_id, EdgeDirection::Outgoing) else {
                    continue;
                };
                for o in outs.iter().filter(|o| o.kind.as_str() == "HasColumn") {
                    if let Ok(Some(n)) = self.store.get_node(o.to_id) {
                        push(&n.name, &mut cols);
                    }
                }
            }
        }
        cols.sort();
        cols
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
        out.insert("perspective".into(), json!(spec.label));
        out.insert("object".into(), json!(name));
        out.insert(
            "in_edges".into(),
            json!(center.metrics.get("fan_in").cloned().unwrap_or(json!(0))),
        );
        out.insert(
            "out_edges".into(),
            json!(center.metrics.get("fan_out").cloned().unwrap_or(json!(0))),
        );
        if !center.annotations.is_empty() {
            out.insert("annotations".into(), json!(center.annotations));
        }
        // Supplement from authoritative sources (when possible; never fabricate when not)
        if let Ok(Some(entry)) = self.store.get_symbol(project_id, "schema", name) {
            let cols = entry
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            out.insert("schema_columns".into(), json!(cols));
        }
        if let Ok(Some(entry)) = self.store.get_symbol(project_id, "route_list", name) {
            out.insert(
                "route_registered".into(),
                entry.get("handler").cloned().unwrap_or(json!(null)),
            );
            if let Some(guards) = entry.get("guards").and_then(|g| g.as_array()) {
                if !guards.is_empty() {
                    let list: Vec<String> = guards
                        .iter()
                        .filter_map(|g| {
                            let full = g.get("class").and_then(|c| c.as_str())?;
                            let short = full.rsplit(['\\', '/']).next().unwrap_or(full);
                            match g.get("arg").and_then(|a| a.as_str()) {
                                Some(arg) => Some(format!("{}({})", short, arg)),
                                None => Some(short.to_string()),
                            }
                        })
                        .collect();
                    if !list.is_empty() {
                        out.insert("middleware".into(), json!(list.join(" · ")));
                    }
                }
            }
        }
        if drawn_edges == 0
            && matches!(
                center.kind.as_str(),
                NodeKind::HTTP_CONTRACT | NodeKind::SCHEDULE
            )
        {
            let hint = if center.kind.as_str() == NodeKind::SCHEDULE {
                "This scheduled task has no expandable semantic dependencies yet: the crontab route did not resolve to a handler, or the handler touches no semantic resource (table / config / cache / event). This is usually the real situation, not a missing view."
            } else {
                "This route has no expandable semantic dependencies yet: the route did not resolve to a handler, or the handler touches no semantic resource. This is usually the real situation, not a missing view."
            };
            out.insert("hint".into(), json!(hint));
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
        // `edges_of` can only query by node; reverse-lookup via the nodes table here is too costly,
        // so a SQL-side query port replaces it.
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

// ---------------------------------------------------------------- semantic / call-chain judgment

/// Call-chain edge: the folded view does "forward discovery" along these edges, treating syntax nodes as transparent.
/// Authoritative definition in `gt_domain::model::kinds::is_chain_edge`.
fn is_chain_edge(kind: &str) -> bool {
    gt_domain::model::kinds::is_chain_edge(kind)
}

/// Semantic-node judgment: first-class semantic kind (`SYNTHESIZED` in `kinds.rs`: `Table` / `HttpContract` /
/// `ConfigKey` / `I18nKey` / `Event` / `Queue` / `Cache` / `Topic`…).
/// `Event` / `Queue` / `Cache` / `Topic` are concrete kinds now, no longer relying on the `category` umbrella.
/// Classification authority is `kinds.rs`.
fn node_is_semantic(n: &gt_domain::model::Node) -> bool {
    NodeKind(n.kind.to_string()).is_semantic()
}

/// Semantic-edge judgment (authoritative source: the semantic-edge set in `kinds.rs`).
fn is_semantic_edge(kind: &str) -> bool {
    gt_domain::model::kinds::is_semantic_edge(kind)
}

/// Bridge-edge judgment (discovery connector between semantic and syntax, authoritative source: `kinds.rs::is_bridge_edge`).
///
/// The fold logic uses it with [`is_semantic_edge`] to decide: a bridge edge is let through by the draw gate, but its syntax endpoint
/// ultimately downgrades into `orphans` accounting (not drawn as a syntax node).
fn is_bridge_edge(kind: &str) -> bool {
    gt_domain::model::kinds::is_bridge_edge(kind)
}

/// Whether it's an **indirect edge from propagation** (P8 replicates along the `Calls` chain, see `propagate.rs`).
///
/// Edges directly hit by FKB rules only carry `evidence`; propagation edges carry `via: "propagate"`,
/// and environment-read kinds (`ReadsConfig` / `ReadsCache`) additionally carry `indirect: true`.
///
/// The two must be distinguished: otherwise any shared method that read some config would make every entry passing through it
/// appear on the graph as "directly reading that config", looking like a real dependency.
fn is_indirect_edge(e: &gt_domain::model::Edge) -> bool {
    e.properties
        .get("indirect")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || e.properties.get("via").and_then(|v| v.as_str()) == Some("propagate")
}

/// The **root-cause touch point** of a propagation edge: `seed_source` recorded by P8, the method node that really performs the action.
///
/// Note `is_indirect_edge` equals "is a propagation edge": direct edges only carry `evidence`, propagation edges always carry
/// `via: "propagate"` (environment-read kinds also carry `indirect: true`).
fn seed_source_of(e: &gt_domain::model::Edge) -> Option<i64> {
    e.properties.get("seed_source").and_then(|v| v.as_i64())
}

/// A node cache within one view request.
///
/// Folding and inline chains query **the same batch of nodes repeatedly**: `get_node` / `file_path` / `get_symbol` each a round trip,
/// a 29-edge graph takes hundreds of queries — measured "call-site location" alone takes ~670ms (2/3 of the object view). Caching makes each node / file / symbol-table entry queried at most once.
///
/// Request-scoped not global: the graph gets rebuilt, a cross-request cache would read stale data.
#[derive(Default)]
pub(crate) struct NodeCache {
    nodes: std::cell::RefCell<HashMap<i64, Option<gt_domain::model::Node>>>,
    files: std::cell::RefCell<HashMap<i64, Option<String>>>,
    symbols: std::cell::RefCell<HashMap<(i64, String, String), Option<serde_json::Value>>>,
}

/// Enumerate the complete **simple paths** of `from → to` on the chain-edge adjacency (DFS, at most `limit` paths, depth cap `max_hops`).
///
/// Why it's needed: a propagation edge only states "upstream reaches this resource", it itself **is not a path**. If used directly as the end of `via`,
/// `via` breaks at the discovery depth, drawing fake paths like "the route itself read the cache" (the real touch point is several hops away). Here we walk from the center all the way to the `seed_source` touch point,
/// completing the whole folded chain — the folded view's `via` must always land on the touch point, otherwise the frontend drawer shows a broken chain.
///
/// * `avoid`: the mid-hop exclusion predicate (pass "is a semantic node"; a semantic node is an endpoint, shouldn't be passed through);
/// * node exploration has a hard budget, guarding against combinatorial explosion on dense subgraphs.
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
            // Simple path: don't return to start, don't repeat nodes; don't pass through excluded nodes (semantic nodes are endpoints).
            if n == from || on_path.contains(&n) || avoid(n) {
                continue;
            }
            on_path.insert(n);
            path.push(n);
            dfs(
                out, from, n, to, max_hops, limit, avoid, path, on_path, acc, budget,
            );
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
        out,
        from,
        from,
        to,
        max_hops,
        limit,
        avoid,
        &mut path,
        &mut on_path,
        &mut acc,
        &mut budget,
    );
    acc
}

/// Access-mode label priority: read/write action (3) > other semantic access (2) > MapsTo (1) > structural edge (0).
///
/// `MapsTo` is a structural mapping (Model→Table), not "how to access", must be over-ridden by a real read/write access;
/// `HandledBy` / `Calls` / `ResolvesTo` structural edges stay 0, so the reverse perspective inherits the real access mode along the call chain toward the semantic ancestor (see **why every access action must be enumerated** below).
///
/// # Why every access action must be enumerated
///
/// In the reverse perspective (who is reading / writing this resource), the real touch point is often a **syntax node** (method),
/// the drawn edge must be "lifted" to its semantic ancestor (route / schedule / contract). The label inherits along the discovery tree:
/// the ancestor reuses the descendant's access mode (`pk`'s `inherited_rank` branch). **Once an access action is missed (rank degrades to 0), the inheritance chain breaks at it**,
/// and the ancestor re-hangs on the last structural edge passed through — so "schedule → method → cache" gets labeled `HandledBy` (read as "cache handled by this task"),
/// while the truth is the method's `Cache::set('crontabCache')`, which should be `WritesCache`.
/// Measured CRMEB's `crontabCache` once gave 6 such mislabeled edges.
pub fn access_rank(kind: &str) -> u8 {
    match kind {
        // Read/write action: **highest priority**, must override structural edges and MapTo.
        "WritesDb" | "ReadsDb" | "WritesCache" | "ReadsCache" => 3,
        // Other semantic accesses: read config / publish / trigger / frontend state change / event bus / page jump / frontend call.
        "ReadsConfig" | "PublishesTo" | "Triggers" | "Mutates" | "Emits" | "ListensTo"
        | "CallsHttp" | "NavigatesTo" => 2,
        "MapsTo" => 1,
        _ => 0,
    }
}

/// **Action strength** within the same `access_rank`: write > read.
///
/// Background: a method often both reads and writes the same table (`Db::name('Goods')->find()` and
/// `Db::name('Goods')->update($data)` written in the same method), so P7 **simultaneously** lays `ReadsDb` and `WritesDb` for the same "method → table" pair —
/// both are facts, but the view draws only one edge, must pick the more informative one, always "write".
///
/// Same for cache: when `Cache::get($k)` and `Cache::set($k)` coexist, label as write.
///
/// If it degrades to "smaller edge id wins", the displayed result depends on build phase order (P5 before P7),
/// but phase order is unrelated to semantics: measured shopxo had 1825 write operations thus crushed into "read DB", without exception.
pub fn action_strength(kind: &str) -> u8 {
    match kind {
        "WritesDb" | "WritesCache" => 2,
        "ReadsDb" | "ReadsCache" => 1,
        _ => 0,
    }
}

/// The access mode on the same resource, the "other half" of a given edge (read ↔ write).
///
/// The same method often has two edges on the same table (first `find()` then `update()`); the folded view draws only one,
/// the suppressed one must be recorded back onto `EdgeView::also_kinds`, or the frontend would say "read+write" as a single edge.
/// DB and cache form their own pairs, never crossing (one edge's end is only one kind of resource).
pub fn counterpart_kinds(kind: &str) -> &'static [&'static str] {
    match kind {
        "WritesDb" | "ReadsDb" => &["WritesDb", "ReadsDb"],
        "WritesCache" | "ReadsCache" => &["WritesCache", "ReadsCache"],
        _ => &[],
    }
}

/// Compare two "node access mode" candidates, return whether `new` beats `old`.
///
/// Priority: ① direct(`false`) beats indirect(`true`); ② higher access rank wins; ③ within the same rank,
/// stronger action wins (write > read, see [`action_strength`]); ④ smaller edge id wins (deterministic tiebreak).
///
/// Edge ids are globally unique, so among all candidates of a given node within the same BFS layer the **winner is unique** —
/// regardless of `HashMap` / edge-query traversal order, the final edge selected is the same, result is deterministic.
fn better_path(
    new_indirect: bool,
    new_kind: &str,
    new_eid: i64,
    old_indirect: bool,
    old_kind: &str,
    old_eid: i64,
) -> bool {
    if new_indirect != old_indirect {
        return !new_indirect; // 新的更直接 → 更优
    }
    let new_rank = access_rank(new_kind);
    let old_rank = access_rank(old_kind);
    if new_rank != old_rank {
        return new_rank > old_rank;
    }
    let new_strength = action_strength(new_kind);
    let old_strength = action_strength(old_kind);
    if new_strength != old_strength {
        return new_strength > old_strength;
    }
    new_eid < old_eid
}

/// Judge read/write by HTTP contract name (e.g. `POST /product`, `GET /products`), as a heuristic for whether the
/// "route access table" is a read-DB or write-DB.
///
/// Judgment priority:
/// 1. **Concrete verb**: write verbs (`POST`/`PUT`/`DELETE`/`PATCH`) → write DB (`WritesDb`),
///    others (incl. `GET`/`HEAD`/`OPTIONS`) → read DB (`ReadsDb`).
/// 2. **Wildcard method** (`ANY` / `RULE`, method-unrestricted, from ThinkPHP auto-routing / `Route::rule`):
///    no bound verb, do a second-level inference by the path's last segment (the controller method name under the PATH_INFO convention, e.g.
///    `submit` / `delete` / `get`); only conclude when hitting a known action root, otherwise fall back to "unknown → read".
fn is_write_http_method(route_name: &str) -> bool {
    let mut parts = route_name.splitn(2, ' ');
    let verb = parts.next().unwrap_or("").to_ascii_uppercase();
    let path = parts.next().unwrap_or("");
    if !gt_domain::model::is_wildcard_http_method(&verb) {
        return matches!(verb.as_str(), "POST" | "PUT" | "DELETE" | "PATCH");
    }
    // Wildcard endpoint: second-level read/write inference by controller method name (path last segment).
    if let Some(write) = infer_write_by_action(path) {
        return write;
    }
    false
}

/// A wildcard (method-unrestricted) auto-route endpoint infers read/write by the path's last segment (under ThinkPHP convention, the controller method name).
/// Hitting a known action root returns `Some(read/write)`, otherwise `None` (hand back to "unknown → read" fallback).
///
/// The last segment may carry prefixes/suffixes (`rechargeSubmit` / `getQRCodePath`), so split by non-alphanumeric + camelCase boundary and match word by word (case-insensitive).
fn infer_write_by_action(path: &str) -> Option<bool> {
    let action = path.rsplit('/').next()?;
    let write = [
        "save", "submit", "add", "create", "insert", "update", "edit", "delete", "remove", "set",
        "store", "upsert", "publish", "import",
    ];
    let read = [
        "get", "detail", "index", "list", "read", "info", "show", "query", "search", "select",
        "view", "find", "page", "export", "check",
    ];
    for token in action_words(action) {
        if write.iter().any(|w| token == *w) {
            return Some(true);
        }
        if read.iter().any(|w| token == *w) {
            return Some(false);
        }
    }
    None
}

/// Split a controller method name into lowercase words (by non-alphanumeric + camelCase boundary), e.g.
/// `rechargeSave` → `["recharge","save"]`，`getQRCodePath` → `["get","qrcode","path"]`。
fn action_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
        } else if c.is_uppercase() && prev_lower {
            // CamelCase boundary: previous word ends, a capital letter starts a new word
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            cur.push(c.to_ascii_lowercase());
            prev_lower = false;
        } else {
            cur.push(c.to_ascii_lowercase());
            prev_lower = c.is_lowercase();
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_verbs_drive_read_write() {
        assert!(is_write_http_method("POST /product"));
        assert!(is_write_http_method("DELETE /order"));
        assert!(!is_write_http_method("GET /products"));
        assert!(!is_write_http_method("HEAD /health"));
    }

    #[test]
    fn wildcard_falls_back_to_action_name() {
        // Auto-route (method-unrestricted): infer by controller method name last segment
        assert!(is_write_http_method("ANY /api/recharge/submit"));
        assert!(is_write_http_method("RULE /api/recharge/delete"));
        assert!(is_write_http_method("ANY /user/rechargeSave")); // 驼峰带前后缀
        assert!(!is_write_http_method("ANY /api/order/detail"));
        assert!(!is_write_http_method("ANY /goods/getQRCodePath"));
    }

    #[test]
    fn wildcard_unknown_action_stays_read() {
        // When no known action root is hit, fall back to "unknown → read", no misjudgment
        assert!(!is_write_http_method("ANY /api/recharge/config"));
    }
}
