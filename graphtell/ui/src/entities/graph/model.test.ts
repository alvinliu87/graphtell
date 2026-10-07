import { describe, expect, it } from 'vitest';

import { edgeColor } from './model';

describe('edge colors', () => {
  it('gives the cross-sub ResolvesToContract bridge a distinct, non-default color', () => {
    // The front-end's contract node -> the back-end's declaration of the same endpoint must be
    // visually distinct on the canvas, not collapsed into the grey "unknown kind" fallback.
    expect(edgeColor('ResolvesToContract')).toBe('#06b6d4');
    expect(edgeColor('ResolvesToContract')).not.toBe('#cbd5e1');
  });
});
