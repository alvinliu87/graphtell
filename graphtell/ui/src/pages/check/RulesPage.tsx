import { useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  Alert,
  Badge,
  Button,
  Card,
  Collapse,
  Empty,
  Input,
  InputNumber,
  Modal,
  Popconfirm,
  Segmented,
  Select,
  Space,
  Switch,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import {
  PlayCircleOutlined,
  SearchOutlined,
  SettingOutlined,
  UndoOutlined,
} from '@ant-design/icons';
import { useNavigate, useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { checkApi } from '@/entities/check';
import { projectApi } from '@/entities/project';
import {
  SEVERITY_COLOR,
  SEVERITY_LABEL,
  SEVERITY_ORDER,
  SEVERITY_RANK,
  ruleCategoryLabel,
  type CheckRule,
  type ProjectRuleConfig,
  type RuleParam,
  type Severity,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';

/**
 * Rule filter: filter by "relationship to this project".
 *
 * `overridden` is the dimension this page needs most -- project-level overrides are scattered across categories,
 * and without it you can't answer "which ones did I actually change", so review and rollback mean flipping through them one by one.
 */
type StateFilter = 'all' | 'enabled' | 'disabled' | 'overridden' | 'not_applicable';

const STATE_FILTERS: { value: StateFilter; label: string }[] = [
  { value: 'all', label: 'All' },
  { value: 'enabled', label: 'Enabled' },
  { value: 'disabled', label: 'Disabled' },
  { value: 'overridden', label: 'Overridden here' },
  { value: 'not_applicable', label: 'N/A here' },
];

/**
 * Whether a rule applies to this project (same criterion as the backend's `CheckRule::applies_to_env`).
 *
 * Deliberately **no** "filter by language": language is the rule author's declared prior, while what the user asks is
 * "can this rule run here". Browsing by language would make someone in a Java project switch on a PHP rule
 * and then find it never runs -- thinking the switch is broken.
 */
function appliesToEnv(
  rule: CheckRule,
  languages: string[],
  frameworks: string[],
): boolean {
  const langs = rule.applies_to.languages ?? [];
  const frames = rule.applies_to.frameworks ?? [];
  const langOk =
    langs.length === 0 || langs.some((l) => languages.some((x) => x.toLowerCase() === l.toLowerCase()));
  if (!langOk) return false;
  return (
    frames.length === 0 ||
    frames.some((f) => frameworks.some((x) => x.toLowerCase() === f.toLowerCase()))
  );
}

/** Display order of categories (any unknown categories are appended at the end). */
const CATEGORY_ORDER = [
  'architecture',
  'security',
  'contract',
  'deadcode',
  'performance',
  'api-hygiene',
];

/**
 * This project's **draft** for a rule: enabled-state override + parameter overrides.
 *
 * Why a draft rather than "write to the DB on click": changing config only takes effect after a re-run,
 * so writing per-rule would turn "tuning three thresholds" into three reruns -- batching and saving once is what people want.
 */
interface Draft {
  /** `null` = inherit the YAML global default; `true/false` = project-level override. */
  enabled: boolean | null;
  options: Record<string, unknown>;
}

/** No-override state: both enabled state and parameters return to the global defaults. */
const INHERIT: Draft = { enabled: null, options: {} };

function draftOf(cfg: Partial<ProjectRuleConfig> | undefined): Draft {
  return { enabled: cfg?.enabled ?? null, options: { ...(cfg?.options ?? {}) } };
}

function draftsOf(cfgs: Record<string, ProjectRuleConfig>): Record<string, Draft> {
  const out: Record<string, Draft> = {};
  for (const [ruleId, cfg] of Object.entries(cfgs)) out[ruleId] = draftOf(cfg);
  return out;
}

/** A parameter's value in the draft: falls back to the rule default if never overridden. */
function paramValue(draft: Draft | undefined, p: RuleParam): unknown {
  const v = draft?.options?.[p.key];
  return v === undefined ? p.default : v;
}

function sameOptions(a: Record<string, unknown>, b: Record<string, unknown>): boolean {
  const ka = Object.keys(a);
  if (ka.length !== Object.keys(b).length) return false;
  return ka.every((k) => Object.is(a[k], b[k]));
}

/**
 * Rule set page: show by category "which rules this project is checked by", and allow **per-project** overrides of
 * enabled state and tunable parameters.
 *
 * Separate from the results page -- rules are knowledge-base-driven declarations; this page owns "by what criteria to check";
 * to verify a single rule, click "run only this rule", which jumps to the results page.
 */
export function RulesPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  const navigate = useNavigate();
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [running, setRunning] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // `saved` = the server-stored value (the comparison baseline), `drafts` = the value the user is editing.
  const [saved, setSaved] = useState<Record<string, ProjectRuleConfig>>({});
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  // ---- Filtering: rules grow to dozens as languages expand, so "find a rule / find what I changed" must have an entry point.
  const [keyword, setKeyword] = useState('');
  const [severities, setSeverities] = useState<Severity[]>([]);
  const [stateFilter, setStateFilter] = useState<StateFilter>('all');
  /** The rule whose parameters are being edited (rules with no tunable parameters show no entry, so never enter this state). */
  const [editing, setEditing] = useState<CheckRule | null>(null);

  const rules = useAsync(() => checkApi.rules(), []);
  const configs = useAsync(() => checkApi.ruleConfigs(id), [id]);
  // The sub-projects' languages / frameworks = this project's tech-stack environment; used to judge whether a rule applies.
  const subs = useAsync(() => projectApi.subProjects(id), [id]);
  // This project's currently persisted violations: counted by rule_id, shown on rule cards as "N hits".
  // Same source as the results page (cap 5000; when exceeded this count reflects "what was loaded" rather than the full set).
  const violations = useAsync(() => checkApi.violations(id, 5000), [id]);
  const ruleCounts = useMemo(() => {
    const m: Record<string, number> = {};
    for (const v of violations.data ?? []) m[v.rule_id] = (m[v.rule_id] ?? 0) + 1;
    return m;
  }, [violations.data]);

  // Seed once when config arrives; afterwards the draft is owned by the user (a refresh must not silently overwrite in-progress edits).
  useEffect(() => {
    if (!configs.data) return;
    setSaved(configs.data);
    setDrafts(draftsOf(configs.data));
  }, [configs.data]);

  const draftFor = (ruleId: string): Draft => drafts[ruleId] ?? INHERIT;

  const sorted = useMemo(
    () =>
      [...(rules.data ?? [])].sort(
        (a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] || a.id.localeCompare(b.id),
      ),
    [rules.data],
  );

  const env = useMemo(() => {
    const list = subs.data ?? [];
    return {
      languages: [...new Set(list.map((s) => s.language))],
      frameworks: [...new Set(list.flatMap((s) => s.frameworks ?? []))],
    };
  }, [subs.data]);

  /** Environment gate: an inapplicable rule can't run (the backend skips it automatically), so the switch is meaningless for it. */
  const applicable = (r: CheckRule): boolean =>
    appliesToEnv(r, env.languages, env.frameworks);

  /** The filtered rules: category collapsing and "enable whole group" act only on these **visible** ones. */
  const filtered = useMemo(() => {
    const kw = keyword.trim().toLowerCase();
    return sorted.filter((r) => {
      const fits = applicable(r);
      if (kw) {
        const hay = `${r.id} ${r.title} ${r.description ?? ''}`.toLowerCase();
        if (!hay.includes(kw)) return false;
      }
      if (severities.length > 0 && !severities.includes(r.severity)) return false;
      if (stateFilter !== 'all') {
        const effective = draftFor(r.id).enabled ?? r.enabled;
        const isOverridden =
          draftFor(r.id).enabled !== null || Object.keys(draftFor(r.id).options).length > 0;
        if (stateFilter === 'enabled' && !effective) return false;
        if (stateFilter === 'disabled' && effective) return false;
        if (stateFilter === 'overridden' && !isOverridden) return false;
        if (stateFilter === 'not_applicable' && fits) return false;
      }
      return true;
    });
  }, [sorted, keyword, severities, stateFilter, drafts, env]);

  const filterActive = keyword.trim() !== '' || severities.length > 0 || stateFilter !== 'all';

  const clearFilters = () => {
    setKeyword('');
    setSeverities([]);
    setStateFilter('all');
  };

  // Aggregate by category (keeping CATEGORY_ORDER, with unknown categories appended after).
  const { categories, byCat } = useMemo(() => {
    const map = new Map<string, CheckRule[]>();
    for (const r of filtered) {
      const arr = map.get(r.category);
      if (arr) arr.push(r);
      else map.set(r.category, [r]);
    }
    const cats = [
      ...CATEGORY_ORDER.filter((c) => map.has(c)),
      ...[...map.keys()].filter((c) => !CATEGORY_ORDER.includes(c)),
    ];
    return { categories: cats, byCat: map };
  }, [sorted]);

  /** Compare against the server-stored value to find the rules actually changed. */
  const dirtyIds = useMemo(() => {
    const out: string[] = [];
    for (const [ruleId, d] of Object.entries(drafts)) {
      const s = draftOf(saved[ruleId]);
      if (s.enabled !== d.enabled || !sameOptions(s.options, d.options)) out.push(ruleId);
    }
    return out;
  }, [drafts, saved]);

  const dirty = dirtyIds.length > 0;

  const setDraft = (ruleId: string, next: Draft) =>
    setDrafts((prev) => ({ ...prev, [ruleId]: next }));

  const setEnabled = (ruleId: string, enabled: boolean | null) =>
    setDraft(ruleId, { ...draftFor(ruleId), enabled });

  const setParam = (ruleId: string, key: string, value: unknown) => {
    const d = draftFor(ruleId);
    setDraft(ruleId, { ...d, options: { ...d.options, [key]: value } });
  };

  /** Category-level batch: only changes the draft; saved to the DB in one shot. */
  const setCategoryEnabled = (cat: string, enabled: boolean) => {
    setDrafts((prev) => {
      const next = { ...prev };
      for (const r of byCat.get(cat) ?? []) next[r.id] = { ...(next[r.id] ?? INHERIT), enabled };
      return next;
    });
  };

  const save = async () => {
    setSaving(true);
    try {
      for (const ruleId of dirtyIds) {
        const d = draftFor(ruleId);
        // Neither an enabled override nor a parameter override = back to the inherited state; delete the row rather than leaving an empty config.
        if (d.enabled === null && Object.keys(d.options).length === 0) {
          await checkApi.resetRuleConfig(id, ruleId);
        } else {
          await checkApi.putRuleConfig(id, {
            rule_id: ruleId,
            enabled: d.enabled,
            options: d.options,
          });
        }
      }
      // Changing the criteria requires a rerun: the violations in the DB are conclusions under the "previous criteria", so not rerunning means nothing changed.
      setRunning('__all__');
      const report = await checkApi.check(id);
      refreshCheckSummary();
      message.success(
        t('Saved & re-run: {{n}} violation(s) ({{run}}/{{total}} rules executed)')
          .replace('{{n}}', String(report.violations.length))
          .replace('{{run}}', String(report.rules_run))
          .replace('{{total}}', String(report.rules_total)),
      );
      const fresh = await checkApi.ruleConfigs(id);
      setSaved(fresh);
      setDrafts(draftsOf(fresh));
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
      setRunning(null);
    }
  };

  const discard = () => {
    setDrafts(draftsOf(saved));
    message.info(t('Unsaved changes discarded'));
  };

  const runOne = async (ruleId: string) => {
    setRunning(ruleId);
    try {
      await checkApi.check(id, [ruleId]);
      refreshCheckSummary();
      message.success(t('Re-ran this rule alone — jumping to the results page'));
      navigate(`/projects/${id}/check`);
    } catch (e) {
      message.error(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(null);
    }
  };

  const renderParam = (rule: CheckRule, p: RuleParam): ReactNode => {
    const d = draftFor(rule.id);
    const value = paramValue(d, p);
    const overridden = d.options?.[p.key] !== undefined;

    let control: ReactNode;
    if (p.kind === 'number') {
      control = (
        <InputNumber
          size="small"
          style={{ width: 160 }}
          min={p.min ?? undefined}
          max={p.max ?? undefined}
          value={typeof value === 'number' ? value : Number(value ?? 0)}
          onChange={(v) => setParam(rule.id, p.key, v ?? p.default)}
        />
      );
    } else if (p.kind === 'bool') {
      control = (
        <Switch size="small" checked={Boolean(value)} onChange={(v) => setParam(rule.id, p.key, v)} />
      );
    } else if (p.kind === 'enum') {
      control = (
        <Select
          size="small"
          style={{ minWidth: 160 }}
          value={String(value ?? '')}
          options={(p.choices ?? []).map((c) => ({ value: c, label: c }))}
          onChange={(v) => setParam(rule.id, p.key, v)}
        />
      );
    } else {
      control = (
        <Input
          size="small"
          style={{ width: 220 }}
          value={String(value ?? '')}
          placeholder={t('Empty = no filter')}
          onChange={(e) => setParam(rule.id, p.key, e.target.value)}
        />
      );
    }

    return (
      <div key={p.key} style={{ display: 'grid', gap: 6, marginBottom: 18 }}>
        <Space size={6} wrap>
          <span style={{ fontWeight: 500 }}>{p.label}</span>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {p.key}
          </Typography.Text>
          {overridden ? (
            <Tag color="blue" style={{ marginInlineEnd: 0 }}>
              {t('Overridden')}
            </Tag>
          ) : null}
        </Space>
        <div>{control}</div>
        {p.description ? (
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {p.description}
          </Typography.Text>
        ) : null}
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {t('Default')} {String(p.default) === '' ? t('(empty)') : String(p.default)}
        </Typography.Text>
      </div>
    );
  };

  const renderRule = (r: CheckRule): ReactNode => {
    const d = draftFor(r.id);
    const fits = applicable(r);
    const hits = ruleCounts[r.id] ?? 0;
    const effective = d.enabled ?? r.enabled;
    const overridden = d.enabled !== null || Object.keys(d.options).length > 0;
    const hasParams = (r.params?.length ?? 0) > 0;
    const paramOverridden = Object.keys(d.options).length > 0;
    const needs = (r.applies_to.languages ?? []).length
      ? (r.applies_to.languages ?? []).join('/')
      : (r.applies_to.frameworks ?? []).join('/');
    return (
      <Card
        key={r.id}
        variant="borderless"
        style={{ borderRadius: 14, opacity: effective && fits ? 1 : 0.6 }}
        title={
          <Space size={8} wrap>
            <Tooltip title={fits ? '' : t('This project is not in the rule’s target environment; the check will skip it automatically')}>
              <Switch
                size="small"
                checked={effective}
                disabled={!fits}
                onChange={(v) => setEnabled(r.id, v)}
              />
            </Tooltip>
            <Tag color={SEVERITY_COLOR[r.severity]}>{t(SEVERITY_LABEL[r.severity])}</Tag>
            <span style={{ fontWeight: 600 }}>{r.title}</span>
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {r.id}
            </Typography.Text>
            <Tag color={hits > 0 ? 'volcano' : 'default'}>
              {hits} {t('violations')}
            </Tag>
            {d.enabled === null ? (
              <Tag>
                {t('Inherits default')}：{r.enabled ? t('On') : t('Off')}
              </Tag>
            ) : (
              <Tag color={d.enabled ? 'green' : 'default'}>
                {t('Project override')}：{d.enabled ? t('On') : t('Off')}
              </Tag>
            )}
            {fits ? null : (
              <Tag color="default">
                {t('N/A here')}：{t('requires')} {needs}
              </Tag>
            )}
          </Space>
        }
        extra={
          <Space size={8}>
            {hasParams ? (
              <Badge dot={paramOverridden} offset={[-4, 4]}>
                <Button
                  size="small"
                  icon={<SettingOutlined />}
                  onClick={() => setEditing(r)}
                >
                  {t('Parameters')}
                  {paramOverridden ? ` · ${t('changed')}` : ''}
                </Button>
              </Badge>
            ) : null}
            {overridden ? (
              <Tooltip title={t('Clear this project override and fall back to the YAML global default')}>
                <Button size="small" icon={<UndoOutlined />} onClick={() => setDraft(r.id, INHERIT)}>
                  {t('Reset to default')}
                </Button>
              </Tooltip>
            ) : null}
            <Button
              size="small"
              icon={<PlayCircleOutlined />}
              loading={running === r.id}
              onClick={() => void runOne(r.id)}
            >
              {t('Run this rule only')}
            </Button>
          </Space>
        }
      >
        <div style={{ color: 'rgba(0,0,0,0.65)', fontSize: 13 }}>
          <div>{r.description ?? t('(no description)')}</div>
          <div style={{ marginTop: 8 }}>
            <Typography.Text type="secondary">
              {t('Scope')}：{r.applies_to.kinds.join(', ') || t('Any kind')}
            </Typography.Text>
          </div>
          <div style={{ marginTop: 6 }}>
            <Typography.Text type="secondary">
              {t('Applies to')}：
              {r.applies_to.languages?.length
                ? r.applies_to.languages.join(', ')
                : t('Language-agnostic')}
              {r.applies_to.frameworks?.length ? ` · ${r.applies_to.frameworks.join(', ')}` : ''}
            </Typography.Text>
          </div>
          {r.remediation ? (
            <div style={{ marginTop: 6 }}>
              <Typography.Text type="secondary">
                {t('Suggested action')}：{r.remediation}
              </Typography.Text>
            </div>
          ) : null}

        </div>
      </Card>
    );
  };

  return (
    <>
      <PageHeader
        title={t('Rule Set')}
        subtitle={t('Rules are declared in backend YAML and only rendered here; you can override enabled state and thresholds per project — saving triggers a re-check.')}
        extra={
          <Space>
            {/* The sidebar no longer has a “Rule Set” item: the only entry to this page is the check page header, so a way back is given here */}
            <Button onClick={() => navigate(`/projects/${id}/check`)}>{t('Back to rule inspection')}</Button>
            <Button onClick={discard} disabled={!dirty || saving}>
              {t('Discard changes')}
            </Button>
            <Popconfirm
              title={t('Save & re-run')}
              description={t('Will write overrides for {{n}} rule(s) and re-run a full check').replace(
                '{{n}}',
                String(dirtyIds.length),
              )}
              okText={t('Save & re-run')}
              cancelText={t('Cancel')}
              onConfirm={() => void save()}
            >
              <Button type="primary" loading={saving || running === '__all__'} disabled={!dirty}>
                {t('Save & re-run')}
              </Button>
            </Popconfirm>
          </Space>
        }
      />

      {rules.loading ? (
        <Typography.Text type="secondary">{t('Loading…')}</Typography.Text>
      ) : sorted.length === 0 ? (
        <Empty description={t('No rules loaded')} />
      ) : (
        <>
          <Space size={8} wrap style={{ marginBottom: 12 }}>
            <Input
              allowClear
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
              prefix={<SearchOutlined style={{ color: 'rgba(0,0,0,0.35)' }} />}
              placeholder={t('Search rule id / name / description')}
              style={{ width: 260 }}
            />
            <Select<Severity[]>
              mode="multiple"
              allowClear
              value={severities}
              onChange={setSeverities}
              placeholder={t('Severity')}
              style={{ minWidth: 170 }}
              options={SEVERITY_ORDER.map((s) => ({ value: s, label: t(SEVERITY_LABEL[s]) }))}
            />
            <Segmented<StateFilter>
              value={stateFilter}
              onChange={setStateFilter}
              options={STATE_FILTERS.map((f) => ({ value: f.value, label: t(f.label) }))}
            />
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {filtered.length} / {sorted.length}
            </Typography.Text>
            {filterActive ? (
              <Button size="small" type="link" onClick={clearFilters}>
                {t('Clear filters')}
              </Button>
            ) : null}
          </Space>

          {filtered.length === 0 ? (
            <Empty description={t('No matching rules')}>
              <Button size="small" onClick={clearFilters}>
                {t('Clear filters')}
              </Button>
            </Empty>
          ) : (
            <Collapse
              defaultActiveKey={categories}
              items={categories.map((cat) => {
                const list = byCat.get(cat)!;
                // An inapplicable rule won't run even when on, so it can't count toward "enabled N".
                const on = list.filter(
                  (r) => applicable(r) && (draftFor(r.id).enabled ?? r.enabled),
                ).length;
                // Total violations currently hit by the rules in this category.
                const hits = list.reduce((s, r) => s + (ruleCounts[r.id] ?? 0), 0);
                return {
                  key: cat,
                  label: (
                    <Space size={8}>
                      <span style={{ fontWeight: 600 }}>{t(ruleCategoryLabel(cat))}</span>
                      <Tag>{list.length}</Tag>
                      <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                        {t('On')} {on}
                      </Typography.Text>
                      <Tag color={hits > 0 ? 'volcano' : 'default'}>
                        {t('Violations')} {hits}
                      </Tag>
                    </Space>
                  ),
                  // Stop propagation: otherwise clicking "enable whole group" would also collapse the group.
                  extra: (
                    <Tooltip title={t('Applies only to the currently filtered rules')}>
                      <Space size={4} onClick={(e) => e.stopPropagation()}>
                        <Button size="small" onClick={() => setCategoryEnabled(cat, true)}>
                          {t('Enable all')}
                        </Button>
                        <Button size="small" onClick={() => setCategoryEnabled(cat, false)}>
                          {t('Disable all')}
                        </Button>
                      </Space>
                    </Tooltip>
                  ),
                  children: <div style={{ display: 'grid', gap: 16 }}>{list.map(renderRule)}</div>,
                };
              })}
            />
          )}
        </>
      )}

      <Alert
        type="info"
        showIcon
        style={{ marginTop: 16 }}
        message={t('Rules are knowledge-driven: after adding or editing YAML rules the frontend needs no change — rebuild or click «Refresh» to apply')}
        description={t('Project overrides only store what differs from the global default: "reset to default" deletes the override row, so rules keep evolving with YAML.')}
      />

      <Modal
        open={editing !== null}
        onCancel={() => setEditing(null)}
        width={520}
        title={
          editing ? (
            <Space size={8} wrap>
              <span style={{ fontWeight: 600 }}>{editing.title}</span>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {editing.id}
              </Typography.Text>
            </Space>
          ) : (
            ''
          )
        }
        okText={t('Done')}
        cancelText={t('Cancel')}
        onOk={() => setEditing(null)}
        // "Restore defaults" goes on the left: it's a destructive action and must not sit in a row with "done / cancel" where it's clicked by accident.
        footer={(_, { OkBtn, CancelBtn }) => (
          <div style={{ display: 'flex', justifyContent: 'space-between' }}>
            <Button
              danger
              disabled={editing ? Object.keys(draftFor(editing.id).options).length === 0 : true}
              onClick={() => {
                if (!editing) return;
                const d = draftFor(editing.id);
                setDraft(editing.id, { ...d, options: {} });
                message.success(t('Parameters reset to rule defaults; click "Save & re-run" to apply'));
              }}
            >
              {t('Reset parameters')}
            </Button>
            <Space>
              <CancelBtn />
              <OkBtn />
            </Space>
          </div>
        )}
      >
        {editing ? (
          <>
            <Typography.Paragraph type="secondary" style={{ fontSize: 12, marginBottom: 18 }}>
              {t('Changes stay in the draft: after closing, click "Save & re-run" at the top-right to persist and re-run')}
            </Typography.Paragraph>
            {editing.params?.map((p) => renderParam(editing, p))}
          </>
        ) : null}
      </Modal>
    </>
  );
}
