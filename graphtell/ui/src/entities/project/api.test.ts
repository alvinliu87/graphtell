import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { projectApi } from './api';

describe('projectApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('list/get/subProjects query the projects endpoints', async () => {
    await projectApi.list();
    expect(http.get).toHaveBeenCalledWith('/api/projects');

    await projectApi.get(2);
    expect(http.get).toHaveBeenCalledWith('/api/projects/2');

    await projectApi.subProjects(2);
    expect(http.get).toHaveBeenCalledWith('/api/projects/2/sub-projects');
  });

  it('create posts the input and update puts a partial', async () => {
    const input = { name: 'CRMEB', root_path: '/code/crmeb' };
    await projectApi.create(input);
    expect(http.post).toHaveBeenCalledWith('/api/projects', input);

    await projectApi.update(2, { name: 'CRMEB2' });
    expect(http.put).toHaveBeenCalledWith('/api/projects/2', { name: 'CRMEB2' });
  });

  it('remove deletes the project', async () => {
    await projectApi.remove(2);
    expect(http.del).toHaveBeenCalledWith('/api/projects/2');
  });
});
