import { useCallback } from 'react';
import { useAsync } from '@/shared/lib/useAsync';
import { graphApi } from './api';
import type { NodeQuery } from './api';
import type { Annotation, Edge, GraphStats, Node } from './model';

export function useGraphStats(projectId: number | undefined) {
  const fn = useCallback(
    () => (projectId === undefined ? Promise.resolve(null) : graphApi.stats(projectId)),
    [projectId],
  );
  const { data, loading, error, reload } = useAsync(fn, [projectId]);
  return { stats: data, loading, error, reload };
}

export function useNodes(projectId: number | undefined, query: NodeQuery) {
  const key = JSON.stringify(query);
  const fn = useCallback(
    () => (projectId === undefined ? Promise.resolve<Node[]>([]) : graphApi.nodes(projectId, query)),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [projectId, key],
  );
  const { data, loading, error, reload } = useAsync(fn, [projectId, key]);
  return { nodes: data ?? [], loading, error, reload };
}

export function useSubgraph(nodeId: number | undefined, depth = 2, maxNodes = 260) {
  const fn = useCallback(
    () =>
      nodeId === undefined
        ? Promise.resolve<{ nodes: Node[]; edges: Edge[] }>({ nodes: [], edges: [] })
        : graphApi.subgraph(nodeId, depth, maxNodes),
    [nodeId, depth, maxNodes],
  );
  const { data, loading, error } = useAsync(fn, [nodeId, depth, maxNodes]);
  return { nodes: data?.nodes ?? [], edges: data?.edges ?? [], loading, error };
}

export function useNodeDetail(nodeId: number | undefined) {
  const fn = useCallback(
    () =>
      nodeId === undefined
        ? Promise.resolve<{ node: Node | null; neighbors: Edge[]; annotations: Annotation[] }>({
            node: null,
            neighbors: [],
            annotations: [],
          })
        : Promise.all([
            graphApi.node(nodeId),
            graphApi.neighbors(nodeId, 'both'),
            graphApi.annotations(nodeId),
          ]).then(([node, neighbors, annotations]) => ({ node, neighbors, annotations })),
    [nodeId],
  );
  const { data, loading, error } = useAsync(fn, [nodeId]);
  return {
    node: data?.node ?? null,
    neighbors: data?.neighbors ?? [],
    annotations: data?.annotations ?? [],
    loading,
    error,
  };
}
