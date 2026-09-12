import type { LayoutMode } from '@/entities/view';

/**
 * 视图现场（URL 即状态）。
 *
 * 为什么必须进 URL：
 * * **刷新** —— 用户 F5 后必须回到同一张图，否则一次误刷新就丢掉几分钟的排查现场
 * * **前进 / 后退** —— 切视角是有成本的导航动作，浏览器返回键必须能撤销
 * * **分享 / 报告** —— 一条链接就是一个结论（"看这个端点的链路"）
 *
 * 采用短键名以保持 URL 可读：`p` 视角、`n` 中心、`d` 深度、`m` 布局、`i` Inspector。
 */
export interface ViewState {
  /** 一级：视角 id。 */
  p: string | null;
  /** 二级：中心对象节点 id。 */
  n: number | null;
  /** 跳数。 */
  d: number;
  /** 布局覆盖（不填则用视角默认布局）。 */
  m: LayoutMode | null;
  /** Inspector 选中的节点（不切视角）。 */
  i: number | null;
  /** Inspector 选中的边。 */
  e: number | null;
}

export const EMPTY_STATE: ViewState = { p: null, n: null, d: 2, m: null, i: null, e: null };

export function encodeViewState(s: ViewState): string {
  const usp = new URLSearchParams();
  if (s.p) usp.set('p', s.p);
  if (s.n !== null) usp.set('n', String(s.n));
  if (s.d !== 2) usp.set('d', String(s.d));
  if (s.m) usp.set('m', s.m);
  if (s.i !== null) usp.set('i', String(s.i));
  if (s.e !== null) usp.set('e', String(s.e));
  const str = usp.toString();
  return str ? `?${str}` : '';
}

export function decodeViewState(search: string): ViewState {
  const usp = new URLSearchParams(search);
  const num = (key: string): number | null => {
    const v = usp.get(key);
    if (v === null) return null;
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  };
  const mode = usp.get('m');
  return {
    p: usp.get('p'),
    n: num('n'),
    d: num('d') ?? 2,
    m: (mode as LayoutMode | null) ?? null,
    i: num('i'),
    e: num('e'),
  };
}

/** 两个现场是否等价（用于避免写入重复的历史记录）。 */
export function sameViewState(a: ViewState, b: ViewState): boolean {
  return a.p === b.p && a.n === b.n && a.d === b.d && a.m === b.m && a.i === b.i && a.e === b.e;
}

/**
 * 修正现场：URL 可能是手写的、过期的（比如节点被重建后 id 变了）。
 * 这里只做"能修就修，修不了就退回默认"，绝不静默展示一张无关的图。
 */
export function reconcileViewState(
  state: ViewState,
  perspectives: Array<{ id: string; mode: 'object' | 'aggregate'; available: number; depth?: number }>,
  candidates: Array<{ id: number }>,
): ViewState {
  const exists = state.p ? perspectives.find((x) => x.id === state.p) : undefined;
  if (!exists) {
    const first = perspectives.find((x) => x.available > 0) ?? perspectives[0];
    return { ...EMPTY_STATE, p: first?.id ?? null, d: first?.depth ?? 2 };
  }
  if (exists.mode === 'aggregate') {
    // 聚合视角没有"单个对象"
    return { ...state, n: null };
  }
  if (state.n !== null && !candidates.some((c) => c.id === state.n)) {
    // 中心对象已不存在：清掉，由页面自动选第一个候选
    return { ...state, n: null };
  }
  return state;
}
