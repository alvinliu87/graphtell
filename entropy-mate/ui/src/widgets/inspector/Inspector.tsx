import { Alert, Button, Collapse, Descriptions, Drawer, Empty, Space, Tag, Tooltip, Typography } from 'antd';
import { Fragment, useEffect, useMemo, useState } from 'react';
import { DownOutlined, InfoCircleOutlined } from '@ant-design/icons';
import type { EdgeEvidence, EdgeView, NodeLocations, SourceLocation, ViaNode } from '@/entities/view';
import { viewApi } from '@/entities/view';
import { nodeColor } from '@/entities/graph';
import { useAsync } from '@/shared/lib/useAsync';
import { LocationBadge, LocationList } from './LocationList';
import { useLocale } from '@/shared/lib/i18n';

/** 稳定的空 via 引用：`?? []` 每次渲染都会生成新数组，会让折叠链的取数 effect 反复触发。 */
const NO_VIA: ViaNode[] = [];

/**
 * 右侧 Inspector。
 *
 * 两类用途：
 * 1. **没有对应视角的节点**（`ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`）
 *    —— 点它**不切顶部筛选器**，只在这里显示属性与"另有 N 处引用"
 * 2. 边 —— 显示证据链：实边单点、虚线边展开途经的每个 CallSite 位置
 */
export function Inspector({
  nodeId,
  edgeId,
  edgeView,
  nodeNameOf,
  projectRoot,
  wslDistro,
  onClose,
  onJumpToReference,
}: {
  nodeId: number | null;
  edgeId: number | null;
  /**
   * 点击的那条边本身。折叠视图里的"直连"其实是提拉出来的，
   * 中间节点只存在于当次视图结果中（按 id 重查拿不到），所以要把它带进来。
   */
  edgeView?: EdgeView | null;
  /** 端点 id → 名字；用于把折叠链首尾两个语义节点也标出名字。 */
  nodeNameOf?: (id: number) => string;
  projectRoot?: string;
  /** WSL 发行版名；非空时跳转 / 复制按 WSL 处理（远程 scheme + UNC 前缀）。 */
  wslDistro?: string;
  onClose: () => void;
  onJumpToReference?: (nodeId: number) => void;
}) {
  const open = nodeId !== null || edgeId !== null;
  const { t } = useLocale();

  return (
    <Drawer
      title={nodeId !== null ? t('节点详情') : t('边证据链')}
      open={open}
      onClose={onClose}
      width={560}
      destroyOnClose
    >
      {nodeId !== null ? (
        <NodePanel
          nodeId={nodeId}
          projectRoot={projectRoot}
          wslDistro={wslDistro}
          onJumpToReference={onJumpToReference}
        />
      ) : null}
      {edgeId !== null ? (
        <EdgePanel
          edgeId={edgeId}
          edgeView={edgeView}
          nodeNameOf={nodeNameOf}
          projectRoot={projectRoot}
          wslDistro={wslDistro}
          onNodeClick={onJumpToReference}
        />
      ) : null}
    </Drawer>
  );
}

function NodePanel({
  nodeId,
  projectRoot,
  wslDistro,
  onJumpToReference,
}: {
  nodeId: number;
  projectRoot?: string;
  wslDistro?: string;
  onJumpToReference?: (nodeId: number) => void;
}) {
  const { data, loading } = useAsync<NodeLocations | null>(
    () => viewApi.nodeLocations(nodeId),
    [nodeId],
  );
  const { t } = useLocale();

  if (loading) return <Typography.Text type="secondary">{t('加载中…')}</Typography.Text>;
  if (!data) return <Empty description={t('未找到该节点')} />;

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label={t('种类')}>
          <Tag color={nodeColor(data.kind)} style={{ color: '#fff' }}>
            {data.kind}
          </Tag>
        </Descriptions.Item>
        <Descriptions.Item label={t('名称')}>{data.name}</Descriptions.Item>
        <Descriptions.Item label={t('节点类型')}>
          {data.synthetic ? t('合成节点（语义对象）') : t('语法节点')}
        </Descriptions.Item>
        <Descriptions.Item label={t('位置')}>
          <LocationBadge count={data.locations.length} />
        </Descriptions.Item>
        <Descriptions.Item label={t('引用')}>{data.reference_count + t(' 条入边')}</Descriptions.Item>
      </Descriptions>

      {data.synthetic ? (
        <Alert
          type="info"
          showIcon
          message={t('这是合成节点：它由多处共现汇聚而成')}
          description={t('下面列出全部出处，请按需逐条验证；这里不会替你挑一个\'看起来像\'的位置。')}
        />
      ) : null}

      <LocationList locations={data.locations} kind={data.kind} projectRoot={projectRoot} wslDistro={wslDistro} />

      {data.reference_count > 0 ? (
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {t('另有 ') + data.reference_count + t(' 处引用指向它。')}
        </Typography.Text>
      ) : null}
    </Space>
  );
}

function EdgePanel({
  edgeId,
  edgeView,
  nodeNameOf,
  projectRoot,
  wslDistro,
  onNodeClick,
}: {
  edgeId: number;
  edgeView?: EdgeView | null;
  nodeNameOf?: (id: number) => string;
  projectRoot?: string;
  wslDistro?: string;
  onNodeClick?: (id: number) => void;
}) {
  const { data, loading } = useAsync<EdgeEvidence | null>(
    () => viewApi.edgeEvidence(edgeId),
    [edgeId],
  );
  const { t } = useLocale();

  // 这条边折叠掉的中间节点：只存在于点击时的视图结果里（按 id 重查拿不到）。
  const via = edgeView?.via ?? NO_VIA;

  // 负数 id = 折叠视图汇总出的合成边（没有对应的单条原始边），查证据注定查不到。
  // 与其显示"未找到该边"让人以为坏了，不如直说它是什么，并把折叠掉的中间节点逐跳列出来。
  // 先于 loading 判断：这条边的证据请求注定失败，没必要先闪一下"加载中…"。
  if (edgeId < 0) {
    return (
      <Space direction="vertical" size={16} style={{ width: '100%' }}>
        <Alert
          type="info"
          showIcon
          message={t('合成边（折叠汇总）')}
          description={
            via.length > 0
              ? t('这条边是把多条调用链汇总后提拉出的语义边，没有与它一一对应的源码位置；下面是它折叠掉的中间节点（自起点到终点），可据此逐跳核对。')
              : t('这条边是把多条调用链汇总后提拉出的语义边，图里没有与它一一对应的原始边，因此没有逐跳证据可查；打开「展开全部语法节点」可看到原始调用。')
          }
        />
        {via.length > 0 && edgeView ? (
          <CollapsedChain
            edge={edgeView}
            via={via}
            paths={[via]}
            nodeNameOf={nodeNameOf}
            projectRoot={projectRoot}
            wslDistro={wslDistro}
            onNodeClick={onNodeClick}
          />
        ) : null}
      </Space>
    );
  }

  if (loading && !edgeView) return <Typography.Text type="secondary">{t('加载中…')}</Typography.Text>;

  // **以"用户点击的那条边"为准**：它带着 via / hops，状态与置信度也与图上悬浮卡一致。
  // `/edges/{id}/evidence` 返回的是**提拉前的 raw 边**——它的端点、状态、置信度都可能不同，
  // 之前拿它冒充这条边，才出现"悬浮卡 0.80/已解析、抽屉 0.54/待验证"的自相矛盾。
  // 现在只把 raw 边当作"底层位置"的来源，并如实标注，绝不顶替这条边本身。
  const edge = edgeView ?? data?.edge ?? null;
  if (!edge) return <Empty description={t('未找到该边')} />;
  const unresolved = !edge.resolved;

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label={t('关系')}>{edge.kind}</Descriptions.Item>
        <Descriptions.Item label={t('状态')}>
          {unresolved ? <Tag color="orange">{t('status.unverified')}</Tag> : <Tag color="green">{t('status.resolved')}</Tag>}
        </Descriptions.Item>
        {edge.indirect ? (
          <Descriptions.Item label={t('性质')}>
            <Space size={4}>
              <Tag color="gold">{t('间接（沿调用链传播）')}</Tag>
              <Tooltip title={t('indirect.tooltip')}>
                <InfoCircleOutlined style={{ color: '#d48806', cursor: 'help' }} />
              </Tooltip>
            </Space>
          </Descriptions.Item>
        ) : null}
        <Descriptions.Item label={t('置信度')}>{edge.confidence.toFixed(2)}</Descriptions.Item>
        {edge.hops !== null ? (
          <Descriptions.Item label={t('跳数')}>{t('途经 ') + edge.hops + t(' 跳')}</Descriptions.Item>
        ) : null}
      </Descriptions>

      {via.length > 0 ? (
        <CollapsedChain
          edge={edge}
          via={via}
          paths={[via]}
          nodeNameOf={nodeNameOf}
          projectRoot={projectRoot}
          wslDistro={wslDistro}
          onNodeClick={onNodeClick}
        />
      ) : null}

      {data?.reason ? <Alert type="warning" showIcon message={data.reason} /> : null}

      {unresolved && via.length === 0 ? (
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {t('未解析的边是推断结果：下面每个位置都是可亲自验证的落点，核对后再采信。')}
        </Typography.Text>
      ) : null}

      {data && data.via.length > 0 ? (
        <Space direction="vertical" size={4}>
          {data.via.map((v, i) => (
            <Tag key={i}>{v}</Tag>
          ))}
        </Space>
      ) : null}

      {data && data.locations.length > 0 ? (
        <div>
          <Typography.Text strong style={{ fontSize: 13 }}>
            {via.length > 0 ? t('底层原始边（提拉前）的证据位置') : t('证据位置')}
          </Typography.Text>
          {via.length > 0 ? (
            <div style={{ marginTop: 4 }}>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {nodeNameOf?.(data.edge.from) ?? `#${data.edge.from}`}
                {' → '}
                {nodeNameOf?.(data.edge.to) ?? `#${data.edge.to}`}（{data.edge.kind} · 置信度{' '}
                {data.edge.confidence.toFixed(2)} · {data.edge.resolved ? '已解析' : '待验证'}）
              </Typography.Text>
            </div>
          ) : null}
          <div style={{ marginTop: 8 }}>
            <LocationList
              locations={data.locations}
              ordered
              projectRoot={projectRoot}
              wslDistro={wslDistro}
              emptyHint={t('这条边没有可跳转的证据位置（可能来自权威源推断）')}
            />
          </div>
        </div>
      ) : null}
    </Space>
  );
}

/**
 * 折叠链：把"提拉"后被折叠掉的中间节点，按 起点 → 中间各跳 → 终点 逐跳列出。
 *
 * 提拉边在图上看似直连，其实是一条多跳调用链被折叠后的结果。这些中间节点只存在于
 * **当次视图结果**（`EdgeView.via`）里，按边 id 重查是拿不到的 —— 所以必须由点击方随身带入。
 * via 节点只带 id/kind/name，因此每一跳的源码位置要按节点 id 现查，才能给出真正的"调用处"。
 */
function CollapsedChain({
  edge,
  via,
  paths,
  nodeNameOf,
  projectRoot,
  wslDistro,
  onNodeClick,
}: {
  edge: EdgeView;
  /** 兼容旧调用：单条路径。 */
  via?: ViaNode[];
  /** 多条路径（优先）；缺省用 `via` 包成单条。路由到表常有多条调用路径（如直查 `value` 与主列表 `getGoodsList`），用此字段呈现分叉。 */
  paths?: ViaNode[][];
  nodeNameOf?: (id: number) => string;
  projectRoot?: string;
  wslDistro?: string;
  /** 点击某跳的节点名 → 在主图中以该节点为中心重绘。 */
  onNodeClick?: (id: number) => void;
}) {
  const { t } = useLocale();
  const name = (id: number) => nodeNameOf?.(id) ?? `#${id}`;
  const pathList = paths && paths.length > 0 ? paths : via ? [via] : [];

  // 所有路径上的节点都要取位置（含起止），避免第一跳"调用处"断头。
  const allIds = useMemo(() => {
    const s = new Set<number>([edge.from, edge.to]);
    pathList.forEach((p) => p.forEach((v) => s.add(v.id)));
    return [...s];
  }, [pathList, edge.from, edge.to]);

  // 存整个 `NodeLocations`：需要 `synthetic` 来决定如何标注"定义处"。
  // 共享节点（ConfigKey / Table / Cache …）由同键多处共现合成，
  // 它的**全部出处并不都属于当前链路** —— 混着展示会让人以为链路串到了无关文件。
  //
  // 优先用后端**内联**在 `edge.node_locations` 里的位置（折叠视图链路是临时提拉的，
  // 中间跳按边 id 重查不到，所以后端一次给全）。只对缺失的节点回退到原接口，
  // 避免对每一跳都发一次 `/nodes/{id}/locations`（N+1）。
  const inline = useMemo(() => {
    const m: Record<number, NodeLocations> = {};
    for (const e of edge.node_locations ?? []) {
      m[e.id] = {
        id: e.id,
        kind: '',
        name: '',
        synthetic: e.synthetic,
        locations: e.locations,
        reference_count: 0,
      };
    }
    return m;
  }, [edge.node_locations]);

  const missingKey = allIds.filter((id) => !(id in inline)).join(',');

  const [fetched, setFetched] = useState<Record<number, NodeLocations>>({});
  useEffect(() => {
    const missing = missingKey ? missingKey.split(',').map(Number) : [];
    if (missing.length === 0) return;
    let alive = true;
    void Promise.all(
      missing.map((id) =>
        viewApi
          .nodeLocations(id)
          .then((r) => [id, r] as const)
          .catch(() => [id, null] as const),
      ),
    ).then((pairs) => {
      if (!alive) return;
      const next: Record<number, NodeLocations> = {};
      for (const [id, r] of pairs) {
        if (r) next[id] = r;
      }
      setFetched(next);
    });
    return () => {
      alive = false;
    };
  }, [missingKey]);

  // 内联数据优先；仅补上后端没有内联的节点。
  const locs = useMemo(() => ({ ...fetched, ...inline }), [fetched, inline]);

  type Step = {
    key: string;
    id: number;
    kind: string | null;
    name: string;
    role: string | null;
    locations: SourceLocation[];
    callSite: SourceLocation | null;
  };

  const renderPath = (p: ViaNode[]) => {
    const steps: Step[] = [
      { key: `from-${edge.from}`, id: edge.from, kind: null, name: name(edge.from), role: '起点', locations: locs[edge.from]?.locations ?? [], callSite: null },
      ...p.map((v) => ({
        key: `via-${v.id}`,
        id: v.id,
        kind: v.kind,
        name: v.name,
        role: null as string | null,
        locations: locs[v.id]?.locations ?? [],
        callSite: v.call_site ?? null,
      })),
      { key: `to-${edge.to}`, id: edge.to, kind: null, name: name(edge.to), role: '终点', locations: locs[edge.to]?.locations ?? [], callSite: edge.to_call_site ?? null },
    ];
    return (
      <div>
        {steps.map((s, i) => (
          <Fragment key={s.key}>
            <div style={{ display: 'flex', alignItems: 'center', gap: 8, padding: '4px 2px' }}>
              <Tag
                color={s.kind ? nodeColor(s.kind) : 'blue'}
                style={s.kind ? { color: '#fff' } : undefined}
              >
                {s.kind ?? t(s.role ?? '')}
              </Tag>
              <Button
                type="link"
                size="small"
                style={{
                  padding: 0,
                  height: 'auto',
                  fontSize: 13,
                  wordBreak: 'break-all',
                  textAlign: 'left',
                  whiteSpace: 'normal',
                }}
                onClick={() => onNodeClick?.(s.id)}
                title={t('在主图中以该节点为中心重绘')}
              >
                {s.name}
              </Button>
            </div>
            {(() => {
              const synthetic = locs[s.id]?.synthetic ?? false;
              const cs = s.callSite;
              const sameAsCallSite = (l: SourceLocation) =>
                !!cs && l.file === cs.file && l.line === cs.line;
              // 共享资源（ConfigKey / Table / Cache…）的"全部出处"**并不都属于当前链路**：
              // 与"调用处"重合的那条已在下面单独显示，这里只列**其余**出处，
              // 并默认折叠 —— 铺开会让人误以为"链路串到了无关文件"。
              const rest =
                synthetic && cs ? s.locations.filter((l) => !sameAsCallSite(l)) : s.locations;
              if (rest.length === 0) return null;
              if (synthetic && cs) {
                return (
                  <Collapse
                    size="small"
                    ghost
                    style={{ marginTop: 4 }}
                    items={[
                      {
                        key: 'rest',
                        label: (
                          <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                            {t('该资源的其他 ') + rest.length + t(' 处读取点（与当前链路无关）')}
                          </Typography.Text>
                        ),
                        children: (
                          <LocationList
                            locations={rest}
                            kind={s.kind ?? undefined}
                            projectRoot={projectRoot}
                            wslDistro={wslDistro}
                            showCopyAll={false}
                          />
                        ),
                      },
                    ]}
                  />
                );
              }
              return (
                <div style={{ marginTop: 4 }}>
                  <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                    {synthetic ? t('全部出处（共享节点）') : t('定义处')}
                  </Typography.Text>
                  <LocationList
                    locations={rest}
                    kind={s.kind ?? undefined}
                    projectRoot={projectRoot}
                    wslDistro={wslDistro}
                    showCopyAll={false}
                  />
                </div>
              );
            })()}
            {s.callSite ? (
              <div style={{ marginTop: 4 }}>
                <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                  {t('调用处')}
                </Typography.Text>
                <LocationList
                  locations={[s.callSite]}
                  kind={s.kind ?? undefined}
                  projectRoot={projectRoot}
                  wslDistro={wslDistro}
                  showCopyAll={false}
                />
              </div>
            ) : null}
            {i < steps.length - 1 ? (
              <div style={{ display: 'flex', justifyContent: 'center', color: '#94a3b8', padding: '2px 0' }}>
                <DownOutlined />
              </div>
            ) : null}
          </Fragment>
        ))}
      </div>
    );
  };

  if (pathList.length === 0) return null;

  return (
    <div>
      <Typography.Text strong style={{ fontSize: 13 }}>
        {t('折叠掉的调用链')}
        {pathList.length > 1
          ? `（${(pathList.length) + t(' 条路径')}）`
          : `（${t('（起止各 1 个 + 中间 ') + pathList[0].length + t(' 跳）')}）`}
      </Typography.Text>
      <div style={{ marginTop: 8 }}>
        {pathList.length === 1 ? (
          renderPath(pathList[0])
        ) : (
          <Collapse
            defaultActiveKey={['0']}
            size="small"
            items={pathList.map((p, idx) => ({
              key: String(idx),
              label: `${t('路径 ') + (idx + 1)}`,
              children: renderPath(p),
            }))}
          />
        )}
      </div>
    </div>
  );
}
