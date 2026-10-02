// @vitest-environment jsdom
import { act } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { EdgeView, SourceLocation } from '@/entities/view';

// 面板一打开就会按边 id 查证据。单测里没有后端，让它"查不到" ——
// 这正是合成边（负 id）在真实环境里的情形：证据只能来自边自身内联的 `to_call_site`。
vi.mock('@/entities/view', () => ({
  viewApi: {
    edgeEvidence: async () => null,
    nodeLocations: async () => null,
  },
}));

import { Inspector } from './Inspector';

declare global {
  // eslint-disable-next-line no-var
  var IS_REACT_ACT_ENVIRONMENT: boolean;
}

const at = (line: number, note: string, snippet: string): SourceLocation => ({
  file: 'crmeb/app/services/product/product/StoreProductServices.php',
  line,
  symbol: null,
  note,
  snippet,
});

/**
 * 直达语义边：`save --投递到--> 队列`。
 *
 * 它没有折叠掉的中间节点（`via` 为空），但在图上依然是"方法 → 语义节点"的一跳。
 * 曾因"只有 via 非空才画链"，这类边在抽屉里只剩一行孤立位置：看不到起点 `save`，
 * 也看不到终点的语义节点，看起来像"这条边没建好"。
 */
const direct: EdgeView = {
  id: -3,
  kind: 'PublishesTo',
  from: 7,
  to: 9,
  resolved: true,
  confidence: 0.8,
  hops: null,
  via: [],
  to_call_site: at(800, '本链路访问该资源的位置', "ProductCopyJob::dispatch('copySliderImage', [$res->id]);"),
  node_locations: [
    { id: 7, synthetic: false, locations: [at(742, '定义', 'public function save()')] },
    { id: 9, synthetic: true, locations: [at(800, '定义', "ProductCopyJob::dispatch('copySliderImage', [$res->id]);")] },
  ],
};

const nameOf = (id: number) =>
  id === 7 ? 'save' : id === 9 ? 'store_product_services' : `#${id}`;

describe('Inspector 边面板', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    globalThis.IS_REACT_ACT_ENVIRONMENT = true;
    // antd 的 Drawer 经 `useBreakpoint` 用到 `matchMedia`，jsdom 未实现。
    if (!window.matchMedia) {
      window.matchMedia = ((query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addListener: () => {},
        removeListener: () => {},
        addEventListener: () => {},
        removeEventListener: () => {},
        dispatchEvent: () => false,
      })) as unknown as typeof window.matchMedia;
    }
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('直达边也按「起点 → 终点」画链，且与路由链路同版式（不重复列证据位置）', async () => {
    await act(async () => {
      root.render(
        <Inspector nodeId={null} edgeId={direct.id} edgeView={direct} nodeNameOf={nameOf} onClose={() => {}} />,
      );
    });

    // Drawer 走 portal，内容挂在 body 上。
    const text = document.body.textContent ?? '';
    expect(text).toContain('start');
    expect(text).toContain('end');
    expect(text).toContain('save');
    expect(text).toContain('store_product_services');
    // 中间那一行 = 边的 `to_call_site`（本链路访问该资源的位置）。
    expect(text).toContain('StoreProductServices.php:800');
    // 没有折叠掉的中间节点，标题就不该谎称"折叠"。
    expect(text).not.toContain('Collapsed call chain');
    expect(text).toContain('Call chain');
    // 链路已给出本边那一行，不应再单列一份"证据位置"。
    expect(text).not.toContain('证据位置');
  });
});
