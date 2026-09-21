import { http } from '@/shared/api/http';
import type { Annotation, Diagnostic, DiagnosticSummary, Edge, GraphStats, Node, SymbolEntry } from './model';

export interface NodeQuery {
  kind?: string;
  name?: string;
  limit?: number;
  offset?: number;
}

const qs = (params: Record<string, string | number | undefined>) => {
  const usp = new URLSearchParams();
  Object.entries(params).forEach(([k, v]) => {
    if (v !== undefined && v !== '') usp.set(k, String(v));
  });
  const s = usp.toString();
  return s ? `?${s}` : '';
};

/** 图实体的数据访问。 */
export const graphApi = {
  stats: (projectId: number) => http.get<GraphStats>(`/api/projects/${projectId}/stats`),
  nodes: (projectId: number, q: NodeQuery = {}) =>
    http.get<Node[]>(`/api/projects/${projectId}/nodes${qs({ ...q })}`),
  node: (id: number) => http.get<Node | null>(`/api/nodes/${id}`),
  neighbors: (id: number, direction: 'in' | 'out' | 'both' = 'both') =>
    http.get<Edge[]>(`/api/nodes/${id}/neighbors${qs({ direction })}`),
  annotations: (id: number) => http.get<Annotation[]>(`/api/nodes/${id}/annotations`),
  subgraph: (id: number, depth = 2, maxNodes = 300) =>
    http.get<{ nodes: Node[]; edges: Edge[] }>(`/api/nodes/${id}/subgraph${qs({ depth, max_nodes: maxNodes })}`),
  symbols: (projectId: number, table: string) =>
    http.get<SymbolEntry[]>(`/api/symbols/${table}${qs({ project_id: projectId })}`),
  diagnostics: (projectId: number) => http.get<Diagnostic[]>(`/api/projects/${projectId}/diagnostics`),
  /** 非规则诊断的严重度汇总（菜单角标用，排除 `rule:` 前缀）。 */
  diagnosticsSummary: (projectId: number) =>
    http.get<DiagnosticSummary>(`/api/projects/${projectId}/diagnostics/summary`),
};
