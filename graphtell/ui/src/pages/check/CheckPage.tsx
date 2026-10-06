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
  Switch,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import {
  CopyOutlined,
  ProfileOutlined,
  ReloadOutlined,
  SafetyCertificateOutlined,
} from '@ant-design/icons';
import { useNavigate, useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';
import { checkApi } from '@/entities/check';
import { projectApi, type SubProject } from '@/entities/project';
import {
  SEVERITY_COLOR,
  SEVERITY_LABEL,
  SEVERITY_ORDER,
  SEVERITY_RANK,
  ruleCategoryLabel,
  type CheckReport,
  type CheckRule,
  type Severity,
  type Violation,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';

/** Accent color for the stat cards (`SEVERITY_COLOR` is an antd semantic color name used for table Tags; the two serve different purposes). */
const SEVERITY_ACCENT: Record<Severity, string> = {
  critical: '#a8071a',
  error: '#ff4d4f',
  warning: '#fa8c16',
  info: '#3d7eff',
};

const SUB_ROLE_LABEL: Record<string, string> = {
  frontend: 'Frontend',
  backend: 'Backend',
};

/**
 * Upper bound for reading persisted violation rows (matches the backend's `DEFAULT_VIOLATION_LIMIT`).
 *
 * Must not be set small: it is a "full fetch before paging", and the page pages 20 at a time itself. It was once 500,
 * while a single check produced 996 rows -- so right after a run you saw the complete result,
 * but after refreshing the page only the truncated 500 remained (truncated by write order, so all critical ones were cut),
 * which looked like "it wasn't persisted and went back to old data".
 */
const STORED_LIMIT = 5000;

/**
 * Rule-check results page: conclusions produced automatically after a build (from the persisted diagnostics table).
 *
 * Design notes:
 * * entering the page shows the **last automatic check**'s persisted result directly, without manual triggering;
 * * the top-bar "refresh" is for backfill / recompute: for a project that has never run (e.g. built before this feature shipped)
 *   it reruns and writes back in one click; new projects already do this automatically at build time;
 * * the rule set lives on its own "rule set" page; this page only talks about conclusions.
 */
export function CheckPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const navigate = useNavigate();
  const { t } = useLocale();
  // Refresh the sidebar "rule check" badge after a manual rerun / refresh.
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [report, setReport] = useState<CheckReport | null>(null);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);
  const [severity, setSeverity] = useState<Severity | 'all'>('all');
  const [ruleFilter, setRuleFilter] = useState<string | 'all'>('all');
  const [subFilter, setSubFilter] = useState<number[]>([]);
  const [limit] = useState(20);
  /**
   * Whether each rule's hit count in the dropdown is affected by the current severity filter.
   * Default false: the count only looks at the sub-project scope, unaffected by severity (so selecting "warning" still shows each rule's total hits).
   * When on: the count matches the list and narrows with the severity filter.
   */
  const [countBySeverity, setCountBySeverity] = useState(false);

  // Sub-project list (for the sub-project filter).
  const { data: subsData } = useAsync(() => projectApi.subProjects(id), [id]);
  const subs: SubProject[] = subsData ?? [];

  // Load the last persisted result on entry (the automatic check already wrote it), don't rerun; the server already filters when filtering by sub-project.
  const stored = useAsync(
    () => checkApi.violations(id, STORED_LIMIT, subFilter.length ? subFilter : undefined),
    [id, subFilter],
  );
  // Persisted total (grouped by severity). Used to tell whether the list was truncated by the cap -- truncating without saying so makes
  // users think "the check only produced this many".
  const summary = useAsync(() => checkApi.summary(id), [id]);
  // The rule list feeds the severity filter dropdown and the "loaded rules" count.
  const rules = useAsync(() => checkApi.rules(), []);

  const violations = report?.violations ?? stored.data ?? [];
  // Sub-project filter: hit any selected sub-project, or belong to none (shared resources, like the code graph's "shared nodes always shown").
  const scoped = useMemo(
    () =>
      subFilter.length === 0
        ? violations
        : violations.filter(
            (v) => v.sub_project_id == null || subFilter.includes(v.sub_project_id),
          ),
    [violations, subFilter],
  );
  const hasResults = scoped.length > 0 || report !== null;
  const loadFailed = stored.error !== null;

  const refresh = async () => {
    setRunning(true);
    setRunError(null);
    try {
      const r = await checkApi.check(id, []);
      setReport(r);
      refreshCheckSummary();
      // The persisted result must be refetched: after the page remounts (switching pages / browser refresh) it is
      // the only data source -- without refetching, the user would still see the previous round's persisted content next time.
      void stored.reload();
      void summary.reload();
      if (r.violations.length === 0 && r.rules_silent.length === 0) {
        message.success(t('Refresh complete — no violations matched'));
      }
    } catch (e) {
      setRunError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  /**
   * Severity counts come from the **persisted summary** first (`check/summary` is a full `COUNT`, same source as the sidebar badge),
   * not from counting the loaded list -- the list has a read cap, so counting yields "how many were loaded",
   * not "how many violations exist"; on an over-limit project the two differ by an order of magnitude.
   *
   * Only when **filtering by sub-project** do we fall back to counting the list: `summary` is project-level, so after filtering it is too large.
   */
  const counts = useMemo(() => {
    const empty = (): Record<Severity, number> => ({ critical: 0, error: 0, warning: 0, info: 0 });
    if (subFilter.length === 0) {
      if (summary.data) {
        return {
          critical: summary.data.critical,
          error: summary.data.error,
          warning: summary.data.warning,
          info: summary.data.info,
        };
      }
      // After a manual refresh the report also carries a full breakdown (same keys as summary), which takes priority over counting the list.
      if (report) {
        const c = empty();
        for (const k of Object.keys(c) as Severity[]) c[k] = report.by_severity[k] ?? 0;
        return c;
      }
    }
    const c = empty();
    for (const v of scoped) c[v.severity] = (c[v.severity] ?? 0) + 1;
    return c;
  }, [summary.data, report, subFilter.length, scoped]);

  // Persisted total vs actually listed rows: compare only when **there is no sub-project filter** (filtering naturally yields fewer).
  const storedTotal = summary.data
    ? summary.data.critical + summary.data.error + summary.data.warning + summary.data.info
    : null;
  const truncated =
    storedTotal != null && subFilter.length === 0 && scoped.length > 0 && scoped.length < storedTotal;

  /**
   * "Summary has data but the list is empty" must be called out separately.
   *
   * Once, any empty list showed "no check results yet", and `stored.error` was never rendered --
   * so a failed request (invalid project id, backend 500, no network) looked exactly like "genuinely 0 violations".
   * Worse, the sidebar badge (the same-source summary) still showed the historical total,
   * producing an interface with "total non-zero but results empty" and no explanation.
   */
  const emptyButShouldHaveData =
    !stored.loading && !loadFailed && scoped.length === 0 && storedTotal != null && storedTotal > 0;

  /**
   * The "count basis" list: by default the same as `scoped` (sub-project scope only);
   * after turning on "count follows severity", narrow by the current severity first so the dropdown hit counts sync with the list.
   */
  const countBase = useMemo(
    () =>
      countBySeverity
        ? scoped.filter((v) => severity === 'all' || v.severity === severity)
        : scoped,
    [scoped, countBySeverity, severity],
  );

  /**
   * Each rule's hit count: based on `countBase` (sub-project scope, optionally narrowed by severity);
   * does not change with the severity filter by default, otherwise the numbers jump around and are hard to read.
   */
  const ruleCounts = useMemo(() => {
    const m: Record<string, number> = {};
    for (const v of countBase) m[v.rule_id] = (m[v.rule_id] ?? 0) + 1;
    return m;
  }, [countBase]);

  /**
   * Rule dropdown options: grouped by `category` (one level, no multi-level tree), within a group sorted by
   * "severity → hit count descending → title" so frequent problems float up; each entry shows its hit count.
   */
  const ruleOptions = useMemo(() => {
    const byCat = new Map<string, CheckRule[]>();
    for (const r of rules.data ?? []) {
      const arr = byCat.get(r.category);
      if (arr) arr.push(r);
      else byCat.set(r.category, [r]);
    }
    const cats = [...byCat.keys()].sort((a, b) => a.localeCompare(b));
    return [
      { label: t('All rules'), value: 'all' },
      ...cats.map((cat) => ({
        label: t(ruleCategoryLabel(cat)),
        options: byCat
          .get(cat)!
          .slice()
          .sort(
            (a, b) =>
              SEVERITY_RANK[b.severity] - SEVERITY_RANK[a.severity] ||
              (ruleCounts[b.id] ?? 0) - (ruleCounts[a.id] ?? 0) ||
              a.title.localeCompare(b.title),
          )
          .map((r) => ({
            label: `${r.title}（${ruleCounts[r.id] ?? 0}）`,
            value: r.id,
          })),
      })),
    ];
  }, [rules.data, ruleCounts, t]);

  const filtered = useMemo(
    () =>
      scoped
        .filter((v) => severity === 'all' || v.severity === severity)
        .filter((v) => ruleFilter === 'all' || v.rule_id === ruleFilter)
        .slice()
        .sort(
          (a, b) =>
            SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] ||
            a.rule_id.localeCompare(b.rule_id),
        ),
    [scoped, severity, ruleFilter],
  );

  const copyLocation = (v: Violation) => {
    const loc = v.file ? `${v.file}${v.line ? `:${v.line}` : ''}` : '';
    if (!loc) return;
    void navigator.clipboard?.writeText(loc);
    message.success(t('Location copied'));
  };

  return (
    <>
      <PageHeader
        title={t('Rule inspection')}
        subtitle={t('Rule conclusions produced automatically after the build (persisted); which rules are enabled and their thresholds are tuned in "Rule Set".')}
        extra={
          <Space>
            {/* The rule-set entry lives here: the motivation to tune rules comes "after seeing the conclusions",
                     not "I want to browse rules" -- so it is an action on the conclusions page, not a parallel destination. */}
            <Button icon={<ProfileOutlined />} onClick={() => navigate(`/projects/${id}/rules`)}>
              {t('Rule Set')}
            </Button>
            <Button
              icon={<ReloadOutlined />}
              loading={running}
              onClick={() => void refresh()}
            >
              {t('Refresh')}
            </Button>
          </Space>
        }
      />

      {runError ? (
        <Alert type="error" showIcon message={runError} style={{ marginBottom: 16 }} />
      ) : null}

      {truncated ? (
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('List truncated at the read limit')}
          description={t('This project has {n} violations; only {m} are listed (read limit {limit}). Sorting is severity-first, so what is cut off are the least severe info-level ones.')
            .replace('{n}', String(storedTotal))
            .replace('{m}', String(scoped.length))
            .replace('{limit}', String(STORED_LIMIT))}
        />
      ) : null}

      {report && report.rules_silent.length > 0 ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 16 }}
          message={`${t('There are')} ${report.rules_silent.length} ${t('rules ran but matched 0')}`}
          description={
            <div>
              <div>
                {t('The most dangerous way a rule fails is not a false positive but a silent zero: its criteria reference an annotation or edge absent from the graph, so it never matches. Before assuming the code is clean, suspect the rule has gone blind.')}
              </div>
              <ul style={{ marginBlock: 8, paddingLeft: 20 }}>
                {report.rules_silent.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </div>
          }
        />
      ) : null}

      {report && report.rules_unavailable.length > 0 ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={`${t('There are')} ${report.rules_unavailable.length} ${t('rules with unmet criteria are disabled')}`}
          description={
            <div>
              <div>
                {t('The edges/annotations the criteria mention are entirely absent from this project graph; running it would only produce vacuously-true false positives (e.g. "no X inbound edge" holds for every node when X does not exist). Better not to run than to report a pile of fake ones.')}
              </div>
              <ul style={{ marginBlock: 8, paddingLeft: 20 }}>
                {report.rules_unavailable.map((s) => (
                  <li key={s}>{s}</li>
                ))}
              </ul>
            </div>
          }
        />
      ) : null}

      {report && report.rules_not_applicable.length > 0 ? (
        <Alert
          type="info"
          showIcon
          style={{ marginBottom: 16 }}
          message={`${t('There are')} ${report.rules_not_applicable.length} ${t('rules not applicable to this project tech stack')}`}
          description={
            <ul style={{ marginBlock: 8, paddingLeft: 20 }}>
              {report.rules_not_applicable.map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ul>
          }
        />
      ) : null}

      {loadFailed ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('Failed to read persisted violations')}
          description={
            <div>
              <div>{stored.error}</div>
              <div style={{ marginTop: 8 }}>
                {t('The sidebar badge comes from the summary endpoint; if it succeeds while this list fails, you will see "total non-zero but results empty".')}
              </div>
            </div>
          }
          action={
            <Button size="small" onClick={() => void stored.reload()}>
              {t('Retry')}
            </Button>
          }
        />
      ) : null}

      {emptyButShouldHaveData && !loadFailed ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('Summary has counts but the list is empty')}
          description={t('Summary shows {n} violations for this project, but the current list read 0 — common causes: the sub-project filter emptied the results, or the last persisted run was wiped by a rebuild while the summary still holds the old value. Click «Refresh» to re-run.').replace('{n}', String(storedTotal))}
        />
      ) : null}

      {!hasResults && !stored.loading && !loadFailed && !emptyButShouldHaveData ? (
        <Empty
          description={t('No check result yet — run one via «Refresh» (new projects run automatically after build)')}
          style={{ marginBlock: 48 }}
        />
      ) : null}

      {hasResults ? (
        <>
          <Row gutter={[16, 16]} style={{ marginBottom: 16 }}>
            <Col xs={12} md={4}>
              <StatCard
                title={t('Rules loaded')}
                value={report?.rules_total ?? rules.data?.length ?? 0}
                accent="#7c5cff"
              />
            </Col>
            {/* The four tiers are generated from `SEVERITY_ORDER`, **same source and order** as the severity filter below --
                     hand-writing two lists is exactly what caused "the stat card has critical but the filter doesn't". */}
            {SEVERITY_ORDER.map((s) => (
              <Col key={s} xs={12} md={4}>
                <StatCard
                  title={t(SEVERITY_LABEL[s])}
                  value={counts[s] ?? 0}
                  accent={SEVERITY_ACCENT[s]}
                />
              </Col>
            ))}
          </Row>

          <Card
            variant="borderless"
            style={{ borderRadius: 14 }}
            title={t('Violations')}
            extra={
              report ? (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('Elapsed')} {report.duration_ms}ms · {t('ran')} {report.rules_run} {t('rules')}
                </Typography.Text>
              ) : (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('Shows the last automatic check result; click «Refresh» to recompute')}
                </Typography.Text>
              )
            }
          >
            <Space wrap style={{ marginBottom: 12 }}>
              <Segmented
                size="small"
                value={severity}
                onChange={(v) => setSeverity(v as Severity | 'all')}
                options={[
                  { label: t('All'), value: 'all' },
                  ...SEVERITY_ORDER.map((s) => ({ label: t(SEVERITY_LABEL[s]), value: s })),
                ]}
              />
              <Select
                size="small"
                style={{ minWidth: 260 }}
                value={ruleFilter}
                onChange={setRuleFilter}
                showSearch
                optionFilterProp="label"
                filterOption={(input, option) => {
                  const q = input.toLowerCase();
                  const lbl = (option?.label ?? '').toString().toLowerCase();
                  const val = ((option as { value?: unknown })?.value ?? '')
                    .toString()
                    .toLowerCase();
                  return lbl.includes(q) || val.includes(q);
                }}
                options={ruleOptions}
              />
              <Space size={6}>
                <Switch size="small" checked={countBySeverity} onChange={setCountBySeverity} />
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('counts follow severity')}
                </Typography.Text>
              </Space>
              <Select
                size="small"
                mode="multiple"
                allowClear
                style={{ minWidth: 200 }}
                placeholder={t('All sub-projects')}
                value={subFilter}
                onChange={(v) => setSubFilter(v ?? [])}
                options={subs.map((s) => ({
                  label: `${s.name}（${t(SUB_ROLE_LABEL[s.role] ?? s.role)}）`,
                  value: s.id,
                }))}
              />
            </Space>
            <Table<Violation>
              rowKey={(v) => `${v.rule_id}-${v.node_id}`}
              loading={stored.loading && !report}
              dataSource={filtered}
              pagination={{ pageSize: limit }}
              locale={{ emptyText: t('No matched violations') }}
              columns={[
                {
                  title: t('Severity'),
                  dataIndex: 'severity',
                  width: 92,
                  render: (s: Severity) => (
                    <Tag color={SEVERITY_COLOR[s]}>{t(SEVERITY_LABEL[s])}</Tag>
                  ),
                },
                { title: t('Rule'), dataIndex: 'rule_id', width: 190, ellipsis: true },
                {
                  title: t('Target'),
                  dataIndex: 'node_name',
                  width: 220,
                  ellipsis: true,
                  render: (name: string, v) => (
                    <Tooltip title={`${v.node_kind} · ${name}`}>
                      <span>{name}</span>
                    </Tooltip>
                  ),
                },
                { title: t('Description'), dataIndex: 'message' },
                {
                  title: t('Location'),
                  dataIndex: 'file',
                  width: 190,
                  ellipsis: true,
                  render: (file: string | null | undefined, v) =>
                    file ? (
                      <Tooltip title={file}>
                        <Button
                          type="link"
                          size="small"
                          icon={<CopyOutlined />}
                          onClick={() => copyLocation(v)}
                          style={{ paddingInline: 4 }}
                        >
                          {file.split('/').pop()}
                          {v.line ? `:${v.line}` : ''}
                        </Button>
                      </Tooltip>
                    ) : (
                      <Typography.Text type="secondary">—</Typography.Text>
                    ),
                },
              ]}
            />
          </Card>
        </>
      ) : null}
    </>
  );
}
