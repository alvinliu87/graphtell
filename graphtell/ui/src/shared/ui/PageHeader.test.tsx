// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { PageHeader } from './PageHeader';
import { mountWithRouter } from '@/test/render';

describe('PageHeader', () => {
  it('renders the title and optional subtitle', () => {
    const { container } = mountWithRouter(<PageHeader title="Code Graph" subtitle="sub text" />);
    expect(container.textContent).toContain('Code Graph');
    expect(container.textContent).toContain('sub text');
  });

  it('renders extra actions and supports compact mode without crashing', () => {
    const { container } = mountWithRouter(
      <PageHeader title="T" extra={<button>act</button>} compact />,
    );
    expect(container.querySelector('button')?.textContent).toBe('act');
  });
});
