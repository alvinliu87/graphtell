// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: {
    get: vi.fn().mockResolvedValue({}),
    post: vi.fn().mockResolvedValue({}),
    put: vi.fn().mockResolvedValue({}),
    del: vi.fn().mockResolvedValue({}),
  },
}));

import { Routes, Route } from 'react-router-dom';
import { CreateProjectModal } from './CreateProjectModal';
import { mountWithRouter } from '@/test/render';

describe('CreateProjectModal', () => {
  it('renders the form when open', () => {
    const { container } = mountWithRouter(
      <Routes>
        <Route path="/projects" element={<CreateProjectModal open onClose={() => {}} />} />
      </Routes>,
      '/projects',
    );
    // Form labels live in the (ported) Modal body on document.body.
    expect(document.body.textContent).toContain('Project name');
    expect(document.body.textContent).toContain('Codebase root');
  });
});
