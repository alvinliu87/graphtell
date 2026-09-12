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
  /** 节点形状：`circle` 用于对象，`rect` 用于 ER / 矩阵表头。 */
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

// ---------------------------------------------------------------- radial

/** 径向 / 同心环：环 = 跳数。对象入口子图默认布局。 */
export function radialLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const cx = width / 2;
  const cy = height / 2;
  const maxRing = Math.max(1, rings.length);
  const base = Math.min(width, height) / 2 - 70;
  const step = base / maxRing;

  const nodes: PlacedNode[] = [
    { ...center, x: cx, y: cy, shape: 'circle' },
  ];

  rings.forEach((ring, i) => {
    const radius = step * (i + 1);
    const count = ring.length;
    if (count === 0) return;
    // 环上均匀排布；节点多时改为双列以保留标签空间
    ring.forEach((n, idx) => {
      const angle = (2 * Math.PI * idx) / count - Math.PI / 2;
      nodes.push({
        ...n,
        x: cx + radius * Math.cos(angle),
        y: cy + radius * Math.sin(angle),
        shape: 'circle',
      });
    });
  });

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
    width,
    height,
    note: '径向布局：中心为当前对象，同心环表示跳数（环 1 = 直接关联）。',
  };
}

// ---------------------------------------------------------------- layered

/** 分层（Sugiyama 简化）：自上而下，层间 90° 正交折线。 */
export function layeredLayout(input: LayoutInput): LayoutResult {
  const { center, rings, edges, width, height } = input;
  const layers: LayoutNode[][] = [[center], ...rings];
  const top = 60;
  const gapY = Math.max(80, (height - 120) / Math.max(1, layers.length - 1));

  const nodes: PlacedNode[] = [];
  layers.forEach((layer, li) => {
    const y = top + li * gapY;
    const count = Math.max(1, layer.length);
    const span = Math.min(width - 120, count * 150);
    layer.forEach((n, idx) => {
      const x = width / 2 - span / 2 + (span / count) * (idx + 0.5);
      nodes.push({ ...n, x, y, shape: 'circle' });
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
    width,
    height: Math.max(height, top + gapY * (layers.length - 1) + 60),
    note: '分层布局：自上而下分层，层间用 90° 正交折线，适合看调用链下钻。',
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
  const gapX = Math.min(220, (width - 140) / Math.max(1, best.length));
  const nodes: PlacedNode[] = [];
  const onSpine = new Set(best);
  best.forEach((id, i) => {
    const found = findNode(input, id);
    if (!found) return;
    nodes.push({
      ...found,
      x: 70 + gapX * (i + 0.5),
      y: spineY,
      shape: 'circle',
    });
  });

  // 非主链节点：按跳数挂到主链下方
  const rest: LayoutNode[] = [];
  rings.forEach((ring, ri) =>
    ring.forEach((n) => {
      if (!onSpine.has(n.id)) rest.push({ ...n, ring: ri + 1 });
    }),
  );
  rest.forEach((n, i) => {
    nodes.push({
      ...n,
      x: 90 + (i % 8) * ((width - 160) / 8),
      y: spineY + 110 + Math.floor(i / 8) * 62,
      shape: 'circle',
    });
  });

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
    width,
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
