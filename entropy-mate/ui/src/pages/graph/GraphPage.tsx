import { Alert, Button, Card, Col, Drawer, Input, Row, Space, Statistic, Switch, Table, Tag, Tooltip, Typography } from 'antd';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
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
import { formatNumber } from '@/shared/lib/format';

/** 图视图页：两级筛选 → 单对象链路子图 → 可跳转的结论面板。 */
export function GraphPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
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
  /** 只有"属于当前视角"的候选才生效；切换视角的瞬间派生为空，等新视角候选到达后才有值。 */
  const candidates = candidateBundle.p === state.p ? candidateBundle.list : [];
  const [candidateSearch, setCandidateSearch] = useState('');
  /** 二级对象下拉是否展开：候选只在展开时才去后端取（按需加载）。
   *  后端 candidates 在无搜索词时会做全量打分（5000 节点 × 每节点 BFS），很慢，
   *  所以已选中节点且未展开下拉时绝不预取，避免每次切视角都白打一次慢查询。 */
  const [dropdownOpen, setDropdownOpen] = useState(false);
  /** 是否展开全部语法节点（默认折叠，只显示语义节点与依赖边）。 */
  const [expandSyntax, setExpandSyntax] = useState(false);
  /** 是否在图上标注边的类型（`ReadsConfig` / `MapsTo`…）。边过多时组件会自动退化为按需标注。 */
  const [showEdgeLabels, setShowEdgeLabels] = useState(true);
  /** 折叠模式下，点击节点后按需展开显示的语法子图（按节点 id 归集）。 */
  const [expanded, setExpanded] = useState<Record<number, { nodes: CanvasNode[]; edges: EdgeView[] }>>({});
  const [expandingId, setExpandingId] = useState<number | null>(null);
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
      return;
    }
    if (candidateBundle.p === state.p) return;
    if (state.n !== null && !dropdownOpen) return;
    const perspective = state.p;
    let alive = true;
    const timer = setTimeout(() => {
      void viewApi.candidates(id, perspective, 300, candidateSearch).then((list) => {
        if (alive) setCandidateBundle({ p: perspective, list });
      });
    }, 150);
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

  const { view, loading, error: objectError } = useObjectView(
    id,
    state.p ?? undefined,
    state.n ?? undefined,
    state.d,
    expandSyntax,
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
      void viewApi
        .object(id, state.p ?? 'route', nodeId, 1, true)
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
    [expanded, id, state.p],
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
        // 折叠模式下顺带就地展开其语法调用链
        if (!expandSyntax) toggleExpand(nodeId);
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
    [state.n, view, pushTrail, expandSyntax, toggleExpand],
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
  // 后端分析的工程根（如 Linux 容器路径）可能和本地开发机路径不一致，
  // 这里允许用户本地覆盖，只影响 IDE 跳转和复制，不改后端数据。
  const rootStorageKey = `em.projectRootOverride.${id}`;
  const [localRoot, setLocalRoot] = useState<string>(() => {
    try {
      return localStorage.getItem(rootStorageKey) ?? '';
    } catch {
      return '';
    }
  });
  const updateLocalRoot = (value: string) => {
    const trimmed = value.trim();
    setLocalRoot(trimmed);
    try {
      if (trimmed) localStorage.setItem(rootStorageKey, trimmed);
      else localStorage.removeItem(rootStorageKey);
    } catch {
      /* ignore */
    }
  };
  const projectRoot = localRoot.trim() || project?.root_path || undefined;

  if (projectId === undefined || Number.isNaN(id)) {
    return <Alert type="error" message="缺少工程 ID" />;
  }

  return (
    <>
      <PageHeader
        title={project ? `图视图 · ${project.name}` : '图视图'}
        subtitle="一级选视角、二级选对象；只渲染当前这一条链路，被省略的部分以计数与未解析记账呈现"
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
          layout={state.m}
          onLayoutChange={(m) => setState((s) => ({ ...s, m }))}
          trail={trail}
          onTrailClick={onTrailClick}
          loading={loading && candidates.length === 0}
        />
        <Space style={{ marginTop: 8 }} align="center" wrap>
          <Tooltip title="展开 Method / CallSite 等语法节点；默认只显示语义节点，点击节点可就地展开其调用链">
            <Space size={6} align="center">
              <Switch size="small" checked={expandSyntax} onChange={setExpandSyntax} />
              <Typography.Text type="secondary">展开语法</Typography.Text>
            </Space>
          </Tooltip>
          <Tooltip title="在边上标注 ReadsConfig / MapsTo 等类型">
            <Space size={6} align="center">
              <Switch size="small" checked={showEdgeLabels} onChange={setShowEdgeLabels} />
              <Typography.Text type="secondary">边类型</Typography.Text>
            </Space>
          </Tooltip>
          <Button size="small" type="primary" ghost onClick={() => setDrawerOpen(true)}>
            结论 / 导航
          </Button>
          {Object.keys(expanded).length > 0 && (
            <Button size="small" onClick={() => setExpanded({})}>
              收起调用（{Object.keys(expanded).length}）
            </Button>
          )}
        </Space>
        <Space style={{ marginTop: 8 }} align="center" wrap>
          <Tooltip title="若后端分析路径与本地不一致（如 Linux/WSL 分析、Windows 本地开发），填写本地绝对路径；留空则使用后端路径">
            <Typography.Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
              本地工程根目录
            </Typography.Text>
          </Tooltip>
          <Input
            size="small"
            style={{ width: 420 }}
            placeholder={project?.root_path ?? '本地绝对路径，例如 C:/Users/.../CRMEB-master'}
            value={localRoot}
            onChange={(e) => updateLocalRoot(e.target.value)}
          />
          {localRoot ? (
            <Button size="small" type="link" onClick={() => updateLocalRoot('')}>
              使用后端路径
            </Button>
          ) : null}
        </Space>
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
          message={`对象 #${state.n} 在「${current?.label ?? state.p}」视角下取不到链路`}
          description={`可能已被删除、或不属于该视角（${errText(objectError)}）。请在左侧一级视角重新选择。`}
          action={
            candidates.length > 0 ? (
              <Button size="small" onClick={() => setState((s) => ({ ...s, n: candidates[0].id }))}>
                换第一个对象
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
            loading={loading || aggLoading || expandingId !== null}
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
              title="未解析记账"
              extra={<Tag color="orange">{view.unresolved.length}</Tag>}
            >
              <Table
                size="small"
                rowKey={(_, i) => String(i)}
                dataSource={view.unresolved}
                pagination={false}
                columns={[
                  { title: '代码', dataIndex: 'code', width: 170 },
                  { title: '说明', dataIndex: 'message' },
                  { title: '位置', dataIndex: 'location', width: 220, ellipsis: true },
                ]}
              />
            </Card>
          ) : null}
        </Col>
      </Row>

      <Drawer
        title="结论与导航"
        placement="right"
        width={360}
        open={drawerOpen}
        onClose={() => setDrawerOpen(false)}
        styles={{ body: { padding: 16 } }}
      >
        <Card variant="borderless" style={{ borderRadius: 14 }} title="结论">
          {view ? (
            <Space direction="vertical" size={10} style={{ width: '100%' }}>
              <Row gutter={12}>
                <Col span={12}>
                  <Statistic title="入边" value={fmt(view.conclusions['入边'])} />
                </Col>
                <Col span={12}>
                  <Statistic title="出边" value={fmt(view.conclusions['出边'])} />
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
                  schema 列数：{String(view.conclusions['schema 列数'])}
                </Typography.Text>
              ) : null}
              {view.conclusions['路由表登记'] ? (
                <Typography.Text type="secondary">
                  路由表登记 handler：{String(view.conclusions['路由表登记'])}
                </Typography.Text>
              ) : null}
            </Space>
          ) : aggView ? (
            <Space direction="vertical" size={6} style={{ width: '100%' }}>
              <Statistic title="分组数" value={aggView.clusters.length} />
              {aggView.matrix ? (
                <Typography.Text type="secondary">
                  共 {formatNumber(aggView.matrix.cells.flat().reduce((a, b) => a + b, 0))} 个单元格取值
                </Typography.Text>
              ) : null}
            </Space>
          ) : (
            <Typography.Text type="secondary">选择一个对象后显示结论</Typography.Text>
          )}
        </Card>

        <Card
          variant="borderless"
          style={{ borderRadius: 14, marginTop: 16 }}
          size="small"
          title="环上节点"
        >
          <Space direction="vertical" size={4} style={{ width: '100%' }}>
            {(view?.rings ?? []).map((ring, i) => (
              <div key={i}>
                <Typography.Text strong style={{ fontSize: 12 }}>
                  环 {i + 1}（{ring.length}）
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
