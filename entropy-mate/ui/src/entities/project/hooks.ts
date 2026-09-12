import { useCallback } from 'react';
import { useAsync } from '@/shared/lib/useAsync';
import { projectApi } from './api';
import type { Project } from './model';

/** 工程列表（含手动刷新）。 */
export function useProjects() {
  const { data, loading, error, reload } = useAsync<Project[]>(() => projectApi.list(), []);
  return { projects: data ?? [], loading, error, reload };
}

/** 单个工程。 */
export function useProject(id: number | undefined) {
  const fn = useCallback(() => (id === undefined ? Promise.resolve(null) : projectApi.get(id)), [id]);
  const { data, loading, error, reload } = useAsync(fn, [id]);
  return { project: data, loading, error, reload };
}
