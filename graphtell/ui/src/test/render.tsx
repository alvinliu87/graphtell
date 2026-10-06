// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { App } from 'antd';
import { MemoryRouter } from 'react-router-dom';
import type { ReactElement } from 'react';

export interface MountResult {
  container: HTMLDivElement;
  /** The underlying DOM (includes antd portals mounted on document.body). */
  html: () => string;
  unmount: () => void;
}

/**
 * Mount a component for a smoke test, wrapped in the antd `App` context (so
 * `message`/`Modal` work) and a `MemoryRouter` (so `useNavigate`/`useParams` work).
 */
export function mountWithRouter(ui: ReactElement, route = '/'): MountResult {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root: Root = createRoot(container);
  act(() => {
    root.render(
      <App>
        <MemoryRouter initialEntries={[route]}>{ui}</MemoryRouter>
      </App>,
    );
  });
  return {
    container,
    html: () => document.body.textContent ?? '',
    unmount: () => {
      act(() => root.unmount());
      container.remove();
    },
  };
}
