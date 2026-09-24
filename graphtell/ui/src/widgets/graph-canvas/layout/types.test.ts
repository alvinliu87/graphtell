import { describe, expect, it } from 'vitest';

import { concentricLayout, layeredLayout, radialLayout } from './types';
import type { LayoutEdge, LayoutNode, PlacedEdge, PlacedNode } from './types';

/**
 * 下面两个判定是**测试侧独立实现**的，刻意不去复用被测代码里的 `segHitsRect` / `countCrossings`：
 * 用同一份判断去断言自己，等于什么都没验。
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

/** 线段是否穿过矩形（slab 法）。 */
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

/** 穿过非端点节点的线段数（硬约束：必须为 0）。 */
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

/** 交叉数（共享端点的边对不算）。 */
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
 * 回归：分层布局的画布宽度必须受容器约束。
 *
 * 旧实现把一层的所有药丸单行铺开，20 条出边就能把画布撑到 3000px+；
 * 画布随后用 `viewBox="0 0 layout.width …"` 渲染，浏览器按 `meet` 把整张图
 * 连文字一起等比压回容器宽 —— 表现就是"出边一多，字全糊"。
 */

const node = (id: number, name: string, ring: number): LayoutNode => ({
  id,
  kind: 'Method',
  name,
  ring,
});

describe('layeredLayout', () => {
  const WIDTH = 1040;

  it('多层时允许画布超出容器宽：超出部分改为横向滚动，不再是整图缩小', () => {
    // 20 条出边，旧实现会产出约 3300px 宽的画布
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

    // 画布可以比容器宽（一轮前不允许，因为当时 viewBox 会把整张图连字一起缩小；
    // 现在 viewBox 与渲染 1:1，超出就是滚动），但**必须给出内容包围盒**供 zoom-to-fit 使用。
    expect(r.content).toBeDefined();
    expect(r.content!.w).toBeLessThan(r.width);
    // 每层一行：节点的不同 y 值个数 == 层数
    const rowsByY = new Set(r.nodes.map((n) => n.y));
    expect(rowsByY.size).toBe(3);
  });

  it('content 包围盒覆盖所有节点，且不超出画布', () => {
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
    // 供 zoom-to-fit 使用：必须比整块画布小，否则 fit 等于没做
    expect(c.w).toBeLessThan(r.width);
    expect(c.h).toBeLessThan(r.height);
  });

  it('同父的兄弟排在一起（减少跨行连线）', () => {
    const center = node(1, 'root', 0);
    const parents = [node(10, 'p0', 1), node(11, 'p1', 1)];
    // 交错给出：不排序的话两个父的孩子会交替出现
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
 * 回归：可读性的两条硬约束。
 *
 * 「边交叉」只能尽量减小（NP-hard），但**在你这个数据里最常见的形状上它本来就该是 0**：
 * 折叠视图里语义节点是终点（`view_service.rs`），所以"一个契约读了 32 个配置键"
 * 就是 center + 单环 —— 所有边共用一个端点，从同一点出发的线段互不相交。
 *
 * 「边穿过节点」是比交叉更严重的问题（药丸被线穿过后名字读不出来），
 * 它是**可判定**的，因此必须 100% 消除，任何布局都不许违反。
 */
describe('可读性硬约束', () => {
  const WIDTH = 1040;

  it('小扇出星形 → 分层两行（中心在上、单环在下），边交叉为 0 且边不穿过节点', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 8 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // 确实是两层：中心一行在上、单环一行在下（与中心辐射的单列区分开）
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

  it('宽扇出星形（> LAYERED_FAN_MAX）→ 改走中心辐射（非同心环），两条硬约束仍成立', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 24 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // 是中心辐射而非同心环：没有环引导线
    expect(r.guides).toBeUndefined();
    // 中心是唯一的最左节点，所有邻居都在中心右侧（无论单列还是等长扇形都如此）
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
   * 回归：宽扇出 + **一条叶子间边**（路由视角里两张表之间的 ForeignKey）。
   *
   * 旧判定 `isStar` 要求每条边都碰中心 —— 一条 ForeignKey 就把判定判死，
   * 19 片叶子落回每层一行的宽扇形（一行 6000px+、两端被裁的实测坏截图）。
   * 现在近似星形（叶子间边 ≤ 3）仍走中心辐射；叶子间边自身若扫过同列邻居，
   * 必须绕行（「边不穿过节点」不因放宽而破例）。
   */
  it('宽扇出 + 个别叶子间边（ForeignKey）→ 仍走中心辐射，叶子间边绕行不穿节点', () => {
    const center = node(1, 'GET /admin/order/detail', 0);
    const fanout = [
      ...Array.from({ length: 4 }, (_, i) => node(10 + i, `middleware_${i}`, 1)),
      ...Array.from({ length: 14 }, (_, i) => node(100 + i, `config_key_${i}`, 1)),
      node(200, 'store_order_refund', 1),
      node(201, 'user', 1),
    ];
    const edges: LayoutEdge[] = [
      ...fanout.map((n) => ({ id: n.id, from: center.id, to: n.id })),
      // 表间外键：两端都不是中心 —— 旧判定下这一条就把整张图推回宽扇形
      { id: 900, from: 200, to: 201 },
    ];

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    // 走了中心辐射而非分层扇形：没有环引导线，中心在最左
    expect(r.guides).toBeUndefined();
    const centerX = r.nodes.find((n) => n.id === center.id)!.x;
    fanout.forEach((n) => {
      expect(r.nodes.find((x) => x.id === n.id)!.x).toBeGreaterThan(centerX);
    });

    // 两条硬约束仍成立（ForeignKey 边要么直连不撞、要么绕行）
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    expect(crossings(r.edges)).toBe(0);
  });

  /**
   * 回归：路由视角的**左→右方向性**。
   *
   * 路由视角的图是「前端调用方 --CallsHttp--> 契约 --ReadsConfig/ReadsCache--> 依赖」，
   * 旧的中心辐射把调用方和依赖混排在中心右侧一列，请求流向在画布上没有方向感。
   * 现在两侧都非空时分左右两列：调用方在中心左侧、依赖在右侧，所有箭头自左向右。
   */
  it('双侧星形（调用方 + 宽依赖扇出）→ 调用方在左、依赖在右，请求流向自左向右', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const caller = node(2, 'orderInvoiceDetail', 1); // 前端 Function（入边）
    const deps = Array.from({ length: 12 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = [
      { id: 900, from: caller.id, to: center.id },
      ...deps.map((n) => ({ id: n.id, from: center.id, to: n.id })),
    ];

    const r = layeredLayout({ center, rings: [[caller, ...deps]], edges, width: WIDTH, height: 720 });

    const cRect = r.nodes.find((n) => n.id === center.id)!;
    const callerRect = r.nodes.find((n) => n.id === caller.id)!;
    // 调用方整体（右缘）在中心左缘的左边；依赖在中心右侧
    expect(callerRect.x + (callerRect.w ?? 0) / 2).toBeLessThan(cRect.x - (cRect.w ?? 0) / 2);
    deps.forEach((n) => {
      expect(r.nodes.find((x) => x.id === n.id)!.x).toBeGreaterThan(cRect.x);
    });
    // 调用边在左通道折行：折点 x 位于调用方与中心之间
    const callEdge = r.edges.find((e) => e.from === caller.id)!;
    const viaX = callEdge.points[1][0];
    expect(viaX).toBeGreaterThan(callerRect.x);
    expect(viaX).toBeLessThan(cRect.x);

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('多层分层：边绝不穿过节点（斜边会自动绕行）', () => {
    const center = node(1, 'handler', 0);
    const ring1 = Array.from({ length: 4 }, (_, i) => node(10 + i, `service_${i}`, 1));
    const ring2 = Array.from({ length: 10 }, (_, i) => node(50 + i, `table_${i}`, 2));
    const edges: LayoutEdge[] = [];
    ring1.forEach((n) => edges.push({ id: n.id, from: center.id, to: n.id }));
    ring2.forEach((n, i) => edges.push({ id: n.id, from: ring1[i % ring1.length].id, to: n.id }));

    const r = layeredLayout({ center, rings: [ring1, ring2], edges, width: WIDTH, height: 720 });

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('截图中那种规模（契约读 32 个配置键）仍满足两条硬约束，且同类聚在弧的一端不被埋没', () => {
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
    // 「同类聚在一起」在等长扇形下表现为：沿弧按角度排序后，Cache / Table 分居弧的两
    // 端（头 / 尾），中间整段都是配置键，不被埋在中间
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
   * 回归：适中扇出（截图的 30 个规模）走**等长扇形**——邻居到中心的半径相等，且
   * 弧上相邻药丸不重叠（限角 110° 的几何保证），两条硬约束仍成立。超大扇出（80）则
   * 退回单列（圆弧面积会反超单列）。
   */
  it('适中扇出（30）→ 等长扇形：邻居到中心半径相等、相邻药丸不重叠', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    const hub = r.nodes.find((n) => n.id === center.id)!;
    const spokes = r.nodes.filter((n) => n.id !== center.id);
    // 弧上每个邻居到中心**圆心**的距离应一致（结构上的"等边"）
    const radii = spokes.map((n) => Math.hypot(n.x - hub.x, n.y - hub.y));
    const maxR = Math.max(...radii);
    const minR = Math.min(...radii);
    expect(maxR - minR).toBeLessThan(1);
    // 相邻（按角度排序）药丸矩形不重叠：限角保证法向间距 ≥ 药丸高
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

  it('超大扇出（80）→ 退回单列（左边缘对齐），等长弧面积反而更大不应启用', () => {
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

  it('同一对端点的多条路径必须错开，不能画成完全重叠的一条线', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 8 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));
    // 第二条通往同一个配置键的**不同路径**（后端现在会各出一条边，via 不同）
    edges.push({ id: 999, from: center.id, to: fanout[0].id });

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });

    const pair = r.edges.filter(
      (e) => e.from === center.id && e.to === fanout[0].id,
    );
    expect(pair.length).toBe(2);
    // 两条边的几何不能相同：否则用户只看到一条线，根本不知道有两条路径
    expect(JSON.stringify(pair[0].points)).not.toBe(JSON.stringify(pair[1].points));
    // 错开后仍满足两条硬约束
    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
  });

  it('多层且某层很宽（会换行）时，边仍不穿过节点', () => {
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
 * 回归：径向入口必须按**图形状**分派，而不是按"有几个环 / 环上有几个节点"。
 *
 * 资源视角（Table / Cache / Event / Queue / Topic）在 `view_service.rs` 里走反向模式
 * （`reverse = kind != HttpContract`），沿入边回溯到每个使用者后，把使用者**直接合成一条到
 * 中心的边**（`MAX_USERS = 80`，其余计入 `hidden`）。此时 `ring` 只是挂在节点上的数字，
 * 并没有对应的"叶子到叶子"的边 —— 传进来的是一颗**星**，而 `layeredLayout` 那种
 * `nonEmpty.length === 1` 的判据在这里必然为假，必须用 `isStar` 来认。
 *
 * 这类数据恰好是 `MAX_USERS` 截断后的量级：同心环会被撑到容器数倍宽，而内容只占环上
 * 一条 30px 宽的带子；改走中心辐射后同样满足两条可读性硬约束，画布小一个数量级。
 */
describe('radialLayout：资源视角（星形）', () => {
  const WIDTH = 1040;

  /** 造一个资源视角形态的输入：`ringSizes` = 每环人数，所有边都是 `使用者 → 中心`。 */
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

  it('80 个使用者：改走中心辐射，两条可读性硬约束都成立，且保留跳数相邻性', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const r = radialLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // 确实走了辐射：所有使用者的左边缘对齐成一列（同心环会沿不同角度散开）
    const lefts = new Set(
      rings.flat().map((n) => {
        const p = r.nodes.find((x) => x.id === n.id)!;
        return Math.round(p.x - (p.w ?? 0) / 2);
      }),
    );
    expect(lefts.size).toBe(1);

    expect(crossings(r.edges)).toBe(0);
    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);

    // 跳数不再由半径表达，必须由相邻性补回来：内环（直接使用者）整段排在外环（追溯得到）之前
    const yOf = (n: LayoutNode) => r.nodes.find((x) => x.id === n.id)!.y;
    expect(Math.max(...rings[0].map(yOf))).toBeLessThan(Math.min(...rings[1].map(yOf)));
  });

  it('同一份数据：同心环会被撑到容器 3 倍宽以上，辐射画布仍小一个数量级', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const input = { center, rings, edges, width: WIDTH, height: 720 };
    const star = radialLayout(input);
    const disc = concentricLayout(input);

    // 同心环：半径 ∝ 人数 ⇒ 画布边长 ∝ 人数，直接横向炸出容器
    expect(disc.width).toBeGreaterThan(3 * WIDTH);
    // 通道按纵向跨度取值（思维导图比例），画布**允许超出容器宽**（超出即横向滚动）：
    // 80 人时 fit 缩放由高度卡住（80 行 × 44px 远超容器高），加宽通道不缩小字号。
    // 不变量是"比同心环窄 + 面积小一个数量级"，而非"收在容器内"。
    expect(star.width).toBeLessThan(disc.width);
    expect(star.width * star.height * 3).toBeLessThan(disc.width * disc.height);
  });

  it('同心环在这种密度下确实会把外环的边压在内环药丸上（记录分派理由）', () => {
    const { center, rings, edges } = resourceStar([55, 25]);
    const disc = concentricLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // 这是**已知缺陷的特征化测试**，不是期望行为：内环半径是按"刚好排满"算的，
    // 所以从中心贯穿到外环的边大概率穿过某个内环药丸（名字直接读不出来）。
    // 若日后同心环自己修好了这条硬约束，应改这条断言并要求同步放宽分派条件，
    // 而不是直接删掉它。
    expect(segmentsThroughNodes(disc.edges, disc.nodes)).toBeGreaterThan(0);
  });

  it('小星形仍走同心环：圆最好看的场景不能被顺手换掉', () => {
    const { center, rings, edges } = resourceStar([2, 3]);
    const r = radialLayout({ center, rings, edges, width: WIDTH, height: 720 });

    // 同心环的标志：有环引导线（= 跳数刻度）；辐射布局没有 guides
    expect(r.guides?.length).toBe(2);
  });

  it('有叶子间边的多层图仍走同心环（把链路视角手动切成径向时不能悄悄变样）', () => {
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
 * 回归：中心辐射的**源端（hub 侧）锚点必须分散**。
 *
 * 旧实现把所有 hub 边的出发点都压在中心药丸竖边的 ±9px（共 18px）内 —— 30 条出边在
 * 近中心处糊成一束，要等线散开才看得出"哪条通向谁"。现在沿中心药丸面向邻居那一侧的
 * 「上边 → 侧边 → 下边」按目标次序铺开，一条边一个出入口。
 *
 * 铺开的红线：出发点仍必须落在这条边**看得见**的那段边界上 —— 线段只能"贴"住中心药丸，
 * 不能钻进药丸底下再从另一侧穿出（药丸不透明时看不出来，悬浮聚焦压成半透明就露馅）。
 */
describe('中心辐射：源端锚点分散', () => {
  const WIDTH = 1040;

  /** 线段与矩形相交的参数区间 [t0, t1]（slab 法，测试侧独立实现）；不相交返回 null。 */
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

  /** 星形图里每条边的 hub 侧端点（边由中心出发则取首点，指向中心则取末点）。 */
  const hubEnds = (edges: PlacedEdge[], centerId: number): Pt[] =>
    edges.map((e) => (e.from === centerId ? e.points[0] : e.points[e.points.length - 1]));

  /** 锚点的铺开幅度：最远两个锚点的距离。 */
  const spread = (ends: Pt[]): number => {
    let m = 0;
    for (const p of ends) for (const q of ends) m = Math.max(m, Math.hypot(p[0] - q[0], p[1] - q[1]));
    return m;
  };

  /** 每个锚点都必须落在中心药丸的**边界**上：贴着节点，不留缝也不悬空。 */
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

  it('等长扇形（30 条出边）：锚点贴在药丸边界上、一条边一个出入口、铺开远宽于旧的 18px', () => {
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

  it('分散后线段仍只"贴"住中心药丸，不钻进去再从另一侧穿出', () => {
    const center = node(1, 'GET /v2/order/invoice_detail', 0);
    const fanout = Array.from({ length: 30 }, (_, i) => node(100 + i, `config_key_${i}`, 1));
    const edges: LayoutEdge[] = fanout.map((n) => ({ id: n.id, from: center.id, to: n.id }));

    const r = layeredLayout({ center, rings: [fanout], edges, width: WIDTH, height: 720 });
    const hub = r.nodes.find((n) => n.id === center.id)!;

    r.edges.forEach((e) => {
      const hit = range(e.points[0], e.points[1], hub.x, hub.y, hub.w ?? 120, hub.h ?? 26);
      // 与中心药丸若有交，只能是端点擦边（长度≈0），不能是"穿进去再从另一侧出来"
      if (hit) expect(hit.t1 - hit.t0).toBeLessThan(0.02);
    });

    expect(segmentsThroughNodes(r.edges, r.nodes)).toBe(0);
    expect(crossings(r.edges)).toBe(0);
  });

  it('单列形态（80 个使用者，边指向中心）同样分散，两条硬约束仍成立', () => {
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
