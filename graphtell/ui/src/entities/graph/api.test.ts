import { describe, it, expect, vi, beforeEach } from 'vitest';
import { graphApi } from './api';

vi.mock('@/shared/api/http', () => ({
  http: { get: vi.fn(), post: vi.fn(), put: vi.fn(), del: vi.fn() },
}));

import { http } from '@/shared/api/http';
const m = http as unknown as {
  get: ReturnType<typeof vi.fn>;
  post: ReturnType<typeof vi.fn>;
  put: ReturnType<typeof vi.fn>;
  del: ReturnType<typeof vi.fn>;
};

beforeEach(() => {
  m.get.mockReset();
  m.post.mockReset();
  m.put.mockReset();
  m.del.mockReset();
});

describe('graphApi request paths', () => {
  it('stats', async () => {
    m.get.mockResolvedValue({ nodes: 0, edges: 0, annotations: 0, by_kind: {} });
    await graphApi.stats(7);
    expect(m.get).toHaveBeenCalledWith('/api/projects/7/stats');
  });

  it('nodes passes the query string through', async () => {
    m.get.mockResolvedValue([]);
    await graphApi.nodes(7, { kind: 'Class', limit: 50, offset: 10 });
    expect(m.get).toHaveBeenCalledWith('/api/projects/7/nodes?kind=Class&limit=50&offset=10');
  });

  it('nodes drops the question mark for an empty query', async () => {
    m.get.mockResolvedValue([]);
    await graphApi.nodes(7);
    expect(m.get).toHaveBeenCalledWith('/api/projects/7/nodes');
  });

  it('neighbors carries the direction', async () => {
    m.get.mockResolvedValue([]);
    await graphApi.neighbors(3, 'in');
    expect(m.get).toHaveBeenCalledWith('/api/nodes/3/neighbors?direction=in');
  });

  it('subgraph maps parameter names', async () => {
    m.get.mockResolvedValue({ nodes: [], edges: [] });
    await graphApi.subgraph(3, 3, 200);
    expect(m.get).toHaveBeenCalledWith('/api/nodes/3/subgraph?depth=3&max_nodes=200');
  });

  it('symbols passes project_id through', async () => {
    m.get.mockResolvedValue([]);
    await graphApi.symbols(9, 'schema');
    expect(m.get).toHaveBeenCalledWith('/api/symbols/schema?project_id=9');
  });

  it('diagnostics', async () => {
    m.get.mockResolvedValue([]);
    await graphApi.diagnostics(7);
    expect(m.get).toHaveBeenCalledWith('/api/projects/7/diagnostics');
  });
});
