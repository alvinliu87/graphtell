import { describe, it, expect } from 'vitest';
import { nodeColor, edgeColor } from './model';

describe('nodeColor', () => {
  it('returns the colour for a known kind', () => expect(nodeColor('Class')).toBe('#3d7eff'));
  it('falls back to Unknown for an unknown kind', () => expect(nodeColor('Nope')).toBe('#9ca3af'));
});

describe('edgeColor', () => {
  it('returns the colour for a known kind', () => expect(edgeColor('Extends')).toBe('#94a3b8'));
  it('falls back for an unknown kind', () => expect(edgeColor('Whatever')).toBe('#cbd5e1'));
});
