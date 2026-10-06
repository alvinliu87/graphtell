// @vitest-environment jsdom
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { beforeEach, describe, expect, it, vi } from 'vitest';

import { useAsync, usePolling } from './useAsync';

declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

beforeEach(() => {
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
});


function renderHook<T>(factory: () => T) {
  const result: { current: T | undefined } = { current: undefined };
  const Wrapper = () => {
    result.current = factory();
    return null;
  };
  const container = document.createElement('div');
  const root: Root = createRoot(container);
  act(() => root.render(createElement(Wrapper)));
  return {
    result,
    flush: async () => {
      await act(async () => {
        await new Promise((r) => setTimeout(r, 10));
      });
    },
    unmount: () => act(() => root.unmount()),
  };
}

describe('useAsync', () => {
  it('exposes data once the async fn resolves', async () => {
    const h = renderHook(() => useAsync(async () => 123, []));
    expect(h.result.current?.loading).toBe(true);
    await h.flush();
    expect(h.result.current?.data).toBe(123);
    expect(h.result.current?.loading).toBe(false);
    expect(h.result.current?.error).toBeNull();
    h.unmount();
  });

  it('captures the message when the async fn rejects', async () => {
    const h = renderHook(() => useAsync(async () => Promise.reject(new Error('boom')), []));
    await h.flush();
    expect(h.result.current?.data).toBeNull();
    expect(h.result.current?.error).toBe('boom');
    h.unmount();
  });

  it('re-runs on reload', async () => {
    const fn = vi.fn().mockResolvedValue('a');
    const h = renderHook(() => useAsync(fn, []));
    await h.flush();
    expect(fn).toHaveBeenCalledTimes(1);
    act(() => h.result.current?.reload());
    await h.flush();
    expect(fn).toHaveBeenCalledTimes(2);
    h.unmount();
  });
});


