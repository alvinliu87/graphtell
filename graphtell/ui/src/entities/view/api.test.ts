import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
import { viewApi } from './api';

describe('viewApi', () => {
  beforeEach(() => vi.clearAllMocks());

  it('perspectives, nodeLocations and edgeEvidence hit their endpoints', async () => {
    await viewApi.perspectives(5);
    expect(http.get).toHaveBeenCalledWith('/api/projects/5/perspectives');

    await viewApi.nodeLocations(99);
    expect(http.get).toHaveBeenCalledWith('/api/nodes/99/locations');

    await viewApi.edgeEvidence(99);
    expect(http.get).toHaveBeenCalledWith('/api/edges/99/evidence');
  });

  it('candidates builds limit, name_contains and sub_project_id params', async () => {
    await viewApi.candidates(5, 'order', 50, 'pay', 2);
    expect(http.get).toHaveBeenCalledWith(
      '/api/projects/5/view/order/candidates?limit=50&name_contains=pay&sub_project_id=2',
    );
  });

  it('candidates omits empty optional params', async () => {
    await viewApi.candidates(5, 'order');
    expect(http.get).toHaveBeenCalledWith('/api/projects/5/view/order/candidates?limit=300');
  });

  it('object and aggregate pass through depth / limit', async () => {
    await viewApi.object(5, 'backend', 11, 3);
    expect(http.get).toHaveBeenCalledWith('/api/projects/5/view/backend?node=11&depth=3');

    await viewApi.aggregate(5, 'backend');
    expect(http.get).toHaveBeenCalledWith('/api/projects/5/aggregate/backend?limit=12');
  });
});
