import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useLocale } from '@/shared/lib/i18n';
import { Empty, Space, Spin, Tag, Tooltip, Typography } from 'antd';
import type { EdgeView, LayoutMode, NodeView, SourceLocation } from '@/entities/view';
import { edgeColor, nodeColor } from '@/entities/graph';
import { truncate } from '@/shared/lib/format';
import { layoutOf, type LayoutInput, type LayoutResult } from './layout/types';

/**
 * 超过这个边数就只在"悬浮 / 选中"时标注边类型。
 *
 * 边类型是语义图的"谓语"（`ReadsConfig` / `MapsTo` / `ReadsCache`…），标出来才读得懂；
 * 但共享资源视图可能有上百条边，全部标注会糊成一片 —— 所以边多时退化成"按需标注"，
 * 悬浮详情卡始终给出完整信息。
 */
const EDGE_LABEL_LIMIT = 40;
// 悬浮聚焦时非聚焦元素的淡出深度：随边数连续变化（边越少压得越浅，边越多压得越深），避免稀疏图像"全图消失"。
const DIM_OPACITY_MIN = 0.12; // 稠密图最深
const DIM_OPACITY_MAX = 0.4; // 稀疏图最浅
const DIM_EDGE_LOW = 4; // 边数低于此取最浅
const DIM_EDGE_HIGH = 40; // 边数高于此取最深

/**
 * 边的稳定唯一键。
 *
 * 折叠视图里存在"合成边"（提拉 / 反向汇总得到，没有真实行），只靠 `id` 会撞键；
 * 同一对端点也可能有多条不同种类的边，所以带上端点一起构成键。
 */
const edgeKey = (e: { id: number; from: number; to: number }) => `${e.id}:${e.from}->${e.to}`;

/**
 * 边标签锚点。
 *
 * 取折线**真实几何中点**（按累计长度），再沿所在段的法线偏移若干像素，
 * 让标签落在边旁而不是压在边线 / 端点上。
 *
 * 不能用 `points[Math.floor(len / 2)]`：直线边只有两个点，索引 1 就是**终点**，
 * 标签会被后绘制的目标节点药丸（不透明白底）整块盖住 —— 表现就是
 * "只有悬浮时才能在卡片里看到边名"。
 */
function labelAnchor(points: Array<[number, number]>, offset = 9): { x: number; y: number } {
  if (points.length === 0) return { x: 0, y: 0 };
  if (points.length === 1) return { x: points[0][0], y: points[0][1] - offset };

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
      const mx = ax + (bx - ax) * t;
      const my = ay + (by - ay) * t;
      const len = Math.hypot(bx - ax, by - ay) || 1;
      // 沿法线偏移：标签贴在边的一侧，不遮住线的走向
      return { x: mx + (-(by - ay) / len) * offset, y: my + ((bx - ax) / len) * offset };
    }
    remain -= segLen[i];
  }
  return { x: points[0][0], y: points[0][1] - offset };
}

export interface CanvasNode {
  id: number;
  kind: string;
  /** 语义节点的类别（目前与 kind 一致）；语法节点为 null。 */
  category?: string | null;
  /** 该节点对应的视角 id（点击即切）；无则为 null。 */
  own_view?: string | null;
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
  } = props;

  const { t } = useLocale();
  const [hover, setHover] = useState<number | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);
  const [pointer, setPointer] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [transform, setTransform] = useState({ x: 0, y: 0, k: 1 });
  const drag = useRef<{ x: number; y: number } | null>(null);

  // 语义图内容切换（fitKey 变化）时，把平移缩放重置回「整图 fit」初始态。
  // 悬浮聚焦 / 手动缩放平移 / 单节点就地展开不改变 fitKey，故不触发重置。
  useEffect(() => {
    setTransform({ x: 0, y: 0, k: 1 });
  }, [fitKey]);

  // 工具栏「适应屏幕」按钮：fitSignal 自增即重置为整图 fit。
  useEffect(() => {
    if (fitSignal === undefined) return;
    setTransform({ x: 0, y: 0, k: 1 });
  }, [fitSignal]);

  // 用真实容器宽度喂给布局，避免"按 1040 设计、再被窄列整体缩小"导致的拥挤。
  // 首帧用默认 width，挂载后 ResizeObserver 量出真实宽度并触发一次重排（无感）。
  const containerRef = useRef<HTMLDivElement | null>(null);
  const [measuredWidth, setMeasuredWidth] = useState<number | null>(null);
  const renderW = measuredWidth ?? width;

  const layout: LayoutResult | null = useMemo(() => {
    if (!center && !clusters?.length && !matrix) return null;
    const input: LayoutInput = {
      center: center ?? { id: -1, kind: 'Unknown', name: '', ring: 0 },
      rings: center ? rings : [],
      edges: edges.map((e) => ({ id: e.id, from: e.from, to: e.to })),
      clusters: clusters?.map((c) => ({
        key: c.key,
        label: c.label,
        count: c.count,
        members: c.members.map((m) => ({ ...m })),
      })),
      matrix,
      width: renderW,
      height,
    };
    return layoutOf(mode)(input);
  }, [mode, center, rings, edges, clusters, matrix, renderW, height]);

  // 节点尺寸表：箭头回退量 / 选中描边要按节点实际形状（矩形药丸需按半宽，而非固定 12px）。
  const nodeRectById = useMemo(() => {
    const m = new Map<number, { shape: 'circle' | 'rect'; w: number; h: number }>();
    layout?.nodes.forEach((n) => m.set(n.id, { shape: n.shape, w: n.w ?? 120, h: n.h ?? 26 }));
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

  // 按端点建索引：合成边 id 不唯一，按 id 查会永远只命中第一条（表现为"所有边都显示成同一种类"）。
  // **必须在所有提前返回之前声明**，否则 loading 时它不执行、数据返回后多出一个 Hook，
  // 会直接触发 "Rendered more hooks than during the previous render" 白屏
  // （`GraphCanvas.test.tsx` 就是守这个用例的）。
  const edgeByPair = useMemo(() => {
    const m = new Map<string, EdgeView>();
    for (const e of edges) m.set(`${e.from}->${e.to}`, e);
    return m;
  }, [edges]);

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
          edgeKeys.add(edgeKey(e));
        } else if (e.to === hover) {
          nodes.add(e.from);
          edgeKeys.add(edgeKey(e));
        }
      }
    }
    if (hoverEdge != null) {
      const ev = edges.find((x) => edgeKey(x) === hoverEdge);
      if (ev) {
        nodes.add(ev.from);
        nodes.add(ev.to);
      }
      edgeKeys.add(hoverEdge);
    }
    return { nodes, edgeKeys, hasFocus: hover != null || hoverEdge != null };
  }, [hover, hoverEdge, edges]);
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
        setTransform((t) => {
          const k = Math.max(0.25, Math.min(3, t.k * factor));
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
  const hoveredEdge = hoverEdge != null ? edges.find((x) => edgeKey(x) === hoverEdge) ?? null : null;

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
      <svg
        width="100%"
        height={Math.min(height, layout.height)}
        viewBox={`0 0 ${layout.width} ${Math.min(height, layout.height)}`}
        ref={svgWheelRef}
        onMouseDown={(e) => {
          drag.current = { x: e.clientX, y: e.clientY };
        }}
        onMouseMove={(e) => {
          if (!drag.current) return;
          const dx = e.clientX - drag.current.x;
          const dy = e.clientY - drag.current.y;
          drag.current = { x: e.clientX, y: e.clientY };
          setTransform((t) => ({ ...t, x: t.x + dx, y: t.y + dy }));
        }}
        onMouseUp={() => (drag.current = null)}
        onMouseLeave={() => (drag.current = null)}
        style={{ cursor: 'grab', display: 'block' }}
      >
        <g transform={`translate(${transform.x},${transform.y}) scale(${transform.k})`}>
          {/* 同心环引导线：显式标出"环 = 跳数"，仅视觉参照，不参与命中 */}
          {layout.guides?.map((g, i) => (
            <g key={`guide${i}`}>
              <circle cx={g.cx} cy={g.cy} r={g.r} fill="none" stroke="#e6ebf3" strokeWidth={1} />
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
            const view = edgeByPair.get(`${e.from}->${e.to}`) ?? edges.find((x) => x.id === e.id);
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
            const _off = _toRect && _toRect.shape === 'rect' ? _toRect.w / 2 + 6 : 12;
            const _tipx = _p1[0] - _off * Math.cos(_ang);
            const _tipy = _p1[1] - _off * Math.sin(_ang);
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
                  strokeWidth={active ? 2.6 : 1.2}
                  strokeOpacity={active ? 1 : 0.8}
                  strokeDasharray={view?.resolved ? undefined : '5 4'}
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
                    悬浮时其它边只淡出、不隐藏标签（不因聚焦而丢信息）。 */}
                {view && showEdgeLabels && (edges.length <= EDGE_LABEL_LIMIT || active || transform.k >= 1.15) ? (
                  <text
                    x={lp.x}
                    y={lp.y}
                    fontSize={10}
                    fontWeight={500}
                    textAnchor="middle"
                    stroke="#ffffff"
                    strokeWidth={3.5}
                    strokeLinejoin="round"
                    paintOrder="stroke"
                    style={{ pointerEvents: 'none', userSelect: 'none' }}
                  >
                    {/* 边种类（按语言本地化为语义谓语，如 `PublishesTo` → `投递到`） */}
                    <tspan fill={edgeColor(view.kind)}>{t(`edge.${view.kind}`)}</tspan>
                    {/* 折叠提示：这条"直连"其实跨了 N 个语法节点，必须标出来，不能让它看起来是真的直连 */}
                    {view.hops ? (
                      <tspan fill="#94a3b8">{` ·经 ${view.hops}${t(' 跳')}`}</tspan>
                    ) : null}
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
            // 节点统一为 rect 药丸，文字内嵌于框内，无需外伸标签。
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
                <rect
                  x={-w / 2}
                  y={-h / 2}
                  width={w}
                  height={h}
                  rx={6}
                  fill="#fff"
                  stroke={fill}
                  strokeWidth={1.4}
                  style={{ transition: 'stroke-width 140ms ease' }}
                />
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
                <text
                  x={0}
                  y={4}
                  fontSize={isCenter ? 13 : 11}
                  fontWeight={isCenter ? 700 : 400}
                  fill={isCenter ? '#0f172a' : '#475569'}
                  textAnchor="middle"
                  style={{ pointerEvents: 'none', userSelect: 'none' }}
                >
                  {/* 语义名（如 `store_order_refund_service`）才是人真正在找的实体，做主；
                      种类 `kind` 仅作小号彩色前缀徽标（保留"看得出是 Queue / Table"的能力），
                      不再喧宾夺主。名字与 kind 同串渲染，宽度与布局 `pillWidth` 估算一致，不会溢出药丸。 */}
                  {n.name && n.name !== n.kind ? (
                    <>
                      <tspan fill={fill} fontWeight={700} fontSize={isCenter ? 10 : 9}>
                        {t(`node.${n.kind}`)}
                      </tspan>
                      <tspan fill="#94a3b8" fontWeight={400}>
                        {' · '}
                      </tspan>
                      <tspan fontWeight={isCenter ? 700 : 500} fontSize={isCenter ? 13 : 11}>
                        {truncate(n.name, 26)}
                      </tspan>
                    </>
                  ) : (
                    <tspan fill={fill} fontWeight={600}>
                      {n.kind}
                    </tspan>
                  )}
                </text>
                {selectedId === n.id ? (
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

      {/* 悬浮详情卡：取代"其它节点 / 边变淡"的旧行为 —— 悬浮即给出可读属性 */}
      {hoveredNode ? (
        <div
          style={{
            position: 'absolute',
            left: Math.min(pointer.x + 16, Math.max(8, renderW - 268)),
            top: Math.min(pointer.y + 16, Math.max(8, height - 170)),
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
            top: Math.min(pointer.y + 16, Math.max(8, height - 150)),
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
        <span style={{ color: '#0f172a', fontWeight: 600 }}>{t(layout.note)}</span>
        <span>{t('虚线 = 待验证假设；实线 = 已解析')}</span>
        <span>{t('滚轮缩放 · 拖拽平移 · 左键单击切视角 · 右键打开位置')}</span>
      </div>
    </div>
  );
}
