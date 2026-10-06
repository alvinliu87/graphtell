// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { http } from '@/shared/api/http';
import { renderHook } from '@/test/renderHook';
import { useProject, useProjects } from './hooks';
import type { Project } from './model';

// The `http` mock is module-shared, so clear call history + injected responses between tests.
beforeEach(() => vi.resetAllMocks());

function makeProject(id: number, status: Project['status']): Project {
  return {
    id,
    name: `p${id}`,
    root_path: `/x/${id}`,
    description: null,
    status,
    config: { exclude_globs: [], required_locales: [], table_prefixes: [], full_pipeline: true },
    created_at: 0,
    updated_at: 0,
  };
}

describe('useProject', () => {
  it('fetches the project when an id is given', async () => {
    const project = makeProject(7, 'ready');
    http.get.mockResolvedValue(project);
    const h = renderHook(() => useProject(7));
    expect(h.result.current?.loading).toBe(true);
    await h.flush();
    expect(h.result.current?.project).toEqual(project);
    expect(h.result.current?.loading).toBe(false);
    expect(h.result.current?.error).toBeNull();
    h.unmount();
  });

  it('does not call the API when id is undefined', () => {
    const h = renderHook(() => useProject(undefined));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.project).toBeNull();
    h.unmount();
  });

  it('surfaces fetch errors', async () => {
    http.get.mockRejectedValue(new Error('nope'));
    const h = renderHook(() => useProject(1));
    await h.flush();
    expect(h.result.current?.error).toBe('nope');
    expect(h.result.current?.project).toBeNull();
    h.unmount();
  });
});

describe('useProjects', () => {
  it('reports building while some project is still indexing', async () => {
    http.get.mockResolvedValue([makeProject(1, 'ready'), makeProject(2, 'indexing')]);
    const h = renderHook(() => useProjects());
    await h.flush();
    expect(h.result.current?.projects).toHaveLength(2);
    expect(h.result.current?.building).toBe(true);
    h.unmount();
  });

  it('stops the building flag once everything is ready', async () => {
    http.get.mockResolvedValue([makeProject(1, 'ready')]);
    const h = renderHook(() => useProjects());
    await h.flush();
    expect(h.result.current?.building).toBe(false);
    h.unmount();
  });

  it('fills the project list from the API response', async () => {
    const list = [makeProject(1, 'ready')];
    http.get.mockResolvedValue(list);
    const h = renderHook(() => useProjects());
    await h.flush();
    expect(h.result.current?.projects).toEqual(list);
    h.unmount();
  });
});
