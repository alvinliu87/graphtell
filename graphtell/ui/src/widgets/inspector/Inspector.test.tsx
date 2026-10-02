// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { EdgeView, SourceLocation } from '@/entities/view';

// The panel queries evidence by edge id as soon as it opens. In a unit test there's no backend, so let it "not find" --
// which is exactly the situation for a synthetic edge (negative id) in production: evidence can only come from the edge's own inlined `to_call_site`.
vi.mock('@/entities/view', () => ({
  viewApi: {
    edgeEvidence: async () => null,
    nodeLocations: async () => null,
  },
}));

import { Inspector } from './Inspector';

declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const at = (line: number, note: string, snippet: string): SourceLocation => ({
  file: 'crmeb/app/services/product/product/StoreProductServices.php',
  line,
  symbol: null,
  note,
  snippet,
});

/**
 * Direct semantic edge: `save --publishes to--> queue`.
 *
 * It has no folded-away middle nodes (`via` empty), but on the graph it is still one hop "method → semantic node".
 * Because the chain was once drawn only when `via` was non-empty, such an edge left just one isolated location in the drawer: you couldn't see the source `save`,
 * nor the target semantic node, so it looked like "this edge wasn't built properly".
 */
const direct: EdgeView = {
  id: -3,
  kind: 'PublishesTo',
  from: 7,
  to: 9,
  resolved: true,
  confidence: 0.8,
  hops: null,
  via: [],
  to_call_site: at(800, 'where this chain accesses the resource', "ProductCopyJob::dispatch('copySliderImage', [$res->id]);"),
  node_locations: [
    { id: 7, synthetic: false, locations: [at(742, 'Definition', 'public function save()')] },
    { id: 9, synthetic: true, locations: [at(800, 'Definition', "ProductCopyJob::dispatch('copySliderImage', [$res->id]);")] },
  ],
};

const nameOf = (id: number) =>
  id === 7 ? 'save' : id === 9 ? 'store_product_services' : `#${id}`;

describe('Inspector edge panel', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    globalThis.IS_REACT_ACT_ENVIRONMENT = true;
    // antd's Drawer uses `matchMedia` via `useBreakpoint`, which jsdom doesn't implement.
    if (!window.matchMedia) {
      window.matchMedia = ((query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addListener: () => {},
        removeListener: () => {},
        addEventListener: () => {},
        removeEventListener: () => {},
        dispatchEvent: () => false,
      })) as unknown as typeof window.matchMedia;
    }
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('a direct edge also draws a chain as “source → target”, with the same layout as the route chain (no duplicate evidence-location listing)', async () => {
    await act(async () => {
      root.render(
        <Inspector nodeId={null} edgeId={direct.id} edgeView={direct} nodeNameOf={nameOf} onClose={() => {}} />,
      );
    });

    // The Drawer goes through a portal, so its content is mounted on body.
    const text = document.body.textContent ?? '';
    expect(text).toContain('start');
    expect(text).toContain('end');
    expect(text).toContain('save');
    expect(text).toContain('store_product_services');
    // The middle line = the edge's `to_call_site` (where this chain accesses the resource).
    expect(text).toContain('StoreProductServices.php:800');
    // With no folded-away middle nodes, the heading must not claim "collapsed".
    expect(text).not.toContain('Collapsed call chain');
    expect(text).toContain('Call chain');
    // The chain already gives this edge's own line, so a separate "evidence location" section shouldn't be listed.
    expect(text).not.toContain('Evidence location');
  });
});
