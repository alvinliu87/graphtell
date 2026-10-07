//! Persistence ports.
//!
//! Split into several fine-grained traits following the **interface segregation principle**, with one empty
//! composite trait [`Persistence`] providing a blanket impl so upper layers can take all capabilities at once.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::Result;
use crate::model::{
    AliasEntry, Annotation, Diagnostic, Edge, NewAnnotation, NewEdge, NewNode, NewProject,
    NewSourceFile, NewSubProject, Node, NodeId, NodeKind, Project, ProjectId, ProjectPatch,
    ProjectRuleConfig, ProjectStatus, SourceFile, SubProject, SubProjectId, SymbolEntry,
};
use crate::model::graph::NodeSummary;

/// One batch write of the graph.
///
/// Each pipeline phase accumulates changes in memory and persists them once at the end of the phase — this keeps
/// atomicity while letting the domain logic stay pure (no dependency on a transaction API).
#[derive(Debug, Clone, Default)]
pub struct GraphDelta {
    pub nodes: Vec<NewNode>,
    pub edges: Vec<NewEdge>,
    pub annotations: Vec<NewAnnotation>,
    pub aliases: Vec<AliasEntry>,
    pub symbols: Vec<SymbolEntry>,
    pub diagnostics: Vec<Diagnostic>,
    /// When `true`, clear this project's graph data first (for re-running the pipeline).
    pub reset_project: bool,
    pub project_id: Option<ProjectId>,
    /// Properties added to a synthetic node by later phases (id -> patch).
    pub property_patches: Vec<(crate::model::NodeId, Value)>,
    /// Appended "co-occurrence locations" of a synthetic node (id -> location).
    pub location_patches: Vec<(crate::model::NodeId, crate::model::SourceLocation)>,
    /// **Node-kind promotion** (id -> new kind).
    ///
    /// Used for "a syntax node recognised as having a semantic role is promoted to a semantic node" — the typical
    /// case is middleware: it is first built by P2 as a `Class`, and once P14 confirms it really is attached to a
    /// route it is promoted to `Middleware` (**only the kind changes, there is still exactly one node** — never create
    /// a second node with the same identity, otherwise the same piece of code appears twice on the graph, fan-in
    /// splits, and jump-to-source offers two locations).
    pub kind_patches: Vec<(crate::model::NodeId, crate::model::NodeKind)>,
}

impl GraphDelta {
    pub fn new(project_id: ProjectId) -> Self {
        Self { project_id: Some(project_id), ..Default::default() }
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.edges.is_empty()
            && self.annotations.is_empty()
            && self.aliases.is_empty()
            && self.symbols.is_empty()
            && self.diagnostics.is_empty()
            && self.property_patches.is_empty()
            && self.location_patches.is_empty()
            && self.kind_patches.is_empty()
            && !self.reset_project
    }
    pub fn merge(&mut self, mut other: GraphDelta) {
        self.property_patches.append(&mut other.property_patches);
        self.location_patches.append(&mut other.location_patches);
        self.kind_patches.append(&mut other.kind_patches);
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
        self.annotations.extend(other.annotations);
        self.aliases.extend(other.aliases);
        self.symbols.extend(other.symbols);
        self.diagnostics.extend(other.diagnostics);
        self.reset_project |= other.reset_project;
    }
}

/// The project-read port.
pub trait ProjectReader: Send + Sync {
    fn get_project(&self, id: ProjectId) -> Result<Option<Project>>;
    fn list_projects(&self) -> Result<Vec<Project>>;
    fn list_sub_projects(&self, project_id: ProjectId) -> Result<Vec<SubProject>>;
    fn list_files(&self, project_id: ProjectId, sub: Option<SubProjectId>) -> Result<Vec<SourceFile>>;
}

/// The project-write port.
pub trait ProjectWriter: Send + Sync {
    fn create_project(&self, new: NewProject) -> Result<Project>;
    fn update_project(&self, id: ProjectId, patch: ProjectPatch) -> Result<Project>;
    fn delete_project(&self, id: ProjectId) -> Result<()>;
    fn set_project_status(&self, id: ProjectId, status: ProjectStatus) -> Result<()>;
    fn replace_sub_projects(
        &self,
        project_id: ProjectId,
        subs: Vec<NewSubProject>,
    ) -> Result<Vec<SubProject>>;
    fn update_sub_project_facts(&self, id: SubProjectId, facts: Value) -> Result<()>;
    /// Back-fill the framework identifier detected by P3.
    fn set_sub_project_frameworks(&self, id: SubProjectId, frameworks: Vec<String>) -> Result<()>;
    fn replace_files(&self, project_id: ProjectId, files: Vec<NewSourceFile>)
        -> Result<Vec<SourceFile>>;
}

/// The graph-write port.
pub trait GraphSink: Send + Sync {
    /// Apply a batch of changes.
    ///
    /// Node ids are allocated by the pipeline in per-project segments (the design convention of
    /// `gt_pipeline::NODE_ID_STRIDE`), so no global coordination such as "the current maximum id" is needed here:
    /// concurrent graph builds cannot overwrite each other.
    fn apply(&self, delta: &GraphDelta) -> Result<()>;
}

/// Node filter conditions.
#[derive(Debug, Clone, Default)]
pub struct NodeFilter {
    pub project_id: ProjectId,
    pub kind: Option<NodeKind>,
    pub name_contains: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Graph statistics.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphStats {
    pub nodes: u64,
    pub edges: u64,
    pub annotations: u64,
    pub by_kind: std::collections::BTreeMap<String, u64>,
    /// Counted by `properties.category` (currently `category` is identical to `kind`).
    #[serde(default)]
    pub by_category: std::collections::BTreeMap<String, u64>,
}

/// The graph-query port.
pub trait GraphQuery: Send + Sync {
    fn query_nodes(&self, filter: &NodeFilter) -> Result<Vec<Node>>;
    fn get_node(&self, id: crate::model::NodeId) -> Result<Option<Node>>;
    /// Fetch nodes in batches (`id IN (...)`, chunked internally).
    ///
    /// The folded view has to inline the locations of every node on an edge's `via` chain at once, and one request can
    /// involve tens or hundreds of nodes — calling `get_node` one by one is an N+1 round trip that measured as nearly
    /// a third of the object-view time.
    fn get_nodes(&self, ids: &[crate::model::NodeId]) -> Result<HashMap<i64, Node>>;
    fn edges_of(
        &self,
        node: crate::model::NodeId,
        direction: EdgeDirection,
    ) -> Result<Vec<Edge>>;
    /// Summaries of every node in a project (id/kind/name/fqn/sub_project_id), so the view layer can preload in one
    /// shot instead of one `get_node` round trip per node during BFS. Returns `id -> summary` for O(1) lookups.
    fn nodes_summary(&self, project_id: ProjectId) -> Result<HashMap<i64, NodeSummary>>;
    /// Fetch "out-edges" in batches: returns `from_id -> edge list`, chunked internally by `from_id IN (...)`.
    fn edges_outgoing(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>>;
    /// Fetch "in-edges" in batches: returns `to_id -> edge list`, chunked internally by `to_id IN (...)`.
    fn edges_incoming(&self, ids: &[NodeId]) -> Result<HashMap<i64, Vec<Edge>>>;
    /// Chain-edge adjacency (only `from_id, to_id`, already filtered to call-chain edges), so candidate scoring BFS can
    /// run in memory. Returns `(outgoing: from_id -> [to_id], incoming: to_id -> [from_id], semantic_incoming:
    /// to_id -> [from_id])`, taking the whole project's chain edges in one go — avoiding per-node queries and the heavy
    /// transfer of a full `Edge` (which carries a `properties` JSON).
    ///
    /// The third map, `semantic_incoming`, contains **semantic edges only** (see
    /// [`crate::model::kinds::is_semantic_edge`]): the badge's "in-edges" must be counted on the semantic basis —
    /// `Calls` / `HasCallSite` among chain edges are syntactic call edges, and counting them into "N in-edges" would
    /// make the number agree neither with the canvas (which draws semantic edges only) nor with any business meaning.
    fn chain_adjacency(
        &self,
        project_id: ProjectId,
    ) -> Result<(
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
        HashMap<i64, Vec<i64>>,
    )>;
    /// Every **edge kind** that occurs in the project.
    ///
    /// The rule engine uses it to check whether a criterion can hold (see [`crate::model::RuleRequirements`]): when the
    /// graph has never had a `Triggers` edge, `no_incoming: Triggers` holds for every node and would report all event
    /// nodes as "never triggered". Confirm the edge kind really exists before running.
    fn edge_kinds(&self, project_id: ProjectId) -> Result<Vec<String>>;
    /// Every **node kind** that occurs in the project.
    ///
    /// Same reasoning as [`Self::edge_kinds`]: recall should take candidates from "the kinds that really exist on the
    /// graph" rather than a hard-coded list — otherwise types added by a new language adapter or a new pipeline phase
    /// are silently missed (not scored low, but never participating in recall at all).
    fn node_kinds(&self, project_id: ProjectId) -> Result<Vec<String>>;
    /// Every annotation `(channel, kind)` that occurs in the project.
    ///
    /// Same reasoning as [`Self::edge_kinds`], used to block vacuously-true false positives caused by "the annotation
    /// was never produced"; `channel` is included because `NoCapability` only looks at the `Capability` channel.
    fn annotation_kinds(&self, project_id: ProjectId) -> Result<Vec<(String, String)>>;
    fn annotations_of(&self, node: crate::model::NodeId) -> Result<Vec<Annotation>>;
    /// Fetch all annotations of a project in one batch (`node_id -> annotation list`).
    ///
    /// The rule engine has to evaluate `HasAnnotation` / `NoAnnotation` for thousands of nodes, and a per-node
    /// `annotations_of` is an N+1 round trip (measured as second-level jitter at the ten-thousand-node scale).
    /// After one preload, rule evaluation runs entirely in memory.
    fn annotations_of_project(
        &self,
        project_id: ProjectId,
    ) -> Result<HashMap<i64, Vec<Annotation>>>;
    /// Fetch all source file paths in a project in one batch (`file_id -> path`).
    fn file_paths(&self, project_id: ProjectId) -> Result<HashMap<i64, String>>;
    fn stats(&self, project_id: ProjectId) -> Result<GraphStats>;
    /// Count nodes by "kind + which parties have evidence on them" (lighter than `query_nodes`, a `COUNT` only).
    /// The view layer uses it to compute candidate counts for perspectives with a side filter (such as the
    /// front-end local-storage / back-end cache split).
    ///
    /// `side` is **scalar**: a node is owned by exactly one party, so it counts when its `side` property equals
    /// the requested party. `None` means no restriction on the side; `kind` = `None` means none on the kind.
    fn count_nodes(
        &self,
        project_id: ProjectId,
        kind: Option<&NodeKind>,
        side: Option<&str>,
    ) -> Result<u64>;
    /// Fetch an edge by primary key (for the "edge evidence chain" query).
    fn find_edge(&self, id: crate::model::EdgeId) -> Result<Option<Edge>>;
    /// Fetch a file path (for jump-to-source).
    fn file_path(&self, id: crate::model::FileId) -> Result<Option<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    Outgoing,
    Incoming,
    Both,
}

/// The authoritative symbol-table read port.
pub trait SymbolTableReader: Send + Sync {
    fn get_symbol(&self, project_id: ProjectId, table: &str, key: &str) -> Result<Option<Value>>;
    fn list_symbols(&self, project_id: ProjectId, table: &str) -> Result<Vec<SymbolEntry>>;
}

/// The diagnostics persistence port.
pub trait DiagnosticSink: Send + Sync {
    fn push_diagnostics(&self, items: &[Diagnostic]) -> Result<()>;
    fn list_diagnostics(
        &self,
        project_id: ProjectId,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
    /// List diagnostics, **excluding** a given code prefix.
    ///
    /// Must be used together with [`DiagnosticSink::count_diagnostics_excluding`]: the page badge counts "excluding
    /// rule:" while the list takes "everything", so the user would see a badge saying 0 errors next to a table full of
    /// something else — two inconsistent definitions destroy trust more than any single error does.
    fn list_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
    /// List diagnostics by code prefix (e.g. `rule:` for all rule violations).
    ///
    /// When `sub_project_id` is `Some(ids)` only diagnostics belonging to those sub-projects are returned, and
    /// **shared diagnostics (`sub_project_id IS NULL`, e.g. cross-sub-project tables / queues) are always kept** —
    /// consistent with the graph view's "shared nodes show under any filter" semantics. `None` means no restriction.
    fn list_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
        limit: u32,
    ) -> Result<Vec<Diagnostic>>;
    /// Clear diagnostics by code prefix, returning the number deleted.
    ///
    /// Rules can run repeatedly; without clearing the previous round's `rule:*` violations the diagnostics table grows
    /// without bound and the user sees "historical conclusions" rather than conclusions about the current code.
    fn clear_diagnostics(&self, project_id: ProjectId, code_prefix: &str) -> Result<u64>;
    /// Aggregate counts by code prefix + severity, returning a `(severity_snake_case, count)` list.
    ///
    /// For lightweight summaries such as the menu badge: no full violation fetch, only grouped counts.
    fn count_diagnostics_by_code(
        &self,
        project_id: ProjectId,
        code_prefix: &str,
        sub_project_id: Option<&[SubProjectId]>,
    ) -> Result<Vec<(String, u64)>>;
    /// Aggregate counts by severity while **excluding** a code prefix, returning a `(severity_snake_case, count)` list.
    ///
    /// The diagnostics page shows "non-rule" diagnostics (missing roots, broken links, identity conflicts, …); rule
    /// violations already have their own compliance-check home, so `rule:` is excluded here to avoid double counting.
    fn count_diagnostics_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, u64)>>;
    /// Aggregate counts by `(code, severity_snake_case)` while **excluding** a code prefix, returning a
    /// `(code, severity, count)` list.
    ///
    /// **Same definition** as [`Self::count_diagnostics_excluding`] (both exclude `rule:`), just at a finer granularity.
    /// Why it must go down to `code`: one engine diagnostic can fire across hundreds of files (`IdentityUnresolved`
    /// alone has 349 entries on sample_project), and reporting "349 entries" carries no information — those are 349 **facts of
    /// the same type**. Reporting "1 type · 349 places" is what tells you which piece of the graph is missing and
    /// whether it matters.
    fn count_diagnostics_by_code_excluding(
        &self,
        project_id: ProjectId,
        exclude_prefix: &str,
    ) -> Result<Vec<(String, String, u64)>>;
}

/// The project-level rule config port (per-project override of a rule's enabled state and parameters).
pub trait RuleConfigStore: Send + Sync {
    /// Take every rule config override of a project (key = rule_id).
    fn get_rule_configs(
        &self,
        project_id: ProjectId,
    ) -> Result<std::collections::HashMap<String, ProjectRuleConfig>>;
    /// Write (or clear) one rule's config. An empty override (enabled=None and options empty) means delete the row.
    fn set_rule_config(&self, cfg: &ProjectRuleConfig) -> Result<()>;
    /// Delete one rule's config for a project (back to inheriting the default).
    fn delete_rule_config(&self, project_id: ProjectId, rule_id: &str) -> Result<()>;
}

/// Composite port: take every persistence capability at once.
pub trait Persistence:
    ProjectReader
    + ProjectWriter
    + GraphSink
    + GraphQuery
    + SymbolTableReader
    + DiagnosticSink
    + RuleConfigStore
    + Send
    + Sync
{
}

impl<T> Persistence for T where
    T: ProjectReader
        + ProjectWriter
        + GraphSink
        + GraphQuery
        + SymbolTableReader
        + DiagnosticSink
        + RuleConfigStore
        + Send
        + Sync,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `is_empty` must be false whenever `reset_project` is set (a re-run clears the graph even with no new
    /// nodes), and false for any populated member; `new()` only stamps the project id and stays empty.
    #[test]
    fn graph_delta_is_empty_reflects_reset_flag_and_contents() {
        assert!(GraphDelta::default().is_empty(), "all-empty must be empty");

        let mut reset = GraphDelta::default();
        reset.reset_project = true;
        assert!(!reset.is_empty(), "reset_project=true is non-empty even with no nodes");

        let mut with_patch = GraphDelta::default();
        with_patch.kind_patches.push((NodeId::new(1), NodeKind::new("Class")));
        assert!(!with_patch.is_empty(), "if any member is non-empty the whole thing is non-empty");

        assert!(GraphDelta::new(ProjectId::new(1)).is_empty(), "new() only sets project_id");
    }

    /// `merge` appends the per-node patches and ORs `reset_project` into the target.
    #[test]
    fn graph_delta_merge_concatenates_and_ors_reset() {
        let mut a = GraphDelta::default();
        a.kind_patches.push((NodeId::new(1), NodeKind::new("Class")));
        a.property_patches.push((NodeId::new(1), json!({ "x": 1 })));

        let mut b = GraphDelta::default();
        b.kind_patches.push((NodeId::new(2), NodeKind::new("Middleware")));
        b.reset_project = true;

        a.merge(b);
        assert_eq!(a.kind_patches.len(), 2, "patches must be appended and merged");
        assert!(a.reset_project, "reset_project must be OR-ed into the target delta");
        assert!(!a.is_empty());
    }

    /// A reset already set on the target must survive merging a non-reset delta.
    #[test]
    fn graph_delta_merge_keeps_an_existing_reset() {
        let mut a = GraphDelta::default();
        a.reset_project = true;
        a.kind_patches.push((NodeId::new(1), NodeKind::new("Class")));

        let b = GraphDelta::default(); // reset_project = false
        a.merge(b);
        assert!(a.reset_project, "an already-set reset_project must not be overwritten by false");
    }
}
