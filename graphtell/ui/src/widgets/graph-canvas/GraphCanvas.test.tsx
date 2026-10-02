// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { EdgeView } from '@/entities/view';
import { GraphCanvas, type CanvasNode } from './GraphCanvas';

/**
 * Regression: Hook order must be stable.
 *
 * Once `useMemo` was placed **after** `if (loading) return …`, so:
 * the first render (loading) ran only 6 Hooks, and the re-render after data arrived ran 1 more ->
 * React threw "Rendered more hooks than during the previous render" and blanked the screen.
 *
 * This case simulates the real "loading -> has data" update path; any change in Hook count gets caught.
 */

declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const center: CanvasNode = { id: 1, kind: 'HttpContract', name: 'POST /apple_login', ring: 0 };
const rings: CanvasNode[][] = [
  [
    { id: 2, kind: 'Table', name: 'wechat_user', ring: 1 },
    { id: 3, kind: 'ConfigKey', name: 'h5_avatar', ring: 1 },
  ],
];
const edges: EdgeView[] = [
  { id: 11, kind: 'MapsTo', from: 1, to: 2, resolved: true, confidence: 1, hops: null },
  { id: -1, kind: 'ReadsConfig', from: 1, to: 3, resolved: true, confidence: 0.8, hops: null },
];

describe('GraphCanvas', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    globalThis.IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('updating from “loading” to “has data” does not break Hook order', () => {
    act(() => {
      root.render(
        <GraphCanvas mode="radial" center={null} rings={[]} edges={[]} loading />,
      );
    });
    expect(container.textContent).toContain('Loading view');

    // Key: an update on the same instance. If the Hook count changed, this would throw.
    expect(() => {
      act(() => {
        root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
      });
    }).not.toThrow();
  });

  it('hovering a node doesn’t crash: the hover card must still render when supplementary fields are missing', () => {
    act(() => {
      root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
    });

    // The hover card once treated `CanvasNode` as `NodeView` and read `locations.length` directly,
    // but a canvas node has no such field -- the screen blanked as soon as the mouse entered a node.
    const label = Array.from(container.querySelectorAll('svg text')).find((t) =>
      t.textContent?.includes('wechat_user'),
    );
    expect(label).toBeTruthy();

    expect(() => {
      act(() => {
        label?.parentElement?.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
      });
    }).not.toThrow();

    expect(container.textContent).toContain('Location');
  });

  it('labels edge kinds when edges are below the threshold (a synthetic negative edge id still resolves)', () => {
    act(() => {
      root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    // The two edges each have their own kind; they must not all degrade to the first edge's kind just because ids repeat or are negative.
    // Without a `LocaleProvider`, `t` falls back to the English source label (e.g. `edge.MapsTo` → 'maps to'), so assert on that.
    expect(labels).toContain('maps to');
    expect(labels).toContain('reads config');
  });

  it('multiple parallel paths with the same (id, from, to) stay independent: hovering lights only one, the card shows each one’s own chain', () => {
    // In a forward perspective one propagated edge expands into multiple paths and `push_edge` reuses the same evidence edge id --
    // so two edges can share an identical (id, from, to). The old implementation keyed only by `id:from->to`,
    // so hovering lit both at once and the card always showed the first (`edges.find` first hit).
    const c: CanvasNode = { id: 1, kind: 'HttpContract', name: 'GET /x', ring: 0 };
    const ringNodes: CanvasNode[][] = [[{ id: 2, kind: 'Cache', name: 'cache', ring: 1 }]];
    const parallel: EdgeView[] = [
      {
        id: 7,
        kind: 'ReadsCache',
        from: 1,
        to: 2,
        resolved: false,
        confidence: 0.5,
        indirect: true,
        hops: 4,
        via: [{ id: 90, kind: 'Method', name: 'a' }],
      },
      {
        id: 7,
        kind: 'ReadsCache',
        from: 1,
        to: 2,
        resolved: false,
        confidence: 0.5,
        indirect: true,
        hops: 6,
        via: [{ id: 91, kind: 'Method', name: 'b' }],
      },
    ];
    act(() => {
      root.render(<GraphCanvas mode="layered" center={c} rings={ringNodes} edges={parallel} />);
    });

    const hits = Array.from(
      container.querySelectorAll('path[stroke="transparent"]'),
    ) as SVGPathElement[];
    expect(hits.length).toBe(2);

    act(() => {
      hits[0].dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    });
    expect(container.textContent).toContain('via 4 hops');
    expect(container.textContent).not.toContain('via 6 hops');

    act(() => {
      hits[1].dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    });
    expect(container.textContent).toContain('via 6 hops');
  });

  it('passing hiddenNodeKinds directly hides that node class (filter logic)', () => {
    act(() => {
      root.render(
        <GraphCanvas
          mode="radial"
          center={center}
          rings={rings}
          edges={edges}
          hiddenNodeKinds={['Table']}
        />,
      );
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('wechat_user'); // A table node is hidden
    expect(labels).toContain('h5_avatar'); // Other kinds of nodes remain
  });

  it('clicking a legend node item hides that node class (click wiring + filter end-to-end)', () => {
    let hidden: string[] = [];
    const onToggle = (k: string) => {
      hidden = hidden.includes(k) ? hidden.filter((x) => x !== k) : [...hidden, k];
    };
    const render2 = () =>
      act(() => {
        root.render(
          <GraphCanvas
            mode="radial"
            center={center}
            rings={rings}
            edges={edges}
            hiddenNodeKinds={hidden}
            onToggleNodeKind={onToggle}
            onToggleEdgeKind={() => {}}
          />,
        );
      });
    render2();

    // The legend is expanded by default; find the "table" legend item and click it.
    const tableRow = Array.from(container.querySelectorAll('span')).find(
      (s) => s.textContent === 'Table',
    );
    expect(tableRow).toBeTruthy();
    act(() => {
      tableRow!.click();
    });
    expect(hidden).toContain('Table');

    // Re-render with the filtered hiddenNodeKinds to verify the canvas really dropped table nodes.
    render2();
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('wechat_user'); // A table node is hidden
    expect(labels).toContain('h5_avatar'); // Other kinds of nodes remain
  });

  /**
   * Regression: when hiding an edge class, points connected only via it must be collapsed too.
   *
   * Otherwise those points have no visible edges yet still get placed in a column by the layout fallback (see the right-column
   * fallback in `layout/types.ts`), looking like "filtered but not really filtered".
   */
  it('when hiding an edge class, points connected only via it are collapsed too', () => {
    act(() => {
      root.render(
        <GraphCanvas
          mode="radial"
          center={center}
          rings={rings}
          edges={edges}
          hiddenEdgeKinds={['ReadsConfig']}
        />,
      );
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('h5_avatar'); // The point connected only via ReadsConfig is collapsed
    expect(labels).toContain('wechat_user'); // The point still carrying a MapsTo edge is kept
    expect(labels).toContain('POST /apple_login'); // The center is always kept as an anchor
    // Collapsing needs visible feedback, otherwise users think the legend button is broken.
    expect(container.textContent).toContain('1 node(s) collapsed');
  });
});
