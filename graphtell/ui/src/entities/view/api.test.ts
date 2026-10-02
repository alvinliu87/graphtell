import { describe, it, expect, vi, beforeEach } from 'vitest';
import { viewApi } from './api';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
const m = http as unknown as { get: ReturnType<typeof vi.fn> };

beforeEach(() => m.get.mockReset());

describe('viewApi request paths', () => {
  it('perspectives', async () => {
    m.get.mockResolvedValue([]);
    await viewApi.perspectives(1);
    expect(m.get).toHaveBeenCalledWith('/api/projects/1/perspectives');
  });

  it('candidates', async () => {
    m.get.mockResolvedValue([]);
    await viewApi.candidates(1, 'table', 100);
    expect(m.get).toHaveBeenCalledWith('/api/projects/1/view/table/candidates?limit=100');
  });

  it('object', async () => {
    m.get.mockResolvedValue(null);
    await viewApi.object(1, 'route', 42, 3);
    expect(m.get).toHaveBeenCalledWith('/api/projects/1/view/route?node=42&depth=3');
  });

  it('aggregate', async () => {
    m.get.mockResolvedValue(null);
    await viewApi.aggregate(1, 'platform');
    expect(m.get).toHaveBeenCalledWith('/api/projects/1/aggregate/platform?limit=12');
  });

  it('nodeLocations', async () => {
    m.get.mockResolvedValue(null);
    await viewApi.nodeLocations(5);
    expect(m.get).toHaveBeenCalledWith('/api/nodes/5/locations');
  });

  it('edgeEvidence', async () => {
    m.get.mockResolvedValue(null);
    await viewApi.edgeEvidence(8);
    expect(m.get).toHaveBeenCalledWith('/api/edges/8/evidence');
  });
});
