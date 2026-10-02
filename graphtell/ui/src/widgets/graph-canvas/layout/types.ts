import type { LayoutMode } from '@/entities/view';
import { truncateMiddle } from '@/shared/lib/format';

export interface LayoutNode {
  id: number;
  kind: string;
  name: string;
  ring: number;
}

export interface LayoutEdge {
  id: number;
  from: number;
  to: number;
  /**
   * Index of this edge inside the input `edges` array (filled in by the caller; the layout passes it through).
   *
   * In the folded view the same pair of endpoints with **the same `id`** can correspond to several
   * **different paths** (different `via`): in a forward perspective one propagation edge (seed) is
   * expanded by `enumerate_chain_paths` into several chains, while `push_edge` in `view_service.rs`
   * reuses one evidence edge id for all of them. Keying only on `id:from->to` collides — which shows up
   * as "hovering one highlights them all, and the hover card always shows the first chain".
   * Carrying the original index makes the key unique, and at render time `edges[seq]` points back
   * precisely at the `EdgeView` that holds this path's own `via`.
   */
  seq?: number;
}

export interface PlacedNode extends LayoutNode {
  x: number;
  y: number;
    /** Node shape: uniformly a `rect` pill (text embedded in the box); identical across all layouts. */
  shape: 'circle' | 'rect';
  w?: number;
  h?: number;
}

export interface PlacedEdge extends LayoutEdge {
  /** Polyline vertices; two points means a straight line, more means an orthogonal polyline. */
  points: Array<[number, number]>;
  /** Orthogonal polyline (90°) or straight line. */
  orthogonal: boolean;
}

export interface PlacedGroup {
  key: string;
  label: string;
  x: number;
  y: number;
  w: number;
  h: number;
  count: number;
}

export interface PlacedCell {
  row: number;
  col: number;
  value: number;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface LayoutResult {
  nodes: PlacedNode[];
  edges: PlacedEdge[];
  groups?: PlacedGroup[];
  cells?: PlacedCell[];
  /** Radial-layout concentric ring guides (visual reference only; not interactive / not hit-testable). */
  guides?: Array<{ cx: number; cy: number; r: number; label: string }>;
  rowHeaders?: Array<{ label: string; x: number; y: number }>;
  colHeaders?: Array<{ label: string; x: number; y: number }>;
  width: number;
  height: number;
  /**
   * True bounding box of the content (world coordinates), used by the canvas for zoom-to-fit.
   *
   * `width/height` are the **canvas dimensions** (normally the container width/height) while the
   * content is often just a small block centred inside; fitting to the canvas size therefore leaves the
   * content as a tiny patch in the middle and the graph looks "very small". When absent the canvas
   * falls back to the whole canvas (old behaviour).
   */
  content?: { x: number; y: number; w: number; h: number };
  /** Description of the layout algorithm (shown to the user, so it stays explainable). */
  note: string;
}

export interface LayoutInput {
  center: LayoutNode;
  rings: LayoutNode[][];
  edges: LayoutEdge[];
  /** Cluster boxes (aggregate perspective). */
  clusters?: Array<{ key: string; label: string; count: number; members: LayoutNode[] }>;
  /** Matrix (multi-end comparison perspective). */
  matrix?: {
    rows: string[];
    cols: string[];
    cells: number[][];
  };
  /**
   * Whether to draw the kind icon inside a node pill. On by default. The caller sets it to false when
   * the kinds actually present in this view number <= 2 — in a homogeneous view (e.g. "who calls X"
   * is almost all Method) the icons are all identical, pure horizontal waste plus visual noise, and
   * colour already distinguishes them; icons are drawn only when kinds >= 3 (icon + colour is what
   * makes multiple types distinguishable at a glance).
   */
  showIcons?: boolean;
  width: number;
  height: number;
}

export type LayoutFn = (input: LayoutInput) => LayoutResult;

/**
 * Layout registry.
 *
 * **No node may drift freely under a force-directed layout**: positions are fully determined by the
 * algorithm and the input order, so the same input always yields the same output — reproducible,
 * diffable in screenshots, and testable in unit tests.
 */
export const LAYOUTS: Record<LayoutMode, LayoutFn> = {
  radial: radialLayout,
  layered: layeredLayout,
  spine: spineLayout,
  compound: compoundLayout,
  matrix: matrixLayout,
  er: erLayout,
};

const PILL_H = 26;

/** Approximate character width: CJK / full-width counts as 1.8 Latin units (at the same font size CJK is about 1.8x as wide). */
function units(s: string): number {
  let u = 0;
  for (const ch of s) u += (ch.codePointAt(0) ?? 0) > 0x2e7f ? 1.8 : 1;
  return u;
}

/**
 * The same font stack as `global.css` — measurement must use the font **actually used for rendering**,
 * otherwise the measurement is wasted.
 */
const FONT_STACK =
  `-apple-system, BlinkMacSystemFont, 'Segoe UI', 'PingFang SC', 'Hiragino Sans GB', ` +
  `'Microsoft YaHei', Roboto, 'Helvetica Neue', Arial, sans-serif`;

/**
 * True text width (canvas `measureText`).
 *
 * The old implementation estimated "visual units x a fixed factor": the factor was generally too wide
 * for the real font (Latin body text ≈ 0.5em/char, the old factor worked out to 0.56–0.57em), so every
 * pill trailed a stretch of fake blank space and looked like it "did not adapt to its content".
 * Here the real pixel width is measured with the same font, size and weight used for rendering, cached
 * by (weight, px, text) (the same name is measured repeatedly during resize / reflow). Environments
 * without canvas (jsdom unit tests / SSR) fall back to the old factor estimate so tests stay
 * deterministic; a null return means "could not measure".
 */
let measureCtx: CanvasRenderingContext2D | null | undefined;
const measureCache = new Map<string, number>();
function measureTextPx(text: string, px: number, weight: number): number | null {
  const key = `${weight}/${px}/${text}`;
  const hit = measureCache.get(key);
  if (hit != null) return hit;
  try {
    if (typeof document === 'undefined') return null;
    if (measureCtx === undefined) measureCtx = document.createElement('canvas').getContext('2d');
    if (!measureCtx) return null;
    measureCtx.font = `${weight} ${px}px ${FONT_STACK}`;
    const w = measureCtx.measureText(text).width;
    if (w > 0) {
      measureCache.set(key, w);
      return w;
    }
    return null;
  } catch {
    return null;
  }
}

/**
 * Pill node width. Must line up with what GraphCanvas actually renders:
 * `icon + name`, with the name at font size 13 / weight 600 (centre) or 11 / 500 (others), and the
 * renderer truncating in the middle at 40 **visual units** — so the width is computed after the same
 * truncation, keeping the two sides on the same basis. Text width prefers a real `measureText`
 * measurement; when that is impossible it falls back to the factor estimate. The 300 cap stops an
 * extreme name from bursting the canvas.
 */
const ICON_AREA = 20; // icon 12 + left padding 6 + gap 3 (text starts at 21)
const ICON_LEFT = 8; // with no icon, only left padding (text starts at 8)
const RIGHT_PAD = 18; // right padding (includes a 1–2px render-error buffer)
const MIN_NAME_UNITS = 4; // keep at least ~4 Latin characters of text width (keeps it clickable / readable; a CJK char ≈ 1.8 units, naturally wider)
function pillWidth(kind: string, name: string, center = false, icon = true): number {
  const px = center ? 13 : 11;
  const weight = center ? 600 : 500;
  // Truncate first, using the same rule as the renderer, then measure — measuring the untruncated full name only yields phantom width.
  const shown = truncateMiddle(name, 40);
  const textPx =
    measureTextPx(shown, px, weight) ?? Math.min(units(name), 40) * (center ? 7.4 : 6.2);
  // The lower bound is derived entirely from the content: left padding + right padding + the text width of at least MIN_NAME_UNITS characters.
  // Having an icon or not only changes `left` (21 with an icon, 8 without), so the tiered constants are absorbed and need no separate upkeep.
  const left = icon ? ICON_AREA + 1 : ICON_LEFT;
  const minNamePx = (center ? 7.4 : 6.2) * MIN_NAME_UNITS;
  return Math.min(300, Math.ceil(left + RIGHT_PAD + Math.max(minNamePx, textPx)));
}

// ---------------------------------------------------------------- radial

/**
 * Whether the graph is a **star**: every edge has the centre as one of its ends.
 *
 * This is a semantic judgement, not a geometric one — in `view_service.rs` a resource perspective
 * (Table / Cache / Event / Queue / Topic) runs in reverse mode (`reverse = kind != HttpContract`), walks
 * inbound edges back to each user and then **folds each user into a single edge to the centre**
 * (`MAX_USERS = 80`, the rest counted in `hidden`); `ring` is only a number attached to the node and
 * has no corresponding leaf-to-leaf edge. So the graph handed to the layout looks like "several rings"
 * while structurally it is a star.
 */
function isStar(centerId: number, edges: LayoutEdge[]): boolean {
  return edges.every((e) => e.from === centerId || e.to === centerId);
}

/**
 * Upper bound on the number of **leaf-to-leaf edges** tolerated by an "approximate star".
 *
 * Requiring every edge to touch the centre (see [`isStar`]) is too brittle: in the route perspective a
 * `ForeignKey` between two tables, or in the table perspective a single foreign key — **one**
 * leaf-to-leaf edge kills the test, and a graph with dozens of leaves falls back to the wide fan of one
 * row per layer (6000px+ per row with both ends clipped; a bad case measured in the `LAYERED_FAN_MAX`
 * notes).
 *
 * Why the threshold is 3: a few leaf-to-leaf edges are **decoration** (one or two foreign keys among 30
 * neighbours) and do not change the structural fact "this is a star" — hub-spoke accommodates them
 * completely (see how `hubSpokeLayout` routes leaf-to-leaf edges around). A genuine multi-layer link
 * graph (a transitive ring-1 -> ring-2 structure) has leaf-to-leaf edges as its **bulk** (dozens of
 * them), far above this threshold, and still takes concentric rings / layers — the behaviour "manually
 * switching a link perspective to radial must not silently change its shape" is pinned by tests and
 * unaffected by this relaxation.
 */
const NEAR_STAR_LEAF_EDGES = 3;

/** Approximate star: almost all edges touch the centre, with at most [`NEAR_STAR_LEAF_EDGES`] leaf-to-leaf edges. */
function isNearStar(centerId: number, edges: LayoutEdge[]): boolean {
  const leafEdges = edges.filter((e) => e.from !== centerId && e.to !== centerId).length;
  return leafEdges <= NEAR_STAR_LEAF_EDGES;
}

/**
 * Single entry point for the radial layout: the layout family is chosen by **graph shape**, rather than
 * making the user try them by hand.
 *
 * Once a star (= the resource perspective above) fans out, concentric rings are the worst choice:
 * 1. Each leaf takes about `pillWidth + GAP ≈ 188px` of arc, so the radius is ≈ 30×n and the canvas
 *    side is twice the radius ⇒ **area grows as n²**; the content only occupies a 30px-wide band on the
 *    ring, so with 80 users roughly 98% of the frame is empty.
 * 2. Leaves on the outer ring have an edge running out from the centre, while the inner ring's radius
 *    is computed as "just enough to fill up" — so that edge very likely crosses straight over some
 *    inner-ring pill's name ("an edge must not pass through a node" is a hard constraint).
 *
 * So a star with fan-out >= `HUB_MIN` switches to hub-and-spoke: also provably 0 crossings and 0
 * node penetrations, with a canvas an order of magnitude smaller; the only cost is vertical scrolling
 * (a mechanical cost — see the trade-off notes in `hubSpokeLayout`).
 *
 * Small stars (< `HUB_MIN`, e.g. the event perspective with "2 producers + 3 consumers") keep
 * concentric circles — the case where a circle looks best and is least replaceable.
 */
export function radialLayout(input: LayoutInput): LayoutResult {
  const leaves = input.rings.flat();
  // "Approximate star" rather than a strict star: one inter-table ForeignKey must not push a graph with dozens of leaves
  // back onto concentric rings (the area ∝ n² bad case). A few leaf-to-leaf edges are absorbed by hub-spoke detours.
  if (leaves.length >= HUB_MIN && isNearStar(input.center.id, input.edges)) {
    return hubSpokeLayout(input, leaves, true);
  }
  return concentricLayout(input);
}

/** Concentric rings: ring = hops. The radial form for small graphs and genuine multi-layer graphs (with leaf-to-leaf edges). */
export function concentricLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const maxRing = Math.max(1, rings.length);
  // Every node is a rectangular pill with embedded text, exactly as in layered / spine / matrix / ER.

  // Each ring's radius is computed independently: the more pills on a ring the wider it is, so the ring must be larger to lay them out along the circumference without overlap;
  // it also has to stay radially clear of the previous ring, and ring 1 must not be covered by the centre pill. That way no ring is crowded whatever its node count.
  const GAP = 18;
  const PAD = 60;
  const maxPillW = Math.max(110, ...rings.flat().map((n) => pillWidth(n.kind, n.name, false, input.showIcons ?? true)), pillWidth(center.kind, center.name, true, input.showIcons ?? true));
  const ringRadii: number[] = [];
  let r = 0;
  for (let i = 0; i < rings.length; i++) {
    const ring = rings[i];
    const arcNeeded = ring.reduce((s, n) => s + pillWidth(n.kind, n.name, false, input.showIcons ?? true) + GAP, 0);
    const circNeeded = arcNeeded / (2 * Math.PI);
    const radialFloor = i === 0 ? maxPillW / 2 + 24 : r + PILL_H + 14;
    r = Math.max(radialFloor, circNeeded);
    ringRadii.push(r);
  }
  const maxR = ringRadii.length ? ringRadii[ringRadii.length - 1] : 0;

  // The canvas grows to fit the content, so a dense graph is not clipped by the viewBox (pan / zoom afterwards).
  const ext = maxR + maxPillW / 2 + PAD;
  const contentW = Math.max(width, ext * 2);
  const contentH = Math.max(height, ext * 2);
  const cx = contentW / 2;
  const cy = contentH / 2;

  const nodes: PlacedNode[] = [
    { ...center, x: cx, y: cy, shape: 'rect', w: pillWidth(center.kind, center.name, true, input.showIcons ?? true), h: PILL_H },
  ];

  const ringCount = rings.length;
  rings.forEach((ring, i) => {
    const radius = ringRadii[i];
    const count = ring.length;
    if (count === 0) return;
    // Nodes are spread evenly within a ring (equal angles); each ring as a whole is rotated by a phase tied to its index,
    // so nodes on different rings fan out in different directions — avoiding "single-node rings all piling up at 12 o'clock" and the resulting collinear overlapping edges.
    // Fixing this at the layout layer is more honest and clearer than bending edges afterwards.
    const phase = ringCount > 1 ? (2 * Math.PI * i) / ringCount : 0;
    ring.forEach((n, idx) => {
      const angle = (2 * Math.PI * idx) / count - Math.PI / 2 + phase;
      nodes.push({
        ...n,
        x: cx + radius * Math.cos(angle),
        y: cy + radius * Math.sin(angle),
        shape: 'rect',
        w: pillWidth(n.kind, n.name, false, input.showIcons ?? true),
        h: PILL_H,
      });
    });
  });

  const pos = new Map<number, [number, number]>();
  const dims = new Map<number, { w: number; h: number }>();
  nodes.forEach((n) => {
    pos.set(n.id, [n.x, n.y]);
    dims.set(n.id, { w: n.w ?? 0, h: n.h ?? PILL_H });
  });

  // Edges are uniformly straight: different rings are already phase-offset by ring index (see above), so endpoints rarely line up again
  // and nothing needs bending afterwards — straight lines are more honest and clearer. Only edges with both endpoints present are kept.
  // Endpoints are pulled back from the **node centre** to the **pill border**: without shrinking, the line body runs under the pill, hidden
  // while the node is opaque, and shows through as soon as focus dims it (semi-transparent).
  const placed: PlacedEdge[] = edges.flatMap((e) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    const da = dims.get(e.from);
    const db = dims.get(e.to);
    const [a2, b2] = da && db ? shrinkToRects(a, da.w, da.h, b, db.w, db.h) : [a, b];
    // `...e` passes `seq` (and the remaining LayoutEdge fields) through: the renderer's `edgeKey` depends on `seq` as the unique edge key,
    // and losing it means hover focus cannot find this edge or reach its two endpoint nodes (only the edge highlights while its nodes get dimmed).
    return [{ ...e, points: [a2, b2], orthogonal: false }];
  });

  // Concentric ring guides: draw "ring = hops" explicitly (ring 1 = directly related).
  // Visual reference only: thin light-grey lines, not interactive; the ring number is labelled outside the top of each ring.
  const guides: LayoutResult['guides'] = Array.from({ length: maxRing }, (_, i) => ({
    cx,
    cy,
    r: ringRadii[i],
    label: `${i + 1}`,
  }));

  return {
    nodes,
    edges: placed,
    guides,
    width: contentW,
    height: contentH,
    note: 'Radial layout: center is the current object; concentric rings denote hops (ring 1 = direct). Ring radius adapts to the number of pills per ring to avoid overlap.',
  };
}

// ---------------------------------------------------------------- layered

// ---------------------------------------------------------------- geometry

type Pt = [number, number];

/**
 * Shrink both ends of a straight line from the **node centres** to the **borders** of the two pills.
 *
 * The concentric-ring layout places nodes on a circumference and draws edges as centre-to-centre
 * straight lines; the line body originally ran underneath the pills, hidden while the pill was opaque,
 * and as soon as hover focus made the node semi-transparent the line showed through (a real bug).
 * After shrinking, the line starts at the source pill's border and ends at the target pill's border, so
 * the arrow (the renderer's `clipArrowTip` takes the endpoint directly when it is outside / on the
 * rectangle) lands naturally on the pill edge.
 *
 * Each end computes its exit parameter along the direction: the distance from the centre to the x/y
 * border divided by the direction component, taking the smaller. If the two ends cross each other after
 * shrinking (nearly overlapping nodes, or a line entirely inside the rectangle), fall back to the
 * original endpoints.
 */
function shrinkToRects(a: Pt, aw: number, ah: number, b: Pt, bw: number, bh: number): [Pt, Pt] {
  const dx = b[0] - a[0];
  const dy = b[1] - a[1];
  const exit = (hw: number, hh: number, vx: number, vy: number): number => {
    const tx = vx !== 0 ? hw / Math.abs(vx) : Infinity;
    const ty = vy !== 0 ? hh / Math.abs(vy) : Infinity;
    return Math.min(tx, ty);
  };
  const tA = exit(aw / 2, ah / 2, dx, dy);
  const tB = exit(bw / 2, bh / 2, dx, dy);
  if (!Number.isFinite(tA) && !Number.isFinite(tB)) return [a, b];
  if (tA + tB >= 1) return [a, b]; // after shrinking, the two ends meet / cross: nodes overlap, leave as is
  return [
    [a[0] + dx * tA, a[1] + dy * tA],
    [b[0] - dx * tB, b[1] - dy * tB],
  ];
}

/**
 * Whether a segment passes through an axis-aligned rectangle (Liang–Barsky clipping).
 *
 * "An edge passing through a node" is a far worse problem than "edges crossing": once a line runs
 * through a pill its name is simply unreadable. So this one is a **hard constraint** and must be
 * eliminated 100% — unlike the crossing count, which can only be minimised.
 */
function segHitsRect(a: Pt, b: Pt, cx: number, cy: number, w: number, h: number): boolean {
  const minX = cx - w / 2;
  const maxX = cx + w / 2;
  const minY = cy - h / 2;
  const maxY = cy + h / 2;
  const dx = b[0] - a[0];
  const dy = b[1] - a[1];
  const slabs: Array<[number, number]> = [
    [-dx, a[0] - minX],
    [dx, maxX - a[0]],
    [-dy, a[1] - minY],
    [dy, maxY - a[1]],
  ];
  let t0 = 0;
  let t1 = 1;
  for (const [p, q] of slabs) {
    if (p === 0) {
      if (q < 0) return false;
      continue;
    }
    const r = q / p;
    if (p < 0) {
      if (r > t1) return false;
      if (r > t0) t0 = r;
    } else {
      if (r < t0) return false;
      if (r < t1) t1 = r;
    }
  }
  return true;
}

/** Whether two segments **truly cross**; collinear or merely touching at an endpoint does not count (that is "meeting", not crossing). */
function segCross(p1: Pt, p2: Pt, p3: Pt, p4: Pt): boolean {
  const o = (a: Pt, b: Pt, c: Pt) =>
    Math.sign((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]));
  return o(p3, p4, p1) * o(p3, p4, p2) < 0 && o(p1, p2, p3) * o(p1, p2, p4) < 0;
}

/** Total crossings between sets of polyline edges. Pairs sharing an endpoint are skipped: they meet at a node, which is not a crossing. */
function countCrossings(polys: Array<{ from: number; to: number; pts: Pt[] }>): number {
  let n = 0;
  for (let i = 0; i < polys.length; i += 1) {
    for (let j = i + 1; j < polys.length; j += 1) {
      const A = polys[i];
      const B = polys[j];
      if (A.from === B.from || A.from === B.to || A.to === B.from || A.to === B.to) continue;
      for (let s = 1; s < A.pts.length; s += 1) {
        for (let t = 1; t < B.pts.length; t += 1) {
          if (segCross(A.pts[s - 1], A.pts[s], B.pts[t - 1], B.pts[t])) n += 1;
        }
      }
    }
  }
  return n;
}

/**
 * Route an edge around a node it would pass through: **shift it in parallel around the collision
 * point**, rather than merely bending the midpoint.
 *
 * The typical case where pushing only the midpoint fails: the target node sits on the second row after
 * wrapping, the collision happens at t≈0.9, and the midpoint displacement decays to 0 at both ends —
 * it never clears the node however far it is pushed. A parallel shift translates the whole segment
 * t∈[tc-0.3, tc+0.3] along the normal, so the displacement is **full** at the collision point and
 * actually works.
 *
 * The shift grows from small to large and both sides are tried, preferring the smallest disturbance. If
 * it truly cannot be routed around (nodes too dense) the last attempt is returned, which is still better
 * than running straight through the node.
 */
function detourAroundNodes(a: Pt, b: Pt, obstacles: PlacedNode[]): Pt[] {
  const straight: Pt[] = [a, b];
  if (obstacles.length === 0) return straight;

  /** Approximate parameter position of the first collision on the a→b line; null when there is no collision. */
  const firstHit = (pts: Pt[]): number | null => {
    for (const o of obstacles) {
      for (let s = 1; s < pts.length; s += 1) {
        if (!segHitsRect(pts[s - 1], pts[s], o.x, o.y, o.w ?? 120, o.h ?? 26)) continue;
        const dx = b[0] - a[0];
        const dy = b[1] - a[1];
        const l2 = dx * dx + dy * dy || 1;
        return Math.min(0.98, Math.max(0.02, ((o.x - a[0]) * dx + (o.y - a[1]) * dy) / l2));
      }
    }
    return null;
  };

  const tc = firstHit(straight);
  if (tc === null) return straight;

  const dx = b[0] - a[0];
  const dy = b[1] - a[1];
  const len = Math.hypot(dx, dy) || 1;
  const nx = -dy / len;
  const ny = dx / len;
  const at = (t: number): Pt => [a[0] + dx * t, a[1] + dy * t];
  const t1 = Math.max(0.02, tc - 0.3);
  const t2 = Math.min(0.98, tc + 0.3);
  const q1 = at(t1);
  const q2 = at(t2);

  let fallback = straight;
  for (const step of [1, 2, 3, 4, 6]) {
    for (const sign of [1, -1]) {
      const off = sign * step * (PILL_H / 2 + 16);
      const bowed: Pt[] = [
        a,
        [q1[0] + nx * off, q1[1] + ny * off],
        [q2[0] + nx * off, q2[1] + ny * off],
        b,
      ];
      if (firstHit(bowed) === null) return bowed;
      fallback = bowed;
    }
  }
  return fallback;
}

/** Whether a polyline passes through any obstacle node (hard-constraint check). */
function pathHits(pts: Pt[], obstacles: PlacedNode[]): boolean {
  for (let s = 1; s < pts.length; s += 1) {
    for (const o of obstacles) {
      if (segHitsRect(pts[s - 1], pts[s], o.x, o.y, o.w ?? 120, o.h ?? 26)) return true;
    }
  }
  return false;
}

/** Keep only nodes intersecting the path bounding box (plus margin), so every edge does not do an O(N) collision check. */
function obstaclesForPath(
  pts: Pt[],
  nodes: PlacedNode[],
  skip: Set<number>,
  margin = 24,
): PlacedNode[] {
  let minX = Number.POSITIVE_INFINITY;
  let maxX = Number.NEGATIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  let maxY = Number.NEGATIVE_INFINITY;
  for (const [x, y2] of pts) {
    if (x < minX) minX = x;
    if (x > maxX) maxX = x;
    if (y2 < minY) minY = y2;
    if (y2 > maxY) maxY = y2;
  }
  return nodes.filter((n) => {
    if (skip.has(n.id)) return false;
    const hw = (n.w ?? 120) / 2;
    const hh = (n.h ?? 26) / 2;
    return !(
      n.x + hw < minX - margin ||
      n.x - hw > maxX + margin ||
      n.y + hh < minY - margin ||
      n.y - hh > maxY + margin
    );
  });
}



/**
 * Grouping of parallel edges (several paths between the same pair of endpoints).
 *
 * The backend now emits one edge per **distinct path** between the same endpoints (it used to dedupe by
 * (kind, from, to) and keep one, losing all branching information). But their endpoints are identical —
 * drawn as-is they **overlap completely** and the user cannot see there are two. So they must be offset
 * group by group.
 */
function indexParallel(edges: LayoutEdge[]): Map<number, { idx: number; count: number }> {
  const groups = new Map<string, number[]>();
  edges.forEach((_, i) => {
    const e = edges[i];
    const k = e.from < e.to ? `${e.from}-${e.to}` : `${e.to}-${e.from}`;
    const list = groups.get(k);
    if (list) list.push(i);
    else groups.set(k, [i]);
  });
  const out = new Map<number, { idx: number; count: number }>();
  for (const list of groups.values()) {
    list.forEach((i, idx) => out.set(i, { idx, count: list.length }));
  }
  return out;
}

/** Normal offset of the `idx`-th edge in a group (centred, symmetric); 0 when there is only one. */
function fanOffset(count: number, idx: number, fan = 11): number {
  return count <= 1 ? 0 : (idx - (count - 1) / 2) * fan;
}

/**
 * Sample a cubic Bézier curve into a polyline.
 *
 * The layout still only produces `points` (the renderer's `M/L` polyline, label anchors and arrow
 * tangents need know nothing about curves): 16 segments are visually indistinguishable from the true
 * curve under `non-scaling-stroke`, and the test-side `segmentsThroughNodes` / `crossings` (judged per
 * segment) apply to the sampled points as usual.
 */
function median(xs: number[]): number {
  if (xs.length === 0) return Number.MAX_SAFE_INTEGER;
  const s = xs.slice().sort((p, q) => p - q);
  const m = Math.floor(s.length / 2);
  return s.length % 2 === 1 ? s[m] : (s[m - 1] + s[m]) / 2;
}

function rangeDown(from: number, to: number): number[] {
  const out: number[] = [];
  if (to >= from) for (let i = from; i < to; i += 1) out.push(i);
  else for (let i = from; i > to; i -= 1) out.push(i);
  return out;
}

/**
 * Barycentre (median) ordering — the standard heuristic for Sugiyama's second phase.
 *
 * **The crossing count is decided by the order within a layer; changing the geometry does not help.**
 * Several sweeps run back and forth: forward ordered by "median order of the parents", backward by
 * "median order of the children". Zero cannot be guaranteed (crossing minimisation is NP-hard), but the
 * crossings can be pushed very low, and the remainder is reported honestly to the user rather than
 * pretended away.
 */
function orderByBarycenter(layers: LayoutNode[][], edges: LayoutEdge[]): LayoutNode[][] {
  if (layers.length <= 1) return layers.map((l) => l.slice());
  const parentsOf = new Map<number, number[]>();
  const childrenOf = new Map<number, number[]>();
  edges.forEach((e) => {
    parentsOf.set(e.to, [...(parentsOf.get(e.to) ?? []), e.from]);
    childrenOf.set(e.from, [...(childrenOf.get(e.from) ?? []), e.to]);
  });

  const work = layers.map((l) => l.slice());
  const snapshot = () => {
    const m = new Map<number, number>();
    work.forEach((l) => l.forEach((n, i) => m.set(n.id, i)));
    return m;
  };

  for (let iter = 0; iter < 4; iter += 1) {
    const forward = iter % 2 === 0;
    for (const li of rangeDown(forward ? 1 : work.length - 2, forward ? work.length : -1)) {
      const pos = snapshot();
      const anchor = new Set(work[li + (forward ? -1 : 1)].map((n) => n.id));
      const rel = forward ? parentsOf : childrenOf;
      work[li] = work[li]
        .map((n, i) => ({
          n,
          i,
          b: median(
            (rel.get(n.id) ?? []).filter((id) => anchor.has(id)).map((id) => pos.get(id) ?? 0),
          ),
        }))
        // `Array.prototype.sort` has been guaranteed stable since ES2019: nodes without neighbours keep their relative order.
        .sort((x, y) => (x.b === y.b ? x.i - y.i : x.b - y.b))
        .map((x) => x.n);
    }
  }
  return work;
}

// ---------------------------------------------------------------- layered

/** Fan-out beyond this on a single ring switches to the radial layout (one row no longer fits). */
const HUB_MIN = 7;

/**
 * Hub-and-spoke: the centre on the left, neighbours in a **single column** on the right, edges as
 * **single straight segments** (from the centre pill's border straight to the target pill's border, no
 * 90°/0° corners):
 *
 * ```
 * centre ●━━━━━━━━━━▶ target (the end lands on the target's near edge; when the line enters the target's x range its y is exactly the target row)
 * ```
 *
 * Why a single straight segment can cut through zero nodes: the endpoint is the target pill's *near
 * edge*, not its centre — the segment's x increases monotonically up to that edge, and by the time it
 * enters the target's x range its y already equals the target row exactly, so it only passes through
 * that one pill; the corridor between the centre and the column is empty, so it touches no other pill.
 * Start points spread monotonically along the centre pill's edge in column order ⇒ they do not cross
 * each other. This is a structural guarantee (independent of `gapX`), not an optimisation result. If
 * the endpoint wrongly used the pill centre, a shallow far-end line would sweep into the left half of a
 * neighbour pill at that neighbour's row, so it must connect to the near edge. The real source of
 * spacing is `ROW_GAP` (target row spacing) and `gapX` (corridor width, which sets the angle).
 *
 * Why not an arc: an arc is likewise provably crossing-free, but 32 nodes around a half circle need a
 * radius of about 1700px and a canvas of about 1800×3400; a single column needs only about 600 wide and
 * n×60 high, and the cost is merely vertical scrolling — and scrolling is a mechanical cost while a
 * crossing is an ambiguity cost.
 */
function hubSpokeLayout(input: LayoutInput, fanout: LayoutNode[], viaStar = false): LayoutResult {
  const { center, edges, width, height } = input;
  const PAD = 40;
  const ROW_GAP = 30; // row gap: pill height 30 ⇒ row gap / pill height = 2.0. A large graph trades a bigger row gap for “wider edge spacing”, at the cost of a taller canvas (acceptable)
  const HUB_GAP_X = 140; // minimum horizontal spacing of the channel (center to column); the actual value adapts to column height (see gapX)

  // ---- Split by side: divide neighbours by edge direction into "callers (left column)" and "dependees (right column)" ----
  //
  // The old implementation put every neighbour in one column right of the centre — fine for a resource
  // perspective ("who uses this table"), but an entry-type centre (HTTP contract) read backwards:
  // frontend callers and backend dependencies were mixed in one column, and the request flow
  // `frontend --CallsHttp--> route --ReadsConfig--> config key` had no sense of direction on the canvas.
  // The left column holds "sources flowing into the centre" (inbound neighbours), the right column
  // "targets the centre flows to" (outbound neighbours), so every arrow naturally points left to right:
  // left→right *is* the request / data flow. A node carrying both kinds is classified as a dependee so
  // the same pill never appears twice.
  const callerIds = new Set<number>();
  const targetIds = new Set<number>();
  edges.forEach((e) => {
    if (e.to === center.id && e.from !== center.id) callerIds.add(e.from);
    if (e.from === center.id && e.to !== center.id) targetIds.add(e.to);
  });
  targetIds.forEach((id) => callerIds.delete(id));
  // Sort key: ring (= hops / trace depth) first, then kind, then name — same as the old implementation (deterministic, reproducible).
  const byRingKindName = (a: LayoutNode, b: LayoutNode) =>
    a.ring - b.ring ||
    (a.kind === b.kind ? 0 : a.kind.localeCompare(b.kind)) ||
    a.name.localeCompare(b.name);
  const callers = fanout.filter((n) => callerIds.has(n.id)).sort(byRingKindName);
  // Neighbours touched by no edge (does not happen in a normal star) default to the right column.
  const rightNodes = fanout
    .filter((n) => targetIds.has(n.id) || (!callerIds.has(n.id) && !targetIds.has(n.id)))
    .sort(byRingKindName);

  const centerW = pillWidth(center.kind, center.name, true, input.showIcons ?? true);
  // Split into two columns only when **both sides are non-empty** — only then does a real "source → centre → destination" through-flow exist.
  // A purely inbound star (resource perspective: everything is "who uses it") or a purely outbound star has no "flow", and forcing two columns
  // only adds a column width out of nowhere (measured: the resource perspective with 80 users exceeds the container width by 4%),
  // so the old single-column form is kept.
  const twoSided = callers.length > 0 && rightNodes.length > 0;
  // Single-sided form: all neighbours (including "callers") go into the right column — same as the old implementation.
  const rightCol = twoSided ? rightNodes : fanout.slice().sort(byRingKindName);
  const leftCol = twoSided ? callers : [];
  const leftW = leftCol.length ? Math.max(...leftCol.map((n) => pillWidth(n.kind, n.name, false, input.showIcons ?? true))) : 0;

  // The corridor (horizontal gap from centre to column) is the main horizontal length of an edge. It does **not grow without bound together with the column height**:
  // see the "equal-length fan" branch below — with a moderate fan-out (about 12~60 neighbours) a limited-angle arc is used instead,
  // making every neighbour's edge to the centre the same length and thus removing the single-column problem where the top / bottom nodes drag out
  // extremely long edges; with a very large fan-out (> ~60) the arc would grow into a big round canvas whose area exceeds the single column, so it falls back to the single column.
  const MAX_GUTTER = 420;
  const rows = Math.max(leftCol.length, rightCol.length);
  const colHalfSpan = (rows * (PILL_H + ROW_GAP)) / 2;
  const gapX = Math.max(HUB_GAP_X, Math.min(Math.round(colHalfSpan * 0.6), MAX_GUTTER));

  // Equal-length fan branch: place right-column neighbours on an "arc centred on the hub with radius arcR" (angle limited to SPAN_MAX,
  // so pills at the two ends of the arc do not overlap for lack of normal spacing). The chord distance between adjacent nodes is constant at
  // ROW_GAP+PILL_H, so arcR is uniquely determined by the chord length and the angle limit; all edges radiate from the centre with length ≈ arcR.
  // Enabled when arcR is shorter than the longest single-column edge (centre right edge → near edge of the top/bottom node) and the neighbour
  // count reaches ARC_MIN; otherwise the old single column is kept.
  const SPAN_MAX = (110 * Math.PI) / 180; // angle limit 110°: normal spacing at the arc end ≥ pill height, guaranteeing no overlap
  const ARC_MIN = 12;
  const rightN = rightCol.length;
  const arcDelta = rightN > 1 ? SPAN_MAX / (rightN - 1) : 0;
  const arcR = rightN > 1 ? (ROW_GAP + PILL_H) / (2 * Math.sin(arcDelta / 2)) : 0;
  const colMaxEdge = Math.hypot(gapX, (rightN * (PILL_H + ROW_GAP)) / 2);
  const useArc = rightN >= ARC_MIN && arcR <= colMaxEdge;

  // With two columns the centre is centred and left→right is symmetric; with one side it degrades to the old form (centre at the far left).
  const hubCx = twoSided ? PAD + leftW + gapX + centerW / 2 : PAD + centerW / 2;
  const leftColLeft = PAD; // left edge of the left-column pill
  const rightColLeft = hubCx + centerW / 2 + gapX;
  const rightW = rightCol.length
    ? Math.max(...rightCol.map((n) => pillWidth(n.kind, n.name, false, input.showIcons ?? true)))
    : centerW;

  const contentW = useArc
    ? Math.max(width, hubCx + centerW / 2 + arcR + rightW / 2 + PAD)
    : Math.max(width, rightColLeft + rightW + PAD);
  const contentH = useArc
    ? Math.max(height, 2 * (arcR * Math.sin(SPAN_MAX / 2) + PILL_H / 2) + 2 * PAD)
    : Math.max(height, PAD * 2 + rows * (PILL_H + ROW_GAP) - ROW_GAP);
  const cy = Math.round(contentH / 2);

  /** Vertical start of one column of pills: centred on the canvas midline, returns the y of the i-th one. */
  const colY = (i: number, n: number) =>
    cy - ((n * (PILL_H + ROW_GAP) - ROW_GAP) / 2) + i * (PILL_H + ROW_GAP) + PILL_H / 2;

  const hub: Pt = [hubCx, cy];
  const nodes: PlacedNode[] = [
    { ...center, x: hub[0], y: hub[1], shape: 'rect', w: centerW, h: PILL_H },
  ];
  leftCol.forEach((n, i) => {
    const w = pillWidth(n.kind, n.name, false, input.showIcons ?? true);
    nodes.push({ ...n, x: leftColLeft + w / 2, y: colY(i, leftCol.length), shape: 'rect', w, h: PILL_H });
  });
  if (useArc) {

    // Right-column neighbours: laid out at equal angular distance on a circle centred at (hubCx, cy) with radius arcR; the middle node (centre of the list)
    // lands at the far right (θ=0) and the two ends fan up / down. The sort key stays "hops → kind → name", so neighbours stay adjacent along the arc.
    const mid = (rightN - 1) / 2;
    rightCol.forEach((n, i) => {
      const w = pillWidth(n.kind, n.name, false, input.showIcons ?? true);
      const theta = (i - mid) * arcDelta;
      nodes.push({
        ...n,
        x: hubCx + arcR * Math.cos(theta),
        y: cy + arcR * Math.sin(theta),
        shape: 'rect',
        w,
        h: PILL_H,
      });
    });
  } else {
    rightCol.forEach((n, i) => {
      const w = pillWidth(n.kind, n.name, false, input.showIcons ?? true);
      nodes.push({ ...n, x: rightColLeft + w / 2, y: colY(i, rightCol.length), shape: 'rect', w, h: PILL_H });
    });
  }

  const pos = new Map(nodes.map((n) => [n.id, [n.x, n.y] as Pt]));
  const parallel = indexParallel(edges);
  const hubCenter: Pt = [hubCx, cy];

  // Straight radial spokes (single straight segment from the centre pill's border straight to the target pill's border): users asked for
  // "connect with a straight line", not a 90°/0° orthogonal polyline. Key correction: a single straight segment **can** cut through zero
  // nodes — as long as the endpoint is the target's *near edge* rather than its centre. For the left column (callers) the endpoint is the
  // right edge of the left-column pill; if the right column uses the equal-length fan, `shrinkToRects` takes the target pill's near border —
  // correct at any angle.
  // (Note: if the endpoint were the pill *centre*, a shallow far-end line would sweep into the left half of a neighbour pill at that row, so it must connect to the near edge.)
  const ATTACH_MAX = PILL_H / 2 - 4;
  const nodeById = new Map(nodes.map((n) => [n.id, n]));

  /**
   * **Source-end (hub side) anchor spreading.**
   *
   * The old implementation squeezed the start point of every hub edge into ±`ATTACH_MAX` (18px in total)
   * on the centre pill's vertical edge — 30 outbound edges crammed into 18px, blurring into one bundle
   * near the centre until the lines spread out enough to see "which one goes where". Now they are spread
   * evenly along the **entire border of the centre pill on the side facing the neighbours** (top edge →
   * side edge → bottom edge) in target order: each edge gets its own entry/exit and the fan opens up as
   * soon as it leaves the pill.
   *
   * The start point cannot simply be moved anywhere — it must land on the part of the border that this
   * edge **visibly** crosses, otherwise the segment dives under the pill and re-emerges on the other side
   * (invisible while the pill is opaque, exposed as soon as hover focus makes it semi-transparent). The
   * test is plain: target above the pill's top edge ⇒ take the **top edge**; below the bottom edge ⇒ the
   * **bottom edge**; otherwise ⇒ the **side edge**. In all three cases the segment leaves the pill
   * immediately after the anchor; and since anchors run top-to-bottom along the border while targets run
   * top-to-bottom too, the two sequences are co-ordered ⇒ the spokes never cross each other (again a
   * structural guarantee, as in the old implementation, not a tuning result).
   *
   * The target endpoint is unchanged ⇒ the displacement happens only at the centre end and decays to 0
   * at the target, so spreading never pushes a line into a neighbour pill (the "an edge must not pass
   * through a node" hard constraint is unaffected).
   */
  const FAN_SLOT = 28; // target spacing between adjacent anchors on the same boundary segment
  const FAN_REACH = Math.min(centerW, 160); // max length usable on each of the top / bottom edges (not past the pill's other end)

  /** Target endpoint of each hub edge plus its parallel-edge offset (anchors are assigned sorted by the offset y). */
  const hubSides = new Map<number, { tgt: Pt; off: number; onLeft: boolean; hubIsFrom: boolean }>();
  edges.forEach((e, ei) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return;
    if (e.from !== center.id && e.to !== center.id) return;
    const hubIsFrom = e.from === center.id;
    const otherId = hubIsFrom ? e.to : e.from;
    const other = hubIsFrom ? b : a;
    const onLeft = twoSided && !hubIsFrom && callerIds.has(e.from);
    const otherNode = nodeById.get(otherId)!;
    // Several paths between the same endpoints: a single straight segment has no "middle section" to offset, so the **whole line is shifted along y**.
    // The shift divides the pill's usable height evenly among the group (total spread 2×ATTACH_MAX=22px, two parallel edges 22px apart, still ~11px after fit zoom),
    // far wider than the old curve scheme's fanOffset×0.6 (only 6.6px, blurring into one bundle after zoom).
    const p = parallel.get(ei);
    const off =
      p && p.count > 1 ? -ATTACH_MAX + (p.idx * (ATTACH_MAX * 2)) / (p.count - 1) : 0;
    const w = otherNode.w ?? 120;
    let tgt: Pt;
    if (onLeft) {
      tgt = [otherNode.x + w / 2, other[1]]; // left column (caller): take the pill's right edge
    } else if (useArc) {
      tgt = shrinkToRects(hubCenter, centerW, PILL_H, other, w, PILL_H)[1];
    } else {
      tgt = [otherNode.x - w / 2, other[1]]; // right column / single column: take the pill's left edge
    }
    hubSides.set(ei, { tgt, off, onLeft, hubIsFrom });
  });

  /** Hub-side anchor: edge index → start point. */
  const hubAnchor = new Map<number, Pt>();
  for (const side of [1, -1] as const) {
    const items = [...hubSides.entries()]
      .filter(([, s]) => (s.onLeft ? -1 : 1) === side)
      .map(([ei, s]) => ({ ei, ty: s.tgt[1] + s.off }))
      .sort((x, y) => x.ty - y.ty || x.ei - y.ei);
    if (items.length === 0) continue;
    const hh = PILL_H / 2;
    const xSide = hubCx + (side * centerW) / 2; // the vertical edge facing that side
    const above = items.filter((it) => it.ty < cy - hh);
    const mid = items.filter((it) => it.ty >= cy - hh && it.ty <= cy + hh);
    const below = items.filter((it) => it.ty > cy + hh);
    /** Points are taken at (i+0.5)/n within the group; the span grows with the count but never exceeds `FAN_REACH` (few edges do not spread needlessly). */
    const place = (group: typeof items, at: (span: number, f: number) => Pt) => {
      const span = Math.min(FAN_REACH, Math.max(0, group.length - 1) * FAN_SLOT);
      group.forEach((it, i) => hubAnchor.set(it.ei, at(span, (i + 0.5) / group.length)));
    };
    // Top edge: from the end away from the corner towards the corner (the topmost target takes the outermost point — steeper ⇒ exit point further in, co-ordered)
    place(above, (span, f) => [xSide - side * span * (1 - f), cy - hh]);
    // Side edge: divided evenly top to bottom
    place(mid, (_span, f) => [xSide, cy - hh + 2 * hh * f]);
    // Bottom edge: from the corner towards the end away from it (the bottommost target takes the outermost point)
    place(below, (span, f) => [xSide - side * span * f, cy + hh]);
  }

  const placed: PlacedEdge[] = edges.flatMap((e, ei) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    const spec = hubSides.get(ei);
    // Neither end is the centre (a leaf-to-leaf edge such as an inter-table ForeignKey): try a straight link first; a straight line between two pills
    // in the same column sweeps across the neighbour pills in between, and on a hit it detours along the side corridor — the "an edge must not
    // pass through a node" hard constraint makes no exception just because the star test was relaxed.
    if (!spec) {
      const straight: Pt[] = [a, b];
      const obstacles = obstaclesForPath(straight, nodes, new Set([e.from, e.to]), 100);
      if (!pathHits(straight, obstacles)) {
        return [{ ...e, points: straight, orthogonal: false }];
      }
      return [{ ...e, points: detourAroundNodes(a, b, obstacles), orthogonal: false }];
    }
    const anchor = hubAnchor.get(ei)!;
    const end: Pt = [spec.tgt[0], spec.tgt[1] + spec.off];
    // The straight link of a radial spoke gets the same probe. The structural guarantee (entering the target's x range at the left edge with y already
    // aligned to the target row) only holds for the **single-column** form; in the equal-length fan the line to an arc-end node sweeps across the pill
    // adjacent to it on the inside (the arc curls back at ±SPAN_MAX/2 and the spoke cuts diagonally through the inner neighbour) — on a hit it detours,
    // rather than pretending not to see it.
    const line: Pt[] = [anchor, end];
    const forward = spec.hubIsFrom;
    const obstacles = obstaclesForPath(line, nodes, new Set([e.from, e.to]), 100);
    const pts = pathHits(line, obstacles)
      ? detourAroundNodes(line[0], line[1], obstacles)
      : line;
    // Direction: from the centre → target; from target → centre (the arrow is decided by the direction of the last point).
    return [{ ...e, points: forward ? pts : pts.slice().reverse(), orthogonal: false }];
  });

  const flowNote = twoSided
    ? `The left column holds ${leftCol.length} callers / sources → center → the right column holds ${rightCol.length} dependents; **arrow direction is the request / data flow (left to right)**;`
    : `The center is on the left, with ${rightCol.length} neighbors arranged to its right;`;
  const arcNote = useArc
    ? `Neighbors are arranged in an **angle-limited equal-length fan** (an arc centered on the center, with fixed adjacent chord spacing and each edge ≈${Math.round(arcR)}px long), eliminating the over-long edges that top / bottom nodes dragged out in the single-column form;`
    : '';
  const fanNote =
    'Each edge\'s start point spreads along the center pill\'s **side facing the neighbor** -- “top → side → bottom” -- in target order (one entry/exit per edge, no longer mushing into a bundle near the center),';
  // The crossing count is reported honestly (same basis as stackedLayout): the single-column form has a structural guarantee of 0 crossings,
  // but the arc ends of the equal-length fan may need a detour as a fallback, and a failed detour must not be reported as "0".
  const polys = placed.map((e) => ({ from: e.from, to: e.to, pts: e.points }));
  const crossings = polys.length <= 600 ? countCrossings(polys) : null;
  const hardNote =
    crossings === 0
      ? 'Edges are single straight segments from the center pill\'s edge directly to the target pill\'s edge (the endpoint lands on the target\'s near edge, so it passes through no node); spacing is guaranteed by row gap and channel width, so this graph has **0 edge crossings and no edge passes through any node**'
      : crossings === null
        ? 'Many edges; crossings were not counted one by one'
        : `There are still **${crossings} edge crossings** (impossible to eliminate completely in the plane); use hover highlighting when verifying them one by one`;
  const tail = 'Scroll vertically when the content is tall.';
  return {
    nodes,
    edges: placed,
    width: contentW,
    height: contentH,
    content: boundsOf(nodes),
    // Spreading the start points is also told to the user: it is the direct explanation of "where an edge appears to come from", otherwise it reads as a random offset.
    note: viaStar
      ? `Radial entry judges this graph a **star** (edges run almost only between “consumers ↔ ${center.name}”, i.e. the resource perspective tracing back along in-edges; at most ${NEAR_STAR_LEAF_EDGES} leaf-to-leaf edges, such as a table-to-table ForeignKey, which detours): the concentric-ring canvas grows with the square of the people count, and outer-ring edges run straight through the center and cover inner-ring pill names, so it switches to center-radial.${flowNote}${arcNote}neighbors are all sorted by “hops → kind → name”; ${fanNote}${hardNote}。${tail}`
      : `Center-radial layout: ${flowNote}${arcNote}neighbors are all sorted by “hops → kind → name”; ${fanNote}${hardNote}。${tail}`,
  };
}

/** Bounding box of a node set (including the extra margin for edge labels). */
function boundsOf(nodes: PlacedNode[]): { x: number; y: number; w: number; h: number } {
  if (nodes.length === 0) return { x: 0, y: 0, w: 1, h: 1 };
  let minX = Number.POSITIVE_INFINITY;
  let maxX = Number.NEGATIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  let maxY = Number.NEGATIVE_INFINITY;
  nodes.forEach((n) => {
    const hw = (n.w ?? 120) / 2;
    const hh = (n.h ?? 26) / 2;
    if (n.x - hw < minX) minX = n.x - hw;
    if (n.x + hw > maxX) maxX = n.x + hw;
    if (n.y - hh < minY) minY = n.y - hh;
    if (n.y + hh > maxY) maxY = n.y + hh;
  });
  // Edge labels are pushed about 9px along the normal at font size 10, with 14px kept clear on each side so they are not clipped
  return { x: minX - 14, y: minY - 14, w: maxX - minX + 28, h: maxY - minY + 28 };
}

/**
 * Multi-layer layout (simplified Sugiyama): top-down, **one row per layer**, layers linked directly.
 *
 * Why insist on one row per layer instead of wrapping into a grid:
 * as soon as a layer wraps into several rows, edges heading for a lower row must pass through the pills
 * of an upper row — a column-aligned grid with a vertical corridor between columns was tried, but the
 * "descending from the source node" part is still blocked by the next row of the source's own layer and
 * needs yet another horizontal detour layered on top, growing ever more complex. With one row per
 * layer, any edge's y range only covers the two row lines of two adjacent layers, so it **structurally
 * cannot touch any node** (apart from its own two endpoints).
 *
 * The cost is that a very wide layer overflows the container — but the viewBox and rendering are now 1:1,
 * so overflow means **horizontal scrolling**, not "the whole graph shrinking together with its text".
 * On the "ambiguity cost > mechanical cost" trade-off this is worth it.
 */
function stackedLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const layers: LayoutNode[][] = [[center], ...rings];
  const top = 60;
  const GAP = 18;
  const PAD = Math.max(24, Math.min(60, width * 0.05));

  const ordered = orderByBarycenter(layers, edges);
  // The centre node must be measured with the "centre" spec (13px / 700): the renderer draws the centre one size larger than a normal node,
  // and a pill measured at 11px / 500 cannot hold the bold large text — the text overflows the box (the route perspective centre used to overflow).
  const widths = ordered.map((layer) =>
    layer.map((n) => pillWidth(n.kind, n.name, n.id === center.id, input.showIcons ?? true)),
  );
  const layerW = widths.map((ws) =>
    ws.reduce((s, w) => s + w, 0) + GAP * Math.max(0, ws.length - 1),
  );
  const contentW = Math.max(width, ...layerW, 0) + PAD * 2;
  const centerX = contentW / 2;

  const totalLayerH = PILL_H * layers.length;
  // Layer spacing: fills the available height by default; when the content is already very tall it falls back to the minimum spacing and the canvas scrolls (rather than clipping).
  const availH = Math.max(120, height - top - 60);
  const gapY =
    layers.length > 1 ? Math.max(56, (availH - totalLayerH) / (layers.length - 1)) : 0;

  const nodes: PlacedNode[] = [];
  let y = top;
  ordered.forEach((layer, li) => {
    const ws = widths[li];
    let cx = centerX - layerW[li] / 2;
    layer.forEach((n, i) => {
      const w = ws[i];
      nodes.push({ ...n, x: cx + w / 2, y, shape: 'rect', w, h: PILL_H });
      cx += w + GAP;
    });
    y += PILL_H + gapY;
  });

  const pos = new Map(nodes.map((n) => [n.id, [n.x, n.y] as Pt]));
  const parallel = indexParallel(edges);
  const placed: PlacedEdge[] = [];
  const polys: Array<{ from: number; to: number; pts: Pt[] }> = [];
  edges.forEach((e, ei) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return;
    // Direct links (no more "down → across → down" orthogonal polylines: those put the horizontal segment of every edge in a layer on one line, stacking all labels on a single y).
    // One row per layer ⇒ a direct link's y range covers only two adjacent row lines, so structurally it touches no node;
    // only a "jumping edge" across layers (possible with folded pull-up) hits the detour fallback below.
    const straight: Pt[] = [a, b];
    const p = parallel.get(ei);
    const off = p ? fanOffset(p.count, p.idx) : 0;
    if (off === 0) {
      if (!pathHits(straight, obstaclesForPath(straight, nodes, new Set([e.from, e.to]), 100))) {
        placed.push({ ...e, points: straight, orthogonal: false });
        polys.push({ from: e.from, to: e.to, pts: straight });
        return;
      }
    } else {
      // Parallel edges: the midpoint is offset along the normal into a willow-leaf shape so several paths do not overlap completely
      const dx = b[0] - a[0];
      const dy = b[1] - a[1];
      const len = Math.hypot(dx, dy) || 1;
      const bowed: Pt[] = [
        a,
        [(a[0] + b[0]) / 2 + (-dy / len) * off, (a[1] + b[1]) / 2 + (dx / len) * off],
        b,
      ];
      if (!pathHits(bowed, obstaclesForPath(bowed, nodes, new Set([e.from, e.to]), 100))) {
        placed.push({ ...e, points: bowed, orthogonal: false });
        polys.push({ from: e.from, to: e.to, pts: bowed });
        return;
      }
    }
    const pts = detourAroundNodes(
      a,
      b,
      obstaclesForPath(straight, nodes, new Set([e.from, e.to]), 100),
    );
    placed.push({ ...e, points: pts, orthogonal: false });
    polys.push({ from: e.from, to: e.to, pts });
  });

  const content = nodes.length > 0 ? boundsOf(nodes) : { x: 0, y: 0, w: contentW, h: height };
  // The crossing count is reported honestly: crossing minimisation is NP-hard, barycentre ordering + detours can only push it down, never to zero.
  // Better to let the user know "this part takes some effort" than to pretend there is nothing.
  const crossings = polys.length <= 600 ? countCrossings(polys) : null;

  const totalH = top + totalLayerH + gapY * Math.max(0, layers.length - 1) + 60;
  return {
    nodes,
    edges: placed,
    width: contentW,
    height: Math.max(height, totalH),
    content,
    note:
      crossings === null
        ? 'Layered layout: layered top-down (layer = hops), within a layer sorted by barycenter, over-wide layers wrap automatically, and edges detour around nodes. Many edges; crossings were not counted one by one.'
        : crossings === 0
          ? 'Layered layout: layered top-down (layer = hops), within a layer sorted by barycenter, over-wide layers wrap automatically, and edges detour around nodes. Currently **0 edge crossings**, so each can be read directly.'
          : `Layered layout: layered top-down (layer = hops), within a layer sorted by barycenter, over-wide layers wrap automatically, and edges detour around nodes. There are still **${crossings} edge crossings** (impossible to eliminate completely in the plane); use hover highlighting when verifying them one by one.`,
  };
}

// ---------------------------------------------------------------- spine

/**
 * Layered layout: always a top-down grid (layer = hops); no shape adaptation.
 *
 * Deliberately distinct from `radial`: radial switches itself to hub-and-spoke when it meets a star
 * (centre on the left, neighbours in a single column on the right), while layered keeps the point of a
 * "layered call chain" — the centre row on top, one row per ring below. For a resource-perspective star
 * such as "a contract reads N config keys" (centre + single ring): a small fan-out
 * (<= `LAYERED_FAN_MAX`) is drawn as a two-row fan, centre on top and leaves in one row below, spreading
 * downwards from the centre; a wide fan-out switches to hub-and-spoke (see `LAYERED_FAN_MAX`).
 * Since all edges share one endpoint (the centre), both forms are equally 0 crossings and 0 node
 * penetrations.
 */
/**
 * Tolerance limit of the layered layout for a **single ring with wide fan-out**.
 *
 * Layered draws "centre + single ring" as a two-row fan (centre on top, leaves in a row below) whose
 * width grows as leaf count × pill width — with 7~10 leaves it is the good-looking "call flow top-down"
 * shape; but "a contract reads 27 config keys" gives a 6000px+ row where, after fit, both ends are
 * clipped and the middle is largely empty (a measured screenshot). Past the limit it switches to
 * hub-and-spoke (hub on the left, single column on the right): width fixed at ~600px, the only cost
 * being vertical scrolling. A small fan-out keeps the fan shape — still the most direct expression of
 * "top-down".
 */
const LAYERED_FAN_MAX = 10;

export function layeredLayout(input: LayoutInput): LayoutResult {
  const leaves = input.rings.flat();
  // As in radialLayout: use the "approximate star" — in the route perspective one inter-table ForeignKey (a leaf-to-leaf edge)
  // once made a graph of 19 leaves count as "not a star", dropping back to the 6000px+ wide single-row fan (a bad case captured in a screenshot).
  if (leaves.length > LAYERED_FAN_MAX && isNearStar(input.center.id, input.edges)) {
    return hubSpokeLayout(input, leaves, false);
  }
  return stackedLayout(input);
}

/** Linear Spine: find one main chain laid out horizontally and hang the remaining nodes below it. */
export function spineLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const adjacency = new Map<number, number[]>();
  edges.forEach((e) => {
    adjacency.set(e.from, [...(adjacency.get(e.from) ?? []), e.to]);
    adjacency.set(e.to, [...(adjacency.get(e.to) ?? []), e.from]);
  });

  // Longest simple path from the centre (deterministic: neighbours in ascending id order)
  const best: number[] = [center.id];
  const visited = new Set<number>([center.id]);
  const walk = (node: number, path: number[]) => {
    if (path.length > best.length) best.splice(0, best.length, ...path);
    for (const next of (adjacency.get(node) ?? []).slice().sort((a, b) => a - b)) {
      if (visited.has(next)) continue;
      visited.add(next);
      walk(next, [...path, next]);
      visited.delete(next);
    }
  };
  walk(center.id, [center.id]);

  const spineY = height * 0.38;
  const GAP = 18;
  const PAD = 60;
  const onSpine = new Set(best);

  // Main chain: pill nodes are laid out along the x axis at their real widths (text embedded, same as radial / matrix / ER).
  const nodes: PlacedNode[] = [];
  let cursor = PAD;
  best.forEach((id) => {
    const found = findNode(input, id);
    if (!found) return;
    const w = pillWidth(found.kind, found.name, found.id === center.id, input.showIcons ?? true);
    nodes.push({ ...found, x: cursor + w / 2, y: spineY, shape: 'rect', w, h: PILL_H });
    cursor += w + GAP;
  });
  const spineW = cursor - GAP + PAD;

  // Non-main-chain nodes: hung below the main chain by hop count, laid out on a grid (uniform slot width, so they stay aligned).
  const rest: LayoutNode[] = [];
  rings.forEach((ring, ri) =>
    ring.forEach((n) => {
      if (!onSpine.has(n.id)) rest.push({ ...n, ring: ri + 1 });
    }),
  );
  const contentW = Math.max(width, spineW);
  const COLS = 8;
  const slotW = Math.min(220, Math.max(120, (contentW - 120) / COLS));
  rest.forEach((n, i) => {
    const col = i % COLS;
    const row = Math.floor(i / COLS);
    nodes.push({
      ...n,
      x: 60 + col * (slotW + GAP) + slotW / 2,
      y: spineY + 110 + row * (PILL_H + GAP),
      shape: 'rect',
      w: slotW,
      h: PILL_H,
    });
  });
  const gridRight = 60 + COLS * (slotW + GAP);
  const finalW = Math.max(contentW, gridRight + 40, width);

  const pos = new Map(nodes.map((n) => [n.id, [n.x, n.y] as [number, number]]));
  const placed: PlacedEdge[] = edges.flatMap((e) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    return [{ ...e, points: [a, b], orthogonal: false }];
  });

  return {
    nodes,
    edges: placed,
    width: finalW,
    height,
    note: 'Spine layout: arranges the longest chain as the main axis, for hop-by-hop verification of taint / risk forensics.',
  };
}

function findNode(input: LayoutInput, id: number): LayoutNode | null {
  if (input.center.id === id) return input.center;
  for (const ring of input.rings) {
    const hit = ring.find((n) => n.id === id);
    if (hit) return hit;
  }
  return null;
}

// ---------------------------------------------------------------- compound

/** Compound clustering: big boxes containing small nodes. Used by the aggregate perspective. */
export function compoundLayout(input: LayoutInput): LayoutResult {
  const clusters = input.clusters ?? [];
  const { width } = input;
  const cols = Math.max(1, Math.ceil(Math.sqrt(clusters.length)));
  const boxW = Math.min(300, (width - 60) / cols - 20);
  const boxH = 150;
  const groups: PlacedGroup[] = [];
  const nodes: PlacedNode[] = [];

  clusters.forEach((c, i) => {
    const gx = 30 + (i % cols) * (boxW + 20);
    const gy = 40 + Math.floor(i / cols) * (boxH + 24);
    groups.push({ key: c.key, label: c.label, x: gx, y: gy, w: boxW, h: boxH, count: c.count });
    c.members.slice(0, 4).forEach((m, mi) => {
      nodes.push({
        ...m,
        x: gx + 20 + (mi % 2) * ((boxW - 40) / 2),
        y: gy + 52 + Math.floor(mi / 2) * 34,
        shape: 'rect',
        w: (boxW - 60) / 2,
        h: 24,
      });
    });
  });

  const height = 40 + Math.ceil(clusters.length / cols) * (boxH + 24) + 30;
  return {
    nodes,
    edges: [],
    groups,
    width,
    height: Math.max(input.height, height),
    note: 'Cluster layout: each box is a group; the box shows only a count and sample members, not a single chain.',
  };
}

// ---------------------------------------------------------------- matrix

/** Matrix: two dimensions as rows and columns, cells holding relation strength. */
export function matrixLayout(input: LayoutInput): LayoutResult {
  const m = input.matrix ?? { rows: [], cols: [], cells: [] };
  const left = 160;
  const top = 90;
  const cellW = Math.max(52, (input.width - left - 40) / Math.max(1, m.cols.length));
  const cellH = 34;

  const cells: PlacedCell[] = [];
  m.cells.forEach((row, ri) =>
    row.forEach((value, ci) => {
      cells.push({
        row: ri,
        col: ci,
        value,
        x: left + ci * cellW,
        y: top + ri * cellH,
        w: cellW - 3,
        h: cellH - 3,
      });
    }),
  );

  return {
    nodes: [],
    edges: [],
    cells,
    rowHeaders: m.rows.map((label, i) => ({ label, x: 12, y: top + i * cellH })),
    colHeaders: m.cols.map((label, i) => ({ label, x: left + i * cellW, y: top - 26 })),
    width: input.width,
    height: Math.max(input.height, top + m.rows.length * cellH + 40),
    note: 'Matrix layout: two dimensions, rows × columns; cell color depth indicates the count, and 0 means that combination genuinely produced nothing.',
  };
}

// ---------------------------------------------------------------- er

/** ER orthogonal: tables laid out in columns, relations drawn as 90° polylines. */
export function erLayout(input: LayoutInput): LayoutResult {
  const all = [input.center, ...input.rings.flat()];
  const cols = Math.max(1, Math.ceil(Math.sqrt(all.length / 2)));
  const boxW = 170;
  const boxH = 46;
  const nodes: PlacedNode[] = all.map((n, i) => ({
    ...n,
    x: 40 + (i % cols) * (boxW + 46),
    y: 50 + Math.floor(i / cols) * (boxH + 60),
    shape: 'rect',
    w: boxW,
    h: boxH,
  }));

  const pos = new Map(nodes.map((n) => [n.id, [n.x + (n.w ?? boxW) / 2, n.y + (n.h ?? boxH) / 2]]));
  const placed: PlacedEdge[] = input.edges.flatMap((e) => {
    const a = pos.get(e.from) as [number, number] | undefined;
    const b = pos.get(e.to) as [number, number] | undefined;
    if (!a || !b) return [];
    const midX = (a[0] + b[0]) / 2;
    return [
      { ...e, points: [a, [midX, a[1]], [midX, b[1]], b], orthogonal: true },
    ];
  });

  return {
    nodes,
    edges: placed,
    width: input.width,
    height: Math.max(input.height, 50 + Math.ceil(all.length / cols) * (boxH + 60) + 40),
    note: 'ER layout: tables are connected by 90° orthogonal lines, for seeing co-occurrence within the same transaction.',
  };
}

export function layoutOf(mode: LayoutMode): LayoutFn {
  return LAYOUTS[mode] ?? radialLayout;
}
