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

import { ProjectsPage } from './ProjectsPage';
import { mountWithRouter } from '@/test/render';

describe('ProjectsPage', () => {
  it('renders the header and the empty state when there are no projects', () => {
    const { container } = mountWithRouter(<ProjectsPage />);
    expect(container.textContent).toContain('Projects');
    expect(container.textContent).toContain('New project');
  });
});
