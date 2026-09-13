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

/** 径向 / 同心环：环 = 跳数。对象入口子图默认布局。 */
export function radialLayout(input: LayoutInput): LayoutResult {
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
    label: `${i + 1} 跳`,
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

/** 分层（Sugiyama 简化）：自上而下，层间 90° 正交折线。 */
export function layeredLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const layers: LayoutNode[][] = [[center], ...rings];
  const top = 60;
  const gapY = Math.max(80, (height - 120) / Math.max(1, layers.length - 1));

  // 全部改用 rect 药丸节点（文字内嵌），与径向 / 矩阵 / ER 保持一致；
  // 同一层内按各药丸实际宽度依次排开，层宽取所有层的最大值，超出容器则整体加宽（由画布平移 / 缩放查看）。
  const GAP = 18;
  const PAD = 60;
  const layerWidths = layers.map(
    (layer) =>
      layer.reduce((s, n) => s + pillWidth(n.kind, n.name), 0) +
      GAP * Math.max(0, layer.length - 1),
  );
  const contentW = Math.max(width, ...layerWidths, 0) + PAD * 2;
  const centerX = contentW / 2;

  const nodes: PlacedNode[] = [];
  layers.forEach((layer, li) => {
    const y = top + li * gapY;
    const widths = layer.map((n) => pillWidth(n.kind, n.name));
    let cx = centerX - layerWidths[li] / 2;
    layer.forEach((n, idx) => {
      const w = widths[idx];
      nodes.push({ ...n, x: cx + w / 2, y, shape: 'rect', w, h: PILL_H });
      cx += w + GAP;
    });
  });

  const pos = new Map(nodes.map((n) => [n.id, [n.x, n.y] as [number, number]]));
  const placed: PlacedEdge[] = edges.flatMap((e) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    // 正交：下 → 横 → 下
    const midY = (a[1] + b[1]) / 2;
    return [
      {
        ...e,
        points: [a, [a[0], midY], [b[0], midY], b],
        orthogonal: true,
      },
    ];
  });

  return {
    nodes,
    edges: placed,
    width: contentW,
    height: Math.max(height, top + gapY * (layers.length - 1) + 60),
    note: '分层布局：自上而下分层（层 = 跳数），层间用 90° 正交折线，适合看调用链下钻。',
  };
}

// ---------------------------------------------------------------- spine

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
