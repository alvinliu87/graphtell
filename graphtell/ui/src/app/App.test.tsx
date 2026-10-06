// @vitest-environment jsdom
import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { App } from './App';

/**
 * App smoke test: mount the whole tree (LocaleProvider + antd ConfigProvider +
 * AntdApp + RouterProvider with the real route table) and confirm the site boots
 * and the default route renders instead of crashing. This is the closest thing
 * to a "does the bundle start" check we can run headless.
 */
describe('App', () => {
  it('boots and renders the default route (project list) without crashing', async () => {
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);

    act(() => {
      root.render(<App />);
    });
    await act(async () => {
      await new Promise((r) => setTimeout(r, 80));
    });

    expect(document.body.textContent).toContain('Projects');
    expect(document.body.textContent).toContain('New project');

    act(() => root.unmount());
    container.remove();
  });
});
