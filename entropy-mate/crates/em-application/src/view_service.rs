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
    AggregateView, Candidate, Cluster, EdgeEvidence, EdgeView, GroupBy, HiddenInfo, MatrixView,
    NodeId, NodeLocations, NodeView, ObjectView, PerspectiveSpec, ProjectId, SourceLocation,
    UnresolvedInfo, ViewRegistry,
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
                    stats.by_kind.get(kind).copied().unwrap_or(0)
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
    pub fn candidates(&self, project_id: ProjectId, perspective: &str, limit: u32) -> Result<Vec<Candidate>> {
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
            name_contains: None,
            limit: Some(limit),
            offset: Some(0),
        })?;
        let mut out: Vec<Candidate> = nodes
            .into_iter()
            .map(|n| {
                let fan = self.store.edges_of(n.id, EdgeDirection::Incoming).map(|e| e.len()).unwrap_or(0);
                Candidate {
                    id: n.id,
                    name: n.name.clone(),
                    badge: Some(format!("入边 {fan}")),
                }
            })
            .collect();
        out.sort_by(|a, b| b.name.cmp(&a.name));
        Ok(out)
    }

    /// 对象类视角：以**一个**对象为中心的链路子图（环 = 跳数）。
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
        let depth = depth.unwrap_or(spec.depth).clamp(1, 4);

        // ---- BFS 分层（环 = 跳数）----
        let mut ring_of: HashMap<i64, u32> = HashMap::new();
        ring_of.insert(center_id.get(), 0);
        let mut queue: VecDeque<(NodeId, u32)> = VecDeque::new();
        queue.push_back((center_id, 0));
        while let Some((id, r)) = queue.pop_front() {
            if r >= depth {
                continue;
            }
            for e in self.store.edges_of(id, EdgeDirection::Both)? {
                let other = if e.from_id == id { e.to_id } else { e.from_id };
                if ring_of.contains_key(&other.get()) {
                    continue;
                }
                ring_of.insert(other.get(), r + 1);
                queue.push_back((other, r + 1));
            }
        }

        let mut rings: Vec<Vec<NodeId>> = vec![vec![]; depth as usize + 1];
        for (id, r) in &ring_of {
            if let Some(slot) = rings.get_mut(*r as usize) {
                slot.push(NodeId(*id));
            }
        }
        for slot in rings.iter_mut() {
            slot.sort_unstable();
        }

        // ---- 只保留"中心对象链路"上的边 ----
        let mut shown_edges: Vec<EdgeView> = Vec::new();
        let mut shown_keys: HashSet<(String, i64, i64)> = HashSet::new();
        let mut hidden_by_kind: BTreeMap<String, usize> = BTreeMap::new();
        let mut hidden_total = 0usize;

        let all_nodes: Vec<NodeId> = ring_of.keys().map(|k| NodeId(*k)).collect();
        for node in &all_nodes {
            for e in self.store.edges_of(*node, EdgeDirection::Both)? {
                let r_from = ring_of.get(&e.from_id.get()).copied();
                let r_to = ring_of.get(&e.to_id.get()).copied();
                let involves_center = e.from_id == center_id || e.to_id == center_id;
                // 只保留：中心相关，或沿 BFS 方向逐环递进的边
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

        // ---- 组装节点视图 ----
        let mut node_views: Vec<NodeView> = Vec::new();
        for (ring, slot) in rings.iter().enumerate() {
            for id in slot {
                if let Some(v) = self.build_node_view(*id, ring as u32)? {
                    node_views.push(v);
                }
            }
        }
        let center_view = self
            .build_node_view(center_id, 0)?
            .ok_or_else(|| DomainError::NotFound(format!("节点 {center_id}")))?;
        let ring_views: Vec<Vec<NodeView>> = rings
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
            note: format!(
                "当前视图只包含「{}」这一条链路：已画 {} 条边，另有 {} 条属于其它对象的链路边被刻意省略。\
                 切换上方的二级对象即可查看它们。",
                center_node.name,
                shown_edges.len(),
                hidden_total
            ),
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
            candidates: self.candidates(project_id, perspective, 300)?,
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
            to: e.to_id,
            resolved,
            confidence: e.confidence,
            hops,
        }
    }

    fn build_node_view(&self, id: NodeId, ring: u32) -> Result<Option<NodeView>> {
        let Some(n) = self.store.get_node(id)? else {
            return Ok(None);
        };
        let registry = self.views.registry();
        let has_own_view = registry.view_for_kind(n.kind.as_str()).is_some();
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
            name: n.name.clone(),
            fqn: n.fqn.clone(),
            ring,
            sub_project_id: n.sub_project_id,
            has_own_view,
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
