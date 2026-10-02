import { useCallback } from 'react';
import { useAsync, usePolling } from '@/shared/lib/useAsync';
import { projectApi } from './api';
import type { Project } from './model';

/** Silent polling interval for the list while some project is still indexing / pending. */
const PROJECTS_POLL_MS = 3000;

/**
 * Project list (manual refresh + automatic follow-up while a build runs).
 *
 * The list is a **snapshot**: without polling, the spinner next to "indexing" is just static
 * decoration — it neither tracks progress nor turns into "ready" on its own (you would have to
 * refresh by hand to find out). While a project is building we poll silently so the animation means
 * something; once everything is ready polling stops by itself, leaving no background requests.
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

/** A single project. */
export function useProject(id: number | undefined) {
  const fn = useCallback(() => (id === undefined ? Promise.resolve(null) : projectApi.get(id)), [id]);
  const { data, loading, error, reload } = useAsync(fn, [id]);
  return { project: data, loading, error, reload };
}
