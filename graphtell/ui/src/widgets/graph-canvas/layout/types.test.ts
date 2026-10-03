import { describe, expect, it } from 'vitest';

import { concentricLayout, layeredLayout, radialLayout } from './types';
import type { LayoutEdge, LayoutNode, PlacedEdge, PlacedNode } from './types';

/**
 * The two predicates below are **implemented independently on the test side**, deliberately not reusing `segHitsRect` / `countCrossings` from the code under test:
 * asserting code against itself with the same predicate verifies nothing.
 */

type Pt = [number, number];

function segments(edges: PlacedEdge[]): Array<{ s: Pt; e: Pt; from: number; to: number }> {
  const out: Array<{ s: Pt; e: Pt; from: number; to: number }> = [];
  for (const ed of edges) {
    for (let i = 1; i < ed.points.length; i += 1) {
      out.push({ s: ed.points[i - 1], e: ed.points[i], from: ed.from, to: ed.to });
    }
  }
  return out;
}

/** Whether a segment passes through a rectangle (slab method). */
function hitsRect(a: Pt, b: Pt, cx: number, cy: number, w: number, h: number): boolean {
  let t0 = 0;
  let t1 = 1;
  const dx = b[0] - a[0];
  const dy = b[1] - a[1];
  const slabs: Array<[number, number]> = [
    [-dx, a[0] - (cx - w / 2)],
    [dx, cx + w / 2 - a[0]],
    [-dy, a[1] - (cy - h / 2)],
    [dy, cy + h / 2 - a[1]],
  ];
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

/** Number of segments passing through a non-endpoint node (hard constraint: must be 0). */
function segmentsThroughNodes(edges: PlacedEdge[], nodes: PlacedNode[]): number {
  let n = 0;
  for (const seg of segments(edges)) {
    for (const node of nodes) {
      if (node.id === seg.from || node.id === seg.to) continue;
      if (hitsRect(seg.s, seg.e, node.x, node.y, node.w ?? 120, node.h ?? 26)) n += 1;
    }
  }
  return n;
}

/** Crossing count (edge pairs sharing an endpoint don't count). */
function crossings(edges: PlacedEdge[]): number {
  const segs = segments(edges);
  const orient = (a: Pt, b: Pt, c: Pt) =>
    Math.sign((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]));
  let n = 0;
  for (let i = 0; i < segs.length; i += 1) {
    for (let j = i + 1; j < segs.length; j += 1) {
      const A = segs[i];
      const B = segs[j];
      if (A.from === B.from || A.from === B.to || A.to === B.from || A.to === B.to) continue;
      if (
        orient(B.s, B.e, A.s) * orient(B.s, B.e, A.e) < 0 &&
        orient(A.s, A.e, B.s) * orient(A.s, A.e, B.e) < 0
      ) {
        n += 1;
      }
    }
  }
  return n;
}

/**
 * Regression: a layered layout's canvas width must be constrained by the container.
 *
 * The old implementation spread all pills of a layer into a single row, so 20 out-edges could push the canvas to 3000px+;
 * the canvas was then rendered with `viewBox="0 0 layout.width …"`, and the browser's `meet` scaled the whole graph
 * back down to the container width, text included -- showing up as "more out-edges, blurrier text".
 */

const node = (id: number, name: string, ring: number): LayoutNode => ({
  id,
  kind: 'Method',
  name,
  ring,
});

describe('layeredLayout', () => {
  const WIDTH = 1040;

  it('with multiple layers the canvas may exceed the container width; overflow scrolls horizontally instead of shrinking the whole graph', () => {
    // 20 out-edges; the old implementation produced a canvas about 3300px wide
    const fanout = Array.from({ length: 20 }, (_, i) => node(100 + i, `handler_number_${i}`, 2));
    const center = node(1, 'job_daily_settlement', 0);
    const ring1 = Array.from({ length: 3 }, (_, i) => node(10 + i, `service_stage_${i}`, 1));
    const edges: LayoutEdge[] = [];
    ring1.forEach((n) => edges.push({ id: n.id, from: center.id, to: n.id }));
    fanout.forEach((n, i) => edges.push({ id: n.id, from: ring1[i % ring1.length].id, to: n.id }));

    const r = layeredLayout({
      center,
      rings: [ring1, fanout],
      edges,
      width: WIDTH,
      height: 720,
    });

    // The canvas may be wider than the container (not allowed a round ago, because viewBox then shrank the whole graph text included;
    // now viewBox is 1:1 with the render, so overflow just scrolls), but a **content bounding box must be provided** for zoom-to-fit.
    expect(r.content).toBeDefined();
    expect(r.content!.w).toBeLessThan(r.width);
    // One row per layer: the number of distinct node y values == number of layers
    const rowsByY = new Set(r.nodes.map((n) => n.y));
    expect(rowsByY.size).toBe(3);
  });

  it('the content bounding box covers all nodes and stays within the canvas', () => {
    const fanout = Array.from({ length: 12 }, (_, i) => node(100 + i, `callee_${i}`, 1));
    const center = node(1, 'POST /orders', 0);
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    expect(r.content).toBeDefined();
    const c = r.content!;
    r.nodes.forEach((n) => {
      const hw = (n.w ?? 0) / 2;
      const hh = (n.h ?? 0) / 2;
      expect(n.x - hw).toBeGreaterThanOrEqual(c.x);
      expect(n.x + hw).toBeLessThanOrEqual(c.x + c.w);
      expect(n.y - hh).toBeGreaterThanOrEqual(c.y);
      expect(n.y + hh).toBeLessThanOrEqual(c.y + c.h);
    });
    // For zoom-to-fit: must be smaller than the whole canvas, otherwise fit does nothing
    expect(c.w).toBeLessThan(r.width);
    expect(c.h).toBeLessThan(r.height);
  });

  it('siblings with the same parent are placed together (fewer cross-row edges)', () => {
    const center = node(1, 'root', 0);
    const parents = [node(10, 'p0', 1), node(11, 'p1', 1)];
    // Given interleaved: without sorting, the two parents' children would alternate
    const children = [node(20, 'a0', 2), node(21, 'b0', 2), node(22, 'a1', 2), node(23, 'b1', 2)];
    const edges: LayoutEdge[] = [
      { id: 1, from: 1, to: 10 },
      { id: 2, from: 1, to: 11 },
      { id: 3, from: 10, to: 20 },
      { id: 4, from: 11, to: 21 },
      { id: 5, from: 10, to: 22 },
      { id: 6, from: 11, to: 23 },
    ];

    const r = layeredLayout({ center, rings: [parents, children], edges, width: WIDTH, height: 720 });
    const order = r.nodes
      .filter((n) => n.ring === 2)
      .sort((a, b) => a.x - b.x)
      .map((n) => n.id);

    expect(order).toEqual([20, 22, 21, 23]);
  });
});

/**
 * Regression: two hard constraints on readability.
 *
 * "Edge crossings" can only be minimized (NP-hard), but **on the shape most common in your data it should already be 0**:
 * in the collapsed view semantic nodes are terminals (`view_service.rs`), so "one contract reads 32 config keys"
 * is center + single ring -- all edges share one endpoint, and segments leaving the same point don't cross each other.
 *
 * "An edge passing through a node" is worse than a crossing (a pierced pill makes the name unreadable),
 * and it is **decidable**, so it must be eliminated 100% -- no layout may violate it.
 */
describe('readability hard constraints', () => {
  const WIDTH = 1040;

  it('small fan-out star -> two layered rows (center on top, single ring below), zero edge crossings and no edge through a node', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 8 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // Indeed two layers: the center row on top, the single-ring row below (distinct from a center-radial single column)
    const ys = [...new Set(r.nodes.map((n) => Math.round(n.y)))];
    expect(ys.length).toBe(2);
    const [topY, bottomY] = ys.sort((a, b) => a - b);
    expect(Math.round(r.nodes.find((n) => n.id === center.id)!.y)).toBe(topY);
    fanout.forEach((n) => {
      expect(Math.round(r.nodes.find((x) => x.id === n.id)!.y)).toBe(bottomY);
    });

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('wide fan-out star (> LAYERED_FAN_MAX) -> switches to center-radial (not concentric rings), both hard constraints still hold', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 24 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // Center-radial rather than concentric rings: no ring guides
    expect(r.guides).toBeUndefined();
    // The center is the unique leftmost node, all neighbors are to its right (true for both a single column and an equal-radius fan)
    const centerX = r.nodes.find((n) => n.id === center.id)!.x;
    const minLeft = Math.min(
      ...r.nodes.filter((n) => n.id !== center.id).map((n) => n.x - (n.w ?? 0) / 2),
    );
    expect(centerX).toBeLessThan(minLeft);
    fanout.forEach((n) => {
      expect(r.nodes.find((x) => x.id === n.id)!.x).toBeGreaterThan(centerX);
    });

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  /**
   * Regression: wide fan-out + **one leaf-to-leaf edge** (a ForeignKey between two tables in the route perspective).
   *
   * The old `isStar` predicate required every edge to touch the center -- one ForeignKey killed the predicate,
   * and 19 leaves fell back to the wide fan with one row per layer (a real bad screenshot: 6000px+ wide, both ends cut off).
   * Now an approximate star (leaf-to-leaf edges ≤ 3) still goes center-radial; if a leaf-to-leaf edge itself sweeps over a same-column neighbor,
   * it must detour ("no edge through a node" is not waived by the relaxation).
   */
  it('wide fan-out + a few leaf-to-leaf edges (ForeignKey) -> still center-radial, leaf-to-leaf edges detour without piercing nodes', () => {
    const center = node(1, 'GET /admin/order/detail', 0);
    const fanout = [
      ...Array.from({ length: 4 }, (_, i) => node(10 + i, `middleware_${i}`, 1)),
      ...Array.from({ length: 14 }, (_, i) => node(100 + i, `config_key_${i}`, 1)),
      node(200, 'store_order_refund', 1),
      node(201, 'user', 1),
    ];
    const edges: LayoutEdge[] = [
      ...fanout.map((n) => ({ id: n.id, from: center.id, to: n.id })),
      // A table-to-table foreign key: neither endpoint is the center -- under the old predicate this one edge pushed the whole graph back to the wide fan
      { id: 900, from: 200, to: 201 },
    ];

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // Went center-radial rather than layered fan: no ring guides, center leftmost
    expect(r.guides).toBeUndefined();
    const centerX = r.nodes.find((n) => n.id === center.id)!.x;
    fanout.forEach((n) => {
      expect(r.nodes.find((x) => x.id === n.id)!.x).toBeGreaterThan(centerX);
    });

    // Both hard constraints still hold (a ForeignKey edge either runs straight without collision or detours)
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    expect(crossings(r.edges)).toBe(0);
  });

  /**
   * Regression: the **left→right directionality** of the route perspective.
   *
   * The route perspective graph is "frontend caller --CallsHttp--> contract --ReadsConfig/ReadsCache--> dependency";
   * the old center-radial mixed callers and dependencies into one column right of the center, so request flow had no sense of direction on the canvas.
   * Now when both sides are non-empty they split into left and right columns: callers left of center, dependencies right, all arrows left to right.
   */
  it('two-sided star (callers + wide dependency fan-out) -> callers on the left, dependencies on the right, request flow left to right', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const caller = node(2, 'orderInvoiceDetail', 1); // Frontend Function (in-edge)
    const deps = Array.from({ length: 12 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = [
      { id: 900, from: caller.id, to: center.id },
      ...deps.map((n) => ({ id: n.id, from: center.id, to: n.id })),
    ];

    const r = layeredLayout({ center, rings: [[caller, ...deps]], edges, width: WIDTH, height: 720 });

    const cRect = r.nodes.find((n) => n.id === center.id)!;
    const callerRect = r.nodes.find((n) => n.id === caller.id)!;
    // Callers as a whole (right edge) are left of the center's left edge; dependencies are right of the center
    expect(callerRect.x + (callerRect.w ?? 0) / 2).toBeLessThan(cRect.x - (cRect.w ?? 0) / 2);
    deps.forEach((n) => {
      expect(r.nodes.find((x) => x.id === n.id)!.x).toBeGreaterThan(cRect.x);
    });
    // The call edge bends in the left channel: the bend x lies between the caller and the center
    const callEdge = r.edges.find((e) => e.from === caller.id)!;
    const viaX = callEdge.points[1][0];
    expect(viaX).toBeGreaterThan(callerRect.x);
    expect(viaX).toBeLessThan(cRect.x);

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('multi-layer: edges never pass through a node (oblique edges detour automatically)', () => {
    const center = node(1, 'handler', 0);
    const ring1 = Array.from({ length: 4 }, (_, i) => node(10 + i, `service_${i}`, 1));
    const ring2 = Array.from({ length: 10 }, (_, i) => node(50 + i, `table_${i}`, 2));
    const edges: LayoutEdge[] = [];
    ring1.forEach((n) => edges.push({ id: n.id, from: center.id, to: n.id }));
    ring2.forEach((n, i) => edges.push({ id: n.id, from: ring1[i % ring1.length].id, to: n.id }));

    const r = layeredLayout({ center, rings: [ring1, ring2], edges, width: WIDTH, height: 720 });

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('the scale from the screenshots (a contract reading 32 config keys) still satisfies both hard constraints, and same-kind nodes cluster at one arc end without being buried', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = [
      ...Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1)),
      { id: 200, kind: 'Cache', name: 'cache', ring: 1 },
      { id: 201, kind: 'Table', name: 'cache_table', ring: 1 },
    ];
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    // Under an equal-radius fan, "same kinds cluster together" means: after sorting by angle along the arc, Cache / Table occupy the two ends
    // (head / tail) of the arc, and the whole middle run is config keys, not buried in between
    const hub = r.nodes.find((n) => n.id === center.id)!;
    const ordered = r.nodes
      .filter((n) => n.id !== center.id)
      .map((n) => ({ kind: n.kind, ang: Math.atan2(n.y - hub.y, n.x - hub.x) }))
      .sort((a, b) => a.ang - b.ang);
    const ends = [ordered[0].kind, ordered[ordered.length - 1].kind].sort();
    expect(ends).toEqual(['Cache', 'Table']);
    expect(ordered.slice(1, ordered.length - 1).every((n) => n.kind === 'Method')).toBe(true);
  });

  /**
   * Regression: moderate fan-out (the 30-node scale in the screenshots) uses an **equal-radius fan** -- neighbors are equidistant from the center, and
   * adjacent pills on the arc don't overlap (a geometric guarantee of the 110° angle limit), so both hard constraints still hold. Very large fan-out (80)
   * falls back to a single column (the arc's area would exceed the single column's).
   */
  it('moderate fan-out (30) -> equal-radius fan: neighbors equidistant from the center, adjacent pills do not overlap', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    const hub = r.nodes.find((n) => n.id === center.id)!;
    const spokes = r.nodes.filter((n) => n.id !== center.id);
    // Every neighbor on the arc should be equidistant from the center's **center point** (structural "equal sides")
    const radii = spokes.map((n) => Math.hypot(n.x - hub.x, n.y - hub.y));
    const maxR = Math.max(...radii);
    const minR = Math.min(...radii);
    expect(maxR - minR).toBeLessThan(1);
    // Adjacent (angle-sorted) pill rectangles don't overlap: the angle limit guarantees normal spacing ≥ pill height
    const byAng = spokes
      .map((n) => ({ n, ang: Math.atan2(n.y - hub.y, n.x - hub.x) }))
      .sort((a, b) => a.ang - b.ang)
      .map((x) => x.n);
    const overlap = (p: typeof byAng[number], q: typeof byAng[number]) => {
      const ax1 = p.x - (p.w ?? 0) / 2;
      const ax2 = p.x + (p.w ?? 0) / 2;
      const ay1 = p.y - (p.h ?? 0) / 2;
      const ay2 = p.y + (p.h ?? 0) / 2;
      const bx1 = q.x - (q.w ?? 0) / 2;
      const bx2 = q.x + (q.w ?? 0) / 2;
      const by1 = q.y - (q.h ?? 0) / 2;
      const by2 = q.y + (q.h ?? 0) / 2;
      return ax1 < bx2 && bx1 < ax2 && ay1 < by2 && by1 < ay2;
    };
    for (let i = 1; i < byAng.length; i += 1) {
      expect(overlap(byAng[i - 1], byAng[i])).toBe(false);
    }

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('very large fan-out (80) -> falls back to a single column (left edges aligned); the equal-radius arc would be larger and must not be used', () => {
    const center = node(1, 'cache_order_summary', 0);
    const fanout = Array.from({ length: 80 }, (_, i) => node(100 + i, `user_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: n.id, to: center.id }));

    const r = radialLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    const lefts = new Set(
      fanout.map((n) => Math.round(r.nodes.find((x) => x.id === n.id)!.x - (r.nodes.find((x) => x.id === n.id)!.w ?? 0) / 2)),
    );
    expect(lefts.size).toBe(1);
    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('multiple paths between the same endpoint pair must be offset, never drawn as one fully overlapping line', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 8 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));
    // A second **different path** to the same config key (the backend now emits one edge each, with different via)
    edges.push({ id: 999, from: center.id, to: fanout[0].id });

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    const pair = r.edges.filter(
      (e) => e.from === center.id && e.to === fanout[0].id,
    );
    expect(pair.length).toBe(2);
    // The two edges must not have identical geometry: otherwise the user sees only one line and never knows there are two paths
    expect(JSON.stringify(pair[0].points)).not.toBe(JSON.stringify(pair[1].points));
    // After offsetting, both hard constraints still hold
    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('with multiple layers and one very wide layer (that wraps), edges still do not pass through nodes', () => {
    const center = node(1, 'handler', 0);
    const ring1 = Array.from({ length: 8 }, (_, i) => node(10 + i, `service_${i}`, 1));
    const ring2 = Array.from({ length: 22 }, (_, i) => node(50 + i, `table_${i}`, 2));
    const edges: LayoutEdge[] = [];
    ring1.forEach((n) => edges.push({ id: n.id, from: center.id, to: n.id }));
    ring2.forEach((n, i) => edges.push({ id: n.id, from: ring1[i % ring1.length].id, to: n.id }));

    const r = layeredLayout({ center, rings: [ring1, ring2], edges, width: WIDTH, height: 720 });

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });
});

/**
 * Regression: radial entry must be dispatched by **graph shape**, not by "how many rings / how many nodes per ring".
 *
 * The resource perspective (Table / Cache / Event / Queue / Topic) runs in reverse mode in `view_service.rs`
 * (`reverse = kind != HttpContract`): after tracing back along in-edges to each consumer, it **synthesizes one direct edge**
 * from the consumer to the center (`MAX_USERS = 80`, the rest counted into `hidden`). Here `ring` is just a number hanging on the node,
 * with no corresponding "leaf-to-leaf" edge -- what comes in is a **star**, and a predicate like `layeredLayout`'s
 * `nonEmpty.length === 1` is necessarily false here, so `isStar` must be what recognizes it.
 *
 * This kind of data is exactly the magnitude after `MAX_USERS` truncation: concentric rings would be stretched to several times the container width
 * while the content occupies only a 30px-wide band on the ring; switching to center-radial satisfies both readability constraints and is an order of magnitude smaller.
 */
describe('radialLayout: resource perspective (star)', () => {
  const WIDTH = 1040;

  /** Build a resource-perspective-shaped input: `ringSizes` = people per ring, all edges are `consumer → center`. */
  const resourceStar = (ringSizes: number[]) => {
    const center: LayoutNode = { id: 1, kind: 'Cache', name: 'cache_order_summary', ring: 0 };
    let seed = 100;
    const rings: LayoutNode[][] = ringSizes.map((n, ri) =>
      Array.from({ length: n }, (_, i) => {
        const id = seed;
        seed += 1;
        return {
          id,
          kind: ri === 0 ? 'HttpContract' : 'Method',
          name:
            ri === 0
              ? `GET /api/admin/order/refund_and_invoice_${i}`
              : `store_order_refund_service_${i}`,
          ring: ri + 1,
        };
      }),
    );
    const edges: LayoutEdge[] = rings.flat().map((n) => ({ id: n.id, from: n.id, to: center.id }));
    return { center, rings, edges };
  };

  it('80 consumers: switches to center-radial, both readability hard constraints hold, and hop-count adjacency is preserved', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const r = radialLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // Indeed went radial: all consumers' left edges align into one column (concentric rings would spread them along different angles)
    const lefts = new Set(
      rings.flat().map((n) => {
        const p = r.nodes.find((x) => x.id === n.id)!;
        return Math.round(p.x - (p.w ?? 0) / 2);
      }),
    );
    expect(lefts.size).toBe(1);

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);

    // Hop count is not expressed by radius, so adjacency must compensate: the inner ring (direct consumers) is ordered entirely before the outer ring (traced back)
    const yOf = (n: LayoutNode) => r.nodes.find((x) => x.id === n.id)!.y;
    expect(Math.max(...rings[0].map(yOf))).toBeLessThan(Math.min(...rings[1].map(yOf)));
  });

  it('same data: concentric rings stretch past 3x the container width, while the radial canvas stays an order of magnitude smaller', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const input = { center, rings, edges, width: WIDTH, height: 720 };
    const star = radialLayout(input);
    const disc = concentricLayout(input);

    // Concentric rings: radius ∝ people ⇒ canvas side ∝ people, blowing straight out of the container horizontally
    expect(disc.width).toBeGreaterThan(3 * WIDTH);
    // The channel takes its value from the vertical span (mind-map proportions), and the canvas is **allowed to exceed the container width** (overflow = horizontal scroll):
    // at 80 people fit scaling is capped by height (80 rows × 44px far exceeds container height), so widening the channel doesn't shrink the font.
    // The invariant is "narrower than concentric rings + an order of magnitude less area", not "fits inside the container".
    expect(star.width).toBeLessThan(disc.width);
    expect(star.width * star.height * 3).toBeLessThan(disc.width * disc.height);
  });

  it('at this density concentric rings do press outer-ring edges onto inner-ring pills (records the reason for dispatch)', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const disc = concentricLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // This is a **characterization test of a known defect**, not desired behavior: the inner ring's radius is computed to "just fill up",
    // * so an edge running from the center through to the outer ring will very likely pierce some inner-ring pill (making the name unreadable).
    // If concentric rings later fix this hard constraint themselves, change this assertion and relax the dispatch condition accordingly,
    // * rather than just deleting it.
    expect(segmentsThroughNodes(disc.edges, disc.nodes)).toBeGreaterThan(0);
  });

  it('a small star still uses concentric rings: the case where rings look best must not be casually replaced', () => {
    const { center, rings, edges } = resourceStar([2, 3]);
    const r = radialLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // Concentric rings' signature: ring guides present (= hop-count ticks); a radial layout has no guides
    expect(r.guides?.length).toBe(2);
  });

  it('a multi-layer graph with leaf-to-leaf edges still uses concentric rings (manually switching a chain perspective to radial must not silently change its look)', () => {
    const center = node(1, 'handler', 0);
    const ring1 = Array.from({ length: 4 }, (_, i) => node(10 + i, `service_${i}`, 1));
    const ring2 = Array.from({ length: 20 }, (_, i) => node(50 + i, `table_${i}`, 2));
    const edges: LayoutEdge[] = [];
    ring1.forEach((n) => edges.push({ id: n.id, from: center.id, to: n.id }));
    ring2.forEach((n, i) => edges.push({ id: n.id, from: ring1[i % ring1.length].id, to: n.id }));

    const r = radialLayout({ center, rings: [ring1, ring2], edges, width: WIDTH, height: 720 });

    expect(r.guides).toBeDefined();
  });
});

/**
 * Regression: center-radial **source-end (hub side) anchors must be spread out**.
 *
 * The old implementation pressed every hub edge's start point into ±9px (18px total) of the center pill's vertical edge -- 30 out-edges
 * mushed into one bundle near the center, and you had to wait for the lines to fan out to tell "which goes to whom". Now they spread along the center pill's
 * side facing the neighbor -- "top → side → bottom" -- in target order, one entry/exit per edge.
 *
 * The red line for spreading: the start point must still land on the **visible** stretch of that edge's boundary -- a segment may only "hug" the center pill,
 * never dive under it and emerge on the other side (invisible while the pill is opaque, but exposed when hover focus makes it translucent).
 */
describe('center-radial: source-side anchors spread out', () => {
  const WIDTH = 1040;

  /** The parameter interval [t0, t1] where a segment intersects a rectangle (slab method, implemented independently on the test side); null when no intersection. */
  const range = (a: Pt, b: Pt, cx: number, cy: number, w: number, h: number) => {
    let t0 = 0;
    let t1 = 1;
    const dx = b[0] - a[0];
    const dy = b[1] - a[1];
    const slabs: Array<[number, number]> = [
      [-dx, a[0] - (cx - w / 2)],
      [dx, cx + w / 2 - a[0]],
      [-dy, a[1] - (cy - h / 2)],
      [dy, cy + h / 2 - a[1]],
    ];
    for (const [p, q] of slabs) {
      if (p === 0) {
        if (q < 0) return null;
        continue;
      }
      const r = q / p;
      if (p < 0) {
        if (r > t1) return null;
        if (r > t0) t0 = r;
      } else {
        if (r < t0) return null;
        if (r < t1) t1 = r;
      }
    }
    return { t0, t1 };
  };

  /** A hub-side endpoint of each edge in a star graph (first point if the edge leaves the center, last point if it points to the center). */
  const hubEnds = (edges: PlacedEdge[], centerId: number): Pt[] =>
    edges.map((e) => (e.from === centerId ? e.points[0] : e.points[e.points.length - 1]));

  /** Anchor spread: the distance between the two farthest anchors. */
  const spread = (ends: Pt[]): number => {
    let m = 0;
    for (const p of ends) for (const q of ends) m = Math.max(m, Math.hypot(p[0] - q[0], p[1] - q[1]));
    return m;
  };

  /** Every anchor must land on the **boundary** of the center pill: hugging the node, no gap and no floating. */
  const expectOnHubBorder = (ends: Pt[], hub: PlacedNode) => {
    const hw = (hub.w ?? 120) / 2;
    const hh = (hub.h ?? 26) / 2;
    ends.forEach((p) => {
      const onSide =
        Math.abs(Math.abs(p[0] - hub.x) - hw) < 0.51 && Math.abs(p[1] - hub.y) <= hh + 0.51;
      const onCap =
        Math.abs(Math.abs(p[1] - hub.y) - hh) < 0.51 && Math.abs(p[0] - hub.x) <= hw + 0.51;
      expect(onSide || onCap).toBe(true);
    });
  };

  it('equal-radius fan (30 out-edges): anchors hug the pill boundary, one entry/exit per edge, spread far wider than the old 18px', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });
    const hub = r.nodes.find((n) => n.id === center.id)!;
    const ends = hubEnds(r.edges, center.id);

    expectOnHubBorder(ends, hub);
    expect(new Set(ends.map((p) => `${Math.round(p[0])}:${Math.round(p[1])}`)).size).toBe(30);
    expect(spread(ends)).toBeGreaterThan(80);
  });

  it('after spreading, a segment still only "hugs" the center pill, never dives in and emerges from the other side', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });
    const hub = r.nodes.find((n) => n.id === center.id)!;

    r.edges.forEach((e) => {
      const hit = range(e.points[0], e.points[1], hub.x, hub.y, hub.w ?? 120, hub.h ?? 26);
      // if it intersects the center pill at all, it can only graze the endpoint (length ≈ 0), never "go in and come out the other side"
      if (hit) expect(hit.t1 - hit.t0).toBeLessThan(0.02);
    });

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    expect(crossings(r.edges)).toBe(0);
  });

  it('single-column shape (80 consumers, edges pointing to the center) spreads too, both hard constraints still hold', () => {
    const center = node(1, 'cache_order_summary', 0);
    const users = Array.from({ length: 80 }, (_, i) => node(100 + i, `user_${i}`, 1));
    const edges: LayoutEdge[] = users.map((n) => ({ id: n.id, from: n.id, to: center.id }));

    const r = radialLayout({ center, rings: [users], edges, width: WIDTH, height: 720 });
    const hub = r.nodes.find((n) => n.id === center.id)!;
    const ends = hubEnds(r.edges, center.id);

    expectOnHubBorder(ends, hub);
    expect(new Set(ends.map((p) => `${Math.round(p[0])}:${Math.round(p[1])}`)).size).toBe(80);
    expect(spread(ends)).toBeGreaterThan(80);

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    expect(crossings(r.edges)).toBe(0);
  });
});
