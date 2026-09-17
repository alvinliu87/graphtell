import { useCallback } from 'react';
import { useAsync, usePolling } from '@/shared/lib/useAsync';
import { projectApi } from './api';
import type { Project } from './model';

/** 有工程还在建图 / 待建图时，列表的静默轮询间隔。 */
const PROJECTS_POLL_MS = 3000;

/**
 * 工程列表（含手动刷新 + 建图期间自动跟进）。
 *
 * 列表本身是**快照**：不做轮询的话，「建图中」的转圈只是一个静态装饰 —— 既不跟进进度，
 * 也不会在完成后自动变成「就绪」（得手动刷新才知道）。有工程在建图时静默轮询，
 * 让动效名副其实；全部就绪后轮询自动停止，不留后台请求。
 */
export function useProjects() {
  const { data, loading, error, reload, silentReload } = useAsync<Project[]>(
    () => projectApi.list(),
    [],
  );
  const projects = data ?? [];
  const building = projects.some((p) => p.status === 'indexing' || p.status === 'created');
  usePolling(silentReload, PROJECTS_POLL_MS, building);
  return { projects, loading, error, reload, building };
}

/** 单个工程。 */
export function useProject(id: number | undefined) {
  const fn = useCallback(() => (id === undefined ? Promise.resolve(null) : projectApi.get(id)), [id]);
  const { data, loading, error, reload } = useAsync(fn, [id]);
  return { project: data, loading, error, reload };
}
