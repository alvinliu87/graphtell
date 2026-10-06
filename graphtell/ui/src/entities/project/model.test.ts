import { describe, expect, it } from 'vitest';

import { STATUS_META, type ProjectStatus } from './model';

describe('project model — status metadata', () => {
  it('has a label and colour for every project status', () => {
    const statuses: ProjectStatus[] = ['created', 'indexing', 'ready', 'failed'];
    for (const s of statuses) {
      expect(STATUS_META[s].label).toBeTruthy();
      expect(STATUS_META[s].color).toBeTruthy();
    }
  });

  it('uses semantic antd colours that read as a lifecycle', () => {
    expect(STATUS_META.ready.color).toBe('success');
    expect(STATUS_META.failed.color).toBe('error');
    expect(STATUS_META.indexing.color).toBe('processing');
  });
});
