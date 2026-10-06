import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { recallApi } from './api';

describe('recallApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('recall posts the query to the project recall endpoint', async () => {
    await recallApi.recall(7, { query: 'order pay', limit: 10 });
    expect(http.post).toHaveBeenCalledWith('/api/projects/7/recall', { query: 'order pay', limit: 10 });
  });

  it('compose posts to the prompt endpoint', async () => {
    const body = { query: 'coupon code', intent: 'find discount logic', with_snippets: true };
    await recallApi.compose(7, body);
    expect(http.post).toHaveBeenCalledWith('/api/projects/7/prompt', body);
  });
});
