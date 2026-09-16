import type { LayoutMode } from '@/entities/view';

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
   * 该边在 `edges` 输入数组中的下标（由调用方填入，布局原样透传）。
   *
   * 折叠视图里同一对端点、**同一个 `id`** 可能对应多条**不同路径**（`via` 不同）：
   * 正向视角下一条传播边（seed）会被 `enumerate_chain_paths` 展开成多条链，
   * 而 `view_service.rs` 的 `push_edge` 给它们复用同一个 evidence 边 id。
   * 只按 `id:from->to` 做键会撞键 —— 表现为"悬浮一条高亮全部、悬浮卡永远显示第一条链"。
   * 带上原始下标，键即唯一，且渲染时可用 `edges[seq]` 精确回指到**含各自 `via` 的那条** `EdgeView`。
   */
  seq?: number;
}

export interface PlacedNode extends LayoutNode {
  x: number;
  y: number;
    /** 节点形状：统一为 `rect` 药丸（文字内嵌于框内），所有布局一致。 */
  shape: 'circle' | 'rect';
  w?: number;
  h?: number;
}

export interface PlacedEdge extends LayoutEdge {
  /** 折线顶点；两点即直线，多点即正交折线。 */
  points: Array<[number, number]>;
  /** 正交折线（90°）还是直线。 */
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
  /** 径向布局的同心环引导线（仅作视觉参照，不参与交互 / 命中）。 */
  guides?: Array<{ cx: number; cy: number; r: number; label: string }>;
  rowHeaders?: Array<{ label: string; x: number; y: number }>;
  colHeaders?: Array<{ label: string; x: number; y: number }>;
  width: number;
  height: number;
  /**
   * 内容的真实包围盒（世界坐标），供画布做 zoom-to-fit。
   *
   * `width/height` 是**画布尺寸**（通常等于容器宽/高），内容往往只是居中在其中的一小块；
   * 若拿画布尺寸当 fit 目标，内容永远只占中间一小片、看起来"整张图很小"。
   * 不提供时画布回退到整块画布（旧行为）。
   */
  content?: { x: number; y: number; w: number; h: number };
  /** 布局算法的说明（展示给用户，保证可解释）。 */
  note: string;
}

export interface LayoutInput {
  center: LayoutNode;
  rings: LayoutNode[][];
  edges: LayoutEdge[];
  /** 聚类框（聚合视角）。 */
  clusters?: Array<{ key: string; label: string; count: number; members: LayoutNode[] }>;
  /** 矩阵（多端对比视角）。 */
  matrix?: {
    rows: string[];
    cols: string[];
    cells: number[][];
  };
  width: number;
  height: number;
}

export type LayoutFn = (input: LayoutInput) => LayoutResult;

/**
 * 布局注册表。
 *
 * **任何节点都不允许力导向自由漂移**：位置完全由算法与输入顺序决定，
 * 同一份输入必然得到同一份输出，因此可复现、可截图对比、可写单测。
 */
export const LAYOUTS: Record<LayoutMode, LayoutFn> = {
  radial: radialLayout,
  layered: layeredLayout,
  spine: spineLayout,
  compound: compoundLayout,
  matrix: matrixLayout,
  er: erLayout,
};

const PILL_H = 30;
/** 药丸节点宽度估算：按 `kind · name`（截断 26 字）算宽，限 96–220px，与 GraphCanvas 内文字截断一致。 */
function pillWidth(kind: string, name: string): number {
  const text = `${kind} · ${name}`.slice(0, 26);
  return Math.max(96, Math.min(220, text.length * 7 + 22));
}

// ---------------------------------------------------------------- radial

/**
 * 图是否为一颗**星**：每条边都有一端是中心。
 *
 * 这是语义判断而非几何判断 —— 资源视角（Table / Cache / Event / Queue / Topic）在
 * `view_service.rs` 里走反向模式（`reverse = kind != HttpContract`），沿入边回溯到每个
 * 使用者后，**把使用者直接合成一条到中心的边**（`MAX_USERS = 80`，其余计入 `hidden`），
 * `ring` 只是挂在节点上的数字、并没有对应的叶子到叶子的边。
 * 于是传到布局的图看起来"有好几环"，结构上却是一颗星。
 */
function isStar(centerId: number, edges: LayoutEdge[]): boolean {
  return edges.every((e) => e.from === centerId || e.to === centerId);
}

/**
 * 径向布局的统一入口：按**图形状**选布局族，而不是让用户自己去试。
 *
 * 星形（= 上述资源视角）一旦扇出上来，同心环是最差的选择：
 * 1. 每片叶子约占 `pillWidth + GAP ≈ 188px` 弧长 ⇒ 半径 ≈ 30×n，画布边长是半径的两倍
 *    ⇒ **面积 ∝ n²**；内容只占环上一条 30px 宽的带子，80 个使用者时约 98% 的画面是空的；
 * 2. 外环的叶子有一条从中心贯穿出来的边，而内环半径恰好是按"刚好排满"算的 ——
 *    这条边大概率从某个内环药丸的名字上横穿过去（"边不穿过节点"是硬约束）。
 *
 * 所以星形且扇出 ≥ `HUB_MIN` 时改走中心辐射：同样可证 0 交叉、0 穿节点，
 * 画布小一个数量级，代价只是纵向滚动（机械成本，见 `hubSpokeLayout` 的取舍说明）。
 *
 * 小星形（< `HUB_MIN`，如"2 个生产者 + 3 个消费者"的事件视角）保留同心圆 ——
 * 那是圆最好看、也最不可替代的场景。
 */
export function radialLayout(input: LayoutInput): LayoutResult {
  const leaves = input.rings.flat();
  if (leaves.length >= HUB_MIN && isStar(input.center.id, input.edges)) {
    return hubSpokeLayout(input, leaves, true);
  }
  return concentricLayout(input);
}

/** 同心环：环 = 跳数。小图与真实多层图（存在叶子到叶子的边）的径向形态。 */
export function concentricLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const maxRing = Math.max(1, rings.length);
  // 所有节点一律用带文字的矩形药丸（文字内嵌），与 layered / spine / matrix / ER 完全统一。

  // 每个环的半径独立计算：环上药丸越多越宽，环就必须越大才能沿圆周排开而不重叠；
  // 同时保证与上一环径向不重叠，且环 1 不被中心药丸盖住。这样无论第几环有多少节点都不挤。
  const GAP = 18;
  const PAD = 60;
  const maxPillW = Math.max(110, ...rings.flat().map((n) => pillWidth(n.kind, n.name)), pillWidth(center.kind, center.name));
  const ringRadii: number[] = [];
  let r = 0;
  for (let i = 0; i < rings.length; i++) {
    const ring = rings[i];
    const arcNeeded = ring.reduce((s, n) => s + pillWidth(n.kind, n.name) + GAP, 0);
    const circNeeded = arcNeeded / (2 * Math.PI);
    const radialFloor = i === 0 ? maxPillW / 2 + 24 : r + PILL_H + 14;
    r = Math.max(radialFloor, circNeeded);
    ringRadii.push(r);
  }
  const maxR = ringRadii.length ? ringRadii[ringRadii.length - 1] : 0;

  // 画布按内容自适应放大，避免密集图被 viewBox 裁切（再由平移 / 缩放查看）。
  const ext = maxR + maxPillW / 2 + PAD;
  const contentW = Math.max(width, ext * 2);
  const contentH = Math.max(height, ext * 2);
  const cx = contentW / 2;
  const cy = contentH / 2;

  const nodes: PlacedNode[] = [
    { ...center, x: cx, y: cy, shape: 'rect', w: pillWidth(center.kind, center.name), h: PILL_H },
  ];

  const ringCount = rings.length;
  rings.forEach((ring, i) => {
    const radius = ringRadii[i];
    const count = ring.length;
    if (count === 0) return;
    // 环内节点均匀等分（夹角相等）；并让每个环整体旋转一个与环序号相关的相位，
    // 使不同环的节点朝不同方向散开——避免"单节点环都堆在 12 点"导致的边共线重叠。
    // 这是从布局层根治，比事后把边掰弯更诚实、更清晰。
    const phase = ringCount > 1 ? (2 * Math.PI * i) / ringCount : 0;
    ring.forEach((n, idx) => {
      const angle = (2 * Math.PI * idx) / count - Math.PI / 2 + phase;
      nodes.push({
        ...n,
        x: cx + radius * Math.cos(angle),
        y: cy + radius * Math.sin(angle),
        shape: 'rect',
        w: pillWidth(n.kind, n.name),
        h: PILL_H,
      });
    });
  });

  const pos = new Map<number, [number, number]>();
  nodes.forEach((n) => {
    pos.set(n.id, [n.x, n.y]);
  });

  // 边统一为直线：不同环已按环序号错开相位（见上），端点很少再共线，
  // 故无需事后把边掰弯——直线更诚实、也更清晰。只保留两端都存在的边。
  const placed: PlacedEdge[] = edges.flatMap((e) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    return [{ id: e.id, from: e.from, to: e.to, points: [a, b], orthogonal: false }];
  });

  // 同心环引导线：把"环 = 跳数"显式画出来（环 1 = 直接关联）。
  // 仅作视觉参照：浅灰细线、不参与交互；环号标在每环顶外侧。
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
    note: '径向布局：中心为当前对象，同心环表示跳数（环 1 = 直接关联）。环半径按各环药丸数量自适应，避免重叠。',
  };
}

// ---------------------------------------------------------------- layered

// ---------------------------------------------------------------- 几何

type Pt = [number, number];

/**
 * 线段是否穿过轴对齐矩形（Liang–Barsky 裁剪）。
 *
 * 「边穿过节点」是比「边交叉」严重得多的问题：药丸被线穿过后名字直接读不出来。
 * 所以这一条是**硬约束**，必须 100% 消除 —— 与只能尽量减小的交叉数不同。
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

/** 两线段是否**真正交叉**；共线或仅端点接触不算（那是"汇合"，不是交叉）。 */
function segCross(p1: Pt, p2: Pt, p3: Pt, p4: Pt): boolean {
  const o = (a: Pt, b: Pt, c: Pt) =>
    Math.sign((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]));
  return o(p3, p4, p1) * o(p3, p4, p2) < 0 && o(p1, p2, p3) * o(p1, p2, p4) < 0;
}

/** 折线边集之间的交叉总数。共享端点的边对跳过：它们在节点处会合，不是交叉。 */
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
 * 给穿过节点的边加绕行：**以碰撞点为中心做平行移位**，而不是只把中点推弯。
 *
 * 只推中点是无效的典型场景：目标节点在换行后的第 2 行，碰撞发生在 t≈0.9 处，
 * 而中点位移在两端衰减到 0 —— 推得再远也绕不开。平行移位把 t∈[tc-0.3, tc+0.3]
 * 整段平移到法线方向，位移在碰撞点处是**满值**，才真正有效。
 *
 * 移位量从小到大、两侧都试，优先最小扰动。实在绕不开（节点过密）时返回最后一次尝试，
 * 至少比直接穿过节点好。
 */
function detourAroundNodes(a: Pt, b: Pt, obstacles: PlacedNode[]): Pt[] {
  const straight: Pt[] = [a, b];
  if (obstacles.length === 0) return straight;

  /** 返回第一次碰撞在 a→b 直线上的近似参数位置；无碰撞返回 null。 */
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

/** 折线是否穿过任一障碍节点（硬约束检查）。 */
function pathHits(pts: Pt[], obstacles: PlacedNode[]): boolean {
  for (let s = 1; s < pts.length; s += 1) {
    for (const o of obstacles) {
      if (segHitsRect(pts[s - 1], pts[s], o.x, o.y, o.w ?? 120, o.h ?? 26)) return true;
    }
  }
  return false;
}

/** 只保留与路径包围盒（含 margin）相交的节点，避免每条边都做 O(N) 碰撞检测。 */
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
 * 平行边（同一对端点之间的多条路径）的分组。
 *
 * 后端现在会为同一对端点的**不同路径**各出一条边（以前按 (kind,from,to) 去重只留一条，
 * 分叉信息全丢）。但它们的端点相同 —— 画出来会**完全重叠**，用户看不出有两条。
 * 所以必须按组把它们错开。
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

/** 组内第 `idx` 条的法线偏移量（居中对称）；只有一条时为 0。 */
function fanOffset(count: number, idx: number, fan = 11): number {
  return count <= 1 ? 0 : (idx - (count - 1) / 2) * fan;
}

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
 * 重心（中位数）排序 —— Sugiyama 第二阶段的标准启发式。
 *
 * **交叉数由层内顺序决定，改几何没用**。这里来回扫几轮：正向按"父节点的中位次序"排，
 * 反向按"子节点的中位次序"排。不能保证 0（交叉最小化是 NP-hard），但能把交叉压到很低，
 * 剩下的如实报给用户，而不是假装没有。
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
        // `Array.prototype.sort` 自 ES2019 起保证稳定：无邻居的节点保持原相对次序。
        .sort((x, y) => (x.b === y.b ? x.i - y.i : x.b - y.b))
        .map((x) => x.n);
    }
  }
  return work;
}

// ---------------------------------------------------------------- layered

/** 单环扇出超过这个数就改用辐射布局（单排放不下）。 */
const HUB_MIN = 7;

/**
 * 中心辐射（hub-and-spoke）：中心在左，邻居**单列**排在右侧，边走三段：
 *
 * ```
 * 中心 ──射线──▶ (通道 x, 目标 y) ──短横──▶ 目标
 * ```
 *
 * 为什么是三段而不是一条直线：列很高（32 个节点约 1400px）而横向间距只有几百 px 时，
 * 直连射线会**斜扫过中间若干药丸**——从中心往下数第 30 个节点，射线必然擦过中间那些。
 * 拆成三段后：纵向段走在列前的通道里（所有药丸都在它右边），横向段走在目标自己的中线上，
 * 射线段只在通道左侧活动。三段各自都碰不到任何节点。
 *
 * 为什么交叉必然为 0：**从同一点出发的两条线段内部永不相交**，射线段之间不交叉；
 * 横向段各自在不同的 y 上、且只在通道右侧，也碰不到别人的射线段。这是结构保证，不是优化结果。
 *
 * 为什么不排成圆弧：圆弧同样可证零交叉（所有目标等距，射线只在端点触到目标圈），
 * 但 32 个节点排半圈需要半径约 1700px、画布约 1800×3400；
 * 单列只要宽约 600、高 n×44，代价只是纵向滚动 —— 而滚动是机械成本，交叉是歧义成本。
 */
function hubSpokeLayout(input: LayoutInput, fanout: LayoutNode[], viaStar = false): LayoutResult {
  const { center, edges, width, height } = input;
  const PAD = 40;
  const ROW_GAP = 14;
  const HUB_GAP_X = 140;
  const GUTTER = 14; // 列前通道宽度

  const centerW = pillWidth(center.kind, center.name);
  const targetW = Math.max(...fanout.map((n) => pillWidth(n.kind, n.name)));
  const colLeft = PAD + centerW + HUB_GAP_X;
  const gutterX = colLeft - GUTTER;
  const contentW = Math.max(width, colLeft + targetW + PAD);
  const contentH = Math.max(height, PAD * 2 + fanout.length * (PILL_H + ROW_GAP) - ROW_GAP);
  const cy = Math.round(contentH / 2);

  const hub: Pt = [PAD + centerW / 2, cy];
  const nodes: PlacedNode[] = [
    { ...center, x: hub[0], y: hub[1], shape: 'rect', w: centerW, h: PILL_H },
  ];
  // 排序键：先环（= 跳数 / 追溯深度）后种类再名字。
  // 环优先是为了在单列里保留"谁是直接用者、谁是追溯出来的"这一层信息（资源视角的 `ring`，
  // 来源 `view_service.rs` 的 `ring_of`）—— 改走单列后跳数不再由半径表达，只能靠相邻性补回来。
  // 同类聚在一起：32 个配置键 + 1 个 Cache 时，Cache 不会被埋在中间。排序确定，可复现。
  fanout
    .slice()
    .sort(
      (a, b) =>
        a.ring - b.ring ||
        (a.kind === b.kind ? 0 : a.kind.localeCompare(b.kind)) ||
        a.name.localeCompare(b.name),
    )
    .forEach((n, i) => {
      const w = pillWidth(n.kind, n.name);
      nodes.push({
        ...n,
        x: colLeft + w / 2,
        y: PAD + i * (PILL_H + ROW_GAP) + PILL_H / 2,
        shape: 'rect',
        w,
        h: PILL_H,
      });
    });

  const pos = new Map(nodes.map((n) => [n.id, [n.x, n.y] as Pt]));
  const parallel = indexParallel(edges);
  const placed: PlacedEdge[] = edges.flatMap((e, ei) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    // 两端必有一端是中心（单环扇出）。资源视角是反向的（沿入边找"谁在用"），
    // 所以两种方向都要支持：通道点始终取**邻居**的 y，再把三段按方向串起来。
    if (e.from !== center.id && e.to !== center.id) {
      return [{ ...e, points: [a, b], orthogonal: false }];
    }
    const hubPos = e.from === center.id ? a : b;
    const tgtPos = e.from === center.id ? b : a;
    const via: Pt = [gutterX, tgtPos[1]];
    // 同一对端点的多条路径：把通道点沿射线法线错开，否则两条边完全重叠。
    // 错开后两条路径只在端点相交，中间形成柳叶形，互不遮挡。
    const p = parallel.get(ei);
    const off = p ? fanOffset(p.count, p.idx) : 0;
    let viaPt = via;
    if (off !== 0) {
      const dx = via[0] - hubPos[0];
      const dy = via[1] - hubPos[1];
      const len = Math.hypot(dx, dy) || 1;
      viaPt = [via[0] + (-dy / len) * off, via[1] + (dx / len) * off];
    }
    const pts: Pt[] =
      e.from === center.id ? [hubPos, viaPt, tgtPos] : [tgtPos, viaPt, hubPos];
    return [{ ...e, points: pts, orthogonal: false }];
  });

  return {
    nodes,
    edges: placed,
    width: contentW,
    height: contentH,
    content: boundsOf(nodes),
    note: viaStar
      ? `径向入口判定本图为**星形**（每条边都只在「使用者 ↔ ${center.name}」之间，即资源视角沿入边回溯的形态）：同心环的画布随人数平方增长，且外环的边会从中心贯穿、压过内环药丸的名字，因此改走中心辐射。中心在左，${fanout.length} 个使用者按「跳数 → 种类 → 名字」单列排在右侧；边走「射线 → 列前通道 → 短横入边」三段。射线共原点、互不相交，纵向段与横向段都避开所有节点，因此本图**边交叉 0 处、边不穿过任何节点**。内容较高时纵向滚动查看。`
      : `中心辐射布局：中心在左，${fanout.length} 个直接邻居单列排在右侧；边走「射线 → 列前通道 → 短横入边」三段。射线共原点、互不相交，纵向段与横向段都避开所有节点，因此本图**边交叉 0 处、边不穿过任何节点**。内容较高时纵向滚动查看。`,
  };
}

/** 节点集合的包围盒（含边标签的外扩余量）。 */
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
  // 边标签沿法线外扩约 9px、字号 10，四周各留 14px 不被裁
  return { x: minX - 14, y: minY - 14, w: maxX - minX + 28, h: maxY - minY + 28 };
}

/**
 * 多层分层（Sugiyama 简化）：自上而下，**每层只排一行**，层间直连。
 *
 * 为什么坚持每层一行，而不是换行成网格：
 * 只要某层排成多行，射向下层行的边就必然穿过上层行的药丸 —— 试过列对齐网格 + 列间隙纵向通道，
 * 但"从源节点下降"这一段仍会被源层自己的下一行挡住，得再叠一层横向绕行，
 * 越补越复杂。而每层一行时，任意一条边的 y 范围只覆盖相邻两层的两条行线，
 * **结构上不可能碰到任何节点**（除了自己的两个端点）。
 *
 * 代价是层很宽时画布会超出容器 —— 但现在 viewBox 与渲染是 1:1，超出就是**横向滚动**，
 * 不再是"整张图连字一起缩小"。按"歧义成本 > 机械成本"的取舍，这是划算的。
 */
function stackedLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const layers: LayoutNode[][] = [[center], ...rings];
  const top = 60;
  const GAP = 18;
  const PAD = Math.max(24, Math.min(60, width * 0.05));

  const ordered = orderByBarycenter(layers, edges);
  const widths = ordered.map((layer) =>
    layer.map((n) => pillWidth(n.kind, n.name)),
  );
  const layerW = widths.map((ws) =>
    ws.reduce((s, w) => s + w, 0) + GAP * Math.max(0, ws.length - 1),
  );
  const contentW = Math.max(width, ...layerW, 0) + PAD * 2;
  const centerX = contentW / 2;

  const totalLayerH = PILL_H * layers.length;
  // 层间距：默认撑满可用高度；内容本来就超高时退回最小间距，由画布滚动查看（不再直接裁掉）。
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
    // 直连（不再用"下→横→下"的正交折线：那会让同层所有边的水平段共线，标签全堆在一条 y 上）。
    // 每层一行 ⇒ 直连的 y 范围只覆盖相邻两条行线，结构上碰不到任何节点；
    // 只有跨层的"跳跃边"（折叠提拉可能产生）才会命中下面的兜底绕行。
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
      // 平行边：中点沿法线错开成柳叶形，避免多条路径完全重叠
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
  // 交叉数如实报出：交叉最小化是 NP-hard，重心排序 + 绕行只能压低、不能清零。
  // 与其假装没有，不如让用户知道"这里要花点力气"。
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
        ? '分层布局：自上而下分层（层 = 跳数），层内按重心排序、超宽自动换行、边遇节点自动绕行。边较多，交叉数未逐一统计。'
        : crossings === 0
          ? '分层布局：自上而下分层（层 = 跳数），层内按重心排序、超宽自动换行、边遇节点自动绕行。当前**边交叉 0 处**，可逐条直读。'
          : `分层布局：自上而下分层（层 = 跳数），层内按重心排序、超宽自动换行、边遇节点自动绕行。当前仍有 **${crossings} 处边交叉**（平面上无法完全消除），逐条确认时请配合悬浮高亮。`,
  };
}

// ---------------------------------------------------------------- spine

/**
 * 分层布局：始终是「自上而下分层（层 = 跳数）」的网格，不做形状自适应。
 *
 * 与 `radial` 刻意区分：径向遇到星形会自己改走中心辐射（中心在左、邻居单列在右），
 * 而分层保持「分层调用链」的本意 —— 中心一行在上、每一环一行在下。对"契约读 N 个配置键"
 * 这种资源视角的星形（中心 + 单环），分层画成两行网格：中心在上、N 个使用者横排在下一行，
 * 自中心向下呈扇形发散。因为全部边共用一个端点（中心），所以同样是 0 交叉、0 穿节点，
 * 但视觉与径向的中心辐射完全不同，切换布局能看出区别。
 */
export function layeredLayout(input: LayoutInput): LayoutResult {
  return stackedLayout(input);
}

/** 线性 Spine：找一条主链横排，其余节点挂在下方。 */
export function spineLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const adjacency = new Map<number, number[]>();
  edges.forEach((e) => {
    adjacency.set(e.from, [...(adjacency.get(e.from) ?? []), e.to]);
    adjacency.set(e.to, [...(adjacency.get(e.to) ?? []), e.from]);
  });

  // 从中心出发的最长简单路径（确定性：邻居按 id 升序）
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

  // 主链：药丸节点按实际宽度沿 x 轴依次排开（文字内嵌，与径向 / 矩阵 / ER 一致）。
  const nodes: PlacedNode[] = [];
  let cursor = PAD;
  best.forEach((id) => {
    const found = findNode(input, id);
    if (!found) return;
    const w = pillWidth(found.kind, found.name);
    nodes.push({ ...found, x: cursor + w / 2, y: spineY, shape: 'rect', w, h: PILL_H });
    cursor += w + GAP;
  });
  const spineW = cursor - GAP + PAD;

  // 非主链节点：按跳数挂到主链下方，网格排布（统一槽宽，保证对齐）。
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
    note: 'Spine 布局：把最长的一条链排成主轴，便于污点 / 风险取证逐跳核对。',
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

/** 聚类 Compound：大框套小节点。聚合视角用。 */
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
    note: '聚类布局：每个框是一个分组，框上只给计数与样例成员，不是单链路。',
  };
}

// ---------------------------------------------------------------- matrix

/** 矩阵：行列两维度，单元格为关系强度。 */
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
    note: '矩阵布局：行 × 列两维度，单元格颜色深浅表示数量，0 表示该组合确实没有产出。',
  };
}

// ---------------------------------------------------------------- er

/** ER 正交：表分列排布，关系用 90° 折线。 */
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
    note: 'ER 布局：表与表之间用 90° 正交连线，用于看同事务共现关系。',
  };
}

export function layoutOf(mode: LayoutMode): LayoutFn {
  return LAYOUTS[mode] ?? radialLayout;
}
