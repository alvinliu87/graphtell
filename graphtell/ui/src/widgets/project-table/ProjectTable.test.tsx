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

import type { Project } from '@/entities/project';
import { ProjectTable } from './ProjectTable';
import { mountWithRouter } from '@/test/render';

const sample: Project = {
  id: 1,
  name: 'CRMEB',
  root_path: '/code/crmeb',
  status: 'ready',
  description: '',
  config: { full_pipeline: true, exclude_globs: [], required_locales: [], table_prefixes: [] },
  created_at: 0,
  updated_at: 0,
};

describe('ProjectTable', () => {
  it('renders project rows and their status tag', () => {
    const { container } = mountWithRouter(
      <ProjectTable projects={[sample]} loading={false} onDeleted={() => {}} />,
    );
    expect(container.textContent).toContain('CRMEB');
    expect(container.textContent).toContain('Ready');
  });

  it('shows the empty state when there are no projects', () => {
    const { container } = mountWithRouter(<ProjectTable projects={[]} loading={false} />);
    expect(container.textContent).toContain('New project');
  });
});
