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
  /**
   * 是否在节点药丸里画种类图标。默认开启。当本次视图实际出现的 kind 数 ≤ 2 时由调用方
   * 置为 false —— 同质视图（如「谁调用了 X」几乎全是 Method）图标全同，纯属占横向空间 +
   * 视觉噪声，靠颜色即可区分；kind ≥ 3 才画图标（多类型时图标 + 颜色才能一眼区分）。
   */
  showIcons?: boolean;
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

const PILL_H = 26;

/** 近似字符宽：CJK / 全角按 1.8 个拉丁位计（同字号下 CJK ≈ 拉丁的 1.8 倍宽）。 */
function units(s: string): number {
  let u = 0;
  for (const ch of s) u += (ch.codePointAt(0) ?? 0) > 0x2e7f ? 1.8 : 1;
  return u;
}

/**
 * 与 `global.css` 一致的字体栈 —— 测量必须用**渲染实际使用的字体**，否则量了也白量。
 */
const FONT_STACK =
  `-apple-system, BlinkMacSystemFont, 'Segoe UI', 'PingFang SC', 'Hiragino Sans GB', ` +
  `'Microsoft YaHei', Roboto, 'Helvetica Neue', Arial, sans-serif`;

/**
 * 真实文本宽度（canvas `measureText`）。
 *
 * 旧实现按「每视觉位 × 固定系数」估算：系数对实际字体普遍偏宽（拉丁正文 ≈ 0.5em/字符，
 * 旧系数折合 0.56–0.57em），于是每个药丸右侧都拖着一段假空白，看起来"没按内容自适应"。
 * 这里直接用与渲染一致的字体 + 字号 + 字重量出真实像素宽，并按 (weight, px, text) 缓存
 * （同一名字在 resize / 重排时会反复量）。无 canvas 环境（jsdom 单测 / SSR）回退到旧的
 * 系数估算，测试保持确定性；返回 null 表示"没量到"。
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
 * 药丸节点宽度。必须与 GraphCanvas 的实际渲染对齐：
 * `图标 + 名称`，名称字号 13 / 字重 600（中心）或 11 / 500（其它），
 * 且渲染端按**视觉宽度**中间截断 40 位 —— 宽度计算同样先截断再测量，口径一致。
 * 文本宽度优先 `measureText` 实测；不可测量时按系数估算兜底。上限 300 防极端长名撑爆画布。
 */
const ICON_AREA = 20; // 图标 12 + 左内边距 6 + 间隔 3（文字起点 21）
const ICON_LEFT = 8; // 无图标时仅左内边距（文字起点 8）
const RIGHT_PAD = 18; // 右内边距（含 1–2px 渲染误差缓冲）
const MIN_NAME_UNITS = 4; // 至少保留约 4 个拉丁字符宽的文本区（保证可点 / 可读；CJK 一字 ≈ 1.8 位，自然更宽）
function pillWidth(kind: string, name: string, center = false, icon = true): number {
  const px = center ? 13 : 11;
  const weight = center ? 600 : 500;
  // 先按渲染端同一规则截断，再量 —— 截断前的全名量出来只会虚宽。
  const shown = truncateMiddle(name, 40);
  const textPx =
    measureTextPx(shown, px, weight) ?? Math.min(units(name), 40) * (center ? 7.4 : 6.2);
  // 下限完全由内容推导：左内边距 + 右侧留白 + 至少 MIN_NAME_UNITS 字符的文本宽。
  // 图标与否只改 `left`（有图标 21 / 无图标 8），分档常量因此被吸收、无需单独维护。
  const left = icon ? ICON_AREA + 1 : ICON_LEFT;
  const minNamePx = (center ? 7.4 : 6.2) * MIN_NAME_UNITS;
  return Math.min(300, Math.ceil(left + RIGHT_PAD + Math.max(minNamePx, textPx)));
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
 * 「近似星形」容忍的**叶子间边数上限**。
 *
 * 星形判定（[`isStar]`）要求每条边都碰中心，太脆：路由视角里两张表之间的
 * `ForeignKey`、表视角里的一条外键 —— **一条**叶子间边就把判定判死，于是
 * 几十片叶子的图落回每层一行的宽扇形（一行 6000px+、两端被裁，`LAYERED_FAN_MAX`
 * 注释里记过的实测坏案例）。
 *
 * 阈值取 3 的理由：少量叶子间边是**装饰**（30 个邻居里的一两条外键），不改变
 * "这是一颗星"的结构事实，hub-spoke 完全容纳得下（见 `hubSpokeLayout` 对叶子间边
 * 的绕行）；而真正的多层链路图（环 1 → 环 2 的传递结构）叶子间边是**主体**
 * （几十条），远超此阈值，仍走同心环 / 分层 —— 「把链路视角手动切成径向时不能
 * 悄悄变样」的行为由测试钉住，不受本放宽影响。
 */
const NEAR_STAR_LEAF_EDGES = 3;

/** 近似星形：绝大多数边都碰中心，仅 ≤ [`NEAR_STAR_LEAF_EDGES`] 条叶子间边。 */
function isNearStar(centerId: number, edges: LayoutEdge[]): boolean {
  const leafEdges = edges.filter((e) => e.from !== centerId && e.to !== centerId).length;
  return leafEdges <= NEAR_STAR_LEAF_EDGES;
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
  // 用「近似星形」而不是严格星形：一条表间 ForeignKey 不该把几十片叶子的图
  // 推回同心环（面积 ∝ n² 的坏案例）。少量叶子间边由 hub-spoke 绕行消化。
  if (leaves.length >= HUB_MIN && isNearStar(input.center.id, input.edges)) {
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

  // 画布按内容自适应放大，避免密集图被 viewBox 裁切（再由平移 / 缩放查看）。
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

  // 边统一为直线：不同环已按环序号错开相位（见上），端点很少再共线，
  // 故无需事后把边掰弯——直线更诚实、也更清晰。只保留两端都存在的边。
  // 端点从**节点中心**收到**药丸边界**：不收缩时线身会钻进药丸底下，
  // 节点不透明时被盖住看不出来，一旦聚焦变暗（半透明）线就透出来了。
  const placed: PlacedEdge[] = edges.flatMap((e) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    const da = dims.get(e.from);
    const db = dims.get(e.to);
    const [a2, b2] = da && db ? shrinkToRects(a, da.w, da.h, b, db.w, db.h) : [a, b];
    // 用 `...e` 透传 `seq`（及其余 LayoutEdge 字段）：渲染侧 `edgeKey` 依赖 `seq` 做边唯一键，
    // 一旦丢失，悬浮聚焦就找不到这条边、连不出它的两个端节点（只有边自己高亮、节点却被压暗）。
    return [{ ...e, points: [a2, b2], orthogonal: false }];
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
 * 把直线的两端从**节点中心**收缩到两端药丸的**边界**。
 *
 * 同心环布局把节点摆在圆周上、边画成中心连中心的直线；线身原本钻在药丸底下，
 * 药丸不透明时被盖住，一旦悬浮聚焦把节点压成半透明，线就透出来（真实 bug）。
 * 收缩后线从源药丸边缘出发、到目标药丸边缘为止，箭头（渲染侧 `clipArrowTip`
 * 对"终点在矩形外/上"直接取终点）自然钉在药丸边缘。
 *
 * 两端各自沿方向求出射参数：从中心到 x/y 边界的距离除以方向分量取小者。
 * 若两端收缩后越过了彼此（节点几乎重叠 / 线完全在矩形内部），退回原始端点。
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
  if (tA + tB >= 1) return [a, b]; // 收缩后两端相遇 / 交叉：节点重叠，保持原样
  return [
    [a[0] + dx * tA, a[1] + dy * tA],
    [b[0] - dx * tB, b[1] - dy * tB],
  ];
}

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

/**
 * 三次贝塞尔曲线采样为折线。
 *
 * 布局仍然只产出 `points`（渲染端 `M/L` 折线、标签锚点、箭头切线全部无需感知曲线）：
 * 16 段采样在 `non-scaling-stroke` 下与真曲线视觉不可分，而测试侧的
 * `segmentsThroughNodes` / `crossings`（按线段判定）也照常适用于采样点。
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
 * 中心辐射（hub-and-spoke）：中心在左，邻居**单列**排在右侧，边为**单段直线**
 * （从中心药丸缘直接连到目标药丸缘，无 90°/0° 折角）：
 *
 * ```
 * 中心 ●━━━━━━━━━━▶ 目标（终点落在目标近侧边缘，线进入目标的 x 区间时 y 恰为目标行）
 * ```
 *
 * 为什么单段直线能零切节点：终点取目标药丸的*近侧边缘*而非中心——线段 x 单调增到该缘，
 * 进入目标 x 区间时 y 已精确等于目标行，只经过目标自己那颗药丸；中心与列之间的通道区是
 * 空白，线也不碰别的药丸。各边出发点在中心药丸缘按列序单调铺开 ⇒ 彼此不相交。这是结构保证
 * （与 `gapX` 无关），不是优化结果。若终点误取药丸中心，远端浅线会在邻居行处扫入邻居药丸左半，
 * 所以必须连到近侧边缘。间隔的真正来源是 `ROW_GAP`（目标行间距）与 `gapX`（通道宽、决定夹角）。
 *
 * 为什么不排成圆弧：圆弧同样可证零交叉，但 32 个节点排半圈需要半径约 1700px、画布约
 * 1800×3400；单列只要宽约 600、高 n×60，代价只是纵向滚动 —— 而滚动是机械成本，
 * 交叉是歧义成本。
 */
function hubSpokeLayout(input: LayoutInput, fanout: LayoutNode[], viaStar = false): LayoutResult {
  const { center, edges, width, height } = input;
  const PAD = 40;
  const ROW_GAP = 30; // 行距：药丸高 30 ⇒ 行距/药丸高 = 2.0。大图靠加大行距换「边间隔大」，代价是画布更高（可接受）
  const HUB_GAP_X = 140; // 通道（中心到列）的最小横向间距；实际取值随列高自适应（见 gapX）

  // ---- 分侧：把邻居按边方向分成「调用方（左列）」与「被依赖方（右列）」----
  //
  // 旧实现把所有邻居排在中心右侧一列 —— 资源视角（"谁在用这张表"）没问题，
  // 但入口类中心（HTTP 契约）的图读反了：前端调用方和后端依赖混在一列，
  // `前端 --CallsHttp--> 路由 --ReadsConfig--> 配置键` 这条请求流向在画布上没有方向感。
  // 左列放"流向中心的来源"（入边邻居），右列放"中心流向的目标"（出边邻居），
  // 所有箭头自然从左指向右，左→右即请求 / 数据流向。
  // 同时挂两种边的节点按"被依赖方"归类，避免同一种 pill 出现两次。
  const callerIds = new Set<number>();
  const targetIds = new Set<number>();
  edges.forEach((e) => {
    if (e.to === center.id && e.from !== center.id) callerIds.add(e.from);
    if (e.from === center.id && e.to !== center.id) targetIds.add(e.to);
  });
  targetIds.forEach((id) => callerIds.delete(id));
  // 排序键：先环（= 跳数 / 追溯深度）后种类再名字，与旧实现一致（确定、可复现）。
  const byRingKindName = (a: LayoutNode, b: LayoutNode) =>
    a.ring - b.ring ||
    (a.kind === b.kind ? 0 : a.kind.localeCompare(b.kind)) ||
    a.name.localeCompare(b.name);
  const callers = fanout.filter((n) => callerIds.has(n.id)).sort(byRingKindName);
  // 无任何边接触的邻居（正常星形里不存在）兜底放右列。
  const rightNodes = fanout
    .filter((n) => targetIds.has(n.id) || (!callerIds.has(n.id) && !targetIds.has(n.id)))
    .sort(byRingKindName);

  const centerW = pillWidth(center.kind, center.name, true, input.showIcons ?? true);
  // 只有**两侧都非空**才分左右两列 —— 那才存在真实的「来源 → 中心 → 去向」穿堂流。
  // 纯入边星形（资源视角：全部是"谁在用它"）或纯出边星形没有"流"，强行分列
  // 只会让画布凭空多出一条列宽（实测资源视角 80 使用者时宽度超出容器 4%），
  // 维持旧的单列形态即可。
  const twoSided = callers.length > 0 && rightNodes.length > 0;
  // 单侧形态：全部邻居（含"调用方"）都进右列 —— 与旧实现一致。
  const rightCol = twoSided ? rightNodes : fanout.slice().sort(byRingKindName);
  const leftCol = twoSided ? callers : [];
  const leftW = leftCol.length ? Math.max(...leftCol.map((n) => pillWidth(n.kind, n.name, false, input.showIcons ?? true))) : 0;

  // 通道（中心到列的横向间距）= 边的主要水平长度。**不与列高同步无限增长**：
  // 见下方「等长扇形」分支——扇出适中（约 12~60 个邻居）时直接改用限角圆弧，
  // 让每个邻居到中心的边等长，从而把单列形态下"顶部 / 底部节点拖出超长边"的问题消除；
  // 超大扇出（> ~60）时圆弧会撑成又大又圆的画布、面积反超单列，仍退回单列。
  const MAX_GUTTER = 420;
  const rows = Math.max(leftCol.length, rightCol.length);
  const colHalfSpan = (rows * (PILL_H + ROW_GAP)) / 2;
  const gapX = Math.max(HUB_GAP_X, Math.min(Math.round(colHalfSpan * 0.6), MAX_GUTTER));

  // 等长扇形分支：把右列邻居摆到「以中心为圆心、半径 arcR 的圆弧」上（限角 SPAN_MAX，
  // 避免弧两端药丸因法向间距不足而重叠）。相邻节点弦距恒为 ROW_GAP+PILL_H，故 arcR 由
  // 弦长与限角唯一确定；所有边从中心辐射、长度都 ≈ arcR。当 arcR 比单列最长边（中心右缘 →
  // 顶/底节点近侧缘）更短、且邻居数达到 ARC_MIN 时启用；否则维持旧的单列。
  const SPAN_MAX = (110 * Math.PI) / 180; // 限角 110°：弧端法向间距 ≥ 药丸高，保证不重叠
  const ARC_MIN = 12;
  const rightN = rightCol.length;
  const arcDelta = rightN > 1 ? SPAN_MAX / (rightN - 1) : 0;
  const arcR = rightN > 1 ? (ROW_GAP + PILL_H) / (2 * Math.sin(arcDelta / 2)) : 0;
  const colMaxEdge = Math.hypot(gapX, (rightN * (PILL_H + ROW_GAP)) / 2);
  const useArc = rightN >= ARC_MIN && arcR <= colMaxEdge;

  // 双列时中心居中，左→右对称；单侧时退化为旧形态（中心在最左）。
  const hubCx = twoSided ? PAD + leftW + gapX + centerW / 2 : PAD + centerW / 2;
  const leftColLeft = PAD; // 左列药丸左缘
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

  /** 一列药丸的纵向起点：以画布中线为轴上下居中，返回第 i 个的 y。 */
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
    // 右列邻居：以 (hubCx, cy) 为圆心、半径 arcR 等角距排布；中间（列表中央）的节点落在
    // 最右侧（θ=0），两端向上 / 下张开。排序键仍保持「跳数 → 种类 → 名字」，相邻沿弧相邻。
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

  // 直线放射（单段直线，从中心药丸缘直接连到目标药丸缘）：用户要求"直接连直线"、不要
  // 90°/0° 的正交折线。关键纠正：单段直线**可以**零切节点——只要终点落在目标的*近侧边缘*
  // 而非中心。左列（调用方）终点取左列药丸右缘；右列若走等长扇形，则用 `shrinkToRects`
  // 取目标药丸的近侧边界——任意角度都正确。
  // （注：终点若取药丸*中心*，远端浅线会在邻居行处扫入邻居药丸左半，故必须连到近侧边缘。）
  const ATTACH_MAX = PILL_H / 2 - 4;
  const nodeById = new Map(nodes.map((n) => [n.id, n]));

  /**
   * **源端（hub 侧）锚点分散**。
   *
   * 旧实现把所有 hub 边的出发点都压在中心药丸竖边的 ±`ATTACH_MAX`（共 18px）内 ——
   * 30 条出边挤在 18px 里，近中心处糊成一束，要等线散开才看得出"哪条通向谁"。
   * 现在改为沿中心药丸**面向邻居那一侧的整条边界**（上边 → 侧边 → 下边）按目标次序
   * 均匀铺开：每条边有各自的出入口，扇骨一出药丸就张开。
   *
   * 出发点不是想挪哪儿就挪哪儿 —— 必须落在这条边**看得见**的那段边界上，否则线段会先钻进
   * 药丸底下再从另一侧穿出（药丸不透明时看不出来，悬浮聚焦压成半透明就露馅）。判据很直白：
   * 目标在药丸上缘之上 ⇒ 取**上边**；在下缘之下 ⇒ 取**下边**；其余 ⇒ 取**侧边**。
   * 三种情形下线段一离开锚点就出到药丸外；且锚点沿边界自上而下、目标也自上而下，
   * 两个序列同序 ⇒ 扇骨互不相交（与旧实现同为结构保证，不是调参结果）。
   *
   * 目标端点不变 ⇒ 位移只发生在中心这一端、到目标处衰减为 0，
   * 所以铺开不会把线推进邻居药丸（"边不穿过节点"这条硬约束不受影响）。
   */
  const FAN_SLOT = 28; // 同一段边界上相邻锚点的目标间距
  const FAN_REACH = Math.min(centerW, 160); // 上 / 下边各最多用掉的长度（不越过药丸另一头）

  /** 每条 hub 边的目标端点与平行边错开量（锚点分配按错开后的 y 排序）。 */
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
    // 同一对端点的多条路径：单段直线没有「中段」可错开，改为**整条线沿 y 平移**。
    // 平移量把药丸可用高度均分给组内各条（总铺开 2×ATTACH_MAX=22px，两条平行边相隔 22px，
    // fit 缩放后仍有 ~11px），远宽于旧曲线方案的 fanOffset×0.6（仅 6.6px，缩放后糊成一束）。
    const p = parallel.get(ei);
    const off =
      p && p.count > 1 ? -ATTACH_MAX + (p.idx * (ATTACH_MAX * 2)) / (p.count - 1) : 0;
    const w = otherNode.w ?? 120;
    let tgt: Pt;
    if (onLeft) {
      tgt = [otherNode.x + w / 2, other[1]]; // 左列（调用方）：取药丸右缘
    } else if (useArc) {
      tgt = shrinkToRects(hubCenter, centerW, PILL_H, other, w, PILL_H)[1];
    } else {
      tgt = [otherNode.x - w / 2, other[1]]; // 右列单列：取药丸左缘
    }
    hubSides.set(ei, { tgt, off, onLeft, hubIsFrom });
  });

  /** hub 侧锚点：边下标 → 出发点。 */
  const hubAnchor = new Map<number, Pt>();
  for (const side of [1, -1] as const) {
    const items = [...hubSides.entries()]
      .filter(([, s]) => (s.onLeft ? -1 : 1) === side)
      .map(([ei, s]) => ({ ei, ty: s.tgt[1] + s.off }))
      .sort((x, y) => x.ty - y.ty || x.ei - y.ei);
    if (items.length === 0) continue;
    const hh = PILL_H / 2;
    const xSide = hubCx + (side * centerW) / 2; // 面向该侧的竖边
    const above = items.filter((it) => it.ty < cy - hh);
    const mid = items.filter((it) => it.ty >= cy - hh && it.ty <= cy + hh);
    const below = items.filter((it) => it.ty > cy + hh);
    /** 组内按 (i+0.5)/n 取点；跨度随条数增长、不超过 `FAN_REACH`（条数少时不无谓外扩）。 */
    const place = (group: typeof items, at: (span: number, f: number) => Pt) => {
      const span = Math.min(FAN_REACH, Math.max(0, group.length - 1) * FAN_SLOT);
      group.forEach((it, i) => hubAnchor.set(it.ei, at(span, (i + 0.5) / group.length)));
    };
    // 上边：由远离拐角的一端走向拐角（最上面的目标取最外侧 —— 越陡 ⇒ 出射点越靠内，同序）
    place(above, (span, f) => [xSide - side * span * (1 - f), cy - hh]);
    // 侧边：自上而下均分
    place(mid, (_span, f) => [xSide, cy - hh + 2 * hh * f]);
    // 下边：由拐角走向远离拐角的一端（最下面的目标取最外侧）
    place(below, (span, f) => [xSide - side * span * f, cy + hh]);
  }

  const placed: PlacedEdge[] = edges.flatMap((e, ei) => {
    const a = pos.get(e.from);
    const b = pos.get(e.to);
    if (!a || !b) return [];
    const spec = hubSides.get(ei);
    // 两端都不是中心（表间 ForeignKey 之类的叶子间边）：先试直连；同列两颗药丸
    // 之间的直线会扫过中间的邻居药丸，撞上就沿侧通道绕行 —— 「边不穿过节点」
    // 这条硬约束不因近似星形的放宽而破例。
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
    // 辐射边的直连也做同一道探测。结构保证（左缘进入目标 x 区间时 y 已对准目标行）
    // 只对**单列**形态成立；等长扇形里弧端节点的连线会扫过它内侧相邻的药丸
    // （弧在 ±SPAN_MAX/2 处回卷，扇骨斜穿内侧邻居）—— 撞上就绕行，不假装没看见。
    const line: Pt[] = [anchor, end];
    const forward = spec.hubIsFrom;
    const obstacles = obstaclesForPath(line, nodes, new Set([e.from, e.to]), 100);
    const pts = pathHits(line, obstacles)
      ? detourAroundNodes(line[0], line[1], obstacles)
      : line;
    // 方向：中心出发 → 目标；目标出发 → 中心（箭头由末端点方向决定）。
    return [{ ...e, points: forward ? pts : pts.slice().reverse(), orthogonal: false }];
  });

  const flowNote = twoSided
    ? `左列 ${leftCol.length} 个调用方 / 来源 → 中心 → 右列 ${rightCol.length} 个被依赖方，**箭头方向即请求 / 数据流向（自左向右）**；`
    : `中心在左，${rightCol.length} 个邻居排在右侧；`;
  const arcNote = useArc
    ? `邻居排成**限角等长扇形**（以中心为圆心的圆弧，相邻弦距固定、每条边长度≈${Math.round(arcR)}px），消除了单列形态下顶部 / 底部节点拖出的超长边；`
    : '';
  const fanNote =
    '每条边的出发点沿中心药丸**面向邻居那一侧**的「上边 → 侧边 → 下边」按目标次序铺开（一条边一个出入口，近中心处不再糊成一束），';
  // 交叉数如实报出（与 stackedLayout 同一口径）：单列形态有结构性的 0 交叉保证，
  // 但等长扇形的弧端可能要靠绕行兜底，绕行失败时不谎报"0 处"。
  const polys = placed.map((e) => ({ from: e.from, to: e.to, pts: e.points }));
  const crossings = polys.length <= 600 ? countCrossings(polys) : null;
  const hardNote =
    crossings === 0
      ? '边为从中心药丸缘直接连到目标药丸缘的单段直线（终点落在目标近侧边缘，故不穿过任何节点），间隔由行距与通道宽度保证，因此本图**边交叉 0 处、边不穿过任何节点**'
      : crossings === null
        ? '边较多，交叉数未逐一统计'
        : `当前仍有 **${crossings} 处边交叉**（平面上无法完全消除），逐条确认时请配合悬浮高亮`;
  const tail = '内容较高时纵向滚动查看。';
  return {
    nodes,
    edges: placed,
    width: contentW,
    height: contentH,
    content: boundsOf(nodes),
    // 出发点分散也要说给用户：这是「边看起来从哪出来」的直接解释，否则会被当成随机偏移。
    note: viaStar
      ? `径向入口判定本图为**星形**（边几乎都只在「使用者 ↔ ${center.name}」之间，即资源视角沿入边回溯的形态；至多 ${NEAR_STAR_LEAF_EDGES} 条叶子间边，如表间 ForeignKey，会绕行）：同心环的画布随人数平方增长，且外环的边会从中心贯穿、压过内环药丸的名字，因此改走中心辐射。${flowNote}${arcNote}邻居均按「跳数 → 种类 → 名字」排序；${fanNote}${hardNote}。${tail}`
      : `中心辐射布局：${flowNote}${arcNote}邻居均按「跳数 → 种类 → 名字」排序；${fanNote}${hardNote}。${tail}`,
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
  // 中心节点必须按「中心」规格量宽（13px / 700）：渲染端中心画得比普通节点大一档，
  // 按 11px / 500 量出来的药丸装不下粗体大字，文字会溢出框（路由视角中心曾溢出）。
  const widths = ordered.map((layer) =>
    layer.map((n) => pillWidth(n.kind, n.name, n.id === center.id, input.showIcons ?? true)),
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
 * 这种资源视角的星形（中心 + 单环）：小扇出（≤ `LAYERED_FAN_MAX`）画成两行扇形，
 * 中心在上、叶子一行在下，自中心向下发散；宽扇出改走中心辐射（见 `LAYERED_FAN_MAX`）。
 * 因为全部边共用一个端点（中心），两种形态同样是 0 交叉、0 穿节点。
 */
/**
 * 分层布局对**单环宽扇出**的容忍上限。
 *
 * 分层把「中心 + 单环」画成两行扇形（中心在上、叶子一行在下），扇形宽度 ∝ 叶子数 ×
 * 药丸宽 —— 7~10 个时是"调用流向自上而下"的好看形态；但"契约读 27 个配置键"时
 * 一行 6000px+，fit 后两端被裁、中间大片空白（实测截图）。超限后改走中心辐射
 * （hub 在左、单列在右）：宽度固定 ~600px，代价只是纵向滚动。
 * 小扇出保留扇形 —— 那仍是"自上而下"最直观的表达。
 */
const LAYERED_FAN_MAX = 10;

export function layeredLayout(input: LayoutInput): LayoutResult {
  const leaves = input.rings.flat();
  // 同 radialLayout：用「近似星形」——路由视角里一条表间 ForeignKey（叶子间边）
  // 曾经把 19 片叶子的图判成"非星形"，落回单行 6000px+ 的宽扇形（实测截图坏案例）。
  if (leaves.length > LAYERED_FAN_MAX && isNearStar(input.center.id, input.edges)) {
    return hubSpokeLayout(input, leaves, false);
  }
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
    const w = pillWidth(found.kind, found.name, found.id === center.id, input.showIcons ?? true);
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
