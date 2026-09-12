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
}

export interface NodeView {
  id: number;
  kind: string;
  name: string;
  fqn: string | null;
  ring: number;
  sub_project_id: number | null;
  /** 该节点种类是否有对应视角 —— 决定"单击是否切视角"。 */
  has_own_view: boolean;
  locations: SourceLocation[];
  annotations: string[];
  metrics: { fan_in?: number; fan_out?: number } | null;
}

export interface EdgeView {
  id: number;
  kind: string;
  from: number;
  to: number;
  /** 实线 = 已解析；虚线 = 待验证假设。 */
  resolved: boolean;
  confidence: number;
  hops: number | null;
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
  candidates: Candidate[];
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
