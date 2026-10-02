import { Alert, Button, Collapse, Descriptions, Drawer, Empty, Space, Tag, Timeline, Tooltip, Typography } from 'antd';
import { Fragment, useEffect, useMemo, useState, type ReactNode } from 'react';
import { CopyOutlined, InfoCircleOutlined } from '@ant-design/icons';
import type { EdgeEvidence, EdgeView, NodeLocations, SourceLocation, ViaNode } from '@/entities/view';
import { viewApi } from '@/entities/view';
import { nodeColor } from '@/entities/graph';
import { copyPath } from '@/shared/lib/ide';
import { useAsync } from '@/shared/lib/useAsync';
import { LocationBadge, LocationList } from './LocationList';
import { edgeKindLabel, useLocale } from '@/shared/lib/i18n';

/** A stable empty-via reference: `?? []` creates a new array every render, which would retrigger the collapsed-chain fetch effect. */
const NO_VIA: ViaNode[] = [];

/** Unified spacing for the Inspector's vertical / horizontal rhythm, avoiding scattered magic numbers. */
const SP = {
  /** Between major blocks: Descriptions <-> chain <-> Alert etc. (the outer Space). */
  block: 16,
  /** Gap between a subsection heading and its content: e.g. "collapsed call chain" <-> timeline. */
  section: 14,
  /** Between timeline hops. */
  step: 12,
  /** Line breaks within a hop: node name <-> location block, between label rows. */
  row: 8,
  /** Tightest: between multiple paths under the same label. */
  tight: 4,
  /** The Tag before a node name <-> the node name. */
  tagGap: 6,
} as const;

/** Minimum width for the Tag before a node name; also the indent baseline for the "call statement / definition copy button" below, relative to the node name's left edge. */
const TAG_W = 64;
/** The call statement and definition copy button below are uniformly indented to the same column as the node name: NAME_INDENT = TAG_W + tagGap. */
const NAME_INDENT = TAG_W + SP.tagGap; // 64 + 6 = 70

/** Node name text color: neutral near-black rather than colored, to avoid stacking too many colors with "the Tag's kind color" and "the filename's blue Link". */
const NODE_NAME_COLOR = '#1f2937';
/** Timeline dot color: uniform neutral gray, no longer colored by kind (kind is already conveyed by the Tag), reducing overall screen color. */
const TIMELINE_DOT_COLOR = '#94a3b8';

/**
 * The Inspector on the right.
 *
 * Two kinds of use:
 * 1. **Nodes with no matching perspective** (`ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`)
 *    -- clicking them **does not switch the top filter**, it only shows properties and "N other references" here
 * 2. Edges -- show the evidence chain: always listed as source → hops → target (semantic nodes); a direct edge is just the source/target two hops
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
   * The edge that was clicked. A "direct" edge in the collapsed view is actually lifted,
   * and the middle nodes exist only in that view result (unreachable by id), so it must be carried in.
   */
  edgeView?: EdgeView | null;
  /** Endpoint id -> name; used to also label the first and last semantic nodes of the collapsed chain. */
  nodeNameOf?: (id: number) => string;
  projectRoot?: string;
  /** WSL distro name; when non-empty, jump / copy is handled as WSL (remote scheme + UNC prefix). */
  wslDistro?: string;
  onClose: () => void;
  onJumpToReference?: (nodeId: number) => void;
}) {
  const open = nodeId !== null || edgeId !== null;
  const { t } = useLocale();

  return (
    <Drawer
      title={nodeId !== null ? t('Node details') : t('Edge evidence chain')}
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

  if (loading) return <Typography.Text type="secondary">{t('Loading…')}</Typography.Text>;
  if (!data) return <Empty description={t('Node not found')} />;

  return (
    <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label={t('Kind')}>
          <Tag color={nodeColor(data.kind)} style={{ color: '#fff' }}>
            {data.kind}
          </Tag>
        </Descriptions.Item>
        <Descriptions.Item label={t('Name')}>{data.name}</Descriptions.Item>
        <Descriptions.Item label={t('Node type')}>
          {data.synthetic ? t('Synthetic node (semantic object)') : t('Syntax node')}
        </Descriptions.Item>
        <Descriptions.Item label={t('Location')}>
          <LocationBadge count={data.locations.length} />
        </Descriptions.Item>
        <Descriptions.Item label={t('References')}>{data.reference_count + t(' in-edges')}</Descriptions.Item>
      </Descriptions>

      {data.synthetic ? (
        <Alert
          type="info"
          showIcon
          message={t('This is a synthetic node: aggregated from multiple sources')}
          description={t("All sources are listed below; verify each as needed. We will not pick a 'looks-like' location for you.")}
        />
      ) : null}

      <LocationList locations={data.locations} kind={data.kind} projectRoot={projectRoot} wslDistro={wslDistro} />

      {data.reference_count > 0 ? (
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {t(' plus ') + data.reference_count + t(' references point to it.')}
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

  // The middle nodes this edge folded away: they exist only in the view result at click time (unreachable by id).
  const via = edgeView?.via ?? NO_VIA;

  // A negative id = a synthetic edge aggregated by the collapsed view (no corresponding single raw edge), so looking up evidence by id is bound to fail.
  // Rather than showing "edge not found" and making it look broken, say plainly what it is and list the folded-away middle nodes hop by hop.
  // Before the loading check: this edge's evidence request is bound to fail, so there's no need to flash "loading…" first.
  //
  // But "synthetic" does **not** mean "no evidence": in a reverse perspective (resource-type center) all edges have negative ids, and many of them are
  // a real direct edge (e.g. `paySuccess --Triggers--> event`) that inlines the trigger point (`to_call_site`)
  // and endpoint locations (`node_locations`) -- these must render normally, not be covered by a blanket "no evidence".
  // Only when all three are missing is it truly unevidenced.
  const hasOwnEvidence = !!edgeView?.to_call_site || (edgeView?.node_locations?.length ?? 0) > 0;
  if (edgeId < 0 && (via.length > 0 || !hasOwnEvidence)) {
    return (
      <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
        <Alert
          type="info"
          showIcon
          message={t('Synthetic edge (collapsed aggregate)')}
          description={
            via.length > 0
              ? t('This edge is a semantic edge aggregated from multiple call chains; there is no single corresponding source location. Below are the intermediate nodes it collapsed (start to end); verify hop by hop.')
              : t('This edge is a semantic edge aggregated from multiple call chains; there is no single corresponding direct edge in the graph and no locatable trigger point, so there is no hop-by-hop evidence.')
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

  if (loading && !edgeView) return <Typography.Text type="secondary">{t('Loading…')}</Typography.Text>;

  // **Take "the edge the user clicked" as authoritative**: it carries via / hops, and its state and confidence match the graph's hover card.
  // `/edges/{id}/evidence` returns the **pre-lift raw edge** -- its endpoints, state and confidence may differ;
  // passing it off as this edge is what produced the self-contradiction "hover card 0.80/resolved, drawer 0.54/unverified".
  // Now the raw edge is used only as the source of "underlying locations", clearly labeled, never standing in for this edge itself.
  const edge = edgeView ?? data?.edge ?? null;
  if (!edge) return <Empty description={t('Edge not found')} />;
  const unresolved = !edge.resolved;
  // Source of evidence locations: prefer the backend `/edges/{id}/evidence` (a real edge);
  // a synthetic edge can't be found by id, so fall back to the edge's own inlined `to_call_site` --
  // direct semantic edges like `paySuccess --Triggers--> event` get the `event('X')` line from it.
  const evidenceLocations = data?.locations?.length
    ? data.locations
    : edgeView?.to_call_site
      ? [edgeView.to_call_site]
      : [];

  // As long as the click carried this edge itself (`from` / `to` / inlined locations), draw it as "source → hops → target",
  // exactly like the route chain presentation. Previously the chain was drawn only when "middle nodes were folded" (`via` non-empty), so a
  // **direct semantic edge** like `save --publishes to--> queue` was left with a single lonely location: you could see neither the source `save`
  // nor the target semantic node -- it looked like "this edge wasn't built properly".
  const showChain = !!edgeView;
  // Whether the chain already gives "this edge's own line" (the call sites of each `via` hop, or a direct edge's `to_call_site`):
  // if it does, don't list an evidence location separately, otherwise the same line appears both at the chain's source hop and under "evidence locations".
  const chainCoversProof = via.length > 0 || !!edgeView?.to_call_site;

  return (
    <Space direction="vertical" size={SP.block} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label={t('Relation')}>
          {edgeKindLabel(t, edge.kind, edge.also_kinds)}
        </Descriptions.Item>
        <Descriptions.Item label={t('Status')}>
          {unresolved ? <Tag color="orange">{t('status.unverified')}</Tag> : <Tag color="green">{t('status.resolved')}</Tag>}
        </Descriptions.Item>
        {edge.indirect ? (
          <Descriptions.Item label={t('Nature')}>
            <Space size={4}>
              <Tag color="gold">{t('Indirect (propagated along the call chain)')}</Tag>
              <Tooltip title={t('indirect.tooltip')}>
                <InfoCircleOutlined style={{ color: '#d48806', cursor: 'help' }} />
              </Tooltip>
            </Space>
          </Descriptions.Item>
        ) : null}
        <Descriptions.Item label={t('Confidence')}>{edge.confidence.toFixed(2)}</Descriptions.Item>
        {edge.hops !== null ? (
          <Descriptions.Item label={t('Hops')}>{t('via ') + edge.hops + t(' hops')}</Descriptions.Item>
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
          {t('Unresolved edges are inferences: each location below is verifiable; verify before trusting.')}
        </Typography.Text>
      ) : null}

      {data && data.via.length > 0 ? (
        <Space direction="vertical" size={4}>
          {data.via.map((v, i) => (
            <Tag key={i}>{v}</Tag>
          ))}
        </Space>
      ) : null}

      {/* The chain already lists each hop's call site (including a direct edge's `to_call_site`), and the raw edge's locations are a subset, so there's
              no need to show them again; only shown when the chain didn't give this edge's own line. */}
      {evidenceLocations.length > 0 && !chainCoversProof ? (
        <div>
          <Typography.Text strong style={{ fontSize: 13 }}>
            {t('Evidence locations')}
          </Typography.Text>
          <div style={{ marginTop: 8 }}>
            <LocationList
              locations={evidenceLocations}
              ordered
              projectRoot={projectRoot}
              wslDistro={wslDistro}
              emptyHint={t('This edge has no jumpable evidence location (may come from authoritative-source inference)')}
            />
          </div>
        </div>
      ) : null}
    </Space>
  );
}

/**
 * Call chain: list this edge hop by hop as source → middle hops → target (semantic node).
 *
 * A lifted edge looks directly connected on the graph, but is really a multi-hop call chain after folding; the folded-away middle nodes exist only in
 * **that view result** (`EdgeView.via`) and can't be re-queried by edge id -- so the clicker must carry them in.
 * via nodes carry only id/kind/name, so each hop's source location must be looked up by node id to give the real "call site".
 *
 * **Direct semantic edges** (`via` empty, e.g. `save --publishes to--> queue`) go through here too: just the source/target two hops,
 * with the middle line taken from the edge's `to_call_site`. That way "click an edge to see the chain" uses the same layout in both cases --
 * you always see who the source is and which semantic node the target is, instead of a single isolated location.
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
  /** Backwards-compatible call: a single path. */
  via?: ViaNode[];
  /** Multiple paths (preferred); defaults to wrapping `via` as a single one. Route-to-table often has several call paths (e.g. the direct `value` lookup and the main list `getGoodsList`); this field presents the branching. */
  paths?: ViaNode[][];
  nodeNameOf?: (id: number) => string;
  projectRoot?: string;
  wslDistro?: string;
  /** Click a hop's node name -> redraw the main graph centered on that node. */
  onNodeClick?: (id: number) => void;
}) {
  const { t } = useLocale();
  const name = (id: number) => nodeNameOf?.(id) ?? `#${id}`;
  const pathList = paths && paths.length > 0 ? paths : via ? [via] : [];

  // Locations for nodes on all paths (including endpoints) are needed, otherwise the first hop's "call site" is headless.
  const allIds = useMemo(() => {
    const s = new Set<number>([edge.from, edge.to]);
    pathList.forEach((p) => p.forEach((v) => s.add(v.id)));
    return [...s];
  }, [pathList, edge.from, edge.to]);

  // Store the whole `NodeLocations`: `synthetic` is needed to decide how to label the "definition site".
  // Shared nodes (ConfigKey / Table / Cache …) are synthesized from multiple co-occurrences of the same key,
  // and **not all of their occurrences belong to the current chain** -- showing them mixed in makes it look like the chain runs into unrelated files.
  //
  // Prefer the locations the backend **inlines** in `edge.node_locations` (a collapsed-view chain is a temporary lift, so middle hops
  // can't be re-queried by edge id -- the backend gives them all at once). Fall back to the original endpoint only for missing nodes,
  // avoiding one `/nodes/{id}/locations` per hop (N+1).
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

  // Inlined data wins; only fill in nodes the backend didn't inline.
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

  // Compact rendering for a single location: keep only `file:line · symbol` and (optionally) the snippet.
  // Compared with `LocationList`'s full gray card, drop the frame and duplicate buttons so each chain hop is lighter.
  // `symbol` is a machine-composed fully-qualified name (e.g. `A::b#C::d:252`) that duplicates the `Tag + node name` above,
  // and makes a row of paths wrap unevenly; change to show it only on hover (same for `note`), keeping just `file:line` + optional snippet visible.
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

  // This is the "edge chain view" (opened by clicking an edge) and should show only locations relevant to this edge (on-path) -- **never** the resource's
  // co-occurrence footprint across the whole codebase ("all N occurrences"). The resource's full footprint belongs to the "click a node" scenario (NodePanel / LocationList);
  // showing it in an edge evidence chain makes people think all N are on the chain -- when really only 1 is, the rest are just "same-kind co-occurrences".
  //  - non-target: the node's own definition site (s.locations[0]), i.e. the method / class location the chain passes through;
  //  - target: the line where this edge reaches it (s.callSite = edge.to_call_site), i.e. where the previous hop called it
  //    (already shown in the previous row's call statement; a copy button here is added for convenient jumping).
  const definitionButton = (s: Step): ReactNode => {
    if (s.locations.length === 0 && !s.callSite) return null;
    const onPath = s.role === 'end' ? s.callSite ?? s.locations[0] : s.locations[0];
    if (!onPath) return null;
    const tip = [onPath.note, onPath.symbol].filter(Boolean).join(' · ');
    return (
      <Tooltip title={tip || t('Copy path:line')}>
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
    // Caller attribution: cs = the line in this node's body that "calls the next hop" (i.e. the next hop's call_site, pointing inside this node's file),
    // which is in the same file as the callee's "definition site", reading as a natural narrative "route calls detail / detail calls tidyOrder …".
    // The definition site is already promoted to the "definition" button on the right of the node name (see definitionButton); only the call statement trunk stays here.
    const cs = nextCallSite;
    const isEnd = s.role === 'end';

    const rows: ReactNode[] = [];
    // Don't render again when it's the same location as the previous row's "call statement" (e.g. two adjacent hops landing on the same file:line).
    const dupCallSite =
      !!prevNextCallSite && !!cs && prevNextCallSite.file === cs.file && prevNextCallSite.line === cs.line;
    if (cs && !dupCallSite) {
      rows.push(<Fragment key="cs">{locationNode(cs)}</Fragment>);
    } else if (!isEnd && !cs) {
      // A non-target hop with no "calls the next hop" statement: this hop isn't a direct `Calls` edge (e.g. a route→handler binding, or an unresolved call),
      // and both sources of the backend's `call_site_between` come up empty. Label it honestly so the call chain doesn't look mysteriously broken here.
      rows.push(
        <Typography.Text
          key="cs"
          type="secondary"
          title={t('This hop is not a direct Calls edge (e.g. a route→handler binding, or an unresolved call), so the backend provides no call site.')}
          style={{ fontSize: 11 }}
        >
          {t('Call statement not resolved')}
        </Typography.Text>,
      );
    }
    if (rows.length === 0) return null;

    // Indent uniformly to the same column as the node name (NAME_INDENT), so the call statement's file:line / snippet align vertically.
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
      { key: `from-${edge.from}`, id: edge.from, kind: null, name: name(edge.from), role: 'start', locations: locs[edge.from]?.locations ?? [], callSite: null },
      ...p.map((v) => ({
        key: `via-${v.id}`,
        id: v.id,
        kind: v.kind,
        name: v.name,
        role: null as string | null,
        locations: locs[v.id]?.locations ?? [],
        callSite: v.call_site ?? null,
      })),
      { key: `to-${edge.to}`, id: edge.to, kind: null, name: name(edge.to), role: 'end', locations: locs[edge.to]?.locations ?? [], callSite: edge.to_call_site ?? null },
    ];
    return (
      <Timeline
        items={steps.map((s, i) => ({
          color: TIMELINE_DOT_COLOR,
          children: (
            <div style={{ marginBottom: SP.step }}>
              {(() => {
                const isEndpoint = !s.kind;
                // Endpoints (source/target) use an outlined light tag: white fill + colored border + colored text, layered against the middle nodes' "solid kind-colored fill" so they don't steal attention.
                const stroke = isEndpoint
                  ? s.role === 'start'
                    ? '#16a34a'
                    : s.role === 'end'
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
                    {/* The node name uses neutral near-black and does not follow the Tag's color: the color budget is spent in only two places -- the Tag (kind color) and the filename (the clickable blue Link).
                             Otherwise a green source / blue Method / red target + blue filename means a screen full of color. Clickability is conveyed by bold + hover hint. */}
                    <Typography.Link
                      style={{ fontSize: 13, fontWeight: 600, wordBreak: 'break-all', color: NODE_NAME_COLOR }}
                      onClick={() => onNodeClick?.(s.id)}
                      title={t('Re-center the main graph on this node')}
                    >
                      {s.name}
                    </Typography.Link>
                    {definitionButton(s)}
                  </Space>
                );
              })()}
              {/* The call statement belongs to the caller: row i shows "the line inside this node that calls the next hop", so it passes the next hop's callSite;
                      the target has no next hop, so naturally only the definition site remains. The previous row's nextCallSite is used for same-location dedup. */}
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
        {/* Only when middle hops were folded away is it a "collapsed call chain"; a direct edge has just the source↔target two hops, so the heading says "call chain" honestly,
                with a layout identical to the route chain. */}
        {t(pathList.some((p) => p.length > 0) ? 'Collapsed call chain' : 'Call chain')}
        {/* With multiple paths, keep the count summary (useful); for a single path the "hop count" is already given by "hops" in the Descriptions above, so it isn't repeated here. */}
        {pathList.length > 1 ? `（${pathList.length}${t(' paths')}）` : null}
      </Typography.Text>
      {/* Leave breathing room between the heading and the first node below, so the heading doesn't stick to the timeline dot. */}
      <div style={{ marginTop: SP.section }}>
        {pathList.length === 1 ? (
          renderPath(pathList[0])
        ) : (
          <Collapse
            defaultActiveKey={['0']}
            size="small"
            items={pathList.map((p, idx) => ({
              key: String(idx),
              label: `${t('Path ') + (idx + 1)}`,
              children: renderPath(p),
            }))}
          />
        )}
      </div>
    </div>
  );
}
