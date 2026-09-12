import { describe, it, expect } from 'vitest';
import {
  formatNumber,
  formatDuration,
  formatTime,
  shortName,
  truncate,
} from './format';

describe('formatNumber', () => {
  it('整数原样返回', () => expect(formatNumber(42)).toBe('42'));
  it('千进制加 k', () => expect(formatNumber(1500)).toBe('1.5k'));
  it('百万加 M', () => expect(formatNumber(2_500_000)).toBe('2.5M'));
  it('非有限数返回 -', () => expect(formatNumber(NaN)).toBe('-'));
});

describe('formatDuration', () => {
  it('毫秒', () => expect(formatDuration(500)).toBe('500ms'));
  it('秒', () => expect(formatDuration(1500)).toBe('1.5s'));
  it('分钟秒', () => expect(formatDuration(125_000)).toBe('2m5s'));
});

describe('formatTime', () => {
  it('0 返回 -', () => expect(formatTime(0)).toBe('-'));
  it('正常时间戳可读', () => expect(formatTime(1_700_000_000_000).length).toBeGreaterThan(0));
});

describe('shortName', () => {
  it('null/空返回 -', () => expect(shortName(null)).toBe('-'));
  it('反斜杠 FQN 取末段', () => expect(shortName('app\\model\\Order')).toBe('Order'));
  it('斜杠路径取末段', () => expect(shortName('a/b/C')).toBe('C'));
});

describe('truncate', () => {
  it('短字符串不变', () => expect(truncate('abc', 5)).toBe('abc'));
  it('长字符串截断并加省略号', () => expect(truncate('abcdef', 5)).toBe('abcd…'));
});
