//! View models: the perspective registry and the response structures of each view kind.
//!
//! # Why perspectives are declared by configuration
//!
//! "route perspective / table perspective / domain aggregate" are **analysis paradigms**, not concepts inherent to
//! the backend. Hard-coding them as a Rust enum would make every new perspective a code change; here one YAML file
//! declares them and the backend only slices according to the declaration, so a new perspective = one more block of
//! config (open-closed principle).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{NodeId, ProjectId, SubProjectId};

/// The perspective registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewRegistry {
    /// Options of the top-level (level one) filter.
    pub perspectives: Vec<PerspectiveSpec>,
    /// Node kind -> perspective id. Clicking a node switches the level-one filter accordingly.
    ///
    /// **Kinds not listed here do not switch the top filter**, they only open the Inspector on the right —
    /// for example asset / structural nodes like `ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`
    /// carry no "single link" semantics.
    pub node_views: BTreeMap<String, String>,
}

impl ViewRegistry {
    pub fn by_id(&self, id: &str) -> Option<&PerspectiveSpec> {
        self.perspectives.iter().find(|p| p.id == id)
    }

    /// The perspective for a node kind (used for "click to switch").
    pub fn view_for_kind(&self, kind: &str) -> Option<&PerspectiveSpec> {
        self.node_views.get(kind).and_then(|id| self.by_id(id))
    }

    /// Perspective resolution with a "side": prefer a dedicated perspective where `node_kind == kind` and `side`
    /// matches (e.g. backend `Cache -> cache`, frontend `Cache -> local_storage`), otherwise fall back to
    /// `view_for_kind`.
    ///
    /// When one `kind` (e.g. `Cache`) is split into a frontend and a backend perspective, clicking a node must
    /// switch to the correct one by its `side` rather than being locked into the single `node_views` mapping.
    pub fn view_for_kind_and_side(
        &self,
        kind: &str,
        side: Option<&str>,
    ) -> Option<&PerspectiveSpec> {
        if let Some(side) = side {
            if let Some(p) = self
                .perspectives
                .iter()
                .find(|p| p.node_kind.as_deref() == Some(kind) && p.side.as_deref() == Some(side))
            {
                return Some(p);
            }
        }
        self.view_for_kind(kind)
    }
}

/// A perspective declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PerspectiveSpec {
    pub id: String,
    pub label: String,
    /// Object-style (single link) or aggregate-style (overview).
    pub mode: ViewMode,
    /// The node kind an object-style perspective covers.
    pub node_kind: Option<String>,
    /// Only include nodes of a given "side": `frontend` / `backend` (filtered on the node property `side`).
    /// Used to split nodes that "share a kind but differ in side" into different perspectives — e.g. both the
    /// backend `Cache` and the frontend `uni.setStorageSync` synthesise a `Cache` node, and `side` splits them
    /// into "cache perspective / local-storage perspective".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// Accept a node as soon as **any** of these parties has evidence on it (set semantics), instead of
    /// requiring the node's scalar `side` to equal one value.
    ///
    /// This is the honest way to filter nodes several parties may write: a contract bridge carries both sides'
    /// evidence, so its derived `side` reads `bridge` and it matches **neither** `side: frontend` nor
    /// `side: backend`. Writing the intent as `side_any: [backend]` — "the backend participates" — keeps those
    /// nodes in view. Nodes written by an older pipeline (scalar `side` only, no derived `sides` set) are still
    /// matched through that scalar, so enabling a `side_any` filter never forces a rebuild.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_any: Option<Vec<String>>,
    /// Default layout algorithm.
    pub layout: LayoutMode,
    /// Grouping dimension of an aggregate perspective.
    pub group_by: Option<GroupBy>,
    /// Row dimension of a matrix perspective.
    pub row_from: Option<GroupBy>,
    /// Column dimension of a matrix perspective.
    pub col_from: Option<GroupBy>,
    /// Hop count when expanding a link.
    pub depth: u32,
    pub description: Option<String>,
    /// Syntax node kinds that an object perspective folds (passes through) by default; after folding only semantic nodes and the dependency edges between them are shown.
    #[serde(default)]
    pub collapsed_kinds: Vec<String>,
}

impl PerspectiveSpec {
    /// The sides this perspective accepts, `side` (one required party) and `side_any` (any one suffices)
    /// combined. Empty ⇒ no side filtering at all.
    ///
    /// Kept in one place so the three filtering sites (availability count / object candidates / aggregate
    /// candidates) cannot drift apart, and so the SQL counting path and the in-memory one agree.
    pub fn accepted_sides(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        if let Some(s) = &self.side {
            out.push(s.as_str());
        }
        if let Some(list) = &self.side_any {
            out.extend(list.iter().map(|s| s.as_str()));
        }
        out
    }

    /// Whether a node qualifies for this perspective's side filter, given its persisted properties.
    ///
    /// `#` `#` A node qualifies when **any** accepted party has evidence: either it is present in the derived
    /// set `sides` (written by the current pipeline), or — for rows written before `sides` existed — the
    /// scalar `side` equals it. Nodes with no side evidence at all never qualify, same as before.
    pub fn matches_sides(&self, props: &Value) -> bool {
        let accepted = self.accepted_sides();
        if accepted.is_empty() {
            return true;
        }
        let sides: Vec<String> = props
            .get("sides")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if sides.iter().any(|s| accepted.contains(&s.as_str())) {
            return true;
        }
        props
            .get("side")
            .and_then(|v| v.as_str())
            .map(|s| accepted.contains(&s))
            .unwrap_or(false)
    }
}

impl Default for PerspectiveSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            mode: ViewMode::Object,
            node_kind: None,
            side: None,
            side_any: None,
            layout: LayoutMode::Radial,
            group_by: None,
            row_from: None,
            col_from: None,
            depth: 2,
            description: None,
            collapsed_kinds: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    /// Object-style: render the link subgraph of **only this one object**.
    Object,
    /// Aggregate-style: not a single link but an aggregate overview (cluster boxes / matrix).
    Aggregate,
}

/// Layout algorithm. **No node may drift freely under a force-directed layout**: positions come from the algorithm and are independent of interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutMode {
    /// Radial: default for resource perspectives (Table / Cache / Event / Queue / Topic).
    /// The frontend dispatches a second time by **graph shape**: these perspectives run in reverse mode where every
    /// user is synthesised into an edge to the centre (see `MAX_USERS` in `view_service.rs`), which is really a star
    /// — with many users it switches to a hub-and-spoke layout (the canvas grows with the square of the user count
    /// and outer-ring edges would cover the inner-ring pills); with few users, or when leaf-to-leaf edges exist, it
    /// is concentric rings instead (a ring = a hop).
    Radial,
    /// Layered (Sugiyama): top-down, orthogonal polylines. For drilling into a call chain.
    Layered,
    /// Linear Spine: one chain laid out horizontally / vertically. For taint forensics.
    Spine,
    /// Compound clustering: big boxes containing small nodes.
    Compound,
    /// Matrix: two dimensions as rows and columns, cells hold relation strength.
    Matrix,
    /// ER orthogonal: table relationships.
    Er,
}

/// Grouping dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    NodeKind,
    SubProject,
    /// Group by node property (`property:domain`).
    Property(String),
}

// ---------------------------------------------------------------- responses

/// A source-location triple (path + symbol + line), for jumping and for guarding against line-number drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLocation {
    pub file: String,
    pub line: u32,
    /// Symbol (class name / method name / table name); used to re-locate when the line number has drifted.
    pub symbol: Option<String>,
    /// Description, e.g. "the `$table` definition of Model", "the call site of `Db::name('store_order')`".
    pub note: Option<String>,
    /// The **source statement** this location corresponds to (e.g. the text of the call-site line).
    ///
    /// With only `file:line`, the user has to open the file before they can judge "is this edge right"; carrying the
    /// statement lets them verify at a glance — especially for edges arrived at **indirectly**, where they need to
    /// see immediately "did the upstream really read this config".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// A node in a view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeView {
    pub id: NodeId,
    pub kind: String,
    /// The **category** of a semantic node (currently the same as `kind`); first-class semantic nodes equal their `kind`, syntax nodes are `None`.
    #[serde(default)]
    pub category: Option<String>,
    pub name: String,
    pub fqn: Option<String>,
    /// Hop distance from the centre (0 = the centre).
    pub ring: u32,
    pub sub_project_id: Option<SubProjectId>,
    /// Whether this node has a corresponding perspective (decides whether "click to switch" is available).
    pub has_own_view: bool,
    /// The **perspective id** for this node (the result of the `node_views` mapping).
    /// On click, level one switches to it and level two selects this node; `null` when there is no matching perspective.
    #[serde(default)]
    pub own_view: Option<String>,
    /// The "side" this node belongs to: `frontend` / `backend` (tagged on semantic nodes by FKB as `side`).
    /// Used by the UI to tell frontend and backend sub-projects apart: the graph shows at a glance which nodes are frontend and which are backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    /// Definition locations for jumping; a synthetic node has **several**.
    pub locations: Vec<SourceLocation>,
    /// Annotation summary (pii / auth.public / data.criticality …).
    pub annotations: Vec<String>,
    /// This node's **columns** (fields of a table / model; bare column names, no table prefix).
    ///
    /// Columns are deliberately **not drawn as independent nodes in the folded view** (`Column` is not a semantic
    /// node — otherwise dozens of tables × a dozen columns each would burst the canvas and eat into the 400-node
    /// budget of the reachable-semantic-node statistic).
    /// But "expand a table and see which fields it has" is a hard requirement, so columns are carried out as a
    /// **property of the node**: they occupy no space in the folded view and only appear once the table is opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Perspective-specific metrics (e.g. the in-edge count).
    pub metrics: Value,
}

/// The location of a node somewhere in a link (start / intermediate hop / end), returned together with the edge.
///
/// A "link" in the folded view is the result of a **temporary lifting**: intermediate hops exist only in the current
/// view response and cannot be retrieved by re-querying on edge id (lifting is not persisted). Previously the
/// frontend could only request `/nodes/{id}/locations` per node (N+1 calls), and the locations it got were not from
/// the same source as "this edge". They are now inlined when the view is built, so the frontend needs no extra
/// request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLocationEntry {
    pub id: NodeId,
    /// Whether this is a synthetic node: for a shared resource (ConfigKey / Table …), "all of its sources" do not
    /// all belong to the current link, so the frontend has to annotate it differently.
    pub synthetic: bool,
    pub locations: Vec<SourceLocation>,
}

/// An edge in a view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeView {
    pub id: i64,
    pub kind: String,
    pub from: NodeId,
    pub to: NodeId,
    /// Whether this is a resolved solid edge (there is traceable evidence, not an unsupported inference).
    /// Note: a dashed line on the canvas only means "indirect (propagated through the call chain)"; it does not imply "pending verification".
    pub resolved: bool,
    pub confidence: f32,
    /// How many hops the dashed edge passes through (`via: 3 hops`).
    pub hops: Option<u32>,
    /// The **intermediate nodes this edge folded away** (ordered "from start to end").
    ///
    /// In the folded view the direct edge between two semantic nodes is a "lifted" one that actually passes through
    /// several syntax nodes; recording them faithfully is what lets the frontend label `via N hops` on the edge and
    /// expand the full call chain on click — "looks directly connected" must not be allowed to deceive. Empty means
    /// the graph really does have a direct edge.
    #[serde(default)]
    pub via: Vec<ViaNode>,
    /// Where the endpoint is "called" (i.e. the CallSite of the hop `last via hop -> to`).
    /// Together with each `ViaNode.call_site`, a folded link shows both "where it is defined" and "where it is called".
    #[serde(default)]
    pub to_call_site: Option<SourceLocation>,
    /// Whether this is an indirect edge obtained by **propagation**: the start itself did not perform the action, it
    /// happened somewhere downstream on its call chain and P8 replicated it along `Calls`.
    ///
    /// Example: route A's handler calls a shared service, that service reads config K, so A gets tagged
    /// `A --ReadsConfig--> K` — true, but a **weak** claim.
    /// The UI draws it dashed / down-weighted so it does not "look like A depends on K directly".
    #[serde(default)]
    pub indirect: bool,
    /// Other access kinds that hold **at the same time** on one location (**contact point -> end**).
    ///
    /// In the folded view one user draws only **one** edge to a resource (chosen by `action_strength`, write > read),
    /// and the fact that got suppressed must not simply vanish: when `Db::name('Goods')->find()` and
    /// `->update($data)` sit in the same method, labelling it read-only or write-only is wrong, the correct label is
    /// "read+write".
    ///
    /// Example: `kind = "WritesDb"`, `also_kinds = ["ReadsDb"]` => the frontend shows "reads+writes DB".
    #[serde(default)]
    pub also_kinds: Vec<String>,
    /// Locations of **every node** on this link (start -> each intermediate hop -> end), inlined when the view is built.
    ///
    /// The order follows the link (start first, end last) so the frontend can render hop by hop directly.
    /// Empty means not inlined (e.g. a non-folded view); the frontend can fall back to `/nodes/{id}/locations`.
    #[serde(default)]
    pub node_locations: Vec<NodeLocationEntry>,
}

/// An intermediate node folded away on an edge (one hop of the call chain).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViaNode {
    pub id: NodeId,
    pub kind: String,
    pub name: String,
    /// Where this hop is "called" (i.e. the CallSite at which the previous hop calls this node).
    /// The start (`from`) does not carry this; every other hop has a "who called me" location.
    #[serde(default)]
    pub call_site: Option<SourceLocation>,
}

/// Deliberately hidden parts — the honesty gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenInfo {
    /// How many neighbours the centre node actually has.
    pub total: usize,
    /// How many neighbours this view drew.

    pub shown: usize,
    /// Neighbours not drawn, tallied by node kind.
    pub by_kind: BTreeMap<String, usize>,
    /// Human-readable explanation.
    pub note: String,
}

/// Unresolved tally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedInfo {
    pub code: String,
    pub message: String,
    pub location: Option<String>,
}

/// **Direct** accesses that cannot be attributed to any semantic entry point (orphan accesses).
///
/// A resource perspective draws semantic nodes only: when the accessor is a syntax node (Method / Function …) and
/// walking up the call chain finds no semantic initiator (route / contract / scheduled job), it can neither be drawn
/// as a semantic user nor appear in the `via` chain of any lifted edge.
///
/// The current treatment is **degradation, not omission**: it takes no canvas space (syntax nodes carry little
/// information and would crowd out the semantic-node budget), but it is tallied honestly and the contact-point
/// location is given — omitting it silently would make "semantic in-edges N" contradict an empty canvas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanAccess {
    pub id: NodeId,
    /// Node kind of the accessor (usually `Method` / `Function`).
    pub kind: String,
    pub name: String,
    /// What it does to the centre resource (`ReadsDb` / `WritesCache` …).
    pub edge_kind: String,
    /// Contact-point location (`file:line`), jumpable for verification.
    #[serde(default)]
    pub location: Option<SourceLocation>,
    /// Optional: when this "direct access" is itself **a semantic edge that can be clicked open** (e.g. the
    /// `Triggers` trigger point in the event perspective), carry the folded edge view (with its `via` call chain) so
    /// the frontend opens the edge-evidence drawer instead of just the node detail. Semantic nodes (consumers) do not
    /// go through here — they are already promoted to visible nodes and drawn on the canvas directly.
    #[serde(default)]
    pub edge: Option<EdgeView>,
}

/// Object-style perspective: a link subgraph centred on **one** object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectView {
    pub project_id: ProjectId,
    pub perspective: String,
    pub layout: LayoutMode,
    pub center: NodeView,
    /// `rings[i]` = the nodes at i+1 hops from the centre.
    pub rings: Vec<Vec<NodeView>>,
    pub edges: Vec<EdgeView>,
    pub hidden: HiddenInfo,
    /// Direct accesses with no semantic entry point (orphans) — not drawn on the canvas, but they must be tallied and verifiable one by one.
    #[serde(default)]
    pub orphans: Vec<OrphanAccess>,
    pub unresolved: Vec<UnresolvedInfo>,
    /// Perspective-specific conclusions (table = reference count / PII / criticality; route = auth / dead endpoint…).
    pub conclusions: Value,
}

/// A candidate for the level-two filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: NodeId,
    pub name: String,
    /// Secondary information for the list (e.g. reference count).
    pub badge: Option<String>,
    /// The sub-project this candidate belongs to (returned when the frontend narrows candidates by sub-project, for linkage and display).
    pub sub_project_id: Option<SubProjectId>,
}

/// A cluster box of an aggregate perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub key: String,
    pub label: String,
    /// Number of nodes in the box (can be large; only the count is given rather than drawing all of them).
    pub count: usize,
    /// Sample nodes actually drawn (the cap is decided by the request).
    pub members: Vec<NodeView>,
}

/// A matrix perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixView {
    pub rows: Vec<String>,
    pub cols: Vec<String>,
    /// `cells[row][col]` = number of associations.
    pub cells: Vec<Vec<u32>>,
    pub row_totals: Vec<u32>,
    pub col_totals: Vec<u32>,
}

/// An aggregate-style perspective.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateView {
    pub project_id: ProjectId,
    pub perspective: String,
    pub layout: LayoutMode,
    pub clusters: Vec<Cluster>,
    pub matrix: Option<MatrixView>,
    pub hidden: HiddenInfo,
    pub unresolved: Vec<UnresolvedInfo>,
    pub conclusions: Value,
    /// Honest explanation when data is insufficient (e.g. the graph has no Domain node yet).
    pub notice: Option<String>,
}

/// A edge's evidence chain (how "a dashed line is an assumption to verify" is implemented).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeEvidence {
    pub edge: EdgeView,
    /// Explains why it was not resolved when `resolved=false`.
    pub reason: Option<String>,
    /// Evidence locations: a single point for a solid edge, every CallSite passed through for a dashed one.

    pub locations: Vec<SourceLocation>,
    /// Intermediate hops (if any).
    pub via: Vec<String>,
}

/// A node's list of definition locations (a synthetic node has several; a single location is never fabricated).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeLocations {
    pub id: NodeId,
    pub kind: String,
    pub name: String,
    /// Whether it is a synthetic node (decides whether the UI warns about "co-occurring in several places").
    pub synthetic: bool,
    pub locations: Vec<SourceLocation>,
    /// How many other places reference this node (for "N other references").
    pub reference_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(side: Option<&str>, side_any: Option<&[&str]>) -> PerspectiveSpec {
        PerspectiveSpec {
            id: "t".into(),
            side: side.map(|s| s.to_string()),
            side_any: side_any.map(|l| l.iter().map(|s| s.to_string()).collect()),
            ..PerspectiveSpec::default()
        }
    }

    /// A contract written by **both** sides: the derived label is `bridge`, the parties live in `sides`.
    fn bridge() -> Value {
        json!({ "side": "bridge", "sides": ["backend", "frontend"] })
    }

    /// A row written before `sides` existed: only the scalar present.
    fn legacy() -> Value {
        json!({ "side": "backend" })
    }

    #[test]
    fn no_side_filter_accepts_everything() {
        let s = spec(None, None);
        assert!(s.accepted_sides().is_empty());
        assert!(s.matches_sides(&bridge()));
        assert!(s.matches_sides(&json!({})));
    }

    #[test]
    fn bridge_node_matches_each_of_its_parties() {
        // Why this matters: with a strict scalar reading, a bridge node matches neither `side: frontend` nor
        // `side: backend`, so "the backend participates" silently dropped every node the front end had touched.
        for side in ["backend", "frontend"] {
            let s = spec(Some(side), None);
            assert!(s.matches_sides(&bridge()), "bridge 应命中 {side}");
            let s_any = spec(None, Some(&[side]));
            assert!(s_any.matches_sides(&bridge()), "side_any: bridge 应命中 {side}");
        }
    }

    #[test]
    fn single_party_node_only_matches_that_party() {
        let s = spec(Some("backend"), None);
        assert!(s.matches_sides(&json!({ "side": "backend", "sides": ["backend"] })));
        assert!(!s.matches_sides(&json!({ "side": "frontend", "sides": ["frontend"] })));
    }

    #[test]
    fn legacy_rows_without_sides_still_match_through_the_scalar() {
        // So switching a perspective to a side filter never forces a rebuild of existing projects.
        assert!(spec(Some("backend"), None).matches_sides(&legacy()));
        assert!(!spec(Some("frontend"), None).matches_sides(&legacy()));
    }

    #[test]
    fn nodes_without_any_side_evidence_never_match() {
        assert!(!spec(Some("backend"), None).matches_sides(&json!({})));
        assert!(!spec(None, Some(&["backend"])).matches_sides(&json!({ "other": 1 })));
    }

    #[test]
    fn accepted_sides_unions_both_declarations() {
        let s = spec(Some("backend"), Some(&["frontend", "external"]));
        let mut got = s.accepted_sides();
        got.sort_unstable();
        assert_eq!(got, vec!["backend", "external", "frontend"]);
    }
}
