import {
  Fragment,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactElement,
} from 'react';
import { edgeKindLabel, useLocale } from '@/shared/lib/i18n';
import { Empty, Space, Spin, Tag, Tooltip, Typography } from 'antd';
import { InfoCircleOutlined } from '@ant-design/icons';
import type { EdgeView, LayoutMode, NodeView, SourceLocation } from '@/entities/view';
import type { SubProject } from '@/entities/project/model';
import { edgeColor, nodeColor } from '@/entities/graph';
import { truncate, truncateMiddle } from '@/shared/lib/format';
import { layoutOf, type LayoutInput, type LayoutResult } from './layout/types';
import { nodeIcon, usedKinds } from './nodeIcons';

/**
 * Beyond this many edges, label edge types only on "hover / selection".
 *
 * The edge type is the semantic graph's "predicate" (`ReadsConfig` / `MapsTo` / `ReadsCache` …); labeling it is what makes the graph readable;
 * but a shared-resource view can have hundreds of edges, and labeling them all turns into mush -- so with many edges it degrades to "label on demand",
 * while the hover detail card always gives the full information.
 *
 * 40 was chosen for a "horizontal layered graph": labels spread across layers and rarely overlap. In a star view (one center dragging 27
 * config keys) every edge's midpoint crowds into the small ring around the center, so twenty-odd labels inevitably stack --
 * hence this threshold must be set by "will the labels physically overlap"; past 14 edges a star already overlaps.
 *
 * This is the **only** criterion and is not tied to zoom: labels and edges live in the same `scale(k)` layer, and uniform scaling doesn't change
 * their relative layout -- "more crowded when zoomed out" is an illusion; what doesn't overlap at k=1 doesn't overlap at k=0.5 either.
 * Zooming only changes the glyph's physical pixels, which the compensating font size below takes care of.
 */
const EDGE_LABEL_LIMIT = 14;
/** Base font size for edge labels (world coords, screen px at k=1). */
const EDGE_LABEL_FONT = 10;
/**
 * Upper bound for the compensating font size (world coords).
 *
 * If the font were left to decay proportionally when zooming out, only 5px would remain at k=0.5 -- that's noise, not information;
 * so we scale up inversely by `10 / min(k, 1)`. **It must be capped**: any larger and it collides with neighboring labels (the edge no longer
 * shrinks proportionally, it's purely the glyph growing), and it must not exceed the node name (11px) -- a predicate louder than the subject
 * reads the priority backwards. 16 means compensation is active over k∈[0.62, 1]; below that it shrinks with the graph again (at the fit floor
 * of 0.5 it's about 8px on screen, still legible).
 */
const EDGE_LABEL_MAX_FONT = 16;
/**
 * Escape hatch for dense graphs: at or above this zoom, label **everything regardless of edge count**.
 *
 * As noted, uniform scaling doesn't change relative overlap, but zooming in pushes most labels out of the viewport -- the density of labels
 * actually in view does drop, which is exactly the "zoom in for detail" moment, so giving full predicates then is worth it.
 */
const EDGE_LABEL_DETAIL_K = 1.15;
/**
 * Zoom range for "fit to screen".
 *
 * Lower bound 0.5: a large star graph (one center dragging 27 pills, bounding box 2000px+) simply doesn't fit the
 * viewport at 0.85 -- fit centers it but both sides still get cut, so "fit to screen" exists in name only and you can only scroll blindly.
 * The trade-off moved from "rather scroll than shrink text" to "see the whole first, then zoom for detail": at 0.5 a 13px glyph is about 6.5px,
 * enough for structure, hard for content. Sparse graphs (≤ `EDGE_LABEL_LIMIT` edges) still show edge labels when zoomed out
 * with the compensating font (see `EDGE_LABEL_MAX_FONT`), so predicates stay readable at that scale;
 * only dense graphs degrade to on-demand labeling. Structure outline + wheel-zoom for detail is always the right usage at this scale.
 * Upper bound 1: a small graph isn't blown up into giant text.
 */
const FIT_MIN_K = 0.5;
const FIT_MAX_K = 1;
/** Padding around content when fitting (world px). */
const FIT_PAD = 16;
// Fade depth for non-focused elements on hover focus: varies continuously with edge count (fewer edges = shallower, more edges = deeper), so a sparse graph never "disappears entirely".
const DIM_OPACITY_MIN = 0.12; // deepest, for dense graphs
const DIM_OPACITY_MAX = 0.4; // lightest, for sparse graphs
const DIM_EDGE_LOW = 4; // below this many edges use the lightest
/** Sub-project colors: one stable hue per sub-project; multiple frontends / backends each get their own color (no longer collapsed into blue / orange buckets). */
const SUB_PROJECT_PALETTE = [
  '#0ea5e9', '#f97316', '#22c55e', '#a855f7', '#eab308',
  '#ec4899', '#14b8a6', '#6366f1', '#ef4444', '#84cc16',
  '#06b6d4', '#f43f5e',
];
const ROLE_LABEL: Record<string, string> = { frontend: 'Frontend', backend: 'Backend' };
const KIND_LABEL: Record<string, string> = {
  admin: 'Admin',
  'mini-program': 'Mini program',
  mobile: 'Mobile',
  h5: 'H5',
  api: 'API',
  worker: 'Jobs / queues',
  bff: 'BFF',
  web: 'Web',
};
function roleLabel(r?: string | null): string {
  if (!r) return 'Unknown';
  const [tier, kind] = r.split(':');
  if (kind) return KIND_LABEL[kind] ?? kind;
  return ROLE_LABEL[tier] ?? tier;
}
/**
 * Legend section subheading (spans both grid columns).
 *
 * The two filter dimensions (entity / relation) behave similarly under a chained perspective; icons alone don't show the difference;
 * add a heading + hover explanation spelling out "what each toggle controls", so they aren't mistaken for synonymous buttons.
 */
const LEGEND_SECTION_TITLE: CSSProperties = {
  gridColumn: '1 / -1',
  marginTop: 2,
  fontSize: 11,
  color: '#94a3b8',
  cursor: 'help',
};

/**
 * The legend item checkbox: draws out the fact that "the legend is a filter".
 *
 * Relying only on "a pointer appears and it fades with a strikethrough on hover" to signal clickability is too weak: someone seeing the legend
 * for the first time reads it as a color key (especially with non-clickable sub-project color rows mixed in below). Checked / empty is the most
 * universal visual grammar for a filter control (same as ECharts / Grafana legends) -- without hovering you can see at a glance
 * "this is clickable" and "is it currently on or off".
 *
 * Why it **doesn't follow the kind color** and is uniformly neutral gray:
 * * with a light kind color (e.g. `HasCallSite: '#e2e8f0'`) as fill, the white ✓ and the border are both invisible on white --
 *   "checked looks unchecked" -- a control's state must not depend on the luck of a semantic color's lightness;
 * * color identity is already carried by the node icon / edge line sample in the row; coloring the control too would repeat the same color
 *   two or three times in one row, making the panel look like a swatch card rather than a filter;
 * * blue is a semantic color in the palette (DB read `#3b82f6`, sub-project `#0ea5e9`) and also the "reset" link color, so a blue checkbox
 would be misread as "DB read"-ish; neutral gray `#64748b` collides with no semantic color,
 and its contrast against the white ✓ is about 4.8:1, readable.
 */
function LegendCheck({ checked }: { checked: boolean }): ReactElement {
  return (
    <span
      style={{
        width: 13,
        height: 13,
        borderRadius: 3,
        border: `1px solid ${checked ? '#64748b' : '#cbd5e1'}`,
        background: checked ? '#64748b' : '#fff',
        color: '#fff',
        display: 'inline-flex',
        alignItems: 'center',
        justifyContent: 'center',
        fontSize: 9,
        lineHeight: 1,
        flex: '0 0 auto',
      }}
    >
      {checked ? '✓' : ''}
    </span>
  );
}
const DIM_EDGE_HIGH = 40; // Above this edge count, use the deepest fade

/**
 * A stable unique key for an edge.
 *
 * The collapsed view has "synthetic edges" (produced by lifting / reverse aggregation, with no real line), so `id` alone collides;
 * the same endpoint pair can also carry multiple edges of different kinds, so endpoints join the key.
 *
 * Still not enough: in a forward perspective one propagated edge (seed) expands into **multiple paths**, and `view_service.rs`'s
 * `push_edge` reuses one evidence edge id for them -- so two edges can share an identical `(id, from, to)`, and keying by `id:from->to` alone
 * still collides (showing up as "hovering one highlights all, the hover card always shows the first chain").
 * Hence `seq` joins the key too (this edge's index in the input `edges` array). On the `EdgeView` side `viewEdgeKeys`
 * pads the same `seq` with the index, so the two keys align.
 */
const edgeKey = (e: { id: number; from: number; to: number; seq?: number }) =>
  `${e.seq ?? '?'}:${e.id}:${e.from}->${e.to}`;

/**
 * Edge label anchor.
 *
 * Takes the polyline's **true geometric midpoint** (by accumulated length); the label is **embedded mid-edge**: text is vertically centered
 * (render side `dominantBaseline="central"`) pressed onto the line, a white stroke cuts a gap behind the text,
 * and the line shows through on both sides -- consistent for all edges (horizontal / diagonal / vertical).
 *
 * Can't use `points[Math.floor(len / 2)]`: a straight edge has only two points, index 1 is the **endpoint**,
 * and the label would be covered entirely by the later-drawn target pill (opaque white) -- showing up as
 * "you can only see the edge name in the card on hover".
 */
function labelAnchor(points: Array<[number, number]>): { x: number; y: number } {
  if (points.length === 0) return { x: 0, y: 0 };
  if (points.length === 1) return { x: points[0][0], y: points[0][1] };

  const segLen: number[] = [];
  let total = 0;
  for (let i = 1; i < points.length; i += 1) {
    const l = Math.hypot(points[i][0] - points[i - 1][0], points[i][1] - points[i - 1][1]);
    segLen.push(l);
    total += l;
  }

  let remain = total / 2;
  for (let i = 0; i < segLen.length; i += 1) {
    const [ax, ay] = points[i];
    const [bx, by] = points[i + 1];
    if (remain <= segLen[i] || i === segLen.length - 1) {
      const t = segLen[i] > 0 ? remain / segLen[i] : 0;
      return { x: ax + (bx - ax) * t, y: ay + (by - ay) * t };
    }
    remain -= segLen[i];
  }
  return { x: points[0][0], y: points[0][1] };
}

/**
 * Clipping intersection (Liang–Barsky) of the last segment with the node rect (center `cx,cy`, size `w×h`),
 * returning the boundary point where the arrow tip should land:
 *
 * - endpoint **outside/above** the rect (radial layout: the endpoint is already the pill's near edge) => take where the segment leaves the rect,
 *   i.e. the endpoint itself -- the arrow is pinned at **the edge's end**;
 * - endpoint **inside** the rect (old layout: the endpoint is the node center) => take where the segment enters the rect, matching the old
 *   `clipToRect` behavior.
 *
 * Can't be replaced by "intersect a ray toward the center": for a wide flat pill with an oblique incoming line, that ray hits the rect's
 * **bottom edge** first and the arrow hangs in the blank space right below the pill (this really happened). Nor by the approximation
 * "pull back half a pill width along the direction": a steep incidence angle throws the arrow outside the pill.
 */
function clipArrowTip(
  from: [number, number],
  to: [number, number],
  cx: number,
  cy: number,
  w: number,
  h: number,
): [number, number] {
  const hw = w / 2;
  const hh = h / 2;
  const dx = to[0] - from[0];
  const dy = to[1] - from[1];
  // Endpoint inside the rect => take the entry intersection; outside/above => take the exit intersection (= the far end)
  const toInside =
    Math.abs(to[0] - cx) <= hw + 1e-6 && Math.abs(to[1] - cy) <= hh + 1e-6;
  let t0 = 0;
  let t1 = 1;
  const clip = (p: number, q: number): boolean => {
    if (Math.abs(p) < 1e-9) return q >= 0; // Parallel and outside bounds => no intersection
    const r = q / p;
    if (p < 0) {
      if (r > t1) return false;
      if (r > t0) t0 = r;
    } else {
      if (r < t0) return false;
      if (r < t1) t1 = r;
    }
    return true;
  };
  const ok =
    clip(-dx, from[0] - (cx - hw)) &&
    clip(dx, cx + hw - from[0]) &&
    clip(-dy, from[1] - (cy - hh)) &&
    clip(dy, cy + hh - from[1]);
  if (!ok) return to; // Segment doesn't intersect the rect (shouldn't happen): fall back to the endpoint
  const t = toInside ? t0 : t1;
  return [from[0] + dx * t, from[1] + dy * t];
}

export interface CanvasNode {
  id: number;
  kind: string;
/** Category of a semantic node (currently the same as kind); null for syntactic nodes. */
  category?: string | null;
/** The perspective id for this node (click to switch); null if none. */
  own_view?: string | null;
/** The "side" the node belongs to: `frontend` / `backend` (from the FKB-annotated `side`). Used to tell frontend / backend sub-projects apart on the graph. */
  side?: string | null;
/** The sub-project id the node belongs to (backend `NodeView.sub_project_id`). Graph coloring / filtering is per sub-project, not a binary frontend/backend. */
  sub_project_id?: number | null;
  name: string;
  ring: number;
  /**
   * The following is supplementary info for the hover card, **all optional**: a caller may pass only a minimal canvas node,
   * and the card must tolerate their absence (this once did `.length` right after an `as NodeView` cast and crashed on hover).
   */
  fqn?: string | null;
  locations?: SourceLocation[];
  annotations?: string[];
  metrics?: { fan_in?: number; fan_out?: number } | null;
}

export interface CanvasCluster {
  key: string;
  label: string;
  count: number;
  members: CanvasNode[];
}

export interface CanvasMatrix {
  rows: string[];
  cols: string[];
  cells: number[][];
}

export interface GraphCanvasProps {
  mode: LayoutMode;
  center: CanvasNode | null;
  rings: CanvasNode[][];
  edges: EdgeView[];
  clusters?: CanvasCluster[];
  matrix?: CanvasMatrix;
  loading?: boolean;
/** The previous center (kept as a neighbor and marked `from` after switching perspective). */
  originId?: number | null;
  selectedId?: number | null;
  hoverEnabled?: boolean;
  width?: number;
  height?: number;
/** Click a node: when `ownView` is its perspective id, switch the level-1 perspective to it and set the level-2 object to that node. */
  onNodeClick?: (id: number, kind: string, ownView: string | null) => void;
/** Right-click / detail icon: open Inspector or navigate; does not switch perspective. */
  onNodeContextMenu?: (id: number, kind: string, event: React.MouseEvent) => void;
  onEdgeClick?: (edge: EdgeView) => void;
  locationsOf?: (id: number) => SourceLocation[];
/** Whether to label edge types on the edges (e.g. `ReadsConfig`). On by default; auto-hidden when there are too many edges or when zoomed out. */
  showEdgeLabels?: boolean;
  /**
   * Identity of the graph's "semantic content". When it changes (switch perspective / select object / switch aggregate view / expand syntax),
   * reset pan/zoom to the "whole-graph fit" initial state. Hover focus, manual zoom/pan, and in-place single-node expansion do **not** change it,
   * so they don't interrupt exploration within the current graph.
   */
  fitKey?: string | number;
/** Manual fit signal: each increment resets the view back to whole-graph fit (the toolbar "fit to screen" button). */
  fitSignal?: number;
  /**
   * Sub-project filter: show nodes and edges by multi-selected `sub_project_id`; the center node is always kept as an anchor.
   * An empty array (default) means no filtering, show everything. Multiple frontends / backends each form their own category,
   * no longer collapsed into two "frontend / backend" buckets.
   */
  subFilter?: number[];
/** The current project's sub-project list (with id / name / role), used for coloring and the legend. */
  subProjects?: SubProject[];
  /**
   * The legend is the filter: the list of hidden node kinds (toggle by node kind).
   * The center node is always kept as an anchor even if its kind is listed. Empty array (default) means hide nothing.
   * State is held by the parent so it can be synced to the URL.
   */
  hiddenNodeKinds?: string[];
  /**
   * The legend is the filter: the list of hidden edge kinds (toggle by edge kind, e.g. turning off all "DB read" removes every
   * `ReadsDb` edge). Empty array (default) means hide nothing.
   * Side effect: points reachable only through hidden edges are collapsed too (derived, not stored in state).
   */
  hiddenEdgeKinds?: string[];
/** Click a legend node item: toggle visibility of that kind. */
  onToggleNodeKind?: (kind: string) => void;
/** Click a legend edge item: toggle visibility of that kind. */
  onToggleEdgeKind?: (kind: string) => void;
/** Clear all legend filters at once. */
  onResetLegendFilters?: () => void;
}

/**
 * The graph canvas.
 *
 * Interaction split (strictly separated, to avoid "wanting to jump to code but switching the perspective instead"):
 * * **hover** -- highlight only, changes no state
 * * **left-click a node** -- switch perspective only when the node has a matching perspective (navigation)
 * * **right-click / detail icon** -- open locations and jump, no perspective switch
 * * **click an edge** -- open the edge's evidence chain
 *
 * Positions are entirely decided by `layoutOf(mode)`; there is **no force-directed free drift**.
 */
export function GraphCanvas(props: GraphCanvasProps) {
  const {
    mode,
    center,
    rings,
    edges,
    clusters,
    matrix,
    loading,
    originId,
    selectedId,
    width = 1040,
    height = 720,
    onNodeClick,
    onNodeContextMenu,
    onEdgeClick,
    showEdgeLabels = true,
    fitKey,
    fitSignal,
    subFilter = [],
    subProjects = [],
    hiddenNodeKinds = [],
    hiddenEdgeKinds = [],
    onToggleNodeKind,
    onToggleEdgeKind,
    onResetLegendFilters,
  } = props;

  const { t } = useLocale();
  const [hover, setHover] = useState<number | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);
  const [pointer, setPointer] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  /**
   * Manual pan/zoom. `null` means "no manual intervention", in which case `fitTransform` (auto fit to screen) is used.
   *
   * Using null rather than storing a fit snapshot lets the view keep recomputing from the layout during the window when the first-frame
   * width is unknown (default 1040) until ResizeObserver measures the real width,
   * instead of being pinned to the one fit computed from 1040.
   */
  const [transform, setTransform] = useState<{ x: number; y: number; k: number } | null>(null);
  const drag = useRef<{ x: number; y: number } | null>(null);

  // On semantic-graph content switch (fitKey change), reset pan/zoom back to the "whole-graph fit" initial state.
  // Hover focus / manual zoom-pan / in-place single-node expansion don't change fitKey, so they don't trigger a reset.
  useEffect(() => {
    setTransform(null);
  }, [fitKey]);

  // Toolbar "fit to screen" button: incrementing fitSignal resets to whole-graph fit.
  useEffect(() => {
    if (fitSignal === undefined) return;
    setTransform(null);
  }, [fitSignal]);

  // Feed the real container width to the layout, avoiding the crowding caused by "designing at 1040 and then being scaled down by a narrow column".
  // The first frame uses a default width; after mount, ResizeObserver measures the real width and triggers one reflow (imperceptible).
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [measuredWidth, setMeasuredWidth] = useState<number | null>(null);
  const renderW = measuredWidth ?? width;
  // Corner legend (lists only the kinds actually present in this data; collapsible). Expanded by default so icon meanings are clear on first look.
  const [legendOpen, setLegendOpen] = useState(true);

  // Number of distinct kinds actually present in this view: when ≤ 2 (a homogeneous view, e.g. "who calls X" is almost all Method)
  // the icons would all look the same on nodes -- taking horizontal space with zero discriminative value, degrading to "color only"; draw icons only when ≥ 3.
  const showNodeIcons = useMemo(() => {
    const ks = new Set<string>();
    if (center) ks.add(center.kind);
    rings?.flat().forEach((n) => ks.add(n.kind));
    clusters?.forEach((c) => c.members.forEach((m) => ks.add(m.kind)));
    return ks.size >= 3;
  }, [center, rings, clusters]);

  // Sub-project filtering: filter nodes and edges by `sub_project_id`; the center node is always kept as an anchor;
  // shared / unknown nodes with `sub_project_id == null` stay under any concrete filter (they belong to all sub-projects).
  // The filtered set feeds the layout, the coloring, and the frontend caller's decisions.
  const { fCenter, fRings, fEdges } = useMemo(() => {
    if (!subFilter || subFilter.length === 0) {
      return { fCenter: center, fRings: rings ?? [], fEdges: edges };
    }
    const allowed = new Set(subFilter);
    const keep = new Set<number>();
    if (center) keep.add(center.id);
    for (const ring of rings ?? []) {
      for (const n of ring) {
        if (n.sub_project_id == null || allowed.has(n.sub_project_id)) keep.add(n.id);
      }
    }
    const fRings = (rings ?? []).map((ring) => ring.filter((n) => keep.has(n.id)));
    const fEdges = edges.filter((e) => keep.has(e.from) && keep.has(e.to));
    return { fCenter: center, fRings, fEdges };
  }, [center, rings, edges, subFilter]);

  // The legend is the filter: on top of "sub-project filtering", also toggle by node kind / edge kind.
  //
  // Invariant: the picture = **the subgraph formed by visible edges**, with the center always kept as an anchor. Three steps:
  // 1. Pick the candidate node set by node kind; don't draw an edge if either endpoint is outside the set (avoid dangling edges);
  // 2. Remove relations by edge kind;
  // 3. Collapse points whose "degree over visible edges is 0" -- points reachable only via hidden edges (e.g. after turning off
  //    "read cache", those reachable only through read-cache) carry no information in this graph, and keeping them just lets
  //    the layout fallback place them in a column (see the right-column fallback in `layout/types.ts`), looking like "filtered but not really".
  //    Collapsing is **derived** (not stored in state); cancelling the filter restores them.
  const { vCenter, vRings, vEdges, cascadeHidden } = useMemo(() => {
    const hn = hiddenNodeKinds ?? [];
    const he = hiddenEdgeKinds ?? [];
    if (hn.length === 0 && he.length === 0) {
      return { vCenter: fCenter, vRings: fRings, vEdges: fEdges, cascadeHidden: 0 };
    }
    const hiddenNodeK = new Set(hn);
    const hiddenEdgeK = new Set(he);
    // Candidate node set: points whose kind isn't hidden; the center is always kept as an anchor.
    const candidates = new Set<number>();
    if (fCenter) candidates.add(fCenter.id);
    for (const ring of fRings) {
      for (const n of ring) {
        if (!hiddenNodeK.has(n.kind)) candidates.add(n.id);
      }
    }
    const vEdges = fEdges.filter(
      (e) => !hiddenEdgeK.has(e.kind) && candidates.has(e.from) && candidates.has(e.to),
    );
    // Visible edges -> points still on the graph (the center always is).
    const linked = new Set<number>();
    if (fCenter) linked.add(fCenter.id);
    for (const e of vEdges) {
      linked.add(e.from);
      linked.add(e.to);
    }
    // Count of additionally collapsed points: those whose own kind isn't hidden but that have no connection left over visible edges.
    let collapsed = 0;
    const vRings = fRings.map((ring) =>
      ring.filter((n) => {
        if (linked.has(n.id)) return true;
        if (hiddenNodeK.has(n.kind)) return false;
        collapsed += 1;
        return false;
      }),
    );
    return { vCenter: fCenter, vRings, vEdges, cascadeHidden: collapsed };
  }, [fCenter, fRings, fEdges, hiddenNodeKinds, hiddenEdgeKinds]);

  // Legend items take the **unfiltered** full set: even a hidden kind must stay in the legend so it can be re-enabled.
  const legendNodeKinds = useMemo(
    () => usedKinds([...(fCenter ? [fCenter.kind] : []), ...(fRings ?? []).flat().map((n) => n.kind)]),
    [fCenter, fRings],
  );
  const legendEdgeKinds = useMemo(
    () => usedKinds((fEdges ?? []).map((e) => e.kind)),
    [fEdges],
  );
  const hiddenNodeSet = new Set(hiddenNodeKinds ?? []);
  const hiddenEdgeSet = new Set(hiddenEdgeKinds ?? []);
  const hasLegendFilter = hiddenNodeSet.size > 0 || hiddenEdgeSet.size > 0;

  // Sub-project coloring: map stably onto the palette after sorting by id; a sub-project always has the same color, and multiple frontends each get a different color.
  const subProjectColors = useMemo(() => {
    const m = new Map<number, string>();
    const ids = (subProjects ?? [])
      .map((s) => s.id)
      .filter((v): v is number => typeof v === 'number')
      .sort((a, b) => a - b);
    ids.forEach((id, i) => m.set(id, SUB_PROJECT_PALETTE[i % SUB_PROJECT_PALETTE.length]));
    return m;
  }, [subProjects]);

  // Node -> sub-project id map (for coloring only; uses the unfiltered full set, filtering doesn't change color semantics).
  const subProjectOf = useMemo(() => {
    const m = new Map<number, number>();
    const add = (n?: CanvasNode | null) => {
      if (n && n.sub_project_id != null) m.set(n.id, n.sub_project_id);
    };
    add(center);
    (rings ?? []).flat().forEach(add);
    return m;
  }, [center, rings]);



  const layout: LayoutResult | null = useMemo(() => {
    if (!fCenter && !clusters?.length && !matrix) return null;
    const input: LayoutInput = {
      center: vCenter ?? { id: -1, kind: 'Unknown', name: '', ring: 0 },
      rings: vCenter ? vRings : [],
      // With `seq` (index): multiple paths with the same (id, from, to) are distinguished by it, see `edgeKey`'s note.
      edges: vEdges.map((e, i) => ({ id: e.id, from: e.from, to: e.to, seq: i })),
      clusters: clusters?.map((c) => ({
        key: c.key,
        label: c.label,
        count: c.count,
        members: c.members.map((m) => ({ ...m })),
      })),
      matrix,
      width: renderW,
      height,
      showIcons: showNodeIcons,
    };
    return layoutOf(mode)(input);
  }, [mode, vCenter, vRings, vEdges, clusters, matrix, renderW, height]);

  // Node size table: arrow pullback / selection stroke must follow the node's real shape (rect pills need half-width, not a fixed 12px).
  // x/y must be stored too: arrow boundary intersection must target the **node center** (clipToRect's contract);
  // you can't just use the polyline endpoint -- under a radial layout the endpoint lands on the pill's near edge, not the center,
  // and treating it as the center pushes the arrow off the pill by half a width, leaving it hanging in mid-air.
  const nodeRectById = useMemo(() => {
    const m = new Map<number, { shape: 'circle' | 'rect'; x: number; y: number; w: number; h: number }>();
    layout?.nodes.forEach((n) =>
      m.set(n.id, { shape: n.shape, x: n.x, y: n.y, w: n.w ?? 120, h: n.h ?? 26 }),
    );
    return m;
  }, [layout]);

  // Depends on loading / layout: loading and empty states return a placeholder div without a ref,
  // so the real width is only observable once the chart container (the div with the ref) is actually mounted.
  useEffect(() => {
    const el = containerRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver((entries) => {
      const w = entries[0]?.contentRect.width;
      if (w && w > 0) setMeasuredWidth(w);
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [loading, layout]);

  // Viewport height (world coords). The svg's viewBox is 1:1 with the render size -- **all** zooming is handed to the inner <g transform>.
  // The old implementation set viewBox to `layout.width` (which under a layered layout can be 3000px+), so the browser's `meet`
  // scaled the whole graph back down to the container width, text included -- hence "text goes blurry as soon as there are many out-edges";
  // at the same time the cursor-anchored wheel conversion broke (that code assumed viewBox was 1:1 with the screen).
  const viewH = Math.max(320, layout ? layout.height : height);
  // The graph area's actual visible height (limited by the outer `maxHeight`): fit computes against **the visible slice**,
  // not the total scroll height -- otherwise tall content gets shrunk smaller than necessary.
  const viewportH = Math.min(height, viewH);

  /**
   * A true zoom-to-fit: compute scale and centering from the **content bounding box** (`layout.content`, falling back to the whole canvas).
   *
   * The old implementation was just `setTransform({x:0,y:0,k:1})`, i.e. "no scaling", so the "fit to screen" button did nothing.
   */
  const fitTransform = useMemo(() => {
    if (!layout) return null;
    const box = layout.content ?? { x: 0, y: 0, w: layout.width, h: layout.height };
    const availW = Math.max(120, renderW - FIT_PAD * 2);
    const availH = Math.max(120, viewportH - FIT_PAD * 2);
    const raw = Math.min(availW / Math.max(1, box.w), availH / Math.max(1, box.h));
    const k = Math.max(FIT_MIN_K, Math.min(FIT_MAX_K, raw));
    return {
      k,
      x: renderW / 2 - (box.x + box.w / 2) * k,
      y: viewportH / 2 - (box.y + box.h / 2) * k,
    };
  }, [layout, renderW, viewportH]);

  // Lets wheel / drag move incrementally from the fit state while "not yet manually intervened" (stable ref, not a useCallback dependency).
  const fitRef = useRef(fitTransform);
  fitRef.current = fitTransform;

  // **Must be declared before all early returns**, otherwise it won't run while loading and an extra Hook appears after data arrives,
  // directly triggering "Rendered more hooks than during the previous render" and a blank screen
  // (`GraphCanvas.test.tsx` guards exactly this case).
  //
  // Index by id: the backend emits one edge per **distinct path** between the same endpoint pair.
  // Note `id` is **not guaranteed unique** -- in a forward perspective, multiple paths expanded from the same propagated edge (seed)
  // reuse the same evidence edge id. So this is only a **fallback** (first hit); exact matching relies on `seq` + `viewEdgeKeys`.
  const edgeById = useMemo(() => {
    const m = new Map<number, EdgeView>();
    for (const e of edges) if (!m.has(e.id)) m.set(e.id, e);
    return m;
  }, [edges]);
  // Fallback: an in-place expanded subgraph may bring ids duplicated with the main graph; fall back to lookup by endpoints.
  const edgeByPair = useMemo(() => {
    const m = new Map<string, EdgeView>();
    for (const e of edges) m.set(`${e.from}->${e.to}`, e);
    return m;
  }, [edges]);
  // `EdgeView` -> unique key: pad `seq` with the **index** to align with the layout's `edgeKey` (which carries `seq`).
  // With it, multiple parallel paths that share (id, from, to) but differ in `via` can each be hovered / clicked independently:
  // hovering lights only the hovered one, and the drawer shows **that path's own** `via` chain.
  const viewEdgeKeys = useMemo(() => {
    const m = new Map<EdgeView, string>();
    edges.forEach((e, i) => m.set(e, edgeKey({ id: e.id, from: e.from, to: e.to, seq: i })));
    return m;
  }, [edges]);
  // Frontend HTTP caller: a function node that is the source of a `CallsHttp` edge. It is essentially a "frontend API entry",
  // isomorphic to a backend Method and a key node explicitly brought into the graph by the contract bridge, so it shouldn't appear as an anonymous syntax pill.
  // Here we only do a **visual upgrade** (kind-colored fill), without changing its kind -- making it read at a glance as a "first-class node"
  // while preserving the collapsed view's existing invariants (semantic nodes / collapse fallback) and backend determinations.
  // Must be declared before the early return: a loading→data Hook count mismatch would blank the screen.
  const frontendCallerIds = useMemo(() => {
    const s = new Set<number>();
    for (const e of fEdges) if (e.kind === 'CallsHttp') s.add(e.from);
    return s;
  }, [fEdges]);

  // Focus: when hovering a node / edge, keep "the target + its direct neighbors" fully lit, fade the rest to ghosts (structure outline still visible).
  // Must be placed before the early return, otherwise a loading→data Hook count mismatch would blank the screen.
  // Consistent across perspectives: hovering always focuses; fade depth adapts to edge count (a sparse graph only dims slightly, never vanishes).
  const focus = useMemo(() => {
    const nodes = new Set<number>();
    const edgeKeys = new Set<string>();
    if (hover != null) {
      nodes.add(hover);
      for (const e of edges) {
        if (e.from === hover) {
          nodes.add(e.to);
          const k = viewEdgeKeys.get(e);
          if (k) edgeKeys.add(k);
        } else if (e.to === hover) {
          nodes.add(e.from);
          const k = viewEdgeKeys.get(e);
          if (k) edgeKeys.add(k);
        }
      }
    }
    if (hoverEdge != null) {
      const ev = edges.find((x) => viewEdgeKeys.get(x) === hoverEdge);
      if (ev) {
        nodes.add(ev.from);
        nodes.add(ev.to);
      }
      edgeKeys.add(hoverEdge);
    }
    return { nodes, edgeKeys, hasFocus: hover != null || hoverEdge != null };
  }, [hover, hoverEdge, edges, viewEdgeKeys]);
  // Hover focus: always keep "the target + direct neighbors" fully lit and fade the rest when hovering a node / edge (same across perspectives).
  // Fade depth interpolates continuously with edge count: 4 edges ≈ 0.4 (light emphasis), 40 edges ≈ 0.12 (deep dim), linear in between.
  const focusing = focus.hasFocus;
  const dimT = Math.min(1, Math.max(0, (edges.length - DIM_EDGE_LOW) / (DIM_EDGE_HIGH - DIM_EDGE_LOW)));
  const dimOpacity = DIM_OPACITY_MAX - (DIM_OPACITY_MAX - DIM_OPACITY_MIN) * dimT;

  // Wheel zoom must use a native non-passive listener: React's synthetic onWheel is registered as passive at the root,
  // so preventDefault has no effect and the page scrolls along. Here a callback ref attaches the listener when the svg actually mounts
  // (during loading / empty early returns the svg doesn't exist; the callback ref re-fires on mount), with explicit passive:false.
  // Must be placed before the early return, otherwise a loading→data render would have a mismatched Hook count and blank the screen.
  const svgWheelRef = useCallback(
    (el: SVGSVGElement | null) => {
      if (!el) return;
      const onWheel = (e: WheelEvent) => {
        e.preventDefault();
        const factor = e.deltaY > 0 ? 0.9 : 1.1;
        // Zoom anchored at the cursor: keep the world point under the cursor fixed (viewBox is 1:1 with the render).
        const rect = el.getBoundingClientRect();
        const cx = e.clientX - rect.left;
        const cy = e.clientY - rect.top;
        setTransform((prev) => {
          const t = prev ?? fitRef.current ?? { x: 0, y: 0, k: 1 };
          // Range narrowed to 0.6–2: zooming is a way to "see spatial relations", not a font-size switch --
          // 0.25 squeezes text to 3px (unreadable), 3 blows pills into giant blocks; neither end adds information.
          const k = Math.max(0.6, Math.min(2, t.k * factor));
          const ratio = k / t.k;
          return { k, x: cx - ratio * (cx - t.x), y: cy - ratio * (cy - t.y) };
        });
      };
      el.addEventListener('wheel', onWheel, { passive: false });
    },
    [setTransform],
  );

  if (loading) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        {/* `tip` only takes effect in nest / fullscreen mode; the text is placed side by side here to avoid an antd warning */}
        <Space direction="vertical" align="center" size={8}>
          <Spin />
          <Typography.Text type="secondary">{t('Loading view…')}</Typography.Text>
        </Space>
      </div>
    );
  }
  if (!layout) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        <Empty description={t('No displayable objects under this perspective')} />
      </div>
    );
  }

  // Use `CanvasNode`'s real type: earlier this lied with `as NodeView` about fields,
  // so the hover card had the data and the type but not the runtime value when accessing `locations.length` -- a hard crash.
  const nodeById = new Map<number, CanvasNode>();
  const allNodes: CanvasNode[] = [];
  if (center) allNodes.push(center);
  rings.flat().forEach((n) => allNodes.push(n));
  allNodes.forEach((n) => nodeById.set(n.id, n));

  const maxCell = Math.max(
    1,
    ...(layout.cells ?? []).map((c) => c.value),
  );



  const hoveredNode = hover != null ? nodeById.get(hover) ?? null : null;
  const hoveredEdge =
    hoverEdge != null ? edges.find((x) => viewEdgeKeys.get(x) === hoverEdge) ?? null : null;

  // Effective transform: auto-fit to screen when not manually zoomed/panned.
  // Named `tf` rather than `view` to avoid confusion with the local `view` (EdgeView) in edge rendering.
  const tf = transform ?? fitTransform ?? { x: 0, y: 0, k: 1 };

  // Edge label font size: compensate inversely **only when zooming out** (`min(k, 1)`). When zooming in it stays put so labels
  // grow with the graph -- which is the expected "zoom in for detail"; `EDGE_LABEL_MAX_FONT` caps it.
  const edgeLabelFont = Math.min(EDGE_LABEL_MAX_FONT, EDGE_LABEL_FONT / Math.min(tf.k, 1));

  return (
    <div
      ref={containerRef}
      style={{
        border: '1px solid #eef0f4',
        borderRadius: 14,
        background: 'linear-gradient(180deg,#fbfcfe,#f4f6fa)',
        overflow: 'hidden',
        position: 'relative',
      }}
      onMouseMove={(e) => {
        const r = e.currentTarget.getBoundingClientRect();
        setPointer({ x: e.clientX - r.left, y: e.clientY - r.top });
      }}
      onMouseLeave={() => {
        setHover(null);
        setHoverEdge(null);
      }}
    >
      {/* When content is taller than the viewport, switch to scrolling. The old implementation used `min(height, layout.height)` + `overflow:hidden`,
          so the overflow was cut off and **could not** be scrolled -- "the bottom half vanishes into thin air" as soon as a layered graph got deep. */}
      <div style={{ maxHeight: height, overflow: 'auto' }}>
        <svg
          width="100%"
          height={viewH}
          viewBox={`0 0 ${renderW} ${viewH}`}
          preserveAspectRatio="xMinYMin meet"
          ref={svgWheelRef}
          onMouseDown={(e) => {
            drag.current = { x: e.clientX, y: e.clientY };
          }}
          onMouseMove={(e) => {
            if (!drag.current) return;
            const dx = e.clientX - drag.current.x;
            const dy = e.clientY - drag.current.y;
            drag.current = { x: e.clientX, y: e.clientY };
            setTransform((prev) => {
              const t = prev ?? fitRef.current ?? { x: 0, y: 0, k: 1 };
              return { ...t, x: t.x + dx, y: t.y + dy };
            });
          }}
          onMouseUp={() => (drag.current = null)}
          onMouseLeave={() => (drag.current = null)}
          style={{ cursor: 'grab', display: 'block' }}
        >
        <g transform={`translate(${tf.x},${tf.y}) scale(${tf.k})`}>
          {/* Concentric ring guides: explicitly mark "ring = hop count", visual reference only, not part of hit-testing */}
          {layout.guides?.map((g, i) => (
            <g key={`guide${i}`}>
              {/* All stroke widths below use `non-scaling-stroke`: zooming changes **spatial relations** only, it doesn't fatten/thin the lines
                  (map semantics). No mush into thick bars when zooming in, no invisibility when zooming out.
                  Text and its white stroke backing are **not** in that group -- they follow the font size, otherwise stroke and glyph drift apart. */}
              <circle
                cx={g.cx}
                cy={g.cy}
                r={g.r}
                fill="none"
                stroke="#e6ebf3"
                strokeWidth={1}
                vectorEffect="non-scaling-stroke"
              />
              <text
                x={g.cx}
                y={g.cy - g.r - 6}
                fontSize={10}
                fill="#aab4c5"
                textAnchor="middle"
                style={{ pointerEvents: 'none', userSelect: 'none' }}
              >
                {g.label + t(' hops')}
              </text>
            </g>
          ))}

          {/* Cluster boxes */}
          {layout.groups?.map((g) => (
            <g key={g.key}>
              <rect
                x={g.x}
                y={g.y}
                width={g.w}
                height={g.h}
                rx={12}
                fill="#ffffff"
                stroke="#dbe2f0"
                strokeWidth={1}
                vectorEffect="non-scaling-stroke"
              />
              <text x={g.x + 14} y={g.y + 22} fontSize={12} fontWeight={600} fill="#334155">
                {truncate(g.label, 22)}
              </text>
              <text x={g.x + g.w - 14} y={g.y + 22} fontSize={11} fill="#94a3b8" textAnchor="end">
                {g.count + t(' members')}
              </text>
            </g>
          ))}

          {/* Matrix */}
          {layout.cells?.map((c, i) => (
            <Tooltip
              key={i}
              title={`${layout.rowHeaders?.[c.row]?.label} × ${layout.colHeaders?.[c.col]?.label}：${c.value}`}
            >
              <rect
                x={c.x}
                y={c.y}
                width={c.w}
                height={c.h}
                rx={4}
                fill={c.value === 0 ? '#f1f5f9' : `rgba(61,126,255,${0.15 + 0.75 * (c.value / maxCell)})`}
              />
            </Tooltip>
          ))}
          {layout.cells && layout.cells.length > 0
            ? layout.cells.map((c, i) =>
                c.value > 0 ? (
                  <text
                    key={`v${i}`}
                    x={c.x + c.w / 2}
                    y={c.y + c.h / 2 + 4}
                    fontSize={11}
                    fill="#0f172a"
                    textAnchor="middle"
                    style={{ pointerEvents: 'none' }}
                  >
                    {c.value}
                  </text>
                ) : null,
              )
            : null}
          {layout.rowHeaders?.map((h, i) => (
            <text key={`r${i}`} x={h.x} y={h.y + 22} fontSize={12} fill="#334155">
              {truncate(h.label, 20)}
            </text>
          ))}
          {layout.colHeaders?.map((h, i) => (
            <text
              key={`c${i}`}
              x={h.x + 2}
              y={h.y + 18}
              fontSize={11}
              fill="#475569"
              transform={`rotate(-32 ${h.x + 2} ${h.y + 18})`}
            >
              {truncate(h.label, 14)}
            </text>
          ))}

          {/* Edges */}
          {layout.edges.map((e) => {
            // Exact back-reference: `seq` = this edge's index in `edges` => directly get **the EdgeView that carries its own via**.
            // Multiple parallel paths sharing (id, from, to) therefore no longer share one view (otherwise the hover card always showed the first).
            // Falls back to lookup by id / endpoints only when the layout carries no `seq` (legacy / synthetic paths).
            const view =
              (e.seq != null ? edges[e.seq] : undefined) ??
              edgeById.get(e.id) ??
              edgeByPair.get(`${e.from}->${e.to}`) ??
              edges.find((x) => x.id === e.id);
            // Focus: when hovering, keep "the target edge + its two endpoint nodes" fully lit and dim the rest by density.
            const inFocus = focus.edgeKeys.has(edgeKey(e));
            const dim = focusing && !inFocus;
            const active = inFocus;
            const d = e.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p[0]},${p[1]}`).join(' ');
            // Label anchor: the true geometric midpoint + normal offset, to avoid landing on the edge line or under the target node
            const lp = labelAnchor(e.points);
            // Direction arrow: draw a small triangle pulled back a few px before the endpoint along the last segment, to avoid being covered by the node.
            // Rect pills pull back by half-width, circles by a fixed amount.
            const _pts = e.points;
            const _p1 = _pts[_pts.length - 1];
            const _p0 = _pts[_pts.length - 2] ?? _pts[0];
            const _ang = Math.atan2(_p1[1] - _p0[1], _p1[0] - _p0[0]);
            const _toRect = nodeRectById.get(e.to);
            // Arrow tip = intersection of the last segment with the target rect: when the endpoint is on the pill edge it is the endpoint itself (pinned at the edge's end),
            // when the endpoint is at the center it degrades to the entry intersection (old layout behavior unchanged).
            const _border = _toRect
              ? clipArrowTip(_p0, _p1, _toRect.x, _toRect.y, _toRect.w, _toRect.h)
              : _p1;
            const _len = Math.hypot(_p1[0] - _p0[0], _p1[1] - _p0[1]) || 1;
            // Arrow tip lands directly on the boundary intersection (no longer pulled back 3px): the endpoint is already pinned to the pill's near edge by the layout,
            // and pulling back only creates a "just short of the end" gap (from real user feedback).
            const _tipx = _border[0];
            const _tipy = _border[1];
            const _a = 6;
            const _s = 0.42;
            const _ax1 = _tipx - _a * Math.cos(_ang - _s);
            const _ay1 = _tipy - _a * Math.sin(_ang - _s);
            const _ax2 = _tipx - _a * Math.cos(_ang + _s);
            const _ay2 = _tipy - _a * Math.sin(_ang + _s);
            return (
              <g key={edgeKey(e)} style={{ opacity: dim ? dimOpacity : 1, transition: 'opacity 140ms' }}>
                {/* Widened transparent hit area, to make thin edges easy to hover */}
                <path
                  d={d}
                  fill="none"
                  stroke="transparent"
                  strokeWidth={12}
                  style={{ cursor: 'pointer' }}
                  onMouseEnter={() => setHoverEdge(edgeKey(e))}
                  onMouseLeave={() => setHoverEdge(null)}
                  onClick={() => view && onEdgeClick?.(view)}
                />
                <path
                  d={d}
                  fill="none"
                  stroke={edgeColor(view?.kind ?? '')}
                  strokeWidth={1.2}
                  strokeOpacity={active ? 1 : 0.8}
                  strokeDasharray={view?.indirect ? '5 4' : undefined}
                  vectorEffect="non-scaling-stroke"
                  style={{ pointerEvents: 'none', transition: 'stroke-width 140ms ease, stroke-opacity 140ms ease' }}
                />
                {/* Direction arrow: shows the flow of a directed dependency (consumer→table / handler→config …).
                    A folded "via N hops" edge also gets an arrow -- the direction still holds, and non-directness is already expressed by the dashed line + annotation. */}
                <polygon
                  points={`${_tipx},${_tipy} ${_ax1},${_ay1} ${_ax2},${_ay2}`}
                  fill={edgeColor(view?.kind ?? '')}
                  fillOpacity={active ? 1 : 0.7}
                  style={{ pointerEvents: 'none' }}
                />
                {/* Edge type: the semantic edge kind (`ReadsConfig` / `MapsTo` …) is this graph's "predicate";
                    labeling it is what makes it readable. **Always shown** by default (no longer requires hover): only when the edge count
                    exceeds the threshold does it degrade to "label just the hovered / selected one" to avoid mush. On hover other edges only fade, never hide
                    their labels (focus never loses information).
                    The threshold looks only at **density** (and uses `layout.edges` rather than the input `edges` -- the collapsed view synthesizes /
                    dedupes, and the former is what actually gets drawn): labels and graph scale together, so relative layout doesn't
                    change with zoom -- whether to label depends on possible overlap (edge count / anchor distribution),
                    not on zooming in or out; font size is separately kept readable by `edgeLabelFont`'s compensation.
                    The exception is `EDGE_LABEL_DETAIL_K`: zooming to that scale = the user is looking at detail, so even dense graphs label everything. */}
                {view &&
                showEdgeLabels &&
                (active ||
                  layout.edges.length <= EDGE_LABEL_LIMIT ||
                  tf.k >= EDGE_LABEL_DETAIL_K) ? (
                  <text
                    x={lp.x}
                    y={lp.y}
                    fontSize={edgeLabelFont}
                    fontWeight={500}
                    textAnchor="middle"
                    dominantBaseline="central"
                    stroke="#ffffff"
                    // The white stroke cutout scales with font size: after compensation the text is bigger, and a fixed 3.5 stroke
                    // would be relatively thinner, unable to cover the edge line underneath.
                    strokeWidth={edgeLabelFont * 0.35}
                    strokeLinejoin="round"
                    paintOrder="stroke"
                    style={{ pointerEvents: 'none', userSelect: 'none' }}
                  >
                    {/* Edge kind (localized per language into a semantic predicate, e.g. `PublishesTo` → "publishes to") */}
                    <tspan fill={edgeColor(view.kind)}>{t(`edge.${view.kind}`)}</tspan>
                    {/* Folded indirect dependencies (spanning N syntactic nodes) are distinguished by the dashed line + legend; the exact hop count is given in the
                        hover card rather than squeezed into the on-line label, which would lengthen the white gap and overlap neighbors. */}
                  </text>
                ) : null}
              </g>
            );
          })}

          {/* Nodes */}
          {layout.nodes.map((n) => {
            const meta = nodeById.get(n.id);
            const isCenter = center?.id === n.id;
            const isOrigin = originId != null && originId === n.id;
            const w = n.w ?? 120;
            const h = n.h ?? 26;
            const fill = nodeColor(n.kind);
            const isFrontendCaller = frontendCallerIds.has(n.id);
            // Node description: kind / category / name. Only `aria-label` for screen readers, no visual tooltip --
            // the hover detail card already gives this information (and more), and a native `<title>` would pop the same content
            // again at the cursor ~1 second later, overlapping and occluding the card. See the note on `<g>`.
            const ariaLabel =
              meta?.category && meta.category !== n.kind
                ? `${t(`node.${n.kind}`)} (${meta.category}) · ${n.name}`
                : `${t(`node.${n.kind}`)} · ${n.name}`;
            // Nodes are uniformly rect pills with text inside the box; no external labels needed.
            // Frontend HTTP callers get a kind-colored fill (light), upgraded from anonymous white syntax pills to a "first-class node" look;
            // other nodes keep a white fill with a kind-colored stroke.
            return (
              <g
                key={n.id}
                transform={`translate(${n.x},${n.y})`}
                aria-label={ariaLabel}
                style={{
                  cursor: meta?.own_view ? 'pointer' : 'default',
                  opacity: focusing && !focus.nodes.has(n.id) ? dimOpacity : 1,
                  transition: 'opacity 140ms',
                }}
                onMouseEnter={() => setHover(n.id)}
                onMouseLeave={() => setHover(null)}
                onClick={() => onNodeClick?.(n.id, n.kind, meta?.own_view ?? null)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  onNodeContextMenu?.(n.id, n.kind, e);
                }}
              >
                {/* All nodes use a uniform 1.4px kind-colored stroke; center emphasis is carried by size + font weight instead,
                    no longer stacking hierarchy via thicker borders, which made same-kind nodes' stroke widths look inconsistent.
                    Frontend HTTP callers additionally get a light kind fill to highlight their "first-class entry" status. */}
                <rect
                  x={-w / 2}
                  y={-h / 2}
                  width={w}
                  height={h}
                  rx={6}
                  fill={isFrontendCaller ? fill : '#fff'}
                  fillOpacity={isFrontendCaller ? 0.16 : 1}
                  stroke={fill}
                  strokeWidth={isFrontendCaller ? 1.8 : 1.4}
                  vectorEffect="non-scaling-stroke"
                  style={{ transition: 'stroke-width 140ms ease' }}
                />
                {/* Frontend/backend marker: a color dot at the node's top-right (blue = frontend / orange = backend) tells at a glance which side a node belongs to.
                    Colors match the legend; syntactic nodes without `side` (File / Class …) get none. */}
                {(() => {
                  const sid = subProjectOf.get(n.id);
                  if (sid == null) return null;
                  const col = subProjectColors.get(sid) ?? '#94a3b8';
                  return (
                    <circle
                      cx={w / 2 - 6}
                      cy={-h / 2 + 6}
                      r={3.4}
                      fill={col}
                      stroke="#fff"
                      strokeWidth={1.2}
                      style={{ pointerEvents: 'none' }}
                    />
                  );
                })()}
                {isOrigin ? (
                  <text
                    x={0}
                    y={-h / 2 - 8}
                    fontSize={10}
                    textAnchor="middle"
                    fill="#ef4444"
                    fontWeight={700}
                  >
                    from
                  </text>
                ) : null}
                {/* A native `<title>` tooltip is deliberately **not** placed here: the hover detail card (below) already gives
                    "kind / category / name / in-out edges / locations / next steps" the instant you hover, while a native title would pop the same
                    information again at the cursor about 1 second later, landing right on top of the card (two tooltips fighting over the same spot).
                    The same copy is hung on `aria-label` instead (see `<g>`): screen readers still get it, with zero visual side effects. */}
                {/* Kind icon: filled with the kind color, replacing the old colored kind text prefix -- saves horizontal space for the name,
                    so long paths (e.g. routes) show more completely. Icon fixed at 12px: the name is the subject, the icon only helps recognize the kind;
                    at 14px the icon is taller than the 13px text and steals attention from the name (the most obvious issue in screenshots).
                    12px is also the clarity floor for this kind of thin-stroke icon; below that the strokes mush.
                    `pillWidth`'s `ICON_AREA` / text start must stay in sync (21 = left padding 6 + icon 12 + gap 3).
                    Drawn only when this view has ≥ 3 kinds (a homogeneous view degrades to color only, see `showNodeIcons`).
                    Must be wrapped in `<foreignObject>`: an antd icon's root element is an HTML `<span>`; put directly inside an SVG `<g>`
                    the browser drops it under the SVG namespace (showing up as the icon vanishing while the 21px icon slot is still reserved -- a blank illusion).
                    Size is controlled via fontSize (width/height attributes on a span have no effect), consistent with the legend's rendering. */}
                {showNodeIcons ? (
                  (() => {
                    const Icon = nodeIcon(n.kind);
                    const ICON = 12;
                    return (
                      <foreignObject
                        x={-w / 2 + 6}
                        y={-ICON / 2}
                        width={ICON}
                        height={ICON}
                        style={{ pointerEvents: 'none', overflow: 'visible' }}
                      >
                        <Icon style={{ color: fill, fontSize: ICON, display: 'block' }} />
                      </foreignObject>
                    );
                  })()
                ) : null}
                {/* Name font size / weight: center 13px/600, others 11px/500.
                    The center already has threefold emphasis -- position (centered / left-column anchor), size (13 vs 11), color (#0f172a vs #475569);
                    adding 700 would be a fourth, and at 13px the glyphs get heavy and cramped, and long route names take more width; 600 is still clearly
                    heavier than 500 and stays consistent with other emphasis in the product (legend headings / cluster labels / hover card names are all 600; 700 is reserved for badge-level small text). */}
                <text
                  x={-w / 2 + (showNodeIcons ? 21 : 8)}
                  y={4}
                  fontSize={isCenter ? 13 : 11}
                  fontWeight={isCenter ? 600 : 500}
                  fill={isCenter ? '#0f172a' : '#475569'}
                  textAnchor="start"
                  style={{ pointerEvents: 'none', userSelect: 'none' }}
                >
                  {/* The semantic name (e.g. `store_order_refund_service` / `GET /v2/order/.../create`) is what a person
                      is actually looking for, so it leads. Kind is already conveyed by the left icon + color and no longer competes. Long names are truncated
                      in the middle at 40 by **visual width** (keeping head and tail; a CJK char counts as 1.8), matching the layout's `pillWidth` estimate so they don't overflow the pill.
                      `tspan` was removed: it repeated the parent's same font size / weight and was even inconsistent with it (400 vs 500),
                      leaving the risk of two sources of truth. */}
                  {truncateMiddle(n.name, 40)}
                </text>
                {/* The selection ring is kept only for "non-center selected nodes" (unreachable in the current interaction; left as an extension point) */}
                {selectedId === n.id && !isCenter ? (
                  <rect
                    x={-w / 2 - 3}
                    y={-h / 2 - 3}
                    width={w + 6}
                    height={h + 6}
                    rx={8}
                    fill="none"
                    stroke="#3d7eff"
                    strokeWidth={1.6}
                  />
                ) : null}
              </g>
            );
          })}
        </g>
        </svg>
      </div>

      {/* Hover detail card: replaces the old "fade other nodes / edges" behavior -- hovering immediately gives readable attributes */}
      {hoveredNode ? (
        <div
          style={{
            position: 'absolute',
            left: Math.min(pointer.x + 16, Math.max(8, renderW - 268)),
            top: Math.min(pointer.y + 16, Math.max(8, viewportH - 170)),
            width: 252,
            padding: '10px 12px',
            borderRadius: 10,
            background: 'rgba(255,255,255,0.98)',
            border: '1px solid #e2e8f0',
            boxShadow: '0 8px 24px rgba(15,23,42,0.12)',
            fontSize: 12,
            lineHeight: 1.65,
            pointerEvents: 'none',
            zIndex: 5,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span
              style={{
                width: 8,
                height: 8,
                borderRadius: 999,
                background: nodeColor(hoveredNode.kind),
              }}
            />
            <b>{t(`node.${hoveredNode.kind}`)}</b>
            {hoveredNode.category && hoveredNode.category !== hoveredNode.kind ? (
              <span style={{ color: '#94a3b8' }}>{t('Category ') + hoveredNode.category}</span>
            ) : null}
          </div>
          <div style={{ fontSize: 13, fontWeight: 600, marginTop: 2, wordBreak: 'break-all' }}>
            {hoveredNode.name}
          </div>
          {hoveredNode.fqn ? (
            <div style={{ color: '#64748b', wordBreak: 'break-all' }}>{hoveredNode.fqn}</div>
          ) : null}
          <div style={{ color: '#64748b' }}>
            {t('In-edges ') + (hoveredNode.metrics?.fan_in ?? 0)} · {t('Out-edges ') + (hoveredNode.metrics?.fan_out ?? 0)} ·{' '}
            {t('Location') + ' ' + (hoveredNode.locations?.length ?? 0)}
          </div>
          {hoveredNode.annotations && hoveredNode.annotations.length > 0 ? (
            <div style={{ marginTop: 4 }}>
              {hoveredNode.annotations.slice(0, 4).map((a) => (
                <Tag key={a} style={{ marginBottom: 2 }} color="blue">
                  {a}
                </Tag>
              ))}
            </div>
          ) : null}
          <div
            style={{ marginTop: 6, color: hoveredNode.own_view ? '#1677ff' : '#94a3b8' }}
          >
            {hoveredNode.own_view
              ? `Click → level one switches to the “${hoveredNode.own_view}” perspective, level two becomes “${hoveredNode.name}”`
              : t('Click to expand the call chain · right-click for locations')}
          </div>
        </div>
      ) : null}

      {hoveredEdge ? (
        <div
          style={{
            position: 'absolute',
            left: Math.min(pointer.x + 16, Math.max(8, renderW - 268)),
            top: Math.min(pointer.y + 16, Math.max(8, viewportH - 150)),
            width: 252,
            padding: '10px 12px',
            borderRadius: 10,
            background: 'rgba(255,255,255,0.98)',
            border: '1px solid #e2e8f0',
            boxShadow: '0 8px 24px rgba(15,23,42,0.12)',
            fontSize: 12,
            lineHeight: 1.65,
            pointerEvents: 'none',
            zIndex: 5,
          }}
        >
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span
              style={{
                width: 8,
                height: 8,
                borderRadius: 999,
                background: edgeColor(hoveredEdge.kind),
              }}
            />
            <b>{edgeKindLabel(t, hoveredEdge.kind, hoveredEdge.also_kinds)}</b>
            <span style={{ color: hoveredEdge.resolved ? '#16a34a' : '#f59e0b' }}>
              {hoveredEdge.resolved ? t('status.resolved') : t('status.unverified')}
            </span>
          </div>
          <div style={{ marginTop: 2, wordBreak: 'break-all' }}>
            {nodeById.get(hoveredEdge.from)?.name ?? `#${hoveredEdge.from}`}
            {' → '}
            {nodeById.get(hoveredEdge.to)?.name ?? `#${hoveredEdge.to}`}
          </div>
          <div style={{ color: '#64748b' }}>
            {t('Confidence') + ' ' + hoveredEdge.confidence.toFixed(2)}
            {hoveredEdge.hops != null ? ` · ${t('via ') + hoveredEdge.hops + t(' hops')}` : ''}
          </div>
          <div style={{ marginTop: 6, color: '#94a3b8' }}>{t('Click to view the evidence chain')}</div>
        </div>
      ) : null}

      {/* Only the **legend** is permanently pinned at the bottom (dashed = indirect is the most easily misread point of this graph);
          layout notes and usage hints are folded into the ⓘ -- copy you read once, not worth a line. */}
      <div
        style={{
          padding: '8px 14px',
          fontSize: 12,
          color: 'rgba(0,0,0,0.5)',
          borderTop: '1px solid #eef0f4',
          display: 'flex',
          gap: 16,
          flexWrap: 'wrap',
          alignItems: 'center',
        }}
      >
        <span>{t('Dashed = indirect via call chain; solid = direct call')}</span>
        <Tooltip title={t(layout.note) + ' ' + t('Scroll to zoom · drag to pan · left-click to switch perspective · right-click to open location')}>
          <InfoCircleOutlined style={{ cursor: 'help', color: 'rgba(0,0,0,0.35)' }} />
        </Tooltip>
      </div>
      {/* Corner legend: lists the kinds actually present in this data -> icon / localized name, collapsible.
          Icons come from the same source as the in-node icons (nodeIcon), colored by that kind -- you can match them by looking at the graph. */}
      {layout.nodes.length > 0 ? (
        <div
          style={{
            position: 'absolute',
            right: 12,
            top: 12,
            background: 'rgba(255,255,255,0.92)',
            border: '1px solid #e5e8ee',
            borderRadius: 10,
            boxShadow: '0 2px 8px rgba(15,23,42,0.08)',
            fontSize: 12,
            color: '#334155',
            maxWidth: 240,
            zIndex: 5,
          }}
        >
          <div
            onClick={() => setLegendOpen((v) => !v)}
            style={{
              display: 'flex',
              alignItems: 'center',
              gap: 6,
              padding: '6px 10px',
              cursor: 'pointer',
              fontWeight: 600,
              userSelect: 'none',
            }}
          >
            <span>{t('Legend')}</span>
            <span style={{ color: 'rgba(0,0,0,0.35)' }}>{legendOpen ? '▾' : '▸'}</span>
            {/* A permanent "click to filter" label is deliberately absent: the checkbox's checked/empty states already say "clickable";
                writing it again would compete for the 240px title row with "legend ▾", "N points collapsed", "reset";
                "what happens on click" is explained by each row's hover hint (copy you read once shouldn't take a permanent slot). */}
            {/* Count of additionally collapsed points: points that lost all visible connections because some relations were turned off.
                Give visible feedback, otherwise a user clicks the legend, sees "fewer points too", and thinks the button is broken. */}
            {cascadeHidden > 0 ? (
              <span
                title={t('These nodes were only reachable through hidden relations, so they are collapsed too')}
                style={{
                  marginLeft: 'auto',
                  fontSize: 11,
                  fontWeight: 400,
                  color: '#94a3b8',
                }}
              >
                {t('{{n}} node(s) collapsed').replace('{{n}}', String(cascadeHidden))}
              </span>
            ) : null}
            {hasLegendFilter ? (
              <span
                onClick={(e) => {
                  e.stopPropagation();
                  onResetLegendFilters?.();
                }}
                style={{
                  marginLeft: cascadeHidden > 0 ? 6 : 'auto',
                  fontSize: 11,
                  fontWeight: 400,
                  color: '#2563eb',
                  cursor: 'pointer',
                }}
              >
                {t('Reset')}
              </span>
            ) : null}
          </div>
          {legendOpen ? (
            <div
              style={{
                padding: '2px 10px 10px',
                display: 'grid',
                gridTemplateColumns: 'auto 1fr',
                gap: '4px 8px',
                maxHeight: 248,
                overflow: 'auto',
              }}
            >
              <div style={LEGEND_SECTION_TITLE} title={t('Hiding a node type also collapses the edges touching it')}>
                {t('Node type')}
              </div>
              {/* Node types: click = show/hide that node class on the canvas (the center node is always kept as an anchor) */}
              {legendNodeKinds.map((k) => {
                const Icon = nodeIcon(k);
                const hidden = hiddenNodeSet.has(k);
                const hint = t('Click to show/hide this node type') + t(' (edges touching it collapse too)');
                return (
                  <Fragment key={`n:${k}`}>
                    <span
                      onClick={() => onToggleNodeKind?.(k)}
                      title={hint}
                      style={{ display: 'inline-flex', alignItems: 'center', cursor: 'pointer' }}
                    >
                      <LegendCheck checked={!hidden} />
                    </span>
                    <span
                      onClick={() => onToggleNodeKind?.(k)}
                      title={hint}
                      style={{
                        display: 'flex',
                        alignItems: 'center',
                        gap: 6,
                        lineHeight: '18px',
                        cursor: 'pointer',
                        opacity: hidden ? 0.45 : 1,
                      }}
                    >
                      {/* Icon 12px, same size as the canvas pills; the first column is a fixed 14px wide and centered so the
                          node section's (icon) and relation section's (14px line sample) text columns start aligned. */}
                      <span
                        style={{
                          display: 'inline-flex',
                          width: 14,
                          justifyContent: 'center',
                          color: nodeColor(k),
                        }}
                      >
                        <Icon style={{ fontSize: 12 }} />
                      </span>
                      <span style={{ textDecoration: hidden ? 'line-through' : 'none' }}>
                        {t(`node.${k}`)}
                      </span>
                    </span>
                  </Fragment>
                );
              })}
              {/* Edge types: click = show/hide that edge class on the canvas (e.g. turning off all "DB read" removes every ReadsDb edge).
                  Collapsing also removes points connected only via it, keeping the picture = the subgraph formed by visible edges (see the filter memo). */}
              {legendEdgeKinds.length > 0 ? (
                <Fragment>
                  <div style={{ height: 1, background: '#eef1f6', margin: '3px 0', gridColumn: '1 / -1' }} />
                  <div style={LEGEND_SECTION_TITLE} title={t('Hiding a relation type also collapses nodes reachable only through it')}>
                    {t('Relation type')}
                  </div>
                  {legendEdgeKinds.map((k) => {
                    const hidden = hiddenEdgeSet.has(k);
                    const hint = t('Click to show/hide this edge type') + t(' (nodes reachable only via it collapse too)');
                    return (
                      <Fragment key={`e:${k}`}>
                        <span
                          onClick={() => onToggleEdgeKind?.(k)}
                          title={hint}
                          style={{ display: 'inline-flex', alignItems: 'center', cursor: 'pointer' }}
                        >
                          <LegendCheck checked={!hidden} />
                        </span>
                        <span
                          onClick={() => onToggleEdgeKind?.(k)}
                          title={hint}
                          style={{
                            display: 'flex',
                            alignItems: 'center',
                            gap: 6,
                            lineHeight: '18px',
                            cursor: 'pointer',
                            opacity: hidden ? 0.45 : 1,
                          }}
                        >
                          <span
                            style={{
                              width: 14,
                              height: 0,
                              borderTop: `2px solid ${edgeColor(k)}`,
                              display: 'inline-block',
                            }}
                          />
                          <span style={{ textDecoration: hidden ? 'line-through' : 'none' }}>
                            {t(`edge.${k}`)}
                          </span>
                        </span>
                      </Fragment>
                    );
                  })}
                </Fragment>
              ) : null}
              {subProjects && subProjects.length > 0 ? (
                <Fragment>
                  <div style={{ height: 1, background: '#eef1f6', margin: '3px 0', gridColumn: '1 / -1' }} />
                  {/* Sub-projects are a color key only and not clickable: leaving the first column empty (no checkbox) is itself the visual
                      distinction for "this section doesn't participate in filtering", avoiding confusion with the two clickable filter groups above. */}
                  <div style={LEGEND_SECTION_TITLE} title={t('Color key only — not clickable')}>
                    {t('Sub-project')}
                  </div>
                  {subProjects.map((sp) => (
                    <Fragment key={sp.id}>
                      <span />
                      <span style={{ display: 'flex', alignItems: 'center', gap: 6, lineHeight: '18px' }}>
                        <span
                          style={{
                            width: 8,
                            height: 8,
                            borderRadius: 99,
                            background: subProjectColors.get(sp.id) ?? '#94a3b8',
                            flex: '0 0 auto',
                          }}
                        />
                        <span>
                          {sp.name}{' '}
                          <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                            （{roleLabel(sp.role)}）
                          </Typography.Text>
                        </span>
                      </span>
                    </Fragment>
                  ))}
                </Fragment>
              ) : null}
            </div>
          ) : null}
        </div>
      ) : null}
    </div>
  );
}
