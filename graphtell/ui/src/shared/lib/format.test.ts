import { describe, it, expect } from 'vitest';
import {
  formatNumber,
  formatDuration,
  formatTime,
  shortName,
  truncate,
} from './format';

describe('formatNumber', () => {
  it('returns integers as-is', () => expect(formatNumber(42)).toBe('42'));
  it('adds k for thousands', () => expect(formatNumber(1500)).toBe('1.5k'));
  it('adds M for millions', () => expect(formatNumber(2_500_000)).toBe('2.5M'));
  it('returns - for non-finite numbers', () => expect(formatNumber(NaN)).toBe('-'));
});

describe('formatDuration', () => {
  it('milliseconds', () => expect(formatDuration(500)).toBe('500ms'));
  it('seconds', () => expect(formatDuration(1500)).toBe('1.5s'));
  it('minutes and seconds', () => expect(formatDuration(125_000)).toBe('2m5s'));
});

describe('formatTime', () => {
  it('0 returns -', () => expect(formatTime(0)).toBe('-'));
  it('a normal timestamp is readable', () => expect(formatTime(1_700_000_000_000).length).toBeGreaterThan(0));
});

describe('shortName', () => {
  it('null / empty returns -', () => expect(shortName(null)).toBe('-'));
  it('backslash FQN takes the last segment', () => expect(shortName('app\\model\\Order')).toBe('Order'));
  it('slash path takes the last segment', () => expect(shortName('a/b/C')).toBe('C'));
});

describe('truncate', () => {
  it('a short string is unchanged', () => expect(truncate('abc', 5)).toBe('abc'));
  it('a long string is truncated with an ellipsis', () => expect(truncate('abcdef', 5)).toBe('abcd…'));
});
