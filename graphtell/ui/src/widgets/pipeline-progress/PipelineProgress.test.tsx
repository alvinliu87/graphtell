// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import type { RunStatus } from '@/entities/pipeline';
import { PipelineProgress } from './PipelineProgress';
import { mountWithRouter } from '@/test/render';

const done: RunStatus = {
  project_id: 1,
  status: 'ok',
  current_phase: 'Resolve',
  phases: [
    { phase: 'Ingest', duration_ms: 1, nodes_created: 5, edges_created: 3, annotations_created: 1, aliases_created: 0, diagnostics: 0 },
    { phase: 'Resolve', duration_ms: 9, nodes_created: 2, edges_created: 1, annotations_created: 0, aliases_created: 0, diagnostics: 0 },
  ],
};

describe('PipelineProgress', () => {
  it('shows the "no run records" state when there is nothing', () => {
    const { container } = mountWithRouter(<PipelineProgress run={null} indexing={false} />);
    expect(container.textContent).toContain('No run records');
  });

  it('renders the completed summary once a run has phases', () => {
    const { container } = mountWithRouter(<PipelineProgress run={done} indexing={false} />);
    expect(container.textContent).toContain('Graphing complete');
    expect(container.textContent).toContain('Nodes');
  });

  it('renders the running stepper while indexing', () => {
    const { container } = mountWithRouter(<PipelineProgress run={done} indexing />);
    expect(container.textContent).toContain('Running');
  });
});
