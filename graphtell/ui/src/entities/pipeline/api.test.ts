import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { pipelineApi } from './api';

describe('pipelineApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('run posts to the project run endpoint', async () => {
    await pipelineApi.run(4);
    expect(http.post).toHaveBeenCalledWith('/api/projects/4/run');
  });

  it('status and health hit their endpoints', async () => {
    await pipelineApi.status(4);
    expect(http.get).toHaveBeenCalledWith('/api/projects/4/run/status');

    await pipelineApi.health();
    expect(http.get).toHaveBeenCalledWith('/api/health');
  });
});
