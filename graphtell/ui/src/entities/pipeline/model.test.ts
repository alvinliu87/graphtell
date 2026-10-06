import { describe, expect, it } from 'vitest';

import { PHASE_HINT, PHASE_LABEL, PHASE_ORDER } from './model';

describe('pipeline model — phase metadata', () => {
  it('orders phases from Ingest to Resolve (matching the backend P0→P7 sequence)', () => {
    expect(PHASE_ORDER).toEqual([
      'Ingest',
      'CfAst',
      'Prepare',
      'AnnotatePre',
      'Synthesize',
      'AnnotatePost',
      'Resolve',
    ]);
  });

  it('has a display label and hint for every phase', () => {
    for (const p of PHASE_ORDER) {
      expect(PHASE_LABEL[p]).toBeTruthy();
      expect(PHASE_HINT[p]).toBeTruthy();
    }
  });
});
