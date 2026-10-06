// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { http } from '@/shared/api/http';
import { renderHook } from '@/test/renderHook';
import { useAggregateView, useObjectView, usePerspectives } from './hooks';
import type { AggregateView, ObjectView, Perspective } from './model';

// The `http` mock is module-shared, so clear call history + injected responses between tests.
beforeEach(() => vi.resetAllMocks());

const perspective: Perspective = {
  id: 'calls',
  label: 'Calls',
  mode: 'object',
  layout: 'radial',
  depth: 2,
  available: 5,
};

const objectView: ObjectView = {
  root_id: 1,
  perspective: 'calls',
  nodes: [],
  edges: [],
};

const aggregateView: AggregateView = {
  perspective: 'calls',
  buckets: [],
};

describe('usePerspectives', () => {
  it('returns the perspective list', async () => {
    http.get.mockResolvedValue([perspective]);
    const h = renderHook(() => usePerspectives(1));
    await h.flush();
    expect(h.result.current?.perspectives).toEqual([perspective]);
    h.unmount();
  });

  it('returns an empty list (no API call) when projectId is undefined', () => {
    const h = renderHook(() => usePerspectives(undefined));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.perspectives).toEqual([]);
    h.unmount();
  });
});

describe('useObjectView', () => {
  it('fetches the object view once all keys are present', async () => {
    http.get.mockResolvedValue(objectView);
    const h = renderHook(() => useObjectView(1, 'calls', 9, 2));
    await h.flush();
    expect(h.result.current?.view).toEqual(objectView);
    h.unmount();
  });

  it('resolves to null without calling the API when a key is missing', () => {
    const h = renderHook(() => useObjectView(1, undefined, 9, 2));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.view).toBeNull();
    h.unmount();
  });
});

describe('useAggregateView', () => {
  it('fetches the aggregate view when keys are present', async () => {
    http.get.mockResolvedValue(aggregateView);
    const h = renderHook(() => useAggregateView(1, 'calls', 12));
    await h.flush();
    expect(h.result.current?.view).toEqual(aggregateView);
    h.unmount();
  });

  it('resolves to null without calling the API when projectId is undefined', () => {
    const h = renderHook(() => useAggregateView(undefined, 'calls'));
    expect(http.get).not.toHaveBeenCalled();
    expect(h.result.current?.view).toBeNull();
    h.unmount();
  });
});
