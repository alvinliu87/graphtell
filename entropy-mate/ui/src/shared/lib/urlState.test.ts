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
  it('空状态编码为空串', () => expect(encodeViewState(EMPTY_STATE)).toBe(''));

  it('往返一致', () => {
    const s: ViewState = { p: 'route', n: 7, d: 3, i: 2, e: 4 };
    const dec = decodeViewState(encodeViewState(s));
    expect(dec).toEqual(s);
  });

  it('默认深度 2 被省略', () => {
    const s: ViewState = { p: 'route', n: 7, d: 2, i: null, e: null };
    const dec = decodeViewState(encodeViewState(s));
    expect(dec.d).toBe(2);
    expect(dec.p).toBe('route');
    expect(dec.n).toBe(7);
  });
});

describe('sameViewState', () => {
  it('相同为 true', () => expect(sameViewState(EMPTY_STATE, EMPTY_STATE)).toBe(true));
  it('不同为 false', () =>
    expect(sameViewState(EMPTY_STATE, { ...EMPTY_STATE, p: 'x' })).toBe(false));
});

describe('reconcileViewState', () => {
  const perspectives = [
    { id: 'route', mode: 'object' as const, available: 0, depth: 2 },
    { id: 'table', mode: 'object' as const, available: 5, depth: 2 },
    { id: 'platform', mode: 'aggregate' as const, available: 3, depth: 2 },
  ];
  it('视角无效时退回第一个有数据的视角', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: null }, perspectives);
    expect(r.p).toBe('table');
  });

  it('聚合视角清掉中心对象', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: 'platform', n: 5 }, perspectives);
    expect(r.p).toBe('platform');
    expect(r.n).toBeNull();
  });

  // 候选列表带 limit 上限、还可能被后端过滤，它不是"节点是否存在"的判据：
  // 用它会把刚导航进来的、排在前 N 之外的节点误判为不存在，再被静默换成第一个候选。
  it('节点不在候选列表里也保留 n（候选不是存在性判据）', () => {
    const r = reconcileViewState({ ...EMPTY_STATE, p: 'table', n: 123 }, perspectives);
    expect(r.p).toBe('table');
    expect(r.n).toBe(123);
  });
});
