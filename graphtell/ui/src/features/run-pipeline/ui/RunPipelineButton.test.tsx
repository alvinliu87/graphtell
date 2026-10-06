// @vitest-environment jsdom
import { act } from 'react';
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', () => ({
  http: {
    get: vi.fn().mockResolvedValue({}),
    post: vi.fn().mockResolvedValue({}),
    put: vi.fn().mockResolvedValue({}),
    del: vi.fn().mockResolvedValue({}),
  },
}));

import { http } from '@/shared/api/http';
import { RunPipelineButton } from './RunPipelineButton';
import { mountWithRouter } from '@/test/render';

describe('RunPipelineButton', () => {
  it('triggers a rebuild and fires onStarted when clicked', async () => {
    const onStarted = vi.fn();
    const { container } = mountWithRouter(<RunPipelineButton projectId={7} onStarted={onStarted} />);
    const btn = container.querySelector('button');
    expect(btn?.textContent).toContain('Rebuild graph');
    await act(async () => {
      btn?.click();
      await new Promise((r) => setTimeout(r, 0));
    });
    expect(http.post).toHaveBeenCalledWith('/api/projects/7/run');
    expect(onStarted).toHaveBeenCalled();
  });
});
