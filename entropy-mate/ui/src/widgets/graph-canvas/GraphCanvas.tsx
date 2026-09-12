import { useMemo, useRef, useState } from 'react';
import { Empty, Spin, Tooltip } from 'antd';
import type { EdgeView, LayoutMode, NodeView, SourceLocation } from '@/entities/view';
import { edgeColor, nodeColor } from '@/entities/graph';
import { truncate } from '@/shared/lib/format';
import { layoutOf, type LayoutInput, type LayoutResult } from './layout/types';

export interface CanvasNode {
  id: number;
  kind: string;
  name: string;
  ring: number;
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
  /** 单击节点：仅当该节点有对应视角时才会切视角。 */
  onNodeClick?: (id: number, kind: string, hasOwnView: boolean) => void;
  /** 右键 / 详情图标：打开 Inspector 或跳转，不切视角。 */
  onNodeContextMenu?: (id: number, kind: string, event: React.MouseEvent) => void;
  onEdgeClick?: (edge: EdgeView) => void;
  locationsOf?: (id: number) => SourceLocation[];
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
  } = props;

  const [hover, setHover] = useState<number | null>(null);
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

  if (loading) {
    return (
      <div style={{ height, display: 'grid', placeItems: 'center' }}>
        <Spin tip="加载视图…" />
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

  const nodeById = new Map<number, NodeView>();
  const allNodes: NodeView[] = [];
  if (center) allNodes.push(center as NodeView);
  rings.flat().forEach((n) => allNodes.push(n as NodeView));
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

  return (
    <div
      style={{
        border: '1px solid #eef0f4',
        borderRadius: 14,
        background: 'linear-gradient(180deg,#fbfcfe,#f4f6fa)',
        overflow: 'hidden',
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
            const view = edges.find((x) => x.id === e.id);
            const dim = hover !== null && e.from !== hover && e.to !== hover;
            const active = hover !== null && (e.from === hover || e.to === hover);
            const d = e.points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p[0]},${p[1]}`).join(' ');
            return (
              <path
                key={e.id}
                d={d}
                fill="none"
                stroke={edgeColor(view?.kind ?? '')}
                strokeWidth={active ? 2.4 : 1.2}
                strokeOpacity={dim ? 0.18 : 0.8}
                strokeDasharray={view?.resolved ? undefined : '5 4'}
                style={{ cursor: 'pointer' }}
                onClick={() => view && onEdgeClick?.(view)}
              />
            );
          })}

          {/* 节点 */}
          {layout.nodes.map((n) => {
            const meta = nodeById.get(n.id);
            const isCenter = center?.id === n.id;
            const isOrigin = originId != null && originId === n.id;
            const dim = hover !== null && hover !== n.id;
            const r = n.shape === 'rect' ? 0 : isCenter ? 15 : 7;
            const w = n.w ?? 120;
            const h = n.h ?? 26;
            const fill = nodeColor(n.kind);
            return (
              <g
                key={n.id}
                transform={`translate(${n.x},${n.y})`}
                opacity={dim ? 0.32 : 1}
                style={{ cursor: (meta?.has_own_view ?? false) ? 'pointer' : 'default' }}
                onMouseEnter={() => setHover(n.id)}
                onMouseLeave={() => setHover(null)}
                onClick={() => onNodeClick?.(n.id, n.kind, meta?.has_own_view ?? false)}
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
                <text
                  x={n.shape === 'rect' ? 0 : r + 6}
                  y={n.shape === 'rect' ? 4 : 4}
                  fontSize={isCenter ? 13 : 11}
                  fontWeight={isCenter ? 700 : 400}
                  fill={isCenter ? '#0f172a' : '#475569'}
                  textAnchor={n.shape === 'rect' ? 'middle' : 'start'}
                  style={{ pointerEvents: 'none', userSelect: 'none' }}
                >
                  {truncate(n.name, n.shape === 'rect' ? 22 : 26)}
                </text>
                {selectedId === n.id ? (
                  <circle r={r + 6} fill="none" stroke="#3d7eff" strokeWidth={1.6} />
                ) : null}
              </g>
            );
          })}
        </g>
      </svg>

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
