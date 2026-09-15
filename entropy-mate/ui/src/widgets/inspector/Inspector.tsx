import { Alert, Button, Collapse, Descriptions, Drawer, Empty, Space, Tag, Typography } from 'antd';
import { Fragment, useEffect, useMemo, useState } from 'react';
import { DownOutlined } from '@ant-design/icons';
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
          {unresolved ? <Tag color="orange">{t('待验证假设（虚线）')}</Tag> : <Tag color="green">{t('已解析（实线）')}</Tag>}
        </Descriptions.Item>
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
          {t('虚线边是推断结果：下面每个位置都是可亲自验证的落点，核对后再采信。')}
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

  const [locs, setLocs] = useState<Record<number, SourceLocation[]>>({});
  useEffect(() => {
    let alive = true;
    void Promise.all(
      allIds.map((id) =>
        viewApi
          .nodeLocations(id)
          .then((r) => [id, r.locations] as const)
          .catch(() => [id, [] as SourceLocation[]] as const),
      ),
    ).then((pairs) => {
      if (alive) setLocs(Object.fromEntries(pairs));
    });
    return () => {
      alive = false;
    };
  }, [allIds]);

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
      { key: `from-${edge.from}`, id: edge.from, kind: null, name: name(edge.from), role: '起点', locations: locs[edge.from] ?? [], callSite: null },
      ...p.map((v) => ({
        key: `via-${v.id}`,
        id: v.id,
        kind: v.kind,
        name: v.name,
        role: null as string | null,
        locations: locs[v.id] ?? [],
        callSite: v.call_site ?? null,
      })),
      { key: `to-${edge.to}`, id: edge.to, kind: null, name: name(edge.to), role: '终点', locations: locs[edge.to] ?? [], callSite: edge.to_call_site ?? null },
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
            {s.locations.length > 0 ? (
              <div style={{ marginTop: 4 }}>
                <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                  {t('定义处')}
                </Typography.Text>
                <LocationList
                  locations={s.locations}
                  kind={s.kind ?? undefined}
                  projectRoot={projectRoot}
                  wslDistro={wslDistro}
                  showCopyAll={false}
                />
              </div>
            ) : null}
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
