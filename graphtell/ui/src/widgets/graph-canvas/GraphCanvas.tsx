import { Fragment, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useLocale } from '@/shared/lib/i18n';
import { Empty, Space, Spin, Tag, Tooltip, Typography } from 'antd';
import { InfoCircleOutlined } from '@ant-design/icons';
import type { EdgeView, LayoutMode, NodeView, SourceLocation } from '@/entities/view';
import type { SubProject } from '@/entities/project/model';
import { edgeColor, nodeColor } from '@/entities/graph';
import { truncate, truncateMiddle } from '@/shared/lib/format';
import { layoutOf, type LayoutInput, type LayoutResult } from './layout/types';
import { nodeIcon, usedKinds } from './nodeIcons';

/**
 * 超过这个边数就只在"悬浮 / 选中"时标注边类型。
 *
 * 边类型是语义图的"谓语"（`ReadsConfig` / `MapsTo` / `ReadsCache`…），标出来才读得懂；
 * 但共享资源视图可能有上百条边，全部标注会糊成一片 —— 所以边多时退化成"按需标注"，
 * 悬浮详情卡始终给出完整信息。
 *
 * 40 是按"横向分层图"定的：标签沿各层散开，不太会叠。星形视图（一个中心拖 27 个
 * 配置键）所有边的中点都挤在中心附近那一小圈里，二十几个标签必然叠成一摞 ——
 * 所以这个阈值必须按"标签会不会物理重叠"取值，14 条以上星形就已经叠了。
 */
const EDGE_LABEL_LIMIT = 14;
/**
 * 「适应屏幕」的缩放区间。
 *
 * 下限 0.5：星形大图（一个中心拖 27 个药丸，包围盒 2000px+）在 0.85 下根本放不进
 * 视口 —— fit 把中心对到视口中央、两边照样被裁，"适应屏幕"名存实亡，只能盲滚。
 * 取舍从「宁可滚，也不缩字」改为「先见全貌，再看细节」：0.5 时 13px 字约 6.5px，
 * 认结构足够、认内容吃力，但边标签在缩小后已自动隐藏（见 `EDGE_LABEL_LIMIT`），
 * 结构轮廓 + 滚轮放大看细节才是这个尺度下的正确用法。
 * 上限是 1：小图不放大成巨号字。
 */
const FIT_MIN_K = 0.5;
const FIT_MAX_K = 1;
/** fit 时内容四周留白（世界坐标 px）。 */
const FIT_PAD = 16;
// 悬浮聚焦时非聚焦元素的淡出深度：随边数连续变化（边越少压得越浅，边越多压得越深），避免稀疏图像"全图消失"。
const DIM_OPACITY_MIN = 0.12; // 稠密图最深
const DIM_OPACITY_MAX = 0.4; // 稀疏图最浅
const DIM_EDGE_LOW = 4; // 边数低于此取最浅
/** 子工程配色：每种子工程一个稳定色相，多个前端 / 多个后端各自不同色（不再压成蓝 / 橙两桶）。 */
const SUB_PROJECT_PALETTE = [
  '#0ea5e9', '#f97316', '#22c55e', '#a855f7', '#eab308',
  '#ec4899', '#14b8a6', '#6366f1', '#ef4444', '#84cc16',
  '#06b6d4', '#f43f5e',
];
const ROLE_LABEL: Record<string, string> = { frontend: '前端', backend: '后端' };
const KIND_LABEL: Record<string, string> = {
  admin: '管理后台',
  'mini-program': '小程序',
  mobile: '移动端',
  h5: 'H5',
  api: 'API',
  worker: '任务/队列',
  bff: 'BFF',
  web: 'Web',
};
function roleLabel(r?: string | null): string {
  if (!r) return '未知';
  const [tier, kind] = r.split(':');
  if (kind) return KIND_LABEL[kind] ?? kind;
  return ROLE_LABEL[tier] ?? tier;
}
const DIM_EDGE_HIGH = 40; // 边数高于此取最深

/**
 * 边的稳定唯一键。
 *
 * 折叠视图里存在"合成边"（提拉 / 反向汇总得到，没有真实行），只靠 `id` 会撞键；
 * 同一对端点也可能有多条不同种类的边，所以带上端点一起构成键。
 *
 * 还不够：正向视角下同一条传播边（seed）会被展开成**多条路径**，而 `view_service.rs`
 * 的 `push_edge` 给它们复用同一个 evidence 边 id —— 于是会出现 `(id, from, to)` 完全相同的
 * 两条边，只按 `id:from->to` 仍然撞键（表现为"悬浮一条高亮全部、悬浮卡永远显示第一条链"）。
 * 因此再带上 `seq`（该边在 `edges` 输入数组中的下标）。`EdgeView` 侧由 `viewEdgeKeys`
 * 用下标补齐同样的 `seq`，两处键才对齐。
 */
const edgeKey = (e: { id: number; from: number; to: number; seq?: number }) =>
  `${e.seq ?? '?'}:${e.id}:${e.from}->${e.to}`;

/**
 * 边标签锚点。
 *
 * 取折线**真实几何中点**（按累计长度），标签**嵌在边正中**：文字垂直居中
 * （渲染侧 `dominantBaseline="central"`）压在线上，白色描边在文字背后挖缺口，
 * 线从文字两侧露出 —— 所有边（水平 / 斜 / 竖）观感一致。
 *
 * 不能用 `points[Math.floor(len / 2)]`：直线边只有两个点，索引 1 就是**终点**，
 * 标签会被后绘制的目标节点药丸（不透明白底）整块盖住 —— 表现就是
 * "只有悬浮时才能在卡片里看到边名"。
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
 * 末段线段与节点矩形（中心 `cx,cy`、尺寸 `w×h`）的裁剪求交（Liang–Barsky），
 * 返回箭头尖应落的边界点：
 *
 * - 终点在矩形**外/上**（辐射布局：终点本来就是药丸近侧边缘）⇒ 取线段离开矩形的交点，
 *   即终点自身 —— 箭头钉在**边的尽头**；
 * - 终点在矩形**内**（旧布局：终点是节点中心）⇒ 取线段进入矩形的交点，与旧
 *   `clipToRect` 行为一致。
 *
 * 不能用"指向中心的射线求交"替代：对宽扁药丸 + 斜入射的线，那条射线会先撞到
 * 矩形**底边**，箭头就悬到药丸正下方的空白里（真实出现过）。也不能用"沿方向退回
 * 半个药丸宽度"近似：入射角陡时会把箭头甩到药丸外面。
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
  // 终点在矩形内 ⇒ 取进入交点；在外/上 ⇒ 取离开交点（= 尽头处）
  const toInside =
    Math.abs(to[0] - cx) <= hw + 1e-6 && Math.abs(to[1] - cy) <= hh + 1e-6;
  let t0 = 0;
  let t1 = 1;
  const clip = (p: number, q: number): boolean => {
    if (Math.abs(p) < 1e-9) return q >= 0; // 平行且在界外 ⇒ 无交
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
  if (!ok) return to; // 线段与矩形不相交（不该发生）：保底用终点
  const t = toInside ? t0 : t1;
  return [from[0] + dx * t, from[1] + dy * t];
}

export interface CanvasNode {
  id: number;
  kind: string;
  /** 语义节点的类别（目前与 kind 一致）；语法节点为 null。 */
  category?: string | null;
  /** 该节点对应的视角 id（点击即切）；无则为 null。 */
  own_view?: string | null;
  /** 节点所属「端」：`frontend` / `backend`（由 FKB 标注的 `side`）。用于图上区分前后端子工程。 */
  side?: string | null;
  /** 节点所属子工程 id（后端 `NodeView.sub_project_id`）。图着色 / 过滤以子工程为单位，而非二元前后端。 */
  sub_project_id?: number | null;
  name: string;
  ring: number;
  /**
   * 以下为悬浮卡片的补充信息，**全部可选**：调用方可能只给出最小画布节点，
   * 卡片必须能容忍它们缺失（曾经这里按 `NodeView` 强转后直接 `.length`，一悬浮就崩）。
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
  /** 上一个中心（切视角后保留为邻居并标记 `from`）。 */
  originId?: number | null;
  selectedId?: number | null;
  hoverEnabled?: boolean;
  width?: number;
  height?: number;
  /** 单击节点：`ownView` 为对应视角 id 时，一级视角切到它、二级对象设为该节点。 */
  onNodeClick?: (id: number, kind: string, ownView: string | null) => void;
  /** 右键 / 详情图标：打开 Inspector 或跳转，不切视角。 */
  onNodeContextMenu?: (id: number, kind: string, event: React.MouseEvent) => void;
  onEdgeClick?: (edge: EdgeView) => void;
  locationsOf?: (id: number) => SourceLocation[];
  /** 是否在边上标注边的类型（如 `ReadsConfig`）。默认开启，边过多或缩小时自动隐藏。 */
  showEdgeLabels?: boolean;
  /**
   * 图「语义内容」的标识。当其变化时（切换视角 / 选中对象 / 切聚合视图 / 展开语法），
   * 重置平移缩放到「整图 fit」初始态。悬浮聚焦、手动缩放平移、单节点就地展开**不**改变它，
   * 以免打断在当前图内的探索。
   */
  fitKey?: string | number;
  /** 手动触发 fit 的信号：每次自增即把视图重置回整图 fit（工具栏「适应屏幕」按钮）。 */
  fitSignal?: number;
  /**
   * 子工程过滤：按 `sub_project_id` 多选显示节点与边，中心节点始终保留作为锚点。
   * 空数组（默认）表示不过滤、全部显示。多个前端 / 多个后端各自成一类，
   * 不再被压成「前端 / 后端」两个桶。
   */
  subFilter?: number[];
  /** 当前工程的子工程列表（含 id / name / role），用于着色与图例。 */
  subProjects?: SubProject[];
}

/**
 * 图画布。
 *
 * 交互分工（严格分离，避免"想跳代码却把视角切走了"）：
 * * **悬停** —— 只高亮，不改变任何状态
 * * **左键单击节点** —— 仅在节点有对应视角时切视角（导航）
 * * **右键 / 详情图标** —— 打开位置与跳转，不切视角
 * * **单击边** —— 打开边的证据链
 *
 * 位置全部由 `layoutOf(mode)` 决定，**不存在力导向自由漂移**。
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
  } = props;

  const { t } = useLocale();
  const [hover, setHover] = useState<number | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);
  const [pointer, setPointer] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  /**
   * 手动平移缩放。`null` 表示"未手动干预"，此时用 `fitTransform`（自动适应屏幕）。
   *
   * 用 null 而不是存一份 fit 快照，是为了让首帧宽度未知（默认 1040）到
   * ResizeObserver 量出真实宽度这段时间内，视图始终跟着布局自动重算，
   * 而不是钉死在按 1040 算出来的那一次 fit 上。
   */
  const [transform, setTransform] = useState<{ x: number; y: number; k: number } | null>(null);
  const drag = useRef<{ x: number; y: number } | null>(null);

  // 语义图内容切换（fitKey 变化）时，把平移缩放重置回「整图 fit」初始态。
  // 悬浮聚焦 / 手动缩放平移 / 单节点就地展开不改变 fitKey，故不触发重置。
  useEffect(() => {
    setTransform(null);
  }, [fitKey]);

  // 工具栏「适应屏幕」按钮：fitSignal 自增即重置为整图 fit。
  useEffect(() => {
    if (fitSignal === undefined) return;
    setTransform(null);
  }, [fitSignal]);

  // 用真实容器宽度喂给布局，避免"按 1040 设计、再被窄列整体缩小"导致的拥挤。
  // 首帧用默认 width，挂载后 ResizeObserver 量出真实宽度并触发一次重排（无感）。
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [measuredWidth, setMeasuredWidth] = useState<number | null>(null);
  const renderW = measuredWidth ?? width;
  // 角落图例（只列本次数据实际出现的 kind；可折叠）。默认展开，便于首次看懂图标含义。
  const [legendOpen, setLegendOpen] = useState(true);

  // 本次视图实际出现的不同 kind 数：≤ 2 时（同质视图，如「谁调用了 X」几乎全是 Method）
  // 图标在节点上全是同一种，既占横向空间又毫无区分度，退化成「只靠颜色」；≥ 3 才画图标。
  const showNodeIcons = useMemo(() => {
    const ks = new Set<string>();
    if (center) ks.add(center.kind);
    rings?.flat().forEach((n) => ks.add(n.kind));
    clusters?.forEach((c) => c.members.forEach((m) => ks.add(m.kind)));
    return ks.size >= 3;
  }, [center, rings, clusters]);

  // 子工程过滤：以 `sub_project_id` 为单位筛选节点与边，中心节点始终保留作为锚点；
  // `sub_project_id == null` 的共享 / 未知节点在任一具体过滤下仍保留（属于所有子工程）。
  // 过滤后的集合同时喂给布局、上色与前端调用方判断。
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

  // 子工程配色：按 id 排序后稳定映射到调色板，同一子工程颜色恒定、多个前端各自不同色。
  const subProjectColors = useMemo(() => {
    const m = new Map<number, string>();
    const ids = (subProjects ?? [])
      .map((s) => s.id)
      .filter((v): v is number => typeof v === 'number')
      .sort((a, b) => a - b);
    ids.forEach((id, i) => m.set(id, SUB_PROJECT_PALETTE[i % SUB_PROJECT_PALETTE.length]));
    return m;
  }, [subProjects]);

  // 节点 → 子工程 id 映射（仅用于上色，用未过滤的全集，过滤不改变颜色语义）。
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
      center: fCenter ?? { id: -1, kind: 'Unknown', name: '', ring: 0 },
      rings: fCenter ? fRings : [],
      // 带上 `seq`（下标）：同一 (id, from, to) 的多条路径靠它区分，见 `edgeKey` 的说明。
      edges: fEdges.map((e, i) => ({ id: e.id, from: e.from, to: e.to, seq: i })),
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
  }, [mode, fCenter, fRings, fEdges, clusters, matrix, renderW, height]);

  // 节点尺寸表：箭头回退量 / 选中描边要按节点实际形状（矩形药丸需按半宽，而非固定 12px）。
  // x/y 也要存：箭头边界求交必须以**节点中心**为靶点（clipToRect 的约定），
  // 不能拿折线终点凑——辐射布局的终点落在药丸近侧边缘而非中心，拿它当中心会把
  // 箭头沿射线推离药丸半个宽度，悬在半空。
  const nodeRectById = useMemo(() => {
    const m = new Map<number, { shape: 'circle' | 'rect'; x: number; y: number; w: number; h: number }>();
    layout?.nodes.forEach((n) =>
      m.set(n.id, { shape: n.shape, x: n.x, y: n.y, w: n.w ?? 120, h: n.h ?? 26 }),
    );
    return m;
  }, [layout]);

  // 依赖 loading / layout：loading 与空态返回的是不带 ref 的占位 div，
  // 只有真正挂载图表容器（带 ref 的 div）后这里才观察得到真实宽度。
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

  // 视口高度（世界坐标）。svg 的 viewBox 与渲染尺寸 1:1 —— 缩放**全部**交给内层 <g transform>。
  // 旧实现把 viewBox 设成 `layout.width`（分层布局下可能 3000px+），浏览器按 `meet` 把整张图
  // 连文字一起等比压回容器宽，于是"出边一多字就糊"；同时滚轮以光标为锚点的换算也失准
  // （那段代码假设 viewBox 与屏幕 1:1）。
  const viewH = Math.max(320, layout ? layout.height : height);
  // 图区实际可视高度（受外层 `maxHeight` 限制）：fit 按**看得见的那块**算，
  // 而不是按滚动总高 —— 否则内容一高就被缩得比必要更小。
  const viewportH = Math.min(height, viewH);

  /**
   * 真正的 zoom-to-fit：按**内容包围盒**（`layout.content`，缺失时退回整块画布）算缩放与居中。
   *
   * 旧实现只是 `setTransform({x:0,y:0,k:1})`，等于"不缩放"，所以「适应屏幕」按钮按了跟没按一样。
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

  // 供滚轮 / 拖拽在"尚未手动干预"时以 fit 态为起点做增量（ref 稳定，不进 useCallback 依赖）。
  const fitRef = useRef(fitTransform);
  fitRef.current = fitTransform;

  // **必须在所有提前返回之前声明**，否则 loading 时它不执行、数据返回后多出一个 Hook，
  // 会直接触发 "Rendered more hooks than during the previous render" 白屏
  // （`GraphCanvas.test.tsx` 就是守这个用例的）。
  //
  // 按 id 建索引：后端会为同一对端点的**不同路径**各出一条边。
  // 注意 `id` **不保证互不相同** —— 正向视角下同一条传播边（seed）展开出的多条路径会复用
  // 同一个 evidence 边 id。所以这里只是**兜底**（首个命中），真正的精确匹配靠 `seq` + `viewEdgeKeys`。
  const edgeById = useMemo(() => {
    const m = new Map<number, EdgeView>();
    for (const e of edges) if (!m.has(e.id)) m.set(e.id, e);
    return m;
  }, [edges]);
  // 兜底：就地展开的子图可能带来与主图重复的 id，此时退回按端点查。
  const edgeByPair = useMemo(() => {
    const m = new Map<string, EdgeView>();
    for (const e of edges) m.set(`${e.from}->${e.to}`, e);
    return m;
  }, [edges]);
  // `EdgeView` → 唯一键：用**下标**补齐 `seq`，与布局产出的 `edgeKey`（带 `seq`）对齐。
  // 有了它，`(id, from, to)` 相同、`via` 不同的多条平行路径也能各自独立悬浮 / 点击：
  // 悬浮谁只点亮谁，点开抽屉也显示**这条路径自己的** `via` 链路。
  const viewEdgeKeys = useMemo(() => {
    const m = new Map<EdgeView, string>();
    edges.forEach((e, i) => m.set(e, edgeKey({ id: e.id, from: e.from, to: e.to, seq: i })));
    return m;
  }, [edges]);
  // 前端 HTTP 调用方：作为 `CallsHttp` 边起点的函数节点。它本质上是「前端 API 入口」，
  // 与后端 Method 同构、是被契约桥显式带入图的关键节点，不该以匿名语法药丸呈现。
  // 这里只做**视觉升级**（带种类色填充），不改其 kind —— 既让它一眼读成「一等节点」，
  // 又不破坏折叠视图「语义节点 / 塌缩兜底」的既有不变量与后端判定。
  // 必须在提前 return 之前声明：loading→数据 两次渲染 Hook 数量不一致会白屏。
  const frontendCallerIds = useMemo(() => {
    const s = new Set<number>();
    for (const e of fEdges) if (e.kind === 'CallsHttp') s.add(e.from);
    return s;
  }, [fEdges]);

  // 聚焦：悬浮 node / edge 时，保留"目标 + 其直连邻居"全亮，其余淡出成鬼影（仍留结构轮廓）。
  // 必须放在提前 return 之前，否则 loading→数据 两次渲染 Hook 数量不一致会白屏。
  // 各视角一致：悬浮永远聚焦；淡出深度随边数自适应（稀疏图只轻微压暗、不整页消失）。
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
  // 悬浮聚焦：始终在悬浮 node / edge 时保留"目标 + 直连邻居"全亮、其余淡出（各视角一致）。
  // 淡出深度随边数连续插值：4 条边≈0.4（轻微强调），40 条边≈0.12（深度压暗），中间线性过渡。
  const focusing = focus.hasFocus;
  const dimT = Math.min(1, Math.max(0, (edges.length - DIM_EDGE_LOW) / (DIM_EDGE_HIGH - DIM_EDGE_LOW)));
  const dimOpacity = DIM_OPACITY_MAX - (DIM_OPACITY_MAX - DIM_OPACITY_MIN) * dimT;

  // 滚轮缩放必须用原生非 passive 监听器：React 合成 onWheel 在根上被注册为 passive，
  // 调用 preventDefault 无效，会导致页面被一起滚动。这里用回调 ref，在 svg 真正挂载时挂上监听
  // （loading / 空态提前返回期间 svg 不存在，回调 ref 会在挂载后自动重新触发），并显式 passive:false。
  // 必须放在提前 return 之前，否则 loading→数据 两次渲染的 Hook 数量不一致会白屏。
  const svgWheelRef = useCallback(
    (el: SVGSVGElement | null) => {
      if (!el) return;
      const onWheel = (e: WheelEvent) => {
        e.preventDefault();
        const factor = e.deltaY > 0 ? 0.9 : 1.1;
        // 以光标为锚点缩放：保持光标下的世界坐标点不动（viewBox 与渲染 1:1）。
        const rect = el.getBoundingClientRect();
        const cx = e.clientX - rect.left;
        const cy = e.clientY - rect.top;
        setTransform((prev) => {
          const t = prev ?? fitRef.current ?? { x: 0, y: 0, k: 1 };
          // 范围收窄到 0.6–2：缩放是"看空间关系"的手段，不是字号开关 ——
          // 0.25 会把字压到 3px（不可读），3 会把药丸撑成巨块，两端都没有信息增量。
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
        {/* `tip` 只在 nest / fullscreen 模式下生效，这里用文字并列避免 antd 告警 */}
        <Space direction="vertical" align="center" size={8}>
          <Spin />
          <Typography.Text type="secondary">{t('加载视图…')}</Typography.Text>
        </Space>
      </div>
    );
  }
  if (!layout) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        <Empty description={t('该视角下暂无可展示的对象')} />
      </div>
    );
  }

  // 就用 `CanvasNode` 的真实类型：早先这里 `as NodeView` 谎报了字段，
  // 于是悬浮卡片访问 `locations.length` 时数据在、类型在、运行时没有 —— 直接崩。
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

  // 生效的变换：未手动缩放平移时自动适应屏幕。
  // 取名 `tf` 而非 `view`，避免与边渲染里的局部 `view`（EdgeView）混淆。
  const tf = transform ?? fitTransform ?? { x: 0, y: 0, k: 1 };

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
      {/* 内容比视口高时改为滚动查看。旧实现用 `min(height, layout.height)` + `overflow:hidden`，
          超出部分被直接裁掉且**无法**滚动 —— 分层图一深就"下半截凭空消失"。 */}
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
          {/* 同心环引导线：显式标出"环 = 跳数"，仅视觉参照，不参与命中 */}
          {layout.guides?.map((g, i) => (
            <g key={`guide${i}`}>
              {/* 以下线宽一律 `non-scaling-stroke`：缩放只改变**空间关系**，不把线一起放大/压细
                  （地图语义）。放大时不糊成粗杠，缩小时也不会细到看不见。
                  文字与其描边白底**不**在此列 —— 它们要跟着字号走，否则描边与字脱节。 */}
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
                {g.label + t(' 跳')}
              </text>
            </g>
          ))}

          {/* 聚类框 */}
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
                {g.count + t(' 个成员')}
              </text>
            </g>
          ))}

          {/* 矩阵 */}
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

          {/* 边 */}
          {layout.edges.map((e) => {
            // 精确回指：`seq` = 这条边在 `edges` 中的下标 ⇒ 直接取到**含各自 via 的那条** EdgeView。
            // 同一 (id, from, to) 的多条平行路径因此不再共用同一个 view（否则悬浮卡永远显示第一条）。
            // 仅当布局未带 `seq`（历史 / 合成路径）时才退回按 id / 端点查。
            const view =
              (e.seq != null ? edges[e.seq] : undefined) ??
              edgeById.get(e.id) ??
              edgeByPair.get(`${e.from}->${e.to}`) ??
              edges.find((x) => x.id === e.id);
            // 聚焦：悬浮时保留"目标边 + 其两端 node"全亮，其余按密度压暗。
            const inFocus = focus.edgeKeys.has(edgeKey(e));
            const dim = focusing && !inFocus;
            const active = inFocus;
            const d = e.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p[0]},${p[1]}`).join(' ');
            // 标签锚点：真实几何中点 + 法线偏移，避免落在边线上或目标节点底下
            const lp = labelAnchor(e.points);
            // 方向箭头：沿末段方向在终点前回退若干 px 画小三角，避免被节点盖住。
            // 矩形药丸按半宽回退，圆形按固定量回退。
            const _pts = e.points;
            const _p1 = _pts[_pts.length - 1];
            const _p0 = _pts[_pts.length - 2] ?? _pts[0];
            const _ang = Math.atan2(_p1[1] - _p0[1], _p1[0] - _p0[0]);
            const _toRect = nodeRectById.get(e.to);
            // 箭头尖 = 末段线段与目标矩形的交点：终点在药丸边缘时就是终点本身（钉在边的尽头），
            // 终点在中心时退化为进入交点（旧布局行为不变）。
            const _border = _toRect
              ? clipArrowTip(_p0, _p1, _toRect.x, _toRect.y, _toRect.w, _toRect.h)
              : _p1;
            const _len = Math.hypot(_p1[0] - _p0[0], _p1[1] - _p0[1]) || 1;
            // 箭头尖直接落在边界交点上（不再回退 3px）：终点已由布局钉在药丸近侧边缘，
            // 回退只会产生"差一点没到尽头"的空隙（用户实测反馈）。
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
                {/* 加宽的透明命中区，便于悬浮细边 */}
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
                {/* 方向箭头：点明有向依赖的流向（consumer→table / handler→config…）。
                    折叠的"经 N 跳"边同样画箭头——方向仍成立，非直连已由虚线 + 注解表达。 */}
                <polygon
                  points={`${_tipx},${_tipy} ${_ax1},${_ay1} ${_ax2},${_ay2}`}
                  fill={edgeColor(view?.kind ?? '')}
                  fillOpacity={active ? 1 : 0.7}
                  style={{ pointerEvents: 'none' }}
                />
                {/* 边的类型：语义边种类（`ReadsConfig` / `MapsTo`…）就是这个图的"谓语"，
                    标出来才读得懂。默认**常显**（不再要求悬浮）：只有边数超过阈值、
                    又没被放大时，才退化为"只标悬浮/选中那条"以防糊成一片。
                    悬浮时其它边只淡出、不隐藏标签（不因聚焦而丢信息）。
                    另外缩小时（k < 0.85）标签物理上更挤，也退化为按需标注 ——
                    反正那时字已经小到读不清，常显只剩噪声。 */}
                {view &&
                showEdgeLabels &&
                (active || (edges.length <= EDGE_LABEL_LIMIT && tf.k >= 0.85) || tf.k >= 1.15) ? (
                  <text
                    x={lp.x}
                    y={lp.y}
                    fontSize={10}
                    fontWeight={500}
                    textAnchor="middle"
                    dominantBaseline="central"
                    stroke="#ffffff"
                    strokeWidth={3.5}
                    strokeLinejoin="round"
                    paintOrder="stroke"
                    style={{ pointerEvents: 'none', userSelect: 'none' }}
                  >
                    {/* 边种类（按语言本地化为语义谓语，如 `PublishesTo` → `投递到`） */}
                    <tspan fill={edgeColor(view.kind)}>{t(`edge.${view.kind}`)}</tspan>
                    {/* 折叠的间接依赖（跨 N 个语法节点）靠虚线 + 图例区分，具体跳数在悬浮卡里给，
                        不再挤进线上标签以免加长白缺口、和相邻边叠。 */}
                  </text>
                ) : null}
              </g>
            );
          })}

          {/* 节点 */}
          {layout.nodes.map((n) => {
            const meta = nodeById.get(n.id);
            const isCenter = center?.id === n.id;
            const isOrigin = originId != null && originId === n.id;
            const w = n.w ?? 120;
            const h = n.h ?? 26;
            const fill = nodeColor(n.kind);
            const isFrontendCaller = frontendCallerIds.has(n.id);
            // 节点统一为 rect 药丸，文字内嵌于框内，无需外伸标签。
            // 前端 HTTP 调用方填充种类色（淡），从匿名白底语法药丸升级为「一等节点」观感；
            // 其余节点保持白底 + 种类色描边。
            return (
              <g
                key={n.id}
                transform={`translate(${n.x},${n.y})`}
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
                {/* 所有节点统一用 1.4px 种类色描边；中心节点的强调改由尺寸 + 字重承担，
                    不再靠加粗边框堆层级，避免同 kind 节点描边粗细看着不一致。
                    前端 HTTP 调用方额外填充淡种类色，凸显其「一等入口」地位。 */}
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
                {/* 前后端标记：节点右上角的色点（蓝=前端 / 橙=后端），一眼区分该节点属于哪一端。
                    颜色与图例一致；无 `side` 的语法节点（File / Class …）不画。 */}
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
                {/* 原生 tooltip：种类 / 类别 / 名称 —— 便于分辨同名或含义不明的节点 */}
                <title>
                  {meta?.category && meta.category !== n.kind
                    ? `${t(`node.${n.kind}`)}（类别 ${meta.category}）· ${n.name}`
                    : `${t(`node.${n.kind}`)} · ${n.name}`}
                </title>
                {/* 种类图标：以 kind 色填充，替代原来的彩色 kind 文字前缀 —— 省下横向空间给名字，
                    长路径（如路由）就能显示更完整。图标固定 14px，左对齐贴在药丸内。
                    仅当本次视图出现的 kind ≥ 3 时才画（同质视图退化为只靠颜色，见 `showNodeIcons`）。
                    必须包 `<foreignObject>`：antd 图标根元素是 HTML `<span>`，直接放进 SVG `<g>`
                    会被浏览器按 SVG 命名空间丢弃（表现为图标消失、但 23px 图标位仍占着 —— 空白假象）。
                    尺寸用 fontSize 控制（span 上的 width/height 属性无效），与图例渲染口径一致。 */}
                {showNodeIcons ? (
                  (() => {
                    const Icon = nodeIcon(n.kind);
                    const ICON = 14;
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
                <text
                  x={-w / 2 + (showNodeIcons ? 23 : 8)}
                  y={4}
                  fontSize={isCenter ? 13 : 11}
                  fontWeight={isCenter ? 700 : 400}
                  fill={isCenter ? '#0f172a' : '#475569'}
                  textAnchor="start"
                  style={{ pointerEvents: 'none', userSelect: 'none' }}
                >
                  {/* 语义名（如 `store_order_refund_service` / `GET /v2/order/.../create`）才是人
                      真正在找的实体，做主。种类已由左侧图标 + 色表达，不再喧宾夺主。长名按**视觉宽度**
                      中间截断 40 位（保头尾，CJK 一字计 1.8 位），与布局 `pillWidth` 估算一致，不会溢出药丸。 */}
                  <tspan fontWeight={isCenter ? 700 : 500} fontSize={isCenter ? 13 : 11}>
                    {truncateMiddle(n.name, 40)}
                  </tspan>
                </text>
                {/* 选中环只给"非中心的选中节点"留（当前交互下不会出现，留作扩展点） */}
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

      {/* 悬浮详情卡：取代"其它节点 / 边变淡"的旧行为 —— 悬浮即给出可读属性 */}
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
              <span style={{ color: '#94a3b8' }}>{t('类别 ') + hoveredNode.category}</span>
            ) : null}
          </div>
          <div style={{ fontSize: 13, fontWeight: 600, marginTop: 2, wordBreak: 'break-all' }}>
            {hoveredNode.name}
          </div>
          {hoveredNode.fqn ? (
            <div style={{ color: '#64748b', wordBreak: 'break-all' }}>{hoveredNode.fqn}</div>
          ) : null}
          <div style={{ color: '#64748b' }}>
            入边 {hoveredNode.metrics?.fan_in ?? 0} · 出边 {hoveredNode.metrics?.fan_out ?? 0} ·{' '}
            位置 {hoveredNode.locations?.length ?? 0}
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
              ? `单击 → 一级切到「${hoveredNode.own_view}」视角，二级为「${hoveredNode.name}」`
              : '单击展开调用链 · 右键看位置'}
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
            <b>{t(`edge.${hoveredEdge.kind}`)}</b>
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
            {t('置信度') + ' ' + hoveredEdge.confidence.toFixed(2)}
            {hoveredEdge.hops != null ? ` · ${t('途经 ') + hoveredEdge.hops + t(' 跳')}` : ''}
          </div>
          <div style={{ marginTop: 6, color: '#94a3b8' }}>{t('单击查看证据链')}</div>
        </div>
      ) : null}

      {/* 底部只常驻**图例**（虚线 = 间接是这张图最易误读的点）；
          布局说明与操作提示收进 ⓘ —— 都是看一次就够的文案，不值得占一行。 */}
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
        <span>{t('虚线 = 经调用链间接；实线 = 直接调用')}</span>
        <Tooltip title={t(layout.note) + ' ' + t('滚轮缩放 · 拖拽平移 · 左键单击切视角 · 右键打开位置')}>
          <InfoCircleOutlined style={{ cursor: 'help', color: 'rgba(0,0,0,0.35)' }} />
        </Tooltip>
      </div>
      {/* 角落图例：列出本次数据实际出现的 kind → 图标 / 本地化名，可折叠。
          图标与节点内图标同源（nodeIcon），颜色取该 kind 色 —— 看图即可对上号。 */}
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
            <span>图例</span>
            <span style={{ color: 'rgba(0,0,0,0.35)' }}>{legendOpen ? '▾' : '▸'}</span>
          </div>
          {legendOpen ? (
            <div
              style={{
                padding: '2px 10px 10px',
                display: 'grid',
                gridTemplateColumns: 'auto 1fr',
                gap: '4px 8px',
                maxHeight: 220,
                overflow: 'auto',
              }}
            >
              {usedKinds(layout.nodes.map((n) => n.kind)).map((k) => {
                const Icon = nodeIcon(k);
                return (
                  <Fragment key={k}>
                    <span
                      style={{
                        display: 'inline-flex',
                        alignItems: 'center',
                        color: nodeColor(k),
                      }}
                    >
                      <Icon style={{ fontSize: 14 }} />
                    </span>
                    <span style={{ lineHeight: '18px' }}>{t(`node.${k}`)}</span>
                  </Fragment>
                );
              })}
              {subProjects && subProjects.length > 0 ? (
                <Fragment>
                  <div style={{ height: 1, background: '#eef1f6', margin: '3px 0', gridColumn: '1 / -1' }} />
                  {subProjects.map((sp) => (
                    <Fragment key={sp.id}>
                      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
                        <span
                          style={{
                            width: 8,
                            height: 8,
                            borderRadius: 99,
                            background: subProjectColors.get(sp.id) ?? '#94a3b8',
                          }}
                        />
                      </span>
                      <span style={{ lineHeight: '18px' }}>
                        {sp.name}{' '}
                        <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                          （{roleLabel(sp.role)}）
                        </Typography.Text>
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
