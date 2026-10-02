import { useCallback } from 'react';
import { useAsync } from '@/shared/lib/useAsync';
import { viewApi } from './api';
import type { AggregateView, ObjectView, Perspective } from './model';

export function usePerspectives(projectId: number | undefined) {
  const fn = useCallback(
    () => (projectId === undefined ? Promise.resolve<Perspective[]>([]) : viewApi.perspectives(projectId)),
    [projectId],
  );
  const { data, loading, error } = useAsync(fn, [projectId]);
  return { perspectives: data ?? [], loading, error };
}

/**
 * Object perspective: fetch the link subgraph of "just this one object".
 *
 * Always a folded view: syntax nodes are folded into each edge's `via` chain with the call site of every hop, so a single click on an edge verifies it hop by hop.
 */
export function useObjectView(
  projectId: number | undefined,
  perspective: string | undefined,
  nodeId: number | undefined,
  depth: number,
) {
  const fn = useCallback(
    () =>
      projectId === undefined || perspective === undefined || nodeId === undefined
        ? Promise.resolve<ObjectView | null>(null)
        : viewApi.object(projectId, perspective, nodeId, depth),
    [projectId, perspective, nodeId, depth],
  );
  const { data, loading, error } = useAsync(fn, [projectId, perspective, nodeId, depth]);
  return { view: data, loading, error };
}

/** Aggregate perspective: not a single link but a grouped overview / matrix. */
export function useAggregateView(
  projectId: number | undefined,
  perspective: string | undefined,
  limit = 12,
) {
  const fn = useCallback(
    () =>
      projectId === undefined || perspective === undefined
        ? Promise.resolve<AggregateView | null>(null)
        : viewApi.aggregate(projectId, perspective, limit),
    [projectId, perspective, limit],
  );
  const { data, loading, error } = useAsync(fn, [projectId, perspective, limit]);
  return { view: data, loading, error };
}
