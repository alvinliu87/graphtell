import { useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Row,
  Segmented,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import { CopyOutlined } from '@ant-design/icons';
import { useNavigate, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import {
  CATEGORY_COLOR,
  actionableCount,
  graphApi,
  groupDiagnostics,
  type Diagnostic,
  type DiagnosticCategory,
  type DiagnosticGroup,
  type DiagnosticSummary,
} from '@/entities/graph';
import {
  SEVERITY_COLOR,
  SEVERITY_LABEL,
  SEVERITY_ORDER,
  SEVERITY_RANK,
  checkApi,
  type Severity,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';

/** Accent color for the stat cards (`SEVERITY_COLOR` is an antd semantic color name used for Tags; the two serve different purposes). */
const SEVERITY_ACCENT: Record<Severity, string> = {
  critical: '#a8071a',
  error: '#ff4d4f',
  warning: '#fa8c16',
  info: '#3d7eff',
};

/**
 * Category badge: the three categories are handled completely differently, so the badge has to be
 * explicit — reading “349 entries” the user's first reaction is “do I have 349 bugs?”, while the
 * truth is usually “one engine limitation occurred 349 times”.
 */
const CATEGORY_TEXT: Record<DiagnosticCategory, { label: string; hint: string }> = {
  actionable: {
    label: 'Worth a look',
    hint: 'May point at a real code problem (dead route / unregistered event) — worth checking.',
  },
  engine: {
    label: 'Engine / knowledge limits',
    hint: 'The engine or framework knowledge could not build this piece: your code is fine, but the graph is missing here and recall weakens.',
  },
  expected: {
    label: 'Expected',
    hint: 'The target is in vendor or excluded by design — nothing to do.',
  },
};

/** Maximum number of sample locations listed per problem type (the rest is only counted; the page should not become a wall). */
const SAMPLE_LIMIT = 3;

/**
 * Build-report page: **build-time** diagnostics (conflicts, missing, unresolved links) -- these are themselves valuable findings.
 *
 * Two design threads (both earned the hard way):
 *
 * 1. **Rule-check conclusions are deliberately not here**: although both live in the diagnostics table, they differ in nature
 *    ("the graph isn't fully built" vs "the code violated a rule"), and mixing them in one table makes both kinds unreadable.
 *    An explicit entry pointing there avoids users thinking "there isn't a single error".
 * 2. **Group by problem type, don't flatten entries**: diagnostics are inherently long-tailed and repetitive (one engine diagnostic
 *    fires once per hundreds of files), so a flat table presents "the same thing happened 349 times" as "349 problems",
 *    and `ORDER BY id DESC` truncation shows write order rather than severity.
 *    After grouping + categorizing + plain-language explanation, "should I care" becomes readable.
 */
export function CoveragePage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  const navigate = useNavigate();

  const list = useAsync<Diagnostic[]>(() => graphApi.diagnostics(id), [id]);
  const summary = useAsync<DiagnosticSummary>(() => graphApi.diagnosticsSummary(id), [id]);
  const check = useAsync(() => checkApi.summary(id), [id]);

  /** View: cards grouped by type / per-item detail table. Grouped by default -- that's the "readable" layer. */
  const [view, setView] = useState<'types' | 'list'>('types');
  const [severity, setSeverity] = useState<Severity | 'all'>('all');
  const [code, setCode] = useState<string | 'all'>('all');

  const s = summary.data;
  const total = s ? s.critical + s.error + s.warning + s.info : 0;
  const listed = list.data?.length ?? 0;

  const groups = useMemo(
    () => groupDiagnostics(summary.data?.by_code, list.data ?? []),
    [summary.data, list.data],
  );
  const actionable = actionableCount(groups);
  const byCodeAccurate = (summary.data?.by_code?.length ?? 0) > 0;

  /** Detail table: severity first → type, using the same severity weights as the "by type" view. */
  const rows = useMemo(
    () =>
      (list.data ?? [])
        .filter((d) => severity === 'all' || d.severity === severity)
        .filter((d) => code === 'all' || d.code === code)
        .slice()
        .sort(
          (a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] || a.code.localeCompare(b.code),
        ),
    [list.data, severity, code],
  );

  /** Plain name: a code not in the glossary shows as-is, never guessing a meaning (`t` falls back to the key itself on a miss). */
  const codeTitle = (c: string) => {
    const key = `diag.${c}.title`;
    const v = t(key);
    return v === key ? c : v;
  };
  const codeWhat = (c: string) => {
    const key = `diag.${c}.what`;
    const v = t(key);
    return v === key ? '' : v;
  };

  const copyLocation = (loc: string | null | undefined) => {
    if (!loc) return;
    void navigator.clipboard?.writeText(loc);
    message.success(t('Location copied'));
  };

  const focusOn = (c: string) => {
    setCode(c);
    setSeverity('all');
    setView('list');
  };

  /** A class's severity distribution, e.g. "warning 349 · info 65": the same code can mean different things at different severities. */
  const severitySplit = (g: DiagnosticGroup) =>
    SEVERITY_ORDER.filter((sev) => (g.bySeverity[sev] ?? 0) > 0)
      .map((sev) => `${t(SEVERITY_LABEL[sev])} ${g.bySeverity[sev]}`)
      .join(' · ');

  const checkTotal = check.data
    ? check.data.critical + check.data.error + check.data.warning + check.data.info
    : 0;
  const unsupported = s?.unsupported_languages ?? [];

  return (
    <>
      <PageHeader
        title={t('Build Report')}
        subtitle={t('Missing root nodes, routes pointing to non-existent handlers, identity conflicts, etc. — these records are the conclusions.')}
        // This page's only entry is the ⓘ Popover beside the code-graph title,
        // so a way back must be given here, otherwise users who come in can't get out (only the browser Back button).
        extra={
          <Button size="small" onClick={() => navigate(`/projects/${id}/graph`)}>
            {t('Back to code graph')}
          </Button>
        }
      />

      <Alert
        type={s && s.error + s.critical > 0 ? 'warning' : 'info'}
        showIcon
        style={{ marginBottom: 12 }}
        message={t('This page is the "Build Report": where the graph is incomplete — not a verdict on your code.')}
        description={
          <div>
            <div>
              {t('Unresolved roots, routes pointing to non-existent handlers, underivable identities — all of them mean "a piece of the graph was not built".')}
            </div>
            <div style={{ marginTop: 4 }}>
              {t('How to read it: start from the problem types — how many there are and which one matters. When one type fires in hundreds of files, the entry count is not the problem count.')}
            </div>
            {checkTotal > 0 ? (
              <div style={{ marginTop: 8 }}>
                <Space size={8} wrap>
                  <span>
                    {t('For rule violations of the code itself, see ')}
                    {t('Rule inspection')}：{t('Total ')}
                    {checkTotal}
                    {t(' entries')}（
                    {SEVERITY_ORDER.map((sev, i) => (
                      <span key={sev}>
                        {i > 0 ? ' · ' : ''}
                        {t(SEVERITY_LABEL[sev])} {check.data?.[sev] ?? 0}
                      </span>
                    ))}
                    ）
                  </span>
                  <Button size="small" onClick={() => navigate(`/projects/${id}/check`)}>
                    {t('View rule inspection')}
                  </Button>
                </Space>
              </div>
            ) : null}
          </div>
        }
      />

      {unsupported.length > 0 ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          message={t('No parser for these languages: ') + unsupported.join(' · ')}
          description={t(' — those sub-projects have file structure only; semantic recall is empty there.')}
        />
      ) : null}

      <Row gutter={[16, 16]} style={{ marginBottom: 16 }}>
        <Col xs={12} md={8}>
          <StatCard
            title={t('Problem types')}
            value={groups.length}
            accent="#7c5cff"
            suffix={groups.length ? t(' types') : undefined}
          />
        </Col>
        {SEVERITY_ORDER.map((sev) => (
          <Col key={sev} xs={12} md={4}>
            <StatCard
              title={t(SEVERITY_LABEL[sev])}
              value={s?.[sev] ?? 0}
              accent={SEVERITY_ACCENT[sev]}
            />
          </Col>
        ))}
      </Row>

      {total > 0 ? (
        <Alert
          type={actionable > 0 ? 'warning' : 'success'}
          showIcon
          style={{ marginBottom: 12 }}
          message={
            <span>
              {groups.length} {t(' problem types')}
              {actionable > 0 ? ` · ${t('Worth a look')} ${actionable} ${t(' entries')}` : ''}
            </span>
          }
          description={
            actionable > 0
              ? t('The rest are engine / knowledge limits and expected cases: they do not change recall conclusions unless that is exactly the part you are looking into.')
              : t('Nothing here needs your attention: all of it is expected or an engine limitation.')
          }
        />
      ) : null}

      {total === 0 && !summary.loading && !summary.error ? (
        <Card variant="borderless" style={{ borderRadius: 14 }}>
          <Empty description={t('No build report — the graph reported nothing unresolved or missing.')} />
        </Card>
      ) : null}

      {total > 0 ? (
        <Card
          variant="borderless"
          style={{ borderRadius: 14 }}
          title={
            <Segmented
              size="small"
              value={view}
              onChange={(v) => setView(v as 'types' | 'list')}
              options={[
                { label: `${t('By type')}（${groups.length}）`, value: 'types' },
                { label: `${t('Entries')}（${listed}）`, value: 'list' },
              ]}
            />
          }
          extra={
            byCodeAccurate ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('Per-type counts come from the full aggregate, unaffected by the entry read cap.')}
              </Typography.Text>
            ) : (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('Per-type counts are derived from the current read window (the summary API returned no per-type counts) and may be too small.')}
              </Typography.Text>
            )
          }
        >
          {view === 'types' ? (
            groups.length === 0 ? (
              <Empty description={t('No report entries')} />
            ) : (
              <>
                {groups.map((g) => {
                  const cat = CATEGORY_TEXT[g.category];
                  const samples = g.samples.slice(0, SAMPLE_LIMIT);
                  return (
                    <Card
                      key={g.code}
                      size="small"
                      style={{ borderRadius: 12, marginBottom: 10, background: '#fcfcfd' }}
                      styles={{ body: { padding: 16 } }}
                    >
                      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                        <Tag color={SEVERITY_COLOR[g.severity]} style={{ marginInlineEnd: 0 }}>
                          {t(SEVERITY_LABEL[g.severity])}
                        </Tag>
                        <Tooltip title={t(cat.hint)}>
                          <Tag color={CATEGORY_COLOR[g.category]} style={{ marginInlineEnd: 0 }}>
                            {t(cat.label)}
                          </Tag>
                        </Tooltip>
                        <Typography.Text strong style={{ fontSize: 15 }}>
                          {codeTitle(g.code)}
                        </Typography.Text>
                        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                          <code>{g.code}</code>
                        </Typography.Text>
                        <span style={{ marginLeft: 'auto', fontSize: 12, color: 'rgba(0,0,0,0.55)' }}>
                          {t('This type has ')}
                          <b>{g.count}</b>
                          {t(' entries')}
                          {severitySplit(g) && severitySplit(g) !== `${t(SEVERITY_LABEL[g.severity])} ${g.count}`
                            ? `（${severitySplit(g)}）`
                            : ''}
                        </span>
                        <Button size="small" type="link" onClick={() => focusOn(g.code)}>
                          {t('View entries')}
                        </Button>
                      </div>

                      {codeWhat(g.code) ? (
                        <Typography.Paragraph
                          type="secondary"
                          style={{ margin: '8px 0 0', fontSize: 13 }}
                        >
                          {codeWhat(g.code)}
                        </Typography.Paragraph>
                      ) : null}

                      {samples.length > 0 ? (
                        <div style={{ marginTop: 6 }}>
                          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                            {t('Sample locations')}
                          </Typography.Text>
                          <ul style={{ margin: '2px 0 0', paddingLeft: 18 }}>
                            {samples.map((d, i) => (
                              <li key={`${d.location ?? ''}-${i}`}>
                                <Tooltip title={d.message}>
                                  <Button
                                    type="link"
                                    size="small"
                                    icon={<CopyOutlined />}
                                    onClick={() => copyLocation(d.location)}
                                    style={{ paddingInline: 0, fontSize: 12, height: 'auto', maxWidth: '100%' }}
                                  >
                                    {/* A location can be a whole absolute path (e.g. a framework-root diagnostic), so it must be truncated here — otherwise it bursts the card */}
                                    <span
                                      style={{
                                        display: 'inline-block',
                                        maxWidth: 560,
                                        overflow: 'hidden',
                                        textOverflow: 'ellipsis',
                                        whiteSpace: 'nowrap',
                                        verticalAlign: 'bottom',
                                      }}
                                    >
                                      {d.location ?? '—'}
                                    </span>
                                  </Button>
                                </Tooltip>
                              </li>
                            ))}
                          </ul>
                          {g.count > samples.length ? (
                            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                              {t('Also')} {g.count - samples.length} {t(' more of the same type; use "View entries" to filter by it.')}
                            </Typography.Text>
                          ) : null}
                        </div>
                      ) : null}
                    </Card>
                  );
                })}
              </>
            )
          ) : (
            <>
              {total > listed ? (
                <Alert
                  type="info"
                  showIcon
                  style={{ marginBottom: 12 }}
                  message={t('Entries truncated by the read limit')}
                  description={
                    `${t('This project has')} ${total} ${t('records, currently listing')} ${listed} ${t(' entries (the entry list is capped): per-type counts come from the full aggregate and stay accurate.')}`
                  }
                />
              ) : null}
              <Space wrap style={{ marginBottom: 12 }}>
                <Segmented
                  size="small"
                  value={severity}
                  onChange={(v) => setSeverity(v as Severity | 'all')}
                  options={[
                    { label: t('All'), value: 'all' },
                    ...SEVERITY_ORDER.map((sev) => ({
                      label: `${t(SEVERITY_LABEL[sev])} ${s?.[sev] ?? 0}`,
                      value: sev,
                    })),
                  ]}
                />
                {/* Type uses a dropdown rather than Segmented: diagnostic types can grow with FKB,
                         and Segmented overflows horizontally on narrow screens (a sidebar + canvas layout is common in this project),
                         while Select truncates by itself. */}
                <Select
                  size="small"
                  style={{ minWidth: 280 }}
                  value={code}
                  onChange={setCode}
                  showSearch
                  optionFilterProp="label"
                  options={[
                    { label: t('All types'), value: 'all' },
                    ...groups.map((g) => ({
                      label: `${codeTitle(g.code)}（${g.count}）`,
                      value: g.code,
                    })),
                  ]}
                />
              </Space>
              <Table<Diagnostic>
                rowKey={(d, i) => `${d.code}-${d.location ?? ''}-${i ?? 0}`}
                loading={list.loading && !list.data}
                dataSource={rows}
                pagination={{ pageSize: 20 }}
                locale={{ emptyText: t('No report entries') }}
                columns={[
                  {
                    title: t('Severity'),
                    dataIndex: 'severity',
                    width: 90,
                    render: (sev: Severity) => (
                      <Tag color={SEVERITY_COLOR[sev]}>{t(SEVERITY_LABEL[sev])}</Tag>
                    ),
                  },
                  {
                    title: t('Type'),
                    dataIndex: 'code',
                    width: 260,
                    render: (c: string) => (
                      <Space direction="vertical" size={0}>
                        <span>{codeTitle(c)}</span>
                        <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                          <code>{c}</code>
                        </Typography.Text>
                      </Space>
                    ),
                  },
                  { title: t('Phase'), dataIndex: 'phase', width: 120 },
                  {
                    title: t('Location'),
                    dataIndex: 'location',
                    width: 300,
                    ellipsis: true,
                    render: (loc: string | null, d) =>
                      loc ? (
                        <Tooltip title={d.message}>
                          <Button
                            type="link"
                            size="small"
                            icon={<CopyOutlined />}
                            onClick={() => copyLocation(loc)}
                            style={{ paddingInline: 4 }}
                          >
                            {loc}
                          </Button>
                        </Tooltip>
                      ) : (
                        <Typography.Text type="secondary">—</Typography.Text>
                      ),
                  },
                  { title: t('Description'), dataIndex: 'message' },
                ]}
              />
            </>
          )}
        </Card>
      ) : null}
    </>
  );
}
