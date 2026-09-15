// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';

import type { EdgeView } from '@/entities/view';
import { GraphCanvas, type CanvasNode } from './GraphCanvas';

/**
 * 回归：Hook 顺序必须稳定。
 *
 * 曾经把 `useMemo` 放在 `if (loading) return …` **之后**，于是：
 * 首次渲染（loading）只跑了 6 个 Hook，数据返回后再渲染多跑 1 个 →
 * React 直接抛 "Rendered more hooks than during the previous render" 白屏。
 *
 * 这个用例模拟真实的"加载中 → 有数据"更新路径，任何 Hook 数量变化都会被抓到。
 */

declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const center: CanvasNode = { id: 1, kind: 'HttpContract', name: 'POST /apple_login', ring: 0 };
const rings: CanvasNode[][] = [
  [
    { id: 2, kind: 'Table', name: 'wechat_user', ring: 1 },
    { id: 3, kind: 'ConfigKey', name: 'h5_avatar', ring: 1 },
  ],
];
const edges: EdgeView[] = [
  { id: 11, kind: 'MapsTo', from: 1, to: 2, resolved: true, confidence: 1, hops: null },
  { id: -1, kind: 'ReadsConfig', from: 1, to: 3, resolved: true, confidence: 0.8, hops: null },
];

describe('GraphCanvas', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    globalThis.IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('从「加载中」更新到「有数据」不会破坏 Hook 顺序', () => {
    act(() => {
      root.render(
        <GraphCanvas mode="radial" center={null} rings={[]} edges={[]} loading />,
      );
    });
    expect(container.textContent).toContain('加载视图');

    // 关键：同一实例上的更新。Hook 数量若发生变化，这里会抛错。
    expect(() => {
      act(() => {
        root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
      });
    }).not.toThrow();
  });

  it('悬浮节点不出错：补充字段缺失时悬浮卡片也要能渲染', () => {
    act(() => {
      root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
    });

    // 悬浮卡片曾把 `CanvasNode` 当 `NodeView` 用，直接读 `locations.length`，
    // 而画布节点根本没有这个字段 —— 鼠标一进节点就白屏。
    const label = Array.from(container.querySelectorAll('svg text')).find((t) =>
      t.textContent?.includes('wechat_user'),
    );
    expect(label).toBeTruthy();

    expect(() => {
      act(() => {
        label?.parentElement?.dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
      });
    }).not.toThrow();

    expect(container.textContent).toContain('位置');
  });

  it('边少于阈值时标注边的种类（合成边 id 为负也能正确取值）', () => {
    act(() => {
      root.render(<GraphCanvas mode="radial" center={center} rings={rings} edges={edges} />);
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    // 两条边各有各的种类，不能因为 id 重复/为负而全部退化成第一条的种类。
    // 未包 `LocaleProvider` 时 `t` 原样回退为 i18n 键（如 `edge.MapsTo`），故按键断言。
    expect(labels).toContain('edge.MapsTo');
    expect(labels).toContain('edge.ReadsConfig');
  });
});
