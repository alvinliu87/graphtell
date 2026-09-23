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

  it('同一 (id, from, to) 的多条平行路径各自独立：悬浮只点亮一条、卡片显示各自的链路', () => {
    // 正向视角下同一条传播边会被展开成多条路径，`push_edge` 复用同一个 evidence 边 id ——
    // 于是出现 (id, from, to) 完全相同的两条边。旧实现只按 `id:from->to` 做键，
    // 悬浮会同时点亮两条、卡片永远显示第一条（`edges.find` 首个命中）。
    const c: CanvasNode = { id: 1, kind: 'HttpContract', name: 'GET /x', ring: 0 };
    const ringNodes: CanvasNode[][] = [[{ id: 2, kind: 'Cache', name: 'cache', ring: 1 }]];
    const parallel: EdgeView[] = [
      {
        id: 7,
        kind: 'ReadsCache',
        from: 1,
        to: 2,
        resolved: false,
        confidence: 0.5,
        indirect: true,
        hops: 4,
        via: [{ id: 90, kind: 'Method', name: 'a' }],
      },
      {
        id: 7,
        kind: 'ReadsCache',
        from: 1,
        to: 2,
        resolved: false,
        confidence: 0.5,
        indirect: true,
        hops: 6,
        via: [{ id: 91, kind: 'Method', name: 'b' }],
      },
    ];
    act(() => {
      root.render(<GraphCanvas mode="layered" center={c} rings={ringNodes} edges={parallel} />);
    });

    const hits = Array.from(
      container.querySelectorAll('path[stroke="transparent"]'),
    ) as SVGPathElement[];
    expect(hits.length).toBe(2);

    act(() => {
      hits[0].dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    });
    expect(container.textContent).toContain('途经 4 跳');
    expect(container.textContent).not.toContain('途经 6 跳');

    act(() => {
      hits[1].dispatchEvent(new MouseEvent('mouseover', { bubbles: true }));
    });
    expect(container.textContent).toContain('途经 6 跳');
  });

  it('直接传 hiddenNodeKinds 可隐藏该类节点（过滤逻辑）', () => {
    act(() => {
      root.render(
        <GraphCanvas
          mode="radial"
          center={center}
          rings={rings}
          edges={edges}
          hiddenNodeKinds={['Table']}
        />,
      );
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('wechat_user'); // 表节点被隐藏
    expect(labels).toContain('h5_avatar'); // 其它类节点仍在
  });

  it('点击图例节点项会隐藏该类节点（点击接线 + 过滤端到端）', () => {
    let hidden: string[] = [];
    const onToggle = (k: string) => {
      hidden = hidden.includes(k) ? hidden.filter((x) => x !== k) : [...hidden, k];
    };
    const render2 = () =>
      act(() => {
        root.render(
          <GraphCanvas
            mode="radial"
            center={center}
            rings={rings}
            edges={edges}
            hiddenNodeKinds={hidden}
            onToggleNodeKind={onToggle}
            onToggleEdgeKind={() => {}}
          />,
        );
      });
    render2();

    // 图例默认展开，直接找「表」图例项并点击。
    const tableRow = Array.from(container.querySelectorAll('span')).find(
      (s) => s.textContent === 'node.Table',
    );
    expect(tableRow).toBeTruthy();
    act(() => {
      tableRow!.click();
    });
    expect(hidden).toContain('Table');

    // 套用过滤后的 hiddenNodeKinds 重渲染，验证画布真的去掉了表节点。
    render2();
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('wechat_user'); // 表节点被隐藏
    expect(labels).toContain('h5_avatar'); // 其它类节点仍在
  });

  /**
   * 回归：隐藏某类边时，只经由它相连的点要一并收起。
   *
   * 否则这些点没有任何可见边，却仍会被布局兜底排进列里占位（见 `layout/types.ts`
   * 的右侧列兜底），看上去像"筛了但没筛掉"。
   */
  it('隐藏某类边时，只经由它相连的点一并收起', () => {
    act(() => {
      root.render(
        <GraphCanvas
          mode="radial"
          center={center}
          rings={rings}
          edges={edges}
          hiddenEdgeKinds={['ReadsConfig']}
        />,
      );
    });
    const labels = Array.from(container.querySelectorAll('svg text')).map((t) => t.textContent);
    expect(labels).not.toContain('h5_avatar'); // 只靠 ReadsConfig 连着的点被连带收起
    expect(labels).toContain('wechat_user'); // 仍挂着 MapsTo 边的点保留
    expect(labels).toContain('POST /apple_login'); // 中心恒保留作锚点
    // 连带收起要有可见反馈，否则用户会以为图例按钮失灵。
    expect(container.textContent).toContain('连带收起 1 点');
  });
});
