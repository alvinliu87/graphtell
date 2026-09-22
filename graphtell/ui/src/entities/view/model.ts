/** 视图实体：与后端 `gt-domain::model::view` 对齐。 */

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
  /** 节点所属「端」：`frontend` / `backend`（由 FKB 标注的 `side`）。用于 UI 区分前后端子工程。 */
  side?: string | null;
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
   * 同一位置（接触点 → 终点）上**同时成立**的其它访问方式。
   *
   * 一个使用者对同一资源只画一条边（写 > 读择优），被压掉的那条事实记在这里：
   * `kind: 'WritesDb'` + `also_kinds: ['ReadsDb']` 表示"这处既读又写"，
   * 前端据此显示「读写库」，而不是只报读或只报写。
   */
  also_kinds?: string[];
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

/**
 * 无法归因到任何语义入口的**直接**访问（孤儿访问）。
 *
 * 访问方是语法节点且沿调用链上溯找不到任何语义发起者（路由 / 契约 / 定时任务）时，
 * 它既画不成语义用户、也不会出现在任何提拉边的 `via` 里。
 *
 * 处理是**降级而非省略**：不占画布（语法节点信息量低、会挤掉语义节点的额度），
 * 但如实记账并给出接触点位置 —— 静默省略会让「语义入边 N」与空白画布自相矛盾。
 */
export interface OrphanAccess {
  id: number;
  /** 访问方的节点种类（通常是 `Method` / `Function`）。 */
  kind: string;
  name: string;
  /** 它对中心资源做的事（`ReadsDb` / `WritesCache`…）。 */
  edge_kind: string;
  location: SourceLocation | null;
  /**
   * 可选：当这次"直连访问"本身是**一条可点击展开的语义边**时（如事件视角的 `Triggers`
   * 触发点），带上折叠后的边视图（含 `via` 调用链）。前端据此打开边证据链抽屉，
   * 而不是只打开节点详情。语义节点（消费者）不走这里——它们已升为可见节点画在画布上。
   */
  edge?: EdgeView | null;
}

export interface ObjectView {
  project_id: number;
  perspective: string;
  layout: LayoutMode;
  center: NodeView;
  rings: NodeView[][];
  edges: EdgeView[];
  hidden: HiddenInfo;
  /** 无语义入口的直接访问（孤儿）：不画在画布上，但必须记账、可逐条核对。 */
  orphans?: OrphanAccess[];
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
