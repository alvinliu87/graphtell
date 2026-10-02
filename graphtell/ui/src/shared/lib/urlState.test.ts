import { describe, it, expect } from 'vitest';
import {
  EMPTY_STATE,
  encodeViewState,
  decodeViewState,
  sameViewState,
  reconcileViewState,
  type ViewState,
} from './urlState';

describe('encode/decode', () => {
  it('empty state encodes to an empty string', () => expect(encodeViewState(EMPTY_STATE)).toBe(''));

  it('round-trip is consistent', () => {
    const s: ViewState = { p: 'route', n: 7, d: 3, i: 2, e: 4 };
    const dec = decodeViewState(encodeViewState(s));
    expect(dec).toEqual(s);
  });

  it('the default depth 2 is omitted', () => {
    const s: ViewState = { p: 'route', n: 7, d: 2, i: null, e: null };
    const dec = decodeViewState(encodeViewState(s));
    expect(dec.d).toBe(2);
    expect(dec.p).toBe('route');
    expect(dec.n).toBe(7);
  });
});

describe('sameViewState', () => {
  it('identical is true', () => expect(sameViewState(EMPTY_STATE, EMPTY_STATE)).toBe(true));
  it('different is false', () =>
    expect(sameViewState(EMPTY_STATE, { ...EMPTY_STATE, p: 'x' })).toBe(false));
});

describe('reconcileViewState', () => {
  const perspectives = [
    { id: 'route', mode: 'object' as const, available: 0, depth: 2 },
    { id: 'table', mode: 'object' as const, available: 5, depth: 2 },
    { id: 'platform', mode: 'aggregate' as const, available: 3, depth: 2 },
  ];
  it('when the perspective is invalid it falls back to the first perspective with data', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: null }, perspectives);
    expect(r.p).toBe('table');
  });

  it('an aggregate perspective clears the center object', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: 'platform', n: 5 }, perspectives);
    expect(r.p).toBe('platform');
    expect(r.n).toBeNull();
  });

  // The candidate list has a limit cap and may also be filtered by the backend, so it is not a criterion for "does the node exist":
  // using it misjudges a node you just navigated to but that ranks beyond the top N as nonexistent, then silently swaps it for the first candidate.
  it('keeps n even when the node isn’t in the candidate list (candidates aren’t an existence criterion)', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: 'table', n: 123 }, perspectives);
    expect(r.p).toBe('table');
    expect(r.n).toBe(123);
  });
});
