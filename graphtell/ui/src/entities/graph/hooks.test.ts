// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { http } from '@/shared/api/http';
import { renderHook } from '@/test/renderHook';
import { useGraphStats, useNodeDetail, useNodes, useSubgraph } from './hooks';
import type { Annotation, Edge, Node } from './model';

// The `http` mock is module-shared, so clear call history + injected responses between tests.
beforeEach(() => vi.resetAllMocks());

function makeNode(id: number): Node {
  return {
    id,
    project_id: 1,
    sub_project_id: null,
    kind: 'Class',
    name: `Node${id}`,
    fqn: null,
    identity: null,
    file_id: null,
    start_line: 1,
    end_line: 2,
    start_byte: 0,
    end_byte: 10,
    language: 'php',
    phase: 'syntax',
    confidence: 1,
    properties: null,
  };
}

const edge: Edge = {
  id: 1,
  project_id: 1,
  kind: 'Calls',
  from_id: 1,
  to_id: 2,
  phase: 'syntax',
  confidence: 1,
  properties: null,
};

const annotation: Annotation = {
  id: 1,
  node_id: 1,
  channel: 'c',
  kind: 'k',
  subkind: null,
  confidence: 1,
  evidence: null,
  phase: 'syntax',
};

const stats = { nodes: 3, edges: 4, annotations: 1, by_kind: { Class: 3 } };

describe('useGraphStats', () => {
  it('fetches stats when a projectId is given', async () => {
    http.get.mockResolvedValue(stats);
    const h = renderHook(() => useGraphStats(1));
    await h.flush();
    expect(h.result.current?.stats).toEqual(stats);
    h.unmount();
  });

  it('does not call the API when projectId is undefined', () => {
    const h = renderHook(() => useGraphStats(undefined));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.stats).toBeNull();
    h.unmount();
  });
});

describe('useNodes', () => {
  it('returns nodes for the given query', async () => {
    const nodes = [makeNode(1), makeNode(2)];
    http.get.mockResolvedValue(nodes);
    const h = renderHook(() => useNodes(1, { kind: 'Class', limit: 10 }));
    await h.flush();
    expect(h.result.current?.nodes).toEqual(nodes);
    h.unmount();
  });

  it('returns an empty list (no API call) when projectId is undefined', () => {
    const h = renderHook(() => useNodes(undefined, {}));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.nodes).toEqual([]);
    h.unmount();
  });
});

describe('useSubgraph', () => {
  it('fetches nodes + edges for a nodeId', async () => {
    http.get.mockResolvedValue({ nodes: [makeNode(1)], edges: [edge] });
    const h = renderHook(() => useSubgraph(1, 2, 260));
    await h.flush();
    expect(h.result.current?.nodes).toHaveLength(1);
    expect(h.result.current?.edges).toHaveLength(1);
    h.unmount();
  });

  it('returns empty (no API call) when nodeId is undefined', () => {
    const h = renderHook(() => useSubgraph(undefined));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.nodes).toEqual([]);
    expect(h.result.current?.edges).toEqual([]);
    h.unmount();
  });
});

describe('useNodeDetail', () => {
  it('fetches node + neighbors + annotations together', async () => {
    http.get.mockImplementation((url: string) => {
      if (url.includes('/neighbors')) return Promise.resolve([edge]);
      if (url.includes('/annotations')) return Promise.resolve([annotation]);
      return Promise.resolve(makeNode(1));
    });
    const h = renderHook(() => useNodeDetail(1));
    await h.flush();
    expect(h.result.current?.node).toEqual(makeNode(1));
    expect(h.result.current?.neighbors).toEqual([edge]);
    expect(h.result.current?.annotations).toEqual([annotation]);
    h.unmount();
  });

  it('returns empty shapes (no API call) when nodeId is undefined', () => {
    const h = renderHook(() => useNodeDetail(undefined));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.node).toBeNull();
    expect(h.result.current?.neighbors).toEqual([]);
    expect(h.result.current?.annotations).toEqual([]);
    h.unmount();
  });
});
