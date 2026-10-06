// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import type { Candidate, Perspective } from '@/entities/view';
import { PerspectivePicker } from './PerspectivePicker';
import { mountWithRouter } from '@/test/render';

const perspectives: Perspective[] = [
  { id: 'route', label: 'Routes', mode: 'object', layout: 'radial', depth: 2, description: 'HTTP routes', available: 12 },
  { id: 'schedule', label: 'Scheduled', mode: 'aggregate', layout: 'matrix', depth: 1, description: 'cron', available: 3 },
];

const candidates: Candidate[] = [{ id: 5, name: 'placeOrder', badge: 'POST' }];

describe('PerspectivePicker', () => {
  it('renders the perspective dropdown and the object dropdown for object perspectives', () => {
    const { container } = mountWithRouter(
      <PerspectivePicker
        perspectives={perspectives}
        perspective="route"
        onPerspectiveChange={() => {}}
        candidates={candidates}
        node={5}
        onNodeChange={() => {}}
        onSearch={() => {}}
        nodeName="placeOrder"
        trail={[]}
        onTrailClick={() => {}}
      />,
    );
    expect(container.textContent).toContain('Routes');
    expect(container.textContent).toContain('placeOrder');
  });

  it('shows the aggregate hint instead of an object dropdown for aggregate perspectives', () => {
    const { container } = mountWithRouter(
      <PerspectivePicker
        perspectives={perspectives}
        perspective="schedule"
        onPerspectiveChange={() => {}}
        candidates={[]}
        node={null}
        onNodeChange={() => {}}
        onSearch={() => {}}
        trail={[]}
        onTrailClick={() => {}}
      />,
    );
    expect(container.textContent).toContain('Aggregate perspective has no single object');
  });
});
