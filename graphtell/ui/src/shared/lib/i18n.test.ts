import { describe, expect, it } from 'vitest';

import { edgeKindLabel, translate } from './i18n';

const identity: (k: string) => string = (k) => k;

describe('edgeKindLabel', () => {
  // A resolver that knows the combined `ReadsDb+WritesDb` entry (as the real dict does),
  // so we can assert the lexicographic-combination branch actually fires.
  const dictT = (k: string): string =>
    k === 'edge.ReadsDb+WritesDb'
      ? 'read+write DB'
      : k === 'edge.WritesDb'
        ? 'writes'
        : k === 'edge.ReadsDb'
          ? 'reads'
          : k;

  it('returns the single-kind label when there are no also_kinds', () => {
    expect(edgeKindLabel(dictT, 'Calls')).toBe('edge.Calls');
  });

  it('combines also_kinds in lexicographic order, independent of argument order', () => {
    expect(edgeKindLabel(dictT, 'WritesDb', ['ReadsDb'])).toBe('read+write DB');
    expect(edgeKindLabel(dictT, 'ReadsDb', ['WritesDb'])).toBe('read+write DB');
  });

  it('falls back to the single-kind label when no combined entry exists', () => {
    const onlySingle = (k: string) => (k === 'edge.WritesDb' ? 'writes' : k);
    expect(edgeKindLabel(onlySingle, 'WritesDb', ['ReadsDb'])).toBe('writes');
  });
});

describe('translate', () => {
  it('falls back to the key itself for unknown keys (no blank screen)', () => {
    expect(translate('totally.unknown.key')).toBe('totally.unknown.key');
  });
});
