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

import { act } from 'react';
import { DeleteProjectButton } from './DeleteProjectButton';
import { mountWithRouter } from '@/test/render';

describe('DeleteProjectButton', () => {
  it('renders a danger button carrying a confirm prompt with the project name', () => {
    const { container } = mountWithRouter(
      <DeleteProjectButton projectId={3} name="SampleProject" onDeleted={() => {}} />,
    );
    // The Popconfirm question only mounts once the button is clicked.
    const btn = container.querySelector('button');
    act(() => {
      btn?.click();
    });
    expect(document.body.textContent).toContain('SampleProject');
  });
});
