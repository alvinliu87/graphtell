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

/** 对象类视角：只取"当前这一个对象"的链路子图。 */
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

/** 聚合类视角：不是单链路，而是分组概览 / 矩阵。 */
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
