import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { fsApi } from './api';

describe('fsApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('browse URL-encodes the path', async () => {
    await fsApi.browse('/code/sample app');
    expect(http.get).toHaveBeenCalledWith('/api/fs/browse?path=' + encodeURIComponent('/code/sample app'));
  });
});
