// @vitest-environment jsdom
import { describe, expect, it } from 'vitest';

import { StatCard } from './StatCard';
import { mountWithRouter } from '@/test/render';

describe('StatCard', () => {
  it('renders the title and value', () => {
    const { container } = mountWithRouter(<StatCard title="Total" value={42} />);
    expect(container.textContent).toContain('Total');
    expect(container.textContent).toContain('42');
  });

  it('renders an icon and suffix when provided', () => {
    const { container } = mountWithRouter(
      <StatCard title="Ready" value={3} suffix=" done" icon={<span>★</span>} />,
    );
    expect(container.textContent).toContain('done');
    expect(container.textContent).toContain('★');
  });
});
