// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

// Stub the graph hook so the drawer has data to render without a backend.
vi.mock('@/entities/graph', () => ({
  useNodeDetail: (id?: number) =>
    id === 999
      ? { node: null, neighbors: [], annotations: [], loading: false }
      : {
          node: {
            id: 1,
            kind: 'Method',
            name: 'saveOrder',
            fqn: 'App\\Service\\saveOrder',
            identity: { kind: 'method', value: 'saveOrder' },
            file_id: 9,
            start_line: 10,
            end_line: 40,
            language: 'php',
            confidence: 0.9,
            properties: { a: 1 },
          },
          neighbors: [
            { id: 2, kind: 'Calls', from_id: 1, to_id: 2, phase: 'CfAst', confidence: 0.8 },
          ],
          annotations: [{ id: 3, channel: 'semantic', kind: 'role', subkind: 'service', confidence: 0.7 }],
          loading: false,
        },
  nodeColor: (k: string) => k,
  edgeColor: (k: string) => k,
}));

import { NodeDetailDrawer } from './NodeDetailDrawer';
import { mountWithRouter } from '@/test/render';

describe('NodeDetailDrawer', () => {
  it('renders node properties, annotations and adjacent edges', () => {
    mountWithRouter(<NodeDetailDrawer nodeId={1} onClose={() => {}} />);
    const text = document.body.textContent ?? '';
    expect(text).toContain('saveOrder');
    expect(text).toContain('Annotations');
    expect(text).toContain('Adjacent edges');
    expect(text).toContain('Calls');
  });

  it('shows the empty "not found" state when the node does not exist', () => {
    // With open=true (defined id) but a null node from the stub, the drawer renders the Empty.
    mountWithRouter(<NodeDetailDrawer nodeId={999} onClose={() => {}} />);
    expect(document.body.textContent).toContain('Node not found');
  });
});
