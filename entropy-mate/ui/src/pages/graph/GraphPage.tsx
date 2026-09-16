import {
  Alert,
  Button,
  Card,
  Col,
  Collapse,
  Drawer,
  Input,
  InputNumber,
  Row,
  Space,
  Statistic,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useParams, useSearchParams, useNavigate } from 'react-router-dom';
import { useProject } from '@/entities/project';
import {
  useAggregateView,
  useObjectView,
  usePerspectives,
  viewApi,
  type Candidate,
  type EdgeView,
  type LayoutMode,
  type SourceLocation,
} from '@/entities/view';
import { GraphCanvas, type CanvasCluster, type CanvasNode, type CanvasMatrix } from '@/widgets/graph-canvas';
import { Inspector } from '@/widgets/inspector';
import { PageHeader } from '@/shared/ui/PageHeader';
import { PipelineProgress } from '@/widgets/pipeline-progress';
import { RunPipelineButton } from '@/features/run-pipeline';
import { useRunStatus } from '@/entities/pipeline';
import { PerspectivePicker, type BreadcrumbItem } from '@/features/perspective-picker';
import {
  decodeViewState,
  encodeViewState,
  reconcileViewState,
  sameViewState,
  type ViewState,
} from '@/shared/lib/urlState';
// 暂时注释：IDE 打开入口已移除，无需再处理「后端根 → 本地根」映射（根模板 / WSL / 按工程覆盖）。
// 以后再考虑加回时恢复此 import。
// import {
//   effectiveTemplate,
//   getWslDistro,
//   getWslMode,
//   resolveProjectRoot,
// } from '@/shared/lib/ide';
import { formatNumber } from '@/shared/lib/format';
import { useLocale } from '@/shared/lib/i18n';
import { FullscreenOutlined } from '@ant-design/icons';

/** 图视图页：两级筛选 → 单对象链路子图 → 可跳转的结论面板。 */
export function GraphPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const navigate = useNavigate();
  const { t } = useLocale();
  const [params, setParams] = useSearchParams();
  const { project, reload: reloadProject } = useProject(id);
  const { run } = useRunStatus(id, project?.status);

  const { perspectives } = usePerspectives(id);

  /** 现场（URL 是唯一真源）。 */
  const [state, setState] = useState<ViewState>(() => decodeViewState(params.toString()));
  /** 面包屑：记录导航过的视角，可回退。 */
  const [trail, setTrail] = useState<BreadcrumbItem[]>([]);
  /** 上一个中心，切视角后保留为邻居并标记 `from`。 */
  const [origin, setOrigin] = useState<{ id: number; name: string } | null>(null);
  /** 候选列表连同它所属的视角一起存：切视角后必须立刻失效，
   *  否则会拿上一视角的列表来填空默认值 / 判断"节点是否存在"。 */
  const [candidateBundle, setCandidateBundle] = useState<{ p: string; list: Candidate[] }>({
    p: '',
    list: [],
  });
  /** 当前视角的候选是否已从后端返回：用于判断"未选节点"时是「还在加载」还是「确实没有任何候选」。 */
  const [candidatesLoaded, setCandidatesLoaded] = useState(false);
  /** 只有"属于当前视角"的候选才生效；切换视角的瞬间派生为空，等新视角候选到达后才有值。 */
  const candidates = candidateBundle.p === state.p ? candidateBundle.list : [];
  const [candidateSearch, setCandidateSearch] = useState('');
  /** 二级对象下拉是否展开：候选只在展开时才去后端取（按需加载）。
   *  后端 candidates 在无搜索词时会取全量候选做"语义依赖价值"排序，但已改为整图预加载后
   *  在内存里跑 BFS（不再逐节点查库），很快；有搜索词时直接按名称返回、不打分。
   *  这里仍按需加载：已选中节点且未展开下拉时绝不预取，避免每次切视角都无谓打一次。 */
  const [dropdownOpen, setDropdownOpen] = useState(false);
  /** 是否在图上标注边的类型（`ReadsConfig` / `MapsTo`…）。边过多时组件会自动退化为按需标注。 */
  const [showEdgeLabels, setShowEdgeLabels] = useState(true);
  /** 折叠模式下，点击节点后按需展开显示的语法子图（按节点 id 归集）。 */
  const [expanded, setExpanded] = useState<Record<number, { nodes: CanvasNode[]; edges: EdgeView[] }>>({});
  const [expandingId, setExpandingId] = useState<number | null>(null);
  /** 就地展开调用链时的跳数：默认 2（多跳），封顶 4 以免节点爆炸；只影响折叠模式下点击节点展开的子图，不动主图中心跳数。 */
  const [expandDepth, setExpandDepth] = useState(2);
  const [inspectNode, setInspectNode] = useState<number | null>(state.i);
  const [inspectEdge, setInspectEdge] = useState<number | null>(state.e);
  /** 点击的那条边本身。合成边的折叠链（`via`）只存在于当次视图结果里，按 id 重查拿不到，
   *  所以必须点击时随身带入 Inspector，否则"途经 N 跳"就会变成一句空话。 */
  const [inspectEdgeView, setInspectEdgeView] = useState<EdgeView | null>(null);
  /** 右侧"结论/导航"面板：默认收起为抽屉浮层，不占用图的横向空间。 */
  const [drawerOpen, setDrawerOpen] = useState(false);
  const lastPushed = useRef<string>('');

  const current = perspectives.find((p) => p.id === state.p) ?? null;
  const isAggregate = current?.mode === 'aggregate';
  /** 未选节点、且候选尚未就绪 / 即将自动选中第一个时：图区应显示 spinner 而非空态，
   *  否则首屏与切视角会先闪一下「该视角下暂无可展示的对象」。 */
  const pendingAutoSelect =
    !isAggregate && state.n === null && (candidates.length > 0 || !candidatesLoaded);

  // 图的语义内容标识：仅「切换视角 / 选中对象 / 切聚合视图」这类导航动作会改变它，
  // 用于触发 GraphCanvas 重新 fit。单节点就地展开、悬浮、手动缩放平移不计入。
  const fitKey = isAggregate ? `agg:${state.p}` : `obj:${state.p ?? ''}:${state.n ?? ''}`;
  /** 手动「适应屏幕」信号：每次 +1 即让 GraphCanvas 重置为整图 fit。 */
  const [fitSignal, setFitSignal] = useState(0);

  // 边面板关闭时同步丢弃随身边对象，避免下次打开残留上一条边的折叠链。
  useEffect(() => {
    if (inspectEdge === null) setInspectEdgeView(null);
  }, [inspectEdge]);

  // ---------------------------------------------------------- URL 同步
  // URL → state（前进 / 后退 / 外部链接）
  useEffect(() => {
    const next = decodeViewState(params.toString());
    setState((prev) => (sameViewState(prev, next) ? prev : next));
  }, [params]);

  // state → URL（只写差异，避免污染历史栈）
  useEffect(() => {
    const search = encodeViewState(state);
    if (search === params.toString()) return;
    if (search === lastPushed.current) return;
    lastPushed.current = search;
    setParams(new URLSearchParams(search), { replace: false });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);

  // 视角未指定 / 非法 → 选第一个有数据的视角
  useEffect(() => {
    if (perspectives.length === 0) return;
    const fixed = reconcileViewState(
      state,
      perspectives.map((p) => ({ id: p.id, mode: p.mode, available: p.available })),
    );
    if (!sameViewState(fixed, state)) setState(fixed);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [perspectives]);

  // 载入二级候选（带防抖的服务端搜索）。按需加载：已持有当前视角的候选则跳过；
  // 已经选中节点、且下拉未展开时也不预取——把那次昂贵的全量打分推迟到用户真正要选对象时。
  useEffect(() => {
    if (!state.p || isAggregate) {
      setCandidateBundle({ p: state.p ?? '', list: [] });
      setCandidatesLoaded(false);
      return;
    }
    if (candidateBundle.p === state.p) return;
    if (state.n !== null && !dropdownOpen) return;
    const perspective = state.p;
    let alive = true;
    setCandidatesLoaded(false);
    const start = () => {
      void viewApi
        .candidates(id, perspective, 300, candidateSearch)
        .then((list) => {
          if (alive) {
            setCandidateBundle({ p: perspective, list });
            setCandidatesLoaded(true);
          }
        })
        .catch(() => {
          if (alive) setCandidatesLoaded(true);
        });
    };
    // 还没选节点时，候选要用来「自动选中第一个对象」——尽快拿到，不要防抖；
    // 仅当用户主动展开下拉时才防抖，避免每次切视角都无谓打一次。
    if (state.n === null) {
      start();
      return () => {
        alive = false;
      };
    }
    const timer = setTimeout(start, 150);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.p, isAggregate, id, candidateSearch, dropdownOpen, state.n, candidateBundle.p]);

  // 对象视角：只在「完全没指定中心」时取第一个候选作为默认值。
  // 已经明确导航到某个节点时**绝不覆盖**——候选列表有上限（300）且可能被过滤，
  // 拿它当"节点是否存在"的判据会把刚点进来的节点误判为不存在、静默换成第一个候选。
  useEffect(() => {
    if (isAggregate || state.n !== null) return;
    if (candidates.length === 0 || candidateSearch !== '') return;
    setState((s) => ({ ...s, n: candidates[0].id }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [candidates, isAggregate, candidateSearch, state.n]);

  // 视图恒为**折叠模式**：折叠时后端会把语法节点收进边的 `via` 链并内联每一跳的调用处，
  // 点边即可逐跳核对；而"展开全部语法节点"是**信息降级**——画了 Method/CallSite，
  // 却丢掉了 via 与每跳调用处，还把图撑成多层单行、要横向滚好几屏。
  const { view, loading, error: objectError } = useObjectView(
    id,
    state.p ?? undefined,
    state.n ?? undefined,
    state.d,
  );
  const { view: aggView, loading: aggLoading } = useAggregateView(
    id,
    isAggregate ? (state.p ?? undefined) : undefined,
    12,
  );

  // ---------------------------------------------------------- 导航
  const pushTrail = useCallback(
    (p: string, node: number | null, nodeName: string) => {
      const label = perspectives.find((x) => x.id === p)?.label ?? p;
      setTrail((prev) => {
        const next = [...prev, { perspective: p, label, node, nodeName }];
        return next.slice(-8);
      });
    },
    [perspectives],
  );

  /** 折叠模式下点击语义节点 → 拉取其局部语法调用子图并就地展开（再次点击收起）。 */
  const toggleExpand = useCallback(
    (nodeId: number) => {
      if (expanded[nodeId]) {
        setExpanded((prev) => {
          const nxt = { ...prev };
          delete nxt[nodeId];
          return nxt;
        });
        return;
      }
      setExpandingId(nodeId);
      // 同样取**折叠**子图：子图里每条边都带 `via` 与每跳调用处，点边即可展开链路。
      void viewApi
        .object(id, state.p ?? 'route', nodeId, expandDepth)
        .then((ov) => {
          setExpanded((prev) => ({
            ...prev,
            [nodeId]: {
              nodes: [ov.center, ...ov.rings.flat()].map(toCanvas),
              edges: ov.edges,
            },
          }));
        })
        .catch(() => {})
        .finally(() => setExpandingId((cur) => (cur === nodeId ? null : cur)));
    },
    [expanded, id, state.p, expandDepth],
  );

  /**
   * 点击节点：
   * * 该节点**有对应视角** → **一级视角切到它、二级对象设为该节点**（"点击即切"）；
   * * 没有对应视角 → 折叠模式下就地展开调用链，并只打开 Inspector。
   */
  const handleNodeClick = useCallback(
    (nodeId: number, _kind: string, ownView: string | null) => {
      if (!ownView) {
        // `ConfigKey` 等语义资产没有"单链路"视角：不切顶部筛选器，只打开 Inspector；
        // 顺带就地展开它的折叠子图（边带 via 链，点边可逐跳核对）
        toggleExpand(nodeId);
        setInspectNode(nodeId);
        setInspectEdge(null);
        setState((s) => ({ ...s, i: nodeId, e: null }));
        return;
      }
      if (state.n !== null && state.n !== nodeId) {
        const center = view?.center;
        if (center) setOrigin({ id: center.id, name: center.name });
      }
      const nodeName =
        view?.rings.flat().find((n) => n.id === nodeId)?.name ??
        (view?.center.id === nodeId ? view.center.name : `#${nodeId}`);
      pushTrail(ownView, nodeId, nodeName);
      // 关键：**一级视角也要切**。只设 `n` 的话该节点不属于当前视角的候选，
      // 会被 `reconcileViewState` 清空，导航实际失效。
      setState((s) => ({ ...s, p: ownView, n: nodeId, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
      setExpanded({});
    },
    [state.n, view, pushTrail, toggleExpand],
  );

  const handleEdgeClick = useCallback((edge: EdgeView) => {
    setInspectEdge(edge.id);
    setInspectEdgeView(edge);
    setInspectNode(null);
  }, []);

  const onTrailClick = useCallback(
    (index: number) => {
      const item = trail[index];
      if (!item) return;
      setTrail((prev) => prev.slice(0, index + 1));
      setState((s) => ({ ...s, p: item.perspective, n: item.node, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
      setOrigin(null);
    },
    [trail],
  );

  // 折叠模式下，把"按需展开的语法子图"合并进当前语义图（按锚点环号偏移，避免重排）。
  const merged = useMemo(() => {
    if (!view) return { center: null as CanvasNode | null, rings: [] as CanvasNode[][], edges: [] as EdgeView[] };
    const base: CanvasNode[] = [view.center, ...view.rings.flat()].map(toCanvas);
    const ringOf = new Map(base.map((n) => [n.id, n.ring]));
    const nodes: CanvasNode[] = [...base];
    const edges: EdgeView[] = [...view.edges];
    const seen = new Set(nodes.map((n) => n.id));
    for (const [anchorId, ctx] of Object.entries(expanded)) {
      const anchorRing = ringOf.get(Number(anchorId)) ?? 0;
      for (const n of ctx.nodes) {
        if (seen.has(n.id)) continue;
        seen.add(n.id);
        nodes.push({ ...n, ring: anchorRing + n.ring });
      }
      for (const e of ctx.edges) edges.push(e);
    }
    const maxRing = nodes.reduce((m, n) => Math.max(m, n.ring), 0);
    const rings: CanvasNode[][] = Array.from({ length: maxRing + 1 }, () => []);
    for (const n of nodes) if (n.ring > 0) rings[n.ring].push(n);
    return { center: toCanvas(view.center), rings, edges };
  }, [view, expanded]);

  /** 端点 id → 名字：折叠链要把首尾两个语义节点也标出名字。 */
  const nodeNameOf = useCallback(
    (nid: number) => {
      const c = merged.center;
      if (c && c.id === nid) return c.name;
      for (const ring of merged.rings) {
        const hit = ring.find((n) => n.id === nid);
        if (hit) return hit.name;
      }
      return `#${nid}`;
    },
    [merged],
  );

  /**
   * 二级筛选器的显示名兜底。
   *
   * 「点击图中的节点切视角」是**直接给节点 id**（不经候选列表），而候选是**按需加载**的
   * （已选中节点且未展开下拉时刻意不预取）—— 于是下拉里找不到匹配 `value` 的选项，
   * antd 会把 value 原样渲染成裸 id（如 `57601`），看起来像筛选器坏了。
   *
   * 这里从当前视图中心 / 面包屑取名字兜底；候选里已命中时返回 null（不生效），
   * 因此不改变既有的按需加载策略与候选排序。
   */
  const selectedNodeName = useMemo(() => {
    if (state.n === null) return null;
    if (candidates.some((c) => c.id === state.n)) return null;
    // 点图导航后 `view` 仍是上一视角的数据（`useAsync` 保留旧值），所以先查面包屑再查中心。
    for (let i = trail.length - 1; i >= 0; i -= 1) {
      if (trail[i].node === state.n && trail[i].nodeName) return trail[i].nodeName;
    }
    return view?.center.id === state.n ? view.center.name : null;
  }, [state.n, candidates, trail, view]);

  // 首次进入时把当前位置压入面包屑
  useEffect(() => {
    if (!state.p || trail.length > 0) return;
    const name = view?.center?.name ?? candidates.find((c) => c.id === state.n)?.name ?? '';
    pushTrail(state.p, state.n, name);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.p, state.n, view, candidates]);

  const clusters: CanvasCluster[] = useMemo(
    () =>
      (aggView?.clusters ?? []).map((c) => ({
        key: c.key,
        label: c.label,
        count: c.count,
        members: c.members.map((m) => ({ id: m.id, kind: m.kind, name: m.name, ring: 1 })),
      })),
    [aggView],
  );
  const matrix: CanvasMatrix | undefined = aggView?.matrix
    ? { rows: aggView.matrix.rows, cols: aggView.matrix.cols, cells: aggView.matrix.cells }
    : undefined;

  const layoutMode: LayoutMode = state.m ?? current?.layout ?? 'radial';
  // 暂时注释：本地根模板 / WSL 模式 / 按工程覆盖 三处设置只在「跳转 IDE」时才需要，
  // 跳转入口已移除，复制绝对路径直接用后端 root_path 即可（以后再考虑加回）。
  //
  // // 按工程覆盖：优先级高于全局根模板，用于模板表达不了的特例；只影响 IDE 跳转与复制，不改后端数据。
  // const rootStorageKey = `em.projectRootOverride.${id}`;
  // const [localRoot, setLocalRoot] = useState<string>(() => {
  //   try {
  //     return localStorage.getItem(rootStorageKey) ?? '';
  //   } catch {
  //     return '';
  //   }
  // });
  // const updateLocalRoot = (value: string) => {
  //   const trimmed = value.trim();
  //   setLocalRoot(trimmed);
  //   try {
  //     if (trimmed) localStorage.setItem(rootStorageKey, trimmed);
  //     else localStorage.removeItem(rootStorageKey);
  //   } catch {
  //     /* ignore */
  //   }
  // };
  // // 本地工程根只影响 IDE 跳转与复制，不改后端数据。
  // // 解析优先级：按工程覆盖（localRoot） > 全局根模板 / WSL 预设（设置页）> 后端 root_path。
  // const projectRoot = resolveProjectRoot(project?.root_path, localRoot, effectiveTemplate());
  // // WSL 模式开启时把 distro 透传给跳转 / 复制逻辑，生成正确的远程 scheme 与 UNC 前缀。
  // const wslDistro = getWslMode() ? getWslDistro() : undefined;
  // // 仅用于界面核对：本工程实际生效根来自哪一层（覆盖 > 模板 / WSL > 后端）。
  // const rootSource = localRoot
  //   ? t('按工程覆盖')
  //   : getWslMode()
  //     ? t('WSL 模式') + '（' + (getWslDistro() || 'Ubuntu') + '）'
  //     : effectiveTemplate()
  //       ? t('全局根模板')
  //       : t('后端 root_path');

  // 复制绝对路径用的本地根：暂不做模板 / WSL / 覆盖变换，直接用后端 root_path。
  const projectRoot = project?.root_path;
  // WSL 映射停用（见上）；Inspector 仍接收该 prop，留空即可，恢复时改回 getWslMode() 计算。
  const wslDistro: string | undefined = undefined;

  if (projectId === undefined || Number.isNaN(id)) {
    return <Alert type="error" message={t('缺少工程 ID')} />;
  }

  return (
    <>
      <PageHeader
        title={project ? `${t('图视图')} · ${project.name}` : t('图视图')}
        subtitle={t('一级选视角、二级选对象；只渲染当前这一条链路，被省略的部分以计数与未解析记账呈现')}
        extra={<RunPipelineButton projectId={id} onStarted={() => void reloadProject()} />}
      />

      <Card variant="borderless" style={{ borderRadius: 14, marginBottom: 12 }}>
        <PerspectivePicker
          perspectives={perspectives}
          perspective={state.p}
          onPerspectiveChange={(p) => {
            pushTrail(p, null, '');
            setCandidateSearch('');
            setState((s) => ({ ...s, p, n: null, i: null, e: null }));
            setOrigin(null);
          }}
          candidates={candidates}
          node={state.n}
          onNodeChange={(n) => {
            const name = candidates.find((c) => c.id === n)?.name ?? '';
            pushTrail(state.p ?? '', n, name);
            setState((s) => ({ ...s, n, i: null, e: null }));
          }}
          onSearch={setCandidateSearch}
          onDropdownVisibleChange={setDropdownOpen}
          nodeName={selectedNodeName}
          layout={state.m}
          onLayoutChange={(m) => setState((s) => ({ ...s, m }))}
          trail={trail}
          onTrailClick={onTrailClick}
          loading={loading && candidates.length === 0}
        />
        <Space style={{ marginTop: 8 }} align="center" wrap>
          <Tooltip title={t('就地展开调用链时向下钻取的跳数（多跳展开，而非仅相邻一环）；只影响点击节点时插入的折叠子图')}>
            <Space size={6} align="center">
              <Typography.Text type="secondary">{t('展开跳数')}</Typography.Text>
              <InputNumber
                size="small"
                min={1}
                max={4}
                value={expandDepth}
                onChange={(v) => setExpandDepth(typeof v === 'number' && v >= 1 ? v : 1)}
                style={{ width: 64 }}
              />
            </Space>
          </Tooltip>
          <Tooltip title={t('在边上标注 ReadsConfig / MapsTo 等类型')}>
            <Space size={6} align="center">
              <Switch size="small" checked={showEdgeLabels} onChange={setShowEdgeLabels} />
              <Typography.Text type="secondary">{t('边类型')}</Typography.Text>
            </Space>
          </Tooltip>
          <Tooltip title={t('重置缩放与平移，使整张图完整显示在当前视窗内')}>
            <Button size="small" icon={<FullscreenOutlined />} onClick={() => setFitSignal((s) => s + 1)}>
              {t('适应屏幕')}
            </Button>
          </Tooltip>
          <Button size="small" type="primary" ghost onClick={() => setDrawerOpen(true)}>
            {t('结论 / 导航')}
          </Button>
          {Object.keys(expanded).length > 0 && (
            <Button size="small" onClick={() => setExpanded({})}>
              {t('收起调用') + '（' + Object.keys(expanded).length + '）'}
            </Button>
          )}
        </Space>
        {/* 暂时注释：IDE 打开入口已移除，按工程覆盖本地根与「当前生效根」展示一并停用（以后再考虑加回）。
        <Collapse
          ghost
          bordered={false}
          defaultActiveKey={[]}
          style={{ marginTop: 8, maxWidth: 760 }}
          items={[
            {
              key: 'override',
              label: (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('高级 · 按工程覆盖本地根（特殊场景才需要）')}
                </Typography.Text>
              ),
              children: (
                <Space align="center" wrap>
                  <Tooltip title={t('本地工程根仅用于 IDE 跳转与复制，不改后端数据。优先级：此处「按工程覆盖」> 全局根模板（设置页）> 后端 root_path。留空即按后两者解析。')}>
                    <Typography.Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                      {t('本地工程根（覆盖）')}
                    </Typography.Text>
                  </Tooltip>
                  <Input
                    size="small"
                    style={{ width: 420 }}
                    placeholder={project?.root_path ?? t('按工程覆盖的本地绝对路径，留空则取全局模板 / 后端路径')}
                    value={localRoot}
                    onChange={(e) => updateLocalRoot(e.target.value)}
                  />
                  {localRoot ? (
                    <Button size="small" type="link" onClick={() => updateLocalRoot('')}>
                      {t('用全局 / 后端路径')}
                    </Button>
                  ) : null}
                  <Button size="small" type="link" onClick={() => navigate('/settings')}>
                    {t('全局根模板设置')}
                  </Button>
                </Space>
              ),
            },
          ]}
        />
        <Typography.Paragraph
          type="secondary"
          style={{ fontSize: 12, marginTop: 4, marginBottom: 0 }}
        >
          {t('本工程当前生效根（来源：') + rootSource + t('）：')}
          <Typography.Text code style={{ fontSize: 12 }}>
            {projectRoot || t('（无法解析，请检查后端 root_path 或上方覆盖）')}
          </Typography.Text>
        </Typography.Paragraph>
        */}
      </Card>

      {isAggregate && aggView?.notice ? (
        <Alert type="info" showIcon style={{ marginBottom: 16 }} message={aggView.notice} />
      ) : null}

      {/* 诚实性守门：这个对象在当前视角下取不到时，如实告知并给一条出路，
          而不是悄悄把中心换成第一个候选（那等于展示一张无关的图）。 */}
      {!isAggregate && state.n !== null && !loading && objectError ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          message={t('对象 #') + state.n + t(' 在「') + (current?.label ?? state.p) + t('」视角下取不到链路')}
          description={t('可能已被删除、或不属于该视角（') + errText(objectError) + t('）。请在左侧一级视角重新选择。')}
          action={
            candidates.length > 0 ? (
              <Button size="small" onClick={() => setState((s) => ({ ...s, n: candidates[0].id }))}>
                {t('换第一个对象')}
              </Button>
            ) : null
          }
        />
      ) : null}

      <Row gutter={[16, 16]}>
        <Col xs={24} xl={24}>
          <GraphCanvas
            mode={layoutMode}
            center={merged.center}
            rings={merged.rings}
            edges={merged.edges}
            clusters={isAggregate ? clusters : undefined}
            matrix={isAggregate ? matrix : undefined}
            loading={loading || aggLoading || expandingId !== null || pendingAutoSelect}
            originId={origin?.id ?? null}
            selectedId={state.n}
            onNodeClick={handleNodeClick}
            onNodeContextMenu={(nodeId) => {
              setInspectNode(nodeId);
              setInspectEdge(null);
              setState((s) => ({ ...s, i: nodeId, e: null }));
            }}
            onEdgeClick={handleEdgeClick}
            showEdgeLabels={showEdgeLabels}
            fitKey={fitKey}
            fitSignal={fitSignal}
            />

          {/* 诚实性守门：省略了什么、为什么省略 */}
          {view ? (
            <Card variant="borderless" style={{ borderRadius: 14, marginTop: 16 }} size="small">
              <Space direction="vertical" size={6} style={{ width: '100%' }}>
                <Typography.Text style={{ fontSize: 13 }}>{view.hidden.note}</Typography.Text>
                <Space size={6} wrap>
                  {Object.entries(view.hidden.by_kind).map(([k, v]) => (
                    <Tag key={k}>
                      {k} {v}
                    </Tag>
                  ))}
                </Space>
              </Space>
            </Card>
          ) : null}

          {view && view.unresolved.length > 0 ? (
            <Card
              variant="borderless"
              style={{ borderRadius: 14, marginTop: 16 }}
              size="small"
              title={t('未解析记账')}
              extra={<Tag color="orange">{view.unresolved.length}</Tag>}
            >
              <Table
                size="small"
                rowKey={(_, i) => String(i)}
                dataSource={view.unresolved}
                pagination={false}
                columns={[
                  { title: t('代码'), dataIndex: 'code', width: 170 },
                  { title: t('说明'), dataIndex: 'message' },
                  { title: t('位置'), dataIndex: 'location', width: 220, ellipsis: true },
                ]}
              />
            </Card>
          ) : null}
        </Col>
      </Row>

      <Drawer
        title={t('结论与导航')}
        placement="right"
        width={360}
        open={drawerOpen}
        onClose={() => setDrawerOpen(false)}
        styles={{ body: { padding: 16 } }}
      >
        <Card variant="borderless" style={{ borderRadius: 14 }} title={t('结论')}>
          {view ? (
            <Space direction="vertical" size={10} style={{ width: '100%' }}>
              <Row gutter={12}>
                <Col span={12}>
                  <Statistic title={t('入边')} value={fmt(view.conclusions['入边'])} />
                </Col>
                <Col span={12}>
                  <Statistic title={t('出边')} value={fmt(view.conclusions['出边'])} />
                </Col>
              </Row>
              <Space size={6} wrap>
                {Array.from(new Set(asArray(view.conclusions['标注']))).map((a) => (
                  <Tag key={a} color="volcano">
                    {a}
                  </Tag>
                ))}
              </Space>
              {view.conclusions['schema 列数'] !== undefined ? (
                <Typography.Text type="secondary">
                  {t('schema 列数：') + String(view.conclusions['schema 列数'])}
                </Typography.Text>
              ) : null}
              {view.conclusions['路由表登记'] ? (
                <Typography.Text type="secondary">
                  {t('路由表登记 handler：') + String(view.conclusions['路由表登记'])}
                </Typography.Text>
              ) : null}
            </Space>
          ) : aggView ? (
            <Space direction="vertical" size={6} style={{ width: '100%' }}>
              <Statistic title={t('分组数')} value={aggView.clusters.length} />
              {aggView.matrix ? (
                <Typography.Text type="secondary">
                  {t('共 ') + formatNumber(aggView.matrix.cells.flat().reduce((a, b) => a + b, 0)) + t(' 个单元格取值')}
                </Typography.Text>
              ) : null}
            </Space>
          ) : (
            <Typography.Text type="secondary">{t('选择一个对象后显示结论')}</Typography.Text>
          )}
        </Card>

        <Card
          variant="borderless"
          style={{ borderRadius: 14, marginTop: 16 }}
          size="small"
          title={t('环上节点')}
        >
          <Space direction="vertical" size={4} style={{ width: '100%' }}>
            {(view?.rings ?? []).map((ring, i) => (
              <div key={i}>
                <Typography.Text strong style={{ fontSize: 12 }}>
                  {t('环') + (i + 1) + '（' + ring.length + '）'}
                </Typography.Text>
                <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}>
                  {ring.slice(0, 12).map((n) => (
                    <Tag
                      key={n.id}
                      color={n.has_own_view ? 'blue' : 'default'}
                      style={{ cursor: 'pointer' }}
                      onClick={() => handleNodeClick(n.id, n.kind, n.own_view)}
                    >
                      {n.name.slice(0, 24)}
                    </Tag>
                  ))}
                  {ring.length > 12 ? <Tag>+{ring.length - 12}</Tag> : null}
                </div>
              </div>
            ))}
          </Space>
        </Card>

        <div style={{ marginTop: 16 }}>
          <PipelineProgress run={run} indexing={project?.status === 'indexing'} />
        </div>
      </Drawer>

      <Inspector
        nodeId={inspectNode}
        edgeId={inspectEdge}
        edgeView={inspectEdgeView}
        nodeNameOf={nodeNameOf}
        projectRoot={projectRoot}
        wslDistro={wslDistro}
        onClose={() => {
          setInspectNode(null);
          setInspectEdge(null);
          setState((s) => ({ ...s, i: null, e: null }));
        }}
      />
    </>
  );
}

function toCanvas(n: {
  id: number;
  kind: string;
  name: string;
  ring: number;
  category?: string | null;
  own_view?: string | null;
  /** 悬浮卡片要显示的信息；缺失时用空值兜底。 */
  fqn?: string | null;
  locations?: SourceLocation[];
  annotations?: string[];
  metrics?: { fan_in?: number; fan_out?: number } | null;
}) {
  return {
    id: n.id,
    kind: n.kind,
    category: n.category ?? null,
    own_view: n.own_view ?? null,
    name: n.name,
    ring: n.ring,
    fqn: n.fqn ?? null,
    locations: n.locations ?? [],
    annotations: n.annotations ?? [],
    metrics: n.metrics ?? null,
  };
}

function fmt(v: unknown): string {
  return typeof v === 'number' ? formatNumber(v) : String(v ?? '-');
}

function asArray(v: unknown): string[] {
  return Array.isArray(v) ? v.map(String) : [];
}

function errText(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  return '请求失败';
}
