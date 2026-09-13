import { useMemo, useRef, useState } from 'react';
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

/**
 * 边的稳定唯一键。
 *
 * 折叠视图里存在"合成边"（提拉 / 反向汇总得到，没有真实行），只靠 `id` 会撞键；
 * 同一对端点也可能有多条不同种类的边，所以带上端点一起构成键。
 */
const edgeKey = (e: { id: number; from: number; to: number }) => `${e.id}:${e.from}->${e.to}`;

export interface CanvasNode {
  id: number;
  kind: string;
  /** 语义节点的类别（如 ExternalSystem）；语法节点为 null。 */
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
    height = 620,
    onNodeClick,
    onNodeContextMenu,
    onEdgeClick,
    showEdgeLabels = true,
  } = props;

  const [hover, setHover] = useState<number | null>(null);
  const [hoverEdge, setHoverEdge] = useState<string | null>(null);
  const [pointer, setPointer] = useState<{ x: number; y: number }>({ x: 0, y: 0 });
  const [transform, setTransform] = useState({ x: 0, y: 0, k: 1 });
  const drag = useRef<{ x: number; y: number } | null>(null);

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
      width,
      height,
    };
    return layoutOf(mode)(input);
  }, [mode, center, rings, edges, clusters, matrix, width, height]);

  // 按端点建索引：合成边 id 不唯一，按 id 查会永远只命中第一条（表现为"所有边都显示成同一种类"）。
  // **必须在所有提前返回之前声明**，否则 loading 时它不执行、数据返回后多出一个 Hook，
  // 会直接触发 "Rendered more hooks than during the previous render" 白屏
  // （`GraphCanvas.test.tsx` 就是守这个用例的）。
  const edgeByPair = useMemo(() => {
    const m = new Map<string, EdgeView>();
    for (const e of edges) m.set(`${e.from}->${e.to}`, e);
    return m;
  }, [edges]);

  if (loading) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        {/* `tip` 只在 nest / fullscreen 模式下生效，这里用文字并列避免 antd 告警 */}
        <Space direction="vertical" align="center" size={8}>
          <Spin />
          <Typography.Text type="secondary">加载视图…</Typography.Text>
        </Space>
      </div>
    );
  }
  if (!layout) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        <Empty description="该视角下暂无可展示的对象" />
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

  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault();
    const factor = e.deltaY > 0 ? 0.9 : 1.1;
    setTransform((t) => ({ ...t, k: Math.max(0.25, Math.min(3, t.k * factor)) }));
  };

  const hoveredNode = hover != null ? nodeById.get(hover) ?? null : null;
  const hoveredEdge = hoverEdge != null ? edges.find((x) => edgeKey(x) === hoverEdge) ?? null : null;

  return (
    <div
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
        onWheel={onWheel}
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
                {g.count} 个成员
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
            // 悬浮只做"正向强调"（加粗），不再把其它边 / 节点变淡
            const active =
              (hover !== null && (e.from === hover || e.to === hover)) || hoverEdge === edgeKey(e);
            const d = e.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p[0]},${p[1]}`).join(' ');
            const mid = e.points[Math.floor(e.points.length / 2)] ?? [0, 0];
            return (
              <g key={edgeKey(e)}>
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
                  style={{ pointerEvents: 'none' }}
                />
                {/* 边的类型：语义边种类（`ReadsConfig` / `MapsTo`…）就是这个图的"谓语"，
                    标出来才读得懂。但边一多就会糊成一片，所以：
                    边数超过阈值时只标"悬浮/选中"的那条；放大后恢复全标。 */}
                {view && showEdgeLabels && (edges.length <= EDGE_LABEL_LIMIT || active || transform.k >= 1.15) ? (
                  <text
                    x={mid[0]}
                    y={mid[1] - 4}
                    fontSize={10}
                    textAnchor="middle"
                    stroke="#ffffff"
                    strokeWidth={3}
                    paintOrder="stroke"
                    style={{ pointerEvents: 'none', userSelect: 'none' }}
                  >
                    {/* 边种类 */}
                    <tspan fill={edgeColor(view.kind)}>{view.kind}</tspan>
                    {/* 折叠提示：这条"直连"其实跨了 N 个语法节点，必须标出来，不能让它看起来是真的直连 */}
                    {view.hops ? (
                      <tspan fill="#94a3b8"> ·经 {view.hops} 跳</tspan>
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
            const r = n.shape === 'rect' ? 0 : isCenter ? 15 : 7;
            const w = n.w ?? 120;
            const h = n.h ?? 26;
            const fill = nodeColor(n.kind);
            return (
              <g
                key={n.id}
                transform={`translate(${n.x},${n.y})`}
                style={{ cursor: meta?.own_view ? 'pointer' : 'default' }}
                onMouseEnter={() => setHover(n.id)}
                onMouseLeave={() => setHover(null)}
                onClick={() => onNodeClick?.(n.id, n.kind, meta?.own_view ?? null)}
                onContextMenu={(e) => {
                  e.preventDefault();
                  onNodeContextMenu?.(n.id, n.kind, e);
                }}
              >
                {n.shape === 'rect' ? (
                  <rect
                    x={-w / 2}
                    y={-h / 2}
                    width={w}
                    height={h}
                    rx={6}
                    fill="#fff"
                    stroke={fill}
                    strokeWidth={1.4}
                  />
                ) : (
                  <circle
                    r={r}
                    fill={fill}
                    stroke={isCenter || isOrigin ? '#0f172a' : '#fff'}
                    strokeWidth={isCenter || isOrigin ? 2.5 : 1.4}
                  />
                )}
                {isOrigin ? (
                  <text
                    x={0}
                    y={-r - 8}
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
                    ? `${n.kind}（类别 ${meta.category}）· ${n.name}`
                    : `${n.kind} · ${n.name}`}
                </title>
                <text
                  x={n.shape === 'rect' ? 0 : r + 6}
                  y={n.shape === 'rect' ? 4 : 4}
                  fontSize={isCenter ? 13 : 11}
                  fontWeight={isCenter ? 700 : 400}
                  fill={isCenter ? '#0f172a' : '#475569'}
                  textAnchor={n.shape === 'rect' ? 'middle' : 'start'}
                  style={{ pointerEvents: 'none', userSelect: 'none' }}
                >
                  {/* 种类徽标（用节点配色高亮）：
                      让 `wechat_user` 看得出是 Table，也让 `Table(cache)` 与 `Cache` 可分辨 */}
                  <tspan fill={fill} fontWeight={600}>
                    {n.kind}
                  </tspan>
                  {n.name && n.name !== n.kind ? (
                    <>
                      <tspan fill="#94a3b8" fontWeight={400}>
                        {' · '}
                      </tspan>
                      <tspan>{truncate(n.name, n.shape === 'rect' ? 18 : 22)}</tspan>
                    </>
                  ) : null}
                </text>
                {selectedId === n.id ? (
                  <circle r={r + 6} fill="none" stroke="#3d7eff" strokeWidth={1.6} />
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
            left: Math.min(pointer.x + 16, Math.max(8, width - 268)),
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
            <b>{hoveredNode.kind}</b>
            {hoveredNode.category && hoveredNode.category !== hoveredNode.kind ? (
              <span style={{ color: '#94a3b8' }}>类别 {hoveredNode.category}</span>
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
            left: Math.min(pointer.x + 16, Math.max(8, width - 268)),
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
            <b>{hoveredEdge.kind}</b>
            <span style={{ color: hoveredEdge.resolved ? '#16a34a' : '#f59e0b' }}>
              {hoveredEdge.resolved ? '已解析' : '待验证'}
            </span>
          </div>
          <div style={{ marginTop: 2, wordBreak: 'break-all' }}>
            {nodeById.get(hoveredEdge.from)?.name ?? `#${hoveredEdge.from}`}
            {' → '}
            {nodeById.get(hoveredEdge.to)?.name ?? `#${hoveredEdge.to}`}
          </div>
          <div style={{ color: '#64748b' }}>
            置信度 {hoveredEdge.confidence.toFixed(2)}
            {hoveredEdge.hops != null ? ` · 途经 ${hoveredEdge.hops} 跳` : ''}
          </div>
          <div style={{ marginTop: 6, color: '#94a3b8' }}>单击查看证据链</div>
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
        <span style={{ color: '#0f172a', fontWeight: 600 }}>{layout.note}</span>
        <span>虚线 = 待验证假设；实线 = 已解析</span>
        <span>滚轮缩放 · 拖拽平移 · 左键单击切视角 · 右键打开位置</span>
      </div>
    </div>
  );
}
