import { http } from '@/shared/api/http';
import type {
  AggregateView,
  Candidate,
  EdgeEvidence,
  NodeLocations,
  ObjectView,
  Perspective,
} from './model';

const qs = (params: Record<string, string | number | boolean | undefined>) => {
  const usp = new URLSearchParams();
  Object.entries(params).forEach(([k, v]) => {
    if (v !== undefined && v !== '') usp.set(k, String(v));
  });
  const s = usp.toString();
  return s ? `?${s}` : '';
};

/** Data access for view entities. */
export const viewApi = {
  perspectives: (projectId: number) =>
    http.get<Perspective[]>(`/api/projects/${projectId}/perspectives`),
  candidates: (projectId: number, perspective: string, limit = 300, search?: string, subProjectId?: number) =>
    http.get<Candidate[]>(
      `/api/projects/${projectId}/view/${perspective}/candidates${qs({ limit, name_contains: search, sub_project_id: subProjectId })}`,
    ),
  object: (projectId: number, perspective: string, node: number, depth?: number) =>
    http.get<ObjectView>(
      `/api/projects/${projectId}/view/${perspective}${qs({ node, depth })}`,
    ),
  aggregate: (projectId: number, perspective: string, limit = 12) =>
    http.get<AggregateView>(
      `/api/projects/${projectId}/aggregate/${perspective}${qs({ limit })}`,
    ),
  nodeLocations: (nodeId: number) => http.get<NodeLocations>(`/api/nodes/${nodeId}/locations`),
  edgeEvidence: (edgeId: number) => http.get<EdgeEvidence | null>(`/api/edges/${edgeId}/evidence`),
};
