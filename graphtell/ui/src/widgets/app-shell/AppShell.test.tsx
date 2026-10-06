// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: {
    get: vi.fn((url: string) => (url.split('?')[0] === '/api/projects' ? [] : {})),
    post: vi.fn().mockResolvedValue({}),
    put: vi.fn().mockResolvedValue({}),
    del: vi.fn().mockResolvedValue({}),
  },
}));

import { Routes, Route } from 'react-router-dom';
import { AppShell } from './AppShell';
import { mountWithRouter } from '@/test/render';

describe('AppShell', () => {
  it('renders the brand and a project-scoped sidebar under a project route', () => {
    const { container } = mountWithRouter(
      <Routes>
        <Route path="/projects/:projectId/graph" element={<AppShell />} />
      </Routes>,
      '/projects/1/graph',
    );
    expect(container.textContent).toContain('GraphTell');
    // Sidebar items (only show under a project route).
    expect(container.textContent).toContain('Code Graph');
    expect(container.textContent).toContain('Prompt augmentation');
  });
});
