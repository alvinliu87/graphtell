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
/// Why it's a separate struct: a dozen cross-referencing HashMaps inside one function body can only dodge the
/// borrow checker via `{ }` scopes and a `pending` staging buffer — exactly why that near-thousand-line logic
/// can't be split or read. Extracted:
/// * `discover()` builds the state once and returns it;
/// * later stages immutably borrow `&Discovery`; closures like `chain_to` / `via_from_ids` /
///   `push_edge` never fight "fill in kind / name" writes (NLL handles it).
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
                    // Perspectives with a side filter: count precisely by `kind + sides` (frontend/backend
                    // cache split, or the set-semantics `side_any` form).
                    let accepted = spec.accepted_sides();
                    if !accepted.is_empty() {
                        let sides: Vec<String> =
                            accepted.iter().map(|s| s.to_string()).collect();
                        self.store
                            .count_nodes(
                                project_id,
                                Some(&gt_domain::model::NodeKind(kind.clone())),
                                &sides,
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

        // Filter by side: split nodes that share a kind but differ in side (e.g. frontend/backend cache) into
        // their own perspectives. Nodes with no side evidence, or with none of the accepted parties, are dropped;
        // a perspective declaring neither `side` nor `side_any` applies no filtering.
        if !spec.accepted_sides().is_empty() {
            nodes.retain(|n| spec.matches_sides(&n.properties));
        }

        // With a search term: match by name directly, skip full scoring (dropdown searches on demand, optimize for speed).
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

    /// Core scoring for candidate objects: rank the nodes returned by `query_nodes` by "semantic value" and truncate.
    ///
    /// `summary` / `out` / `inc` / `sem_inc` come from `nodes_summary` + `chain_adjacency`,
    /// **the caller loads them once and reuses** -- `object_view` and `candidates` share the same copy,
    /// avoiding reloading the whole-graph summary + chain adjacency on every request (cost grows with project size).
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

    /// A candidate object's "value": the number of semantic nodes (table / config / cache / event …) reachable via its call chain (chain edges).
    /// Higher value means richer business dependencies, making it a better default object to open.
    /// All done in memory: `summary` provides node semantics, `out`/`inc` are integer chain-edge adjacency, no DB lookups.
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
        // Consistent with the view: also treat the center's inbound chain neighbors as ring 1 (needed for table / external-system perspectives).
        // `inc` already holds only chain edges, no need to re-check `is_chain_edge`.
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
            // `out` already holds only chain edges, just iterate the targets.
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

    /// Object perspective: a link subgraph centered on **one** object (ring = hop count).
    ///
    /// Always a **collapsed** view: only semantic nodes (Table / HttpContract / ConfigKey / I18nKey / Event /
    /// Queue / Cache / Topic) and the semantic edges between them; syntactic nodes (Method / CallSite / Class …)
    /// are folded into the edge's `via` chain, with each hop's call site inlined -- click an edge to verify hop by hop.
    ///
    /// An `expand` param to unfold all syntactic nodes is deliberately absent: that branch does no lifting, generates no `via`,
    /// and inlines no call sites -- **more nodes drawn but less evidence**, and it blows the graph into multi-layer single rows needing several horizontal scrolls.
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
        // Third-party / library sub-project nodes (role == "library") are terminals, not expanded outward.
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
        // `inc` / `sem_inc` only serve candidate scoring, so they are not needed here (see the `ObjectView` note).
        let (out, _inc, _sem_inc) = self.store.chain_adjacency(project_id)?;

        // Semantic-ness is computed on demand from kind, avoiding repeated DB lookups for "is this a semantic node".
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
        // Visible nodes: "semantic nodes / force-visible nodes" within the ring. The lifting phase may register out-of-ring semantic targets into the ring,
        // so compute this after lifting.
        let visible_nodes: Vec<i64> = d
            .ring_of
            .keys()
            .copied()
            .filter(|&id| {
                d.semantic_of.get(&id).copied().unwrap_or(false) || d.force_visible.contains(&id)
            })
            .collect();
        // ---- keep only visible nodes truly connected to the center (drop orphan leaves, preserve "single-chain" honesty) ----
        let mut touched: HashSet<i64> = HashSet::new();
        touched.insert(center_id.get());
        for e in &shown_edges {
            touched.insert(e.from.get());
            touched.insert(e.to.get());
        }

        // ---- assemble the ring and node view of visible nodes ----
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
                                    // Same-family coloring + `category` sync (frontend decides side / click-to-switch perspective by `category`).
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

        // Orphan accesses are booked into the "folded entries" account: they are also accessors saved off the canvas,
        // just carrying contact-point locations too, verifiable one by one on the frontend (see `ObjectView.orphans`).
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

    /// Discovery phase: layered BFS along the **call chain** from the center, producing all state needed for the collapsed view.
    ///
    /// See `Discovery`'s docs: state freezes after this phase, later phases are read-only.
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
        // Edge cache: shared by BFS and later "lift / keep-edge-as-is" to avoid repeated `edges_of` round-trips on the same node.
        let mut out_edges: HashMap<i64, Vec<gt_domain::model::Edge>> = HashMap::new();
        let mut in_edges: HashMap<i64, Vec<gt_domain::model::Edge>> = HashMap::new();

        let force_visible: HashSet<i64> = HashSet::new();
        // Reverse mode: the **semantic edge kind** encountered traveling from center to this node (i.e. "how the resource is accessed").
        let mut path_kind: HashMap<i64, String> = HashMap::new();
        // Parallel to `path_kind`: whether the semantic edge encountered on the way is an indirectly-propagated edge.
        // The resource perspective ("who is reading this config") must also distinguish direct readers from "upstream-read" indirect entries.
        let mut path_indirect: HashMap<i64, bool> = HashMap::new();
        let mut path_from_center: HashSet<i64> = HashSet::new();
        // The **edge id** a node won with when competing in its layer: used for the deterministic tie-break "smaller edge id wins".
        let mut best_edge_id_of: HashMap<i64, i64> = HashMap::new();
        kind_of.insert(center_id.get(), center_node.kind.to_string());
        name_of.insert(center_id.get(), center_node.name.clone());
        semantic_of.insert(center_id.get(), node_is_semantic(&center_node));
        ring_of.insert(center_id.get(), 0);

        // Fetch edges per layer in batches (chunked `IN` queries), write into the edge cache.
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

        // Prefetch the center node's bidirectional edges (used by Event seeding / forward seeding / layer-0 expansion).
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
                // The center's **out-edges** (e.g. `event --HandledBy--> listener`): must not be reversed when drawing.
                if center_side {
                    path_from_center.insert(node);
                }
                if !ring_of.contains_key(&node) {
                    ring_of.insert(node, 1);
                    parent_of.insert(node, center_id.get());
                    semantic_of.insert(node, is_consumer);
                    // Key: also backfill kind / name. Otherwise the BFS-final "backfill" step would, due to
                    // missing `kind_of`, recompute `semantic_of` from kind, overwriting our promotion back to false.
                    if let Some(m) = summary.get(&node) {
                        kind_of.insert(node, m.kind.clone());
                        name_of.insert(node, m.name.clone());
                    }
                }
            }
        }

        // Forward mode: also treat the center's inbound chain neighbors as ring 1 (needed for table / external-system perspectives).
        // These ring-1 nodes expand together with the center's children in layer 1.
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

        // Layered BFS: fetch edges per layer in one batch, compressing the original "one `edges_of` round-trip per node" into a few chunked queries;
        // semantic nodes are terminals (collapsed mode does not penetrate outward), third-party library sub-project nodes are not expanded either.
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
                // Semantic nodes are terminals, no further outward penetration
                if id != center_id.get() && sem_of(id) {
                    continue;
                }
                // Collapsed mode: also bring a method's "declaring class" into the chain, surfacing **class-level semantic edges** (Dao→Model, Model→Table).
                // Safety: `Declares` is not in the chain-edge set, so starting from the class node won't re-expand its pile of methods.
                if k == "Method" {
                    if let Some(inc) = in_edges.get(&id) {
                        for e in inc {
                            if e.kind.as_str() != "Declares"
                                || ring_of.contains_key(&e.from_id.get())
                            {
                                continue;
                            }
                            // Keep the semantic edge kind along the path, avoid later being overwritten by edges like `HandledBy`
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
                // The collapsed view discovers unidirectionally along the call chain: forward (route) drills down via out-edges, reverse (resource) backtracks via in-edges.
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
                            // Path already fixed at a nearer layer (or anomaly is farther), skip.
                            continue;
                        }
                        // r == ring + 1: re-seen at this layer, decide promotion by comparator.
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
            // Merge the "forward-seeded ring 1" into layer 1 for expansion.
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

        // Backfill kind / semantics for all reachable nodes (needed by the edge-display phase) -- read directly from the preloaded node summary,
        // no per-node `get_node` round-trips.
        for &id in ring_of.keys() {
            if !kind_of.contains_key(&id) {
                if let Some(m) = summary.get(&id) {
                    kind_of.insert(id, m.kind.clone());
                    name_of.insert(id, m.name.clone());
                    semantic_of.insert(id, NodeKind(m.kind.clone()).is_semantic());
                }
            }
        }

        // Backfill the in-ring nodes' edge cache (the original logic did one more `edges_of` per in-ring node during lift / keep-edge-as-is;
        // here we backfill in one batch -- equivalent behavior but compressing N round-trips into a few chunked queries).
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

    /// Inline the link nodes' locations.
    ///
    /// The collapsed view's "chain" is a **temporary lift** result; middle hops exist only in this response, and re-querying by edge id
    /// can't fetch them -- here we query once and inline into `EdgeView` in chain order (start → each hop → end),
    /// saving the frontend N+1 round-trips of requesting `/nodes/{id}/locations` per node,
    /// and these locations share the same source as "this edge".
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
            // Prefer: seeds that can find a "same-kind direct edge with evidence" from the out-edge cache (almost all within the ring hit).
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
            // kind / short name come from this view's already-preloaded `summary`; don't `get_node` for each again.
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
            // The same target is asked repeatedly by multiple shown edges (parallel paths); cache in-edges, don't query DB each time.
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
                // Prefer the BFS-stage pre-fetched in-edge cache; for cache misses (the few un-pre-fetched nodes)
                // fall back to a point query. Avoid one DB round-trip per shown edge (N+1).
                let cached_ins = d.in_edges.get(&e.to.get()).cloned();
                let store_ins = if cached_ins.is_some() {
                    None
                } else {
                    // The same target is asked repeatedly by multiple shown edges (parallel paths); cache it, don't query DB each time.
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
                // ---- "read+write": record the other access mode suppressed by `action_strength` back onto the edge ----
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

    /// Collapsed lift + semantic-edge collection: lift the semantic edges hanging on syntactic nodes (Method / CallSite) up to
    /// the "semantic node that initiated them", and record the folded-away middle hops into `via`.
    ///
    /// See `Discovery`'s docs: this method takes `&mut Discovery` (the lift phase registers out-of-ring semantic
    /// targets into the ring), while the closure borrows immutably overall, relying on NLL to release the borrow after its last use.
    #[allow(clippy::too_many_arguments)]
    fn fold_and_collect(
        &self,
        d: &mut Discovery,
        center_id: NodeId,
        _center_node: &gt_domain::model::Node,
        summary: &HashMap<i64, NodeSummary>,
        out: &HashMap<i64, Vec<i64>>,
    ) -> Result<FoldResult> {
        // `sem_of` depends only on `summary`; rebuild it inside this method (no need to pass the closure across function boundaries).
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

        // Semantic-edge priority (for edge lifting on the collapsed path): see `Self::agg_rank`.
        let mut shown_edges: Vec<EdgeView> = Vec::new();
        let mut shown_keys: HashSet<(String, i64, i64, Vec<i64>)> = HashSet::new();
        let mut hidden_by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut hidden_total = 0usize;
        // Direct access with no semantic entry (orphan): not on canvas, only accounted (see the reverse branch's collection below).
        let mut orphans: Vec<OrphanAccess> = Vec::new();

        // Collect "folded-away middle nodes" along the BFS parent chain, in order **ancestor → target** (so the frontend can string them into a chain directly).
        // `child` is the node actually holding the semantic edge, `ancestor` is the lifted semantic node; return empty when they are adjacent.
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
        // `indirect`: whether this semantic edge was propagated along the call chain by P8 (the start node itself did not perform the action).
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
            // Add the "call site" to each hop: `prev` (start / previous hop) → this hop's CallSite location.
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
                // Filled uniformly by the "inline link locations" step (see end of `object_view`).
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
                                        // Even if not found (truly no in-edges), record an empty placeholder to avoid the same node
                                        // being queried repeatedly in later layers.
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
        // Filter by side (frontend/backend cache split, or the "any of these parties" form).
        if !spec.accepted_sides().is_empty() {
            nodes.retain(|n| spec.matches_sides(&n.properties));
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
        // `indirect` (P8 propagation along the call chain) only decides dashed styling and the "indirect" label; it does not down-weight as "to-verify".
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
        let mut callers: Vec<i64> = Vec::new(); // Nearest first: [direct caller, …, farthest]
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
                        break; // Reached a semantic entry (route, etc.); the chain caps here
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
            v.push(producer); // Contact point as the last `via` hop
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
/// `Event` / `Queue` / `Cache` / `Topic` are concrete kinds, not the `category` umbrella.
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
        return !new_indirect; // Newer and more direct → better
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
    use gt_domain::model::{Edge, EdgeId, EdgeKind, Phase};

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
        assert!(is_write_http_method("ANY /user/rechargeSave")); // CamelCase with prefix/suffix
        assert!(!is_write_http_method("ANY /api/order/detail"));
        assert!(!is_write_http_method("ANY /goods/getQRCodePath"));
    }

    #[test]
    fn wildcard_unknown_action_stays_read() {
        // When no known action root is hit, fall back to "unknown → read", no misjudgment
        assert!(!is_write_http_method("ANY /api/recharge/config"));
    }

    // ---- access-mode classification: the priority ladder that decides folded-view edge labels ----

    #[test]
    fn access_rank_priority_ladder() {
        // read/write action outranks other semantic access, which outranks MapsTo, which outranks structural edges
        assert_eq!(access_rank("WritesDb"), 3);
        assert_eq!(access_rank("ReadsDb"), 3);
        assert_eq!(access_rank("ReadsCache"), 3);
        assert_eq!(access_rank("ReadsConfig"), 2);
        assert_eq!(access_rank("Triggers"), 2);
        assert_eq!(access_rank("CallsHttp"), 2);
        assert_eq!(access_rank("MapsTo"), 1);
        assert_eq!(access_rank("Calls"), 0, "结构边 Calls 不应带访问模式");
        assert_eq!(access_rank("HandledBy"), 0);
        assert_eq!(access_rank("Whatever"), 0, "未知 kind 退化为 0");
    }

    #[test]
    fn action_strength_write_beats_read() {
        assert_eq!(action_strength("WritesDb"), 2);
        assert_eq!(action_strength("WritesCache"), 2);
        assert_eq!(action_strength("ReadsDb"), 1);
        assert_eq!(action_strength("ReadsCache"), 1);
        assert_eq!(action_strength("MapsTo"), 0, "非读写动作无强弱");
    }

    #[test]
    fn counterpart_kinds_pairs_read_and_write() {
        let db = counterpart_kinds("WritesDb");
        assert_eq!(db.len(), 2);
        assert_eq!(db[0], "WritesDb");
        assert_eq!(db[1], "ReadsDb");
        let cache = counterpart_kinds("ReadsCache");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[0], "WritesCache");
        assert_eq!(cache[1], "ReadsCache");
        assert!(
            counterpart_kinds("MapsTo").is_empty(),
            "结构边无配对（被抑制的读写须记入 also_kinds）"
        );
    }

    // ---- better_path: the deterministic edge winner (direct > rank > strength > smaller eid) ----

    #[test]
    fn better_path_direct_beats_indirect() {
        assert!(
            better_path(false, "ReadsDb", 5, true, "ReadsDb", 1),
            "直接边优于传播边（同 kind）"
        );
        assert!(
            !better_path(true, "ReadsDb", 5, false, "ReadsDb", 1),
            "间接边不应胜过直接边"
        );
    }

    #[test]
    fn better_path_higher_rank_wins() {
        // WritesDb(rank3) over ReadsConfig(rank2) even with the larger edge id
        assert!(better_path(false, "WritesDb", 99, false, "ReadsConfig", 1));
    }

    #[test]
    fn better_path_stronger_action_wins_within_rank() {
        assert!(better_path(false, "WritesDb", 99, false, "ReadsDb", 1), "同 rank：写优于读");
        assert!(!better_path(false, "ReadsDb", 1, false, "WritesDb", 99));
    }

    #[test]
    fn better_path_smaller_eid_is_deterministic_tiebreak() {
        // identical (direct, rank, strength): smaller edge id wins → result independent of traversal order
        assert!(better_path(false, "ReadsDb", 3, false, "ReadsDb", 7));
        assert!(!better_path(false, "ReadsDb", 7, false, "ReadsDb", 3));
    }

    // ---- enumerate_chain_paths: DFS simple paths with depth / limit / avoid ----

    #[test]
    fn enumerate_chain_paths_finds_simple_chain() {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        out.insert(1, vec![2]);
        out.insert(2, vec![3]);
        out.insert(3, vec![]);
        let paths = enumerate_chain_paths(&out, 1, 3, 10, 5, &|_| false);
        assert_eq!(paths, vec![vec![1, 2, 3]]);
    }

    #[test]
    fn enumerate_chain_paths_avoid_blocks_intermediate() {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        out.insert(1, vec![2]);
        out.insert(2, vec![3]);
        let paths = enumerate_chain_paths(&out, 1, 3, 10, 5, &|n| n == 2);
        assert!(paths.is_empty(), "中间语义节点被 avoid 时应无路径");
    }

    #[test]
    fn enumerate_chain_paths_no_path_when_unreachable_or_same() {
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        out.insert(1, vec![2]);
        out.insert(2, vec![3]);
        assert!(enumerate_chain_paths(&out, 3, 1, 10, 5, &|_| false).is_empty(), "反向不可达");
        assert!(enumerate_chain_paths(&out, 1, 1, 10, 5, &|_| false).is_empty(), "from==to 无路径");
    }

    #[test]
    fn enumerate_chain_paths_respects_limit_and_hops() {
        // diamond: 1->2->3 and 1->4->3
        let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
        out.insert(1, vec![2, 4]);
        out.insert(2, vec![3]);
        out.insert(4, vec![3]);
        let one = enumerate_chain_paths(&out, 1, 3, 10, 1, &|_| false);
        assert_eq!(one.len(), 1, "limit=1 只返回一条");
        let shallow = enumerate_chain_paths(&out, 1, 3, 1, 5, &|_| false);
        assert!(shallow.is_empty(), "max_hops=1 不足以到达 2 跳外");
        let all = enumerate_chain_paths(&out, 1, 3, 10, 5, &|_| false);
        assert_eq!(all.len(), 2, "两条路径都应枚举到");
    }

    // ---- propagation vs direct edge distinction ----

    fn edge_with_props(props: Value) -> Edge {
        Edge {
            id: EdgeId::new(0),
            project_id: ProjectId::new(1),
            kind: EdgeKind::new("ReadsConfig"),
            from_id: NodeId::new(1),
            to_id: NodeId::new(2),
            phase: Phase::new("Synthesize"),
            confidence: 1.0,
            properties: props,
        }
    }

    #[test]
    fn is_indirect_edge_detects_propagation_and_indirect_flag() {
        assert!(is_indirect_edge(&edge_with_props(json!({ "indirect": true }))));
        assert!(is_indirect_edge(&edge_with_props(json!({ "via": "propagate" }))));
        assert!(!is_indirect_edge(&edge_with_props(json!({ "evidence": "x" }))));
        assert!(!is_indirect_edge(&edge_with_props(Value::Null)));
    }

    #[test]
    fn seed_source_of_reads_propagation_touch_point() {
        assert_eq!(seed_source_of(&edge_with_props(json!({ "seed_source": 42 }))), Some(42));
        assert_eq!(seed_source_of(&edge_with_props(json!({ "other": 1 }))), None);
    }

    // ---- file/line parsing + HTTP write/read inference ----

    #[test]
    fn split_file_line_parses_line_number() {
        assert_eq!(split_file_line("a/b.go:123"), ("a/b.go".to_string(), 123));
        assert_eq!(split_file_line("a/b.go"), ("a/b.go".to_string(), 0));
        assert_eq!(split_file_line("x:notanum"), ("x".to_string(), 0));
    }

    #[test]
    fn infer_write_by_action_matches_known_roots() {
        assert_eq!(infer_write_by_action("submit"), Some(true));
        assert_eq!(infer_write_by_action("rechargeSave"), Some(true));
        assert_eq!(infer_write_by_action("detail"), Some(false));
        assert_eq!(infer_write_by_action("getQRCodePath"), Some(false));
        assert_eq!(
            infer_write_by_action("weirdName"),
            None,
            "未知动作根 → 交回 '读' 兜底"
        );
    }

    #[test]
    fn action_words_splits_camelcase_and_separators() {
        assert_eq!(
            action_words("rechargeSave"),
            vec!["recharge".to_string(), "save".to_string()]
        );
        assert_eq!(
            action_words("getQRCodePath"),
            vec!["get".to_string(), "qrcode".to_string(), "path".to_string()]
        );
        assert_eq!(
            action_words("order/detail"),
            vec!["order".to_string(), "detail".to_string()]
        );
    }
}
