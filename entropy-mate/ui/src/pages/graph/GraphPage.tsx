import { Alert, Card, Col, Row, Space, Statistic, Table, Tag, Typography } from 'antd';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useParams, useSearchParams } from 'react-router-dom';
import { useProject } from '@/entities/project';
import {
  useAggregateView,
  useObjectView,
  usePerspectives,
  viewApi,
  type Candidate,
  type LayoutMode,
} from '@/entities/view';
import { GraphCanvas, type CanvasCluster, type CanvasMatrix } from '@/widgets/graph-canvas';
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
  const [candidates, setCandidates] = useState<Candidate[]>([]);
  const [inspectNode, setInspectNode] = useState<number | null>(state.i);
  const [inspectEdge, setInspectEdge] = useState<number | null>(state.e);
  const lastPushed = useRef<string>('');

  const current = perspectives.find((p) => p.id === state.p) ?? null;
  const isAggregate = current?.mode === 'aggregate';

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
      candidates,
    );
    if (!sameViewState(fixed, state)) setState(fixed);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [perspectives, candidates]);

  // 载入二级候选
  useEffect(() => {
    if (!state.p || isAggregate) {
      setCandidates([]);
      return;
    }
    let alive = true;
    void viewApi.candidates(id, state.p, 400).then((list) => {
      if (alive) setCandidates(list);
    });
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.p, isAggregate, id]);

  // 对象视角：中心为空时取第一个候选
  useEffect(() => {
    if (isAggregate || candidates.length === 0) return;
    if (state.n === null || !candidates.some((c) => c.id === state.n)) {
      setState((s) => ({ ...s, n: candidates[0].id }));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [candidates, isAggregate]);

  const { view, loading } = useObjectView(id, state.p ?? undefined, state.n ?? undefined, state.d);
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

  /** 点到有视角的节点 → 切换一级 + 二级；没有视角的节点 → 只开 Inspector。 */
  const handleNodeClick = useCallback(
    (nodeId: number, kind: string, hasOwnView: boolean) => {
      if (!hasOwnView) {
        // `ConfigKey` / `KeyPattern` / `Component` / `SecretLocation` 等：
        // 不切顶部筛选器，只打开 Inspector
        setInspectNode(nodeId);
        setInspectEdge(null);
        setState((s) => ({ ...s, i: nodeId, e: null }));
        return;
      }
      const target = nodeId;
      if (state.n !== null && state.n !== target) {
        const center = view?.center;
        if (center) setOrigin({ id: center.id, name: center.name });
      }
      const nodeName =
        view?.rings.flat().find((n) => n.id === target)?.name ??
        (view?.center.id === target ? view.center.name : `#${target}`);
      pushTrail(state.p ?? '', target, nodeName);
      setState((s) => ({ ...s, n: target, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
    },
    [state.p, state.n, view, pushTrail],
  );

  const handleEdgeClick = useCallback((edge: { id: number }) => {
    setInspectEdge(edge.id);
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
  const projectRoot = project?.root_path ?? undefined;

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

      <Card variant="borderless" style={{ borderRadius: 14, marginBottom: 16 }}>
        <PerspectivePicker
          perspectives={perspectives}
          perspective={state.p}
          onPerspectiveChange={(p) => {
            pushTrail(p, null, '');
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
          layout={state.m}
          onLayoutChange={(m) => setState((s) => ({ ...s, m }))}
          trail={trail}
          onTrailClick={onTrailClick}
          loading={loading && candidates.length === 0}
        />
      </Card>

      {isAggregate && aggView?.notice ? (
        <Alert type="info" showIcon style={{ marginBottom: 16 }} message={aggView.notice} />
      ) : null}

      <Row gutter={[16, 16]}>
        <Col xs={24} xl={16}>
          <GraphCanvas
            mode={layoutMode}
            center={view?.center ? toCanvas(view.center) : null}
            rings={(view?.rings ?? []).map((r) => r.map(toCanvas))}
            edges={view?.edges ?? []}
            clusters={isAggregate ? clusters : undefined}
            matrix={isAggregate ? matrix : undefined}
            loading={loading || aggLoading}
            originId={origin?.id ?? null}
            selectedId={state.n}
            onNodeClick={handleNodeClick}
            onNodeContextMenu={(nodeId) => {
              setInspectNode(nodeId);
              setInspectEdge(null);
              setState((s) => ({ ...s, i: nodeId, e: null }));
            }}
            onEdgeClick={handleEdgeClick}
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

        <Col xs={24} xl={8}>
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
                  {asArray(view.conclusions['标注']).map((a) => (
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
                        onClick={() => handleNodeClick(n.id, n.kind, n.has_own_view)}
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
        </Col>
      </Row>

      <Inspector
        nodeId={inspectNode}
        edgeId={inspectEdge}
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

function toCanvas(n: { id: number; kind: string; name: string; ring: number }) {
  return { id: n.id, kind: n.kind, name: n.name, ring: n.ring };
}

function fmt(v: unknown): string {
  return typeof v === 'number' ? formatNumber(v) : String(v ?? '-');
}

function asArray(v: unknown): string[] {
  return Array.isArray(v) ? v.map(String) : [];
}
