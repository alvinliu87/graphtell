// @vitest-environment jsdom
import { act, StrictMode, type ReactNode } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { describe, it, vi } from 'vitest';
import {
  MemoryRouter,
  Route,
  Routes,
  useLocation,
  useNavigate,
  useNavigationType,
} from 'react-router-dom';

let calls: string[] = [];
let log: string[] = [];
let failView = false;
let emptyCandidates = false;

function stub(url: string): unknown {
  calls.push(url);
  if (url.includes('/perspectives')) {
    return [
      { id: 'route', label: '路由', mode: 'object', layout: 'layered', depth: 2, available: 10 },
      { id: 'table', label: '数据表', mode: 'object', layout: 'radial', depth: 2, available: 5 },
      { id: 'overview', label: '总览', mode: 'aggregate', layout: 'compound', depth: 2, available: 3 },
    ];
  }
  if (url.includes('/sub-projects')) return [];
  if (url.includes('/candidates')) {
    if (emptyCandidates) return [];
    return url.includes('/table/candidates')
      ? [{ id: 22, name: 'users', badge: null }]
      : [{ id: 11, name: 'GET /a', badge: null }];
  }
  if (url.includes('/view/')) {
    if (failView) throw new Error('404 对象不属于该视角');
    const id = url.includes('node=22') ? 22 : 11;
    return {
      project_id: 1,
      perspective: 'route',
      layout: 'layered',
      center: { id, kind: 'HttpContract', category: null, name: 'x', fqn: null, ring: 0, sub_project_id: null, has_own_view: true, own_view: null, locations: [], annotations: [], metrics: null },
      rings: [],
      edges: [],
      hidden: { total: 0, shown: 0, by_kind: {}, note: '' },
      unresolved: [],
      conclusions: {},
    };
  }
  if (url.includes('/api/projects/1')) {
    return { id: 1, name: 'p1', root_path: '/tmp', status: 'ready', kind: 'auto' };
  }
  return null;
}

vi.mock('@/shared/api/http', () => ({
  http: {
    get: (url: string) => Promise.resolve(stub(url)),
    post: () => Promise.resolve(null),
    put: () => Promise.resolve(null),
    del: () => Promise.resolve(null),
  },
  initApiBase: () => Promise.resolve(''),
}));

import { GraphPage } from './GraphPage';

let go: (n: number) => void = () => {};

function Probe() {
  const loc = useLocation();
  const type = useNavigationType();
  const nav = useNavigate();
  go = (n: number) => nav(n);
  log.push(`${loc.search || '(空)'} [${type}]`);
  return null;
}

async function tick(ms = 80) {
  await act(async () => {
    await new Promise((r) => setTimeout(r, ms));
  });
}

function tree(entry: string, strict: boolean) {
  const inner = (
    <StrictModeOrNot strict={strict}>
      <Probe />
      <Routes>
        <Route path="/projects/:projectId/graph" element={<GraphPage />} />
      </Routes>
    </StrictModeOrNot>
  );
  return (
    <MemoryRouter initialEntries={[entry]}>
      {inner}
    </MemoryRouter>
  );
}

function StrictModeOrNot({ strict, children }: { strict: boolean; children: ReactNode }) {
  return strict ? <StrictMode>{children}</StrictMode> : <>{children}</>;
}

async function setup(entry: string, strict = false) {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root = createRoot(container);
  await act(async () => {
    root.render(tree(entry, strict));
  });
  await tick();
  return {
    container,
    async switchTo(label: string) {
      const picker = container.querySelectorAll('.ant-select')[0] as HTMLElement;
      await act(async () => {
        picker
          .querySelector('.ant-select-selector')!
          .dispatchEvent(new MouseEvent('mousedown', { bubbles: true }));
      });
      await tick(30);
      const options = Array.from(document.querySelectorAll('.ant-select-item-option'));
      const target = options.find((o) => (o.textContent ?? '').includes(label));
      if (!target) throw new Error(`找不到选项：${label}，现有：${options.map((o) => o.textContent)}`);
      await act(async () => {
        target.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      });
      await tick(200);
    },
    async back() {
      await act(async () => {
        go(-1);
      });
      await tick(150);
    },
    done() {
      act(() => root.unmount());
      container.remove();
    },
  };
}

function report(name: string) {
  // eslint-disable-next-line no-console
  console.log(`\n### ${name}\nURL: ${JSON.stringify(log)}\n请求: ${JSON.stringify(calls)}`);
}

globalThis.IS_REACT_ACT_ENVIRONMENT = true;
(window as unknown as { matchMedia: unknown }).matchMedia = () => ({
  matches: false, media: '', addListener: () => {}, removeListener: () => {},
  addEventListener: () => {}, removeEventListener: () => {}, dispatchEvent: () => false, onchange: null,
});
(globalThis as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
  observe() {} unobserve() {} disconnect() {}
};

describe('debug: 切换视角后 URL 是否稳定', () => {
  it('场景 1：普通切换到对象视角', async () => {
    const h = await setup('/projects/1/graph?p=route&n=11');
    await h.switchTo('数据表');
    await h.back();
    await h.back();
    report('场景1 对象视角 + 两次后退');
    h.done();
  });

  it('场景 2：切到聚合视角', async () => {
    calls = [];
    log = [];
    const h = await setup('/projects/1/graph?p=route&n=11');
    await h.switchTo('总览');
    report('场景2 聚合视角');
    h.done();
  });

  it('场景 3：候选为空', async () => {
    calls = [];
    log = [];
    emptyCandidates = true;
    const h = await setup('/projects/1/graph?p=route&n=11');
    await h.switchTo('数据表');
    report('场景3 候选为空');
    emptyCandidates = false;
    h.done();
  });

  it('场景 4：对象取不到', async () => {
    calls = [];
    log = [];
    failView = true;
    const h = await setup('/projects/1/graph?p=route&n=11');
    await h.switchTo('数据表');
    report('场景4 对象取不到');
    failView = false;
    h.done();
  });

  it('场景 6：StrictMode 下连切两次视角再后退', async () => {
    calls = [];
    log = [];
    const h = await setup('/projects/1/graph?p=route&n=11', true);
    await h.switchTo('数据表');
    await h.switchTo('路由');
    await h.back();
    await h.back();
    report('场景6 StrictMode');
    h.done();
  });

  it('场景 5：URL 无 p', async () => {
    calls = [];
    log = [];
    const h = await setup('/projects/1/graph');
    await tick(200);
    report('场景5 无 p');
    h.done();
  });
});
