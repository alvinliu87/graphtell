import { describe, it, expect } from 'vitest';
import { nodeColor, edgeColor } from './model';

describe('nodeColor', () => {
  it('已知种类返回配色', () => expect(nodeColor('Class')).toBe('#3d7eff'));
  it('未知种类回退到 Unknown', () => expect(nodeColor('Nope')).toBe('#9ca3af'));
});

describe('edgeColor', () => {
  it('已知种类返回配色', () => expect(edgeColor('Extends')).toBe('#94a3b8'));
  it('未知种类回退', () => expect(edgeColor('Whatever')).toBe('#cbd5e1'));
});
