import { Alert, Button, Collapse, Descriptions, Drawer, Empty, Space, Tag, Timeline, Tooltip, Typography } from 'antd';
import { Fragment, useEffect, useMemo, useState, type ReactNode } from 'react';
import { CopyOutlined, InfoCircleOutlined } from '@ant-design/icons';
import type { EdgeEvidence, EdgeView, NodeLocations, SourceLocation, ViaNode } from '@/entities/view';
import { viewApi } from '@/entities/view';
import { nodeColor } from '@/entities/graph';
import { copyPath } from '@/shared/lib/ide';
import { useAsync } from '@/shared/lib/useAsync';
import { LocationBadge, LocationList } from './LocationList';
import { useLocale } from '@/shared/lib/i18n';

/** 稳定的空 via 引用：`?? []` 每次渲染都会生成新数组，会让折叠链的取数 effect 反复触发。 */
const NO_VIA: ViaNode[] = [];

/** Inspector 纵向 / 横向节奏统一间距，避免散落 magic number。 */
const SP = {
  /** 大区块之间：Descriptions ↔ 链 ↔ Alert 等（外层 Space）。 */
  block: 16,
  /** 小节标题与其内容的间距：如「折叠掉的调用链」↔ 时间线。 */
  section: 14,
  /** 时间线各跳之间。 */
  step: 12,
  /** 跳内分行：节点名 ↔ 位置块、标签行之间。 */
  row: 8,
  /** 最紧凑：同标签下多个路径之间。 */
  tight: 4,
  /** 节点名前 Tag ↔ 节点名。 */
  tagGap: 6,
} as const;

/** 节点名前 Tag 的最小宽度；同时作为下方「调用语句 / 定义复制按钮」相对节点名左缘的缩进基准。 */
const TAG_W = 64;
/** 下方调用语句、定义复制按钮统一缩进到与节点名同列：NAME_INDENT = TAG_W + tagGap。 */
const NAME_INDENT = TAG_W + SP.tagGap; // 64 + 6 = 70

/** 节点名文字色：中性近黑而非彩色，避免与「Tag 的 kind 色」和「文件名的蓝 Link」堆叠出过多颜色。 */
const NODE_NAME_COLOR = '#1f2937';
/** 时间线圆点色：统一中性灰，不再按 kind 上色（kind 已由 Tag 表达），减少整屏色彩。 */
const TIMELINE_DOT_COLOR = '#94a3b8';

/**
 * 右侧 Inspector。
 *
 * 两类用途：
 * 1. **没有对应视角的节点**（`ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`）
 *    —— 点它**不切顶部筛选器**，只在这里显示属性与"另有 N 处引用"
 * 2. 边 —— 显示证据链：一律按 起点 → 各跳 → 终点（语义节点）列出，直达边即为 起点/终点 两跳
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
    <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
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
          message={t('这是合成节点：它由多处来源汇聚而成')}
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

  // 负数 id = 折叠视图汇总出的合成边（没有对应的单条原始边），按 id 查证据注定查不到。
  // 与其显示"未找到该边"让人以为坏了，不如直说它是什么，并把折叠掉的中间节点逐跳列出来。
  // 先于 loading 判断：这条边的证据请求注定失败，没必要先闪一下"加载中…"。
  //
  // 但"合成"**不等于"没有证据"**：反向视角（资源类中心）的边一律是负 id，其中不少就是
  // 一条真实的直接边（如 `paySuccess --Triggers--> 事件`），它内联了触发点（`to_call_site`）
  // 与端点位置（`node_locations`） —— 这些必须照常渲染，不能一句"没有证据"就盖过去。
  // 只有三者全无时才是真的无据可查。
  const hasOwnEvidence = !!edgeView?.to_call_site || (edgeView?.node_locations?.length ?? 0) > 0;
  if (edgeId < 0 && (via.length > 0 || !hasOwnEvidence)) {
    return (
      <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
        <Alert
          type="info"
          showIcon
          message={t('合成边（折叠汇总）')}
          description={
            via.length > 0
              ? t('这条边是把多条调用链聚合后归纳出的语义边，没有与之对应的单一源码位置；下面是被它折叠的中间节点（自起点到终点），可据此逐跳核对。')
              : t('这条边是把多条调用链聚合后归纳出的语义边，图里没有与之对应的单条直接边，也没有可定位的触发点，因此没有逐跳证据可查。')
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
  // 证据位置的来源：优先用后端 `/edges/{id}/evidence`（真实边）；
  // 合成边按 id 查不到它，此时退到边自己内联的 `to_call_site` ——
  // `paySuccess --Triggers--> 事件` 这类直接语义边就是靠它给出 `event('X')` 那一行。
  const evidenceLocations = data?.locations?.length
    ? data.locations
    : edgeView?.to_call_site
      ? [edgeView.to_call_site]
      : [];

  // 只要点边时随身带了这条边本身（`from` / `to` / 内联位置），就把它画成「起点 → 各跳 → 终点」，
  // 与路由链路的呈现完全一致。此前仅在"折叠出了中间节点"（`via` 非空）时才画链，于是
  // `save --投递到--> 队列` 这类**直达语义边**只剩孤零零一个位置：既看不到起点 `save`，
  // 也看不到终点的语义节点 —— 看起来像"这条边没建好"。
  const showChain = !!edgeView;
  // 链路里是否已给出「本边自己那一行」（`via` 各跳的调用处，或直达边的 `to_call_site`）：
  // 给了就不必再单列一份证据位置，否则同一行会在链路的起点跳与「证据位置」里各出现一次。
  const chainCoversProof = via.length > 0 || !!edgeView?.to_call_site;

  return (
    <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
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

      {showChain ? (
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

      {unresolved && !chainCoversProof ? (
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

      {/* 链路已把每一跳的调用处逐条列出（直达边的 `to_call_site` 也在其中），raw 边的位置是其子集，
          无需重复展示；只在链路没给出本边那一行时显示。 */}
      {evidenceLocations.length > 0 && !chainCoversProof ? (
        <div>
          <Typography.Text strong style={{ fontSize: 13 }}>
            {t('证据位置')}
          </Typography.Text>
          <div style={{ marginTop: 8 }}>
            <LocationList
              locations={evidenceLocations}
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
 * 调用链：把这条边按 起点 → 中间各跳 → 终点（语义节点）逐跳列出。
 *
 * 折叠提拉边在图上看似直连，其实是一条多跳调用链被折叠后的结果；被折掉的中间节点只存在于
 * **当次视图结果**（`EdgeView.via`）里，按边 id 重查是拿不到的 —— 所以必须由点击方随身带入。
 * via 节点只带 id/kind/name，因此每一跳的源码位置要按节点 id 现查，才能给出真正的"调用处"。
 *
 * **直达语义边**（`via` 为空，如 `save --投递到--> 队列`）也走这里：只有 起点/终点 两跳，
 * 中间那一行取自边的 `to_call_site`。这样"点边看链"在两种情形下是同一套版式 ——
 * 都能看见起点是谁、终点是哪个语义节点，而不是只剩一行孤立的位置。
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

  // 单条位置的紧凑渲染：只保留 `file:line · symbol` 与（可选）snippet。
  // 相比 `LocationList` 的整块灰卡，去掉外框与重复按钮，让链路每一跳更轻。
  // `symbol` 是机器合成的全限定名（如 `A::b#C::d:252`），与上方 `Tag + 节点名` 重复，
  // 且会让一行路径换行参差；改为只在悬停时显示（`note` 同理），可见行只保留 `file:line` + 可选 snippet。
  const locationNode = (loc: SourceLocation) => {
    const tip = [loc.note, loc.symbol].filter(Boolean).join(' · ');
    const title = loc.file + ':' + loc.line + (tip ? ' — ' + tip : '');
    return (
      <div style={{ minWidth: 0 }}>
        <Tooltip title={title}>
          <Typography.Link
            style={{ fontSize: 12, wordBreak: 'break-all' }}
            onClick={() => void copyPath(loc, projectRoot)}
          >
            {loc.file}:{loc.line}
          </Typography.Link>
        </Tooltip>
        {loc.snippet ? (
        <pre
          style={{
            margin: '4px 0 0',
            padding: '4px 8px',
            fontSize: 11,
            fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
            background: '#f6f8fa',
            borderRadius: 6,
            color: '#475569',
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-all',
            maxHeight: 120,
            overflow: 'auto',
          }}
        >
          {loc.snippet}
        </pre>
      ) : null}
      </div>
    );
  };

  // 这是「边链路视图」（点边打开），只应展示与本边相关（on-path）的位置，**绝不**展示资源在全代码库的
  // 共现足迹（"全部出处 N 处"）。资源的完整足迹属于「点节点」场景（NodePanel / LocationList），
  // 在边证据链里出现会让人误以为 N 处都在链上——其实只有 1 处在链上，其余只是「同类共现」。
  //  - 非终点：节点自身定义处（s.locations[0]），即链路途经的方法 / 类位置；
  //  - 终点：本边到达它的那一行（s.callSite = edge.to_call_site），即上一跳调用它的位置
  //    （它已在上一行的调用语句里显示过，这里再给一个复制按钮方便跳转）。
  const definitionButton = (s: Step): ReactNode => {
    if (s.locations.length === 0 && !s.callSite) return null;
    const onPath = s.role === '终点' ? s.callSite ?? s.locations[0] : s.locations[0];
    if (!onPath) return null;
    const tip = [onPath.note, onPath.symbol].filter(Boolean).join(' · ');
    return (
      <Tooltip title={tip || t('复制 path:line')}>
        <Button
          size="small"
          type="text"
          icon={<CopyOutlined />}
          style={{ fontSize: 11, flexShrink: 0 }}
          onClick={() => void copyPath(onPath, projectRoot)}
        />
      </Tooltip>
    );
  };

  const stepDescription = (s: Step, nextCallSite?: SourceLocation | null, prevNextCallSite?: SourceLocation | null): ReactNode => {
    // 调用方归属：cs = 本节点体内「调下一跳」的那一行（即下一跳的 call_site，指向本节点文件内），
    // 与被调方的「定义处」同属一个文件，读起来是「route 调 detail / detail 调 tidyOrder …」的自然叙述。
    // 定义处已提到节点名右侧的「定义」按钮（见 definitionButton），这里只保留调用语句这一主干。
    const cs = nextCallSite;
    const isEnd = s.role === '终点';

    const rows: ReactNode[] = [];
    // 与上一行「调用语句」同址时不重复渲染（如 相邻两跳恰好落在同一 file:line）。
    const dupCallSite =
      !!prevNextCallSite && !!cs && prevNextCallSite.file === cs.file && prevNextCallSite.line === cs.line;
    if (cs && !dupCallSite) {
      rows.push(<Fragment key="cs">{locationNode(cs)}</Fragment>);
    } else if (!isEnd && !cs) {
      // 非终点却拿不到「调下一跳」的调用语句：该跳不是直接的 `Calls` 边（如 路由→handler 的绑定，或调用未解析），
      // 后端 `call_site_between` 两种来源都落空。如实标注，避免调用链在这里看起来莫名断掉。
      rows.push(
        <Typography.Text
          key="cs"
          type="secondary"
          title={t('该跳不是直接的 Calls 边（如 路由→handler 的绑定，或调用未解析），后端未给出「调用处」')}
          style={{ fontSize: 11 }}
        >
          {t('未解析到调用语句')}
        </Typography.Text>,
      );
    }
    if (rows.length === 0) return null;

    // 统一缩进到与节点名同列（NAME_INDENT），调用语句的 file:line / snippet 上下对齐。
    return (
      <div style={{ paddingLeft: NAME_INDENT }}>
        <Space direction="vertical" size={SP.row} style={{ width: '100%' }}>
          {rows}
        </Space>
      </div>
    );
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
      <Timeline
        items={steps.map((s, i) => ({
          color: TIMELINE_DOT_COLOR,
          children: (
            <div style={{ marginBottom: SP.step }}>
              {(() => {
                const isEndpoint = !s.kind;
                // 端点（起点/终点）用描边淡标签：白底 + 彩边 + 彩字，与中间节点「按 kind 实心填充」分层、不抢眼。
                const stroke = isEndpoint
                  ? s.role === '起点'
                    ? '#16a34a'
                    : s.role === '终点'
                      ? '#dc2626'
                      : '#1677ff'
                  : '#1677ff';
                return (
                  <Space size={SP.tagGap} wrap style={{ rowGap: 2 }}>
                    <Tag
                      color={isEndpoint ? undefined : nodeColor(s.kind ?? '')}
                      style={{
                        ...(isEndpoint
                          ? { backgroundColor: '#fff', borderColor: stroke, color: stroke }
                          : { color: '#fff' }),
                        minWidth: 64,
                        margin: 0,
                        textAlign: 'center',
                      }}
                    >
                      {s.kind ?? t(s.role ?? '')}
                    </Tag>
                    {/* 节点名走中性近黑，不跟着 Tag 上色：颜色额度只留两处 —— Tag（kind 色）与文件名（可跳转的蓝 Link）。
                        否则绿起点 / 蓝 Method / 红终点 + 蓝文件名，一屏全是颜色。可点性由加粗与悬停提示表达。 */}
                    <Typography.Link
                      style={{ fontSize: 13, fontWeight: 600, wordBreak: 'break-all', color: NODE_NAME_COLOR }}
                      onClick={() => onNodeClick?.(s.id)}
                      title={t('在主图中以该节点为中心重绘')}
                    >
                      {s.name}
                    </Typography.Link>
                    {definitionButton(s)}
                  </Space>
                );
              })()}
              {/* 调用语句归属调用方：行 i 展示「本节点体内调下一跳」的那一行，故传入下一跳的 callSite；
                  终点无下一跳，自然只留定义处。上一行的 nextCallSite 用于同址去重。 */}
              <div style={{ marginTop: SP.row }}>{stepDescription(s, i < steps.length - 1 ? steps[i + 1].callSite : null, i > 0 ? steps[i].callSite : null)}</div>
            </div>
          ),
        }))}
      />
    );
  };

  if (pathList.length === 0) return null;

  return (
    <div>
      <Typography.Text strong style={{ fontSize: 13 }}>
        {/* 有中间跳被折掉才叫「折叠掉的调用链」；直达边只有 起点↔终点 两跳，标题如实写「调用链」，
            版式与路由链路完全一致。 */}
        {t(pathList.some((p) => p.length > 0) ? '折叠掉的调用链' : '调用链')}
        {/* 多路径时保留条数汇总（有用）；单路径的"跳数"已由顶部 Descriptions 的「跳数」给出，这里不再重复。 */}
        {pathList.length > 1 ? `（${pathList.length}${t(' 条路径')}）` : null}
      </Typography.Text>
      {/* 标题与下方第一个节点之间留出呼吸间隙，避免标题贴住时间线圆点。 */}
      <div style={{ marginTop: SP.section }}>
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
