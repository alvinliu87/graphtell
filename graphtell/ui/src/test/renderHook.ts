// Thin `renderHook` used by data-hook unit tests. Mirrors the inline helper in
// `shared/lib/useAsync.test.ts` but is shared so every hook test stays DRY.
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';

export interface RenderHookResult<T> {
  result: { current: T | undefined };
  flush: () => Promise<void>;
  unmount: () => void;
}

/** Render a hook factory, returning a live `result.current` plus `flush`/`unmount`. */
export function renderHook<T>(factory: () => T): RenderHookResult<T> {
  const result = { current: undefined as T | undefined };
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
