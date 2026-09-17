/** 视图实体：与后端 `em-domain::model::view` 对齐。 */

export type ViewMode = 'object' | 'aggregate';

export type LayoutMode = 'radial' | 'layered' | 'spine' | 'compound' | 'matrix' | 'er';

export interface Perspective {
  id: string;
  label: string;
  mode: ViewMode;
  layout: LayoutMode;
  depth: number;
  description?: string | null;
  available: number;
}

export interface SourceLocation {
  file: string;
  line: number;
  symbol: string | null;
  note: string | null;
  /** 该位置对应的源码片段（如调用语句），后端提供时显示在位置下方便于核对。 */
  snippet?: string | null;
}

export interface NodeView {
  id: number;
  kind: string;
  /** 语义节点的类别（目前与 kind 一致）；第一类语义节点等于 kind，语法节点为 null。 */
  category: string | null;
  name: string;
  fqn: string | null;
  ring: number;
  sub_project_id: number | null;
  /** 该节点种类是否有对应视角 —— 决定"单击是否切视角"。 */
  has_own_view: boolean;
  /** 该节点对应的视角 id；单击时一级切到它、二级设为该节点。 */
  own_view: string | null;
  locations: SourceLocation[];
  annotations: string[];
  metrics: { fan_in?: number; fan_out?: number } | null;
}

/** 边上被折叠掉的中间节点（调用链的一环）。 */
export interface ViaNode {
  id: number;
  kind: string;
  name: string;
  /** 本跳的"调用处"（上一跳调用本节点的 CallSite 位置）。起点不携带。 */
  call_site?: SourceLocation | null;
}

export interface EdgeView {
  id: number;
  kind: string;
  from: number;
  to: number;
  /** 是否为可追溯的真实语义依赖（非语义/合成边才为 false）。虚线/实线由 `indirect` 表示直接性。 */
  resolved: boolean;
  confidence: number;
  hops: number | null;
  /**
   * 这条边折叠掉的中间节点（从起点到终点排序）。
   * 折叠视图里语义节点看似直连，实际是"提拉"过的 —— 这里如实记录中间经过的语法节点。
   */
  via?: ViaNode[];
  /** 终点这跳的"调用处"（即 via 最后一跳 → to 的 CallSite 位置）。 */
  to_call_site?: SourceLocation | null;
  /**
   * 是否为**传播得来**的间接边：起点自身并未执行该动作，
   * 而是其调用链下游某处发生过（P8 沿 Calls 复刻）。
   *
   * 例：路由 A 的 handler 调了共享服务，该服务读了配置 K，
   * 则 A 会被标上 `--ReadsConfig--> K`：事实成立，调用链与接触点均可核实；
   * UI 仅以虚线 + 金色「间接」标签区分其"非起点直接动作"，不再降权为待验证假设。
   */
  indirect?: boolean;
  /**
   * 这条链路（起点 → 各中间跳 → 终点）**每个节点**的位置，后端构建视图时内联。
   *
   * 折叠视图的链路是临时提拉的，中间跳按边 id 重查不到，因此由后端一次给全，
   * 前端无需再对每个节点单独请求 `/nodes/{id}/locations`。
   * 为空表示未内联（非折叠视图等），前端回退到原接口。
   */
  node_locations?: NodeLocationEntry[];
}

/** 链路中某节点（起点 / 中间跳 / 终点）的位置，随边一并返回。 */
export interface NodeLocationEntry {
  id: number;
  /** 合成节点：它的"全部出处"并不都属于当前链路，UI 需换一种标注。 */
  synthetic: boolean;
  locations: SourceLocation[];
}

export interface HiddenInfo {
  total: number;
  shown: number;
  by_kind: Record<string, number>;
  note: string;
}

export interface UnresolvedInfo {
  code: string;
  message: string;
  location: string | null;
}

export interface Candidate {
  id: number;
  name: string;
  badge: string | null;
}

export interface ObjectView {
  project_id: number;
  perspective: string;
  layout: LayoutMode;
  center: NodeView;
  rings: NodeView[][];
  edges: EdgeView[];
  hidden: HiddenInfo;
  unresolved: UnresolvedInfo[];
  conclusions: Record<string, unknown>;
}

export interface Cluster {
  key: string;
  label: string;
  count: number;
  members: NodeView[];
}

export interface MatrixView {
  rows: string[];
  cols: string[];
  cells: number[][];
  row_totals: number[];
  col_totals: number[];
}

export interface AggregateView {
  project_id: number;
  perspective: string;
  layout: LayoutMode;
  clusters: Cluster[];
  matrix: MatrixView | null;
  hidden: HiddenInfo;
  unresolved: UnresolvedInfo[];
  conclusions: Record<string, unknown>;
  notice: string | null;
}

export interface EdgeEvidence {
  edge: EdgeView;
  reason: string | null;
  locations: SourceLocation[];
  via: string[];
}

export interface NodeLocations {
  id: number;
  kind: string;
  name: string;
  /** 合成节点：位置必然来自多处共现，UI 必须给列表而不是单点。 */
  synthetic: boolean;
  locations: SourceLocation[];
  reference_count: number;
}
