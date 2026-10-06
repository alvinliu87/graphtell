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
import { RecallPage } from './RecallPage';
import { mountWithRouter } from '@/test/render';

describe('RecallPage', () => {
  it('renders the prompt-augmentation form under a project route', () => {
    const { container } = mountWithRouter(
      <Routes>
        <Route path="/projects/:projectId/recall" element={<RecallPage />} />
      </Routes>,
      '/projects/7/recall',
    );
    expect(container.textContent).toContain('Prompt augmentation');
    expect(container.textContent).toContain('Compose prompt');
  });
});
