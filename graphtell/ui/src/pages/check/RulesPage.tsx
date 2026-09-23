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
  type CheckRule,
  type ProjectRuleConfig,
  type RuleParam,
  type Severity,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';

/**
 * 规则筛选：按"与本工程的关系"筛。
 *
 * `overridden` 是这一页最需要的维度 —— 工程级覆盖是散落在各分类里的，
 * 没有它就无从回答"我到底改过哪几条"，复核与回退都只能一条条翻。
 */
type StateFilter = 'all' | 'enabled' | 'disabled' | 'overridden' | 'not_applicable';

const STATE_FILTERS: { value: StateFilter; label: string }[] = [
  { value: 'all', label: '全部' },
  { value: 'enabled', label: '已启用' },
  { value: 'disabled', label: '已停用' },
  { value: 'overridden', label: '本工程改过' },
  { value: 'not_applicable', label: '不适用' },
];

/**
 * 规则是否适用于本工程（与后端 `CheckRule::applies_to_env` 同一口径）。
 *
 * 刻意**不做**"按语言筛选"：语言是规则作者声明的先验，用户要问的是
 * "这条规则在我这儿能不能跑"。直接按语言翻，会让人在 Java 工程里打开
 * PHP 规则的开关、然后发现它压根不跑 —— 以为开关坏了。
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

/** 分类展示顺序（其余未知分类追加在末尾）。 */
const CATEGORY_ORDER = ['architecture', 'security', 'contract', 'deadcode'];

/** 分类标题（中英双语，fallback 到原始 slug）。 */
const CATEGORY_LABEL: Record<string, { 'zh-CN': string; 'en-US': string }> = {
  architecture: { 'zh-CN': '架构', 'en-US': 'Architecture' },
  security: { 'zh-CN': '安全', 'en-US': 'Security' },
  contract: { 'zh-CN': '契约', 'en-US': 'Contract' },
  deadcode: { 'zh-CN': '死代码', 'en-US': 'Dead Code' },
};

function catLabel(cat: string, lang: 'zh-CN' | 'en-US'): string {
  return CATEGORY_LABEL[cat]?.[lang] ?? CATEGORY_LABEL[cat]?.['en-US'] ?? cat;
}

/**
 * 本工程对一条规则的**草稿**：启用态覆盖 + 参数覆盖。
 *
 * 为什么是草稿而不是"点了就写库"：改配置必须重跑一次检查才见效，
 * 逐条写库会让"调三个阈值"变成三次重跑 —— 攒一批再保存才是人想要的。
 */
interface Draft {
  /** `null` = 继承 YAML 全局默认；`true/false` = 工程级覆盖。 */
  enabled: boolean | null;
  options: Record<string, unknown>;
}

/** 无覆盖态：启用态与参数都回归全局默认。 */
const INHERIT: Draft = { enabled: null, options: {} };

function draftOf(cfg: Partial<ProjectRuleConfig> | undefined): Draft {
  return { enabled: cfg?.enabled ?? null, options: { ...(cfg?.options ?? {}) } };
}

function draftsOf(cfgs: Record<string, ProjectRuleConfig>): Record<string, Draft> {
  const out: Record<string, Draft> = {};
  for (const [ruleId, cfg] of Object.entries(cfgs)) out[ruleId] = draftOf(cfg);
  return out;
}

/** 参数在草稿里的取值：没被覆盖过就取规则默认值。 */
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
 * 规则集页：按分类展示「这个工程会被哪些规则检查」，并允许**按工程**覆盖
 * 启用态与可调参数。
 *
 * 与结果页分离 —— 规则是知识库驱动的声明，本页负责"要按什么口径检查"；
 * 想验证单条规则可点「只跑这条规则」，会跳到结果页。
 */
export function RulesPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t, lang } = useLocale();
  const navigate = useNavigate();
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [running, setRunning] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  // `saved` = 服务端已存值（比对基准），`drafts` = 用户正在编辑的值。
  const [saved, setSaved] = useState<Record<string, ProjectRuleConfig>>({});
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  // ---- 筛选：规则会随语言扩展增长到几十条，"找规则 / 找我改过哪些"必须有入口。
  const [keyword, setKeyword] = useState('');
  const [severities, setSeverities] = useState<Severity[]>([]);
  const [stateFilter, setStateFilter] = useState<StateFilter>('all');
  /** 正在编辑参数的规则（没有可调参数的规则不会出现入口，也就不会进这个状态）。 */
  const [editing, setEditing] = useState<CheckRule | null>(null);

  const rules = useAsync(() => checkApi.rules(), []);
  const configs = useAsync(() => checkApi.ruleConfigs(id), [id]);
  // 子工程的语言/框架 = 本工程的技术栈环境，用它判断规则适不适用。
  const subs = useAsync(() => projectApi.subProjects(id), [id]);
  // 本工程当前落库违规：按 rule_id 计数，用于规则卡上显示「命中 N 条」。
  // 与结果页同源（上限 5000，超限时此计数反映「已载入」而非全量）。
  const violations = useAsync(() => checkApi.violations(id, 5000), [id]);
  const ruleCounts = useMemo(() => {
    const m: Record<string, number> = {};
    for (const v of violations.data ?? []) m[v.rule_id] = (m[v.rule_id] ?? 0) + 1;
    return m;
  }, [violations.data]);

  // 配置到达后播种一次；之后草稿由用户掌控（刷新不会悄悄覆盖手上的编辑）。
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

  /** 环境闸门：不适用的规则跑不起来（后端会自动跳过），开关对它无效。 */
  const applicable = (r: CheckRule): boolean =>
    appliesToEnv(r, env.languages, env.frameworks);

  /** 筛选后的规则：分类折叠与「整组启用」都只作用于**看得见的**这些。 */
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

  // 按分类聚合（保持 CATEGORY_ORDER 顺序，未知分类追加其后）。
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

  /** 与服务端已存值相比，找出真正改过的规则。 */
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

  /** 分类级批量：只改草稿，保存时一次性落库。 */
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
        // 既没有启用覆盖、也没有参数覆盖 = 回归继承态，直接删行而不是留一条空配置。
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
      // 改了口径就必须重跑：库里的违规是"上次口径"的结论，不重跑等于没改。
      setRunning('__all__');
      const report = await checkApi.check(id);
      refreshCheckSummary();
      message.success(
        t('已保存并重跑：命中 {{n}} 条违规（跑 {{run}}/{{total}} 条规则）')
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
    message.info(t('已放弃未保存的修改'));
  };

  const runOne = async (ruleId: string) => {
    setRunning(ruleId);
    try {
      await checkApi.check(id, [ruleId]);
      refreshCheckSummary();
      message.success(t('已单独重跑该规则，跳转到结果页'));
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
          placeholder={t('留空 = 不过滤')}
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
              {t('已覆盖')}
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
          {t('默认')} {String(p.default) === '' ? t('（空）') : String(p.default)}
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
            <Tooltip title={fits ? '' : t('本工程不是该规则的适用环境，检查时会被自动跳过')}>
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
              {hits} {t('条违规')}
            </Tag>
            {d.enabled === null ? (
              <Tag>
                {t('继承默认')}：{r.enabled ? t('启用') : t('停用')}
              </Tag>
            ) : (
              <Tag color={d.enabled ? 'green' : 'default'}>
                {t('工程覆盖')}：{d.enabled ? t('启用') : t('停用')}
              </Tag>
            )}
            {fits ? null : (
              <Tag color="default">
                {t('不适用')}：{t('需要')} {needs}
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
                  {t('参数设置')}
                  {paramOverridden ? ` · ${t('已改')}` : ''}
                </Button>
              </Badge>
            ) : null}
            {overridden ? (
              <Tooltip title={t('清除本工程的覆盖，回到 YAML 全局默认')}>
                <Button size="small" icon={<UndoOutlined />} onClick={() => setDraft(r.id, INHERIT)}>
                  {t('恢复默认')}
                </Button>
              </Tooltip>
            ) : null}
            <Button
              size="small"
              icon={<PlayCircleOutlined />}
              loading={running === r.id}
              onClick={() => void runOne(r.id)}
            >
              {t('只跑这条规则')}
            </Button>
          </Space>
        }
      >
        <div style={{ color: 'rgba(0,0,0,0.65)', fontSize: 13 }}>
          <div>{r.description ?? t('（无说明）')}</div>
          <div style={{ marginTop: 8 }}>
            <Typography.Text type="secondary">
              {t('作用范围')}：{r.applies_to.kinds.join(', ') || t('不限种类')}
            </Typography.Text>
          </div>
          <div style={{ marginTop: 6 }}>
            <Typography.Text type="secondary">
              {t('适用环境')}：
              {r.applies_to.languages?.length
                ? r.applies_to.languages.join(', ')
                : t('跨语言通用')}
              {r.applies_to.frameworks?.length ? ` · ${r.applies_to.frameworks.join(', ')}` : ''}
            </Typography.Text>
          </div>
          {r.remediation ? (
            <div style={{ marginTop: 6 }}>
              <Typography.Text type="secondary">
                {t('处理建议')}：{r.remediation}
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
        title={t('规则集')}
        subtitle={t('规则由后端 YAML 声明，前端只渲染；可在本工程内覆盖启用态与阈值，保存后自动重跑')}
        extra={
          <Space>
            <Button onClick={discard} disabled={!dirty || saving}>
              {t('放弃修改')}
            </Button>
            <Popconfirm
              title={t('保存并重跑')}
              description={t('将对 {{n}} 条规则写入覆盖，并重跑一次全量检查').replace(
                '{{n}}',
                String(dirtyIds.length),
              )}
              okText={t('保存并重跑')}
              cancelText={t('取消')}
              onConfirm={() => void save()}
            >
              <Button type="primary" loading={saving || running === '__all__'} disabled={!dirty}>
                {t('保存并重跑')}
              </Button>
            </Popconfirm>
          </Space>
        }
      />

      {rules.loading ? (
        <Typography.Text type="secondary">{t('加载中…')}</Typography.Text>
      ) : sorted.length === 0 ? (
        <Empty description={t('没有装载任何规则')} />
      ) : (
        <>
          <Space size={8} wrap style={{ marginBottom: 12 }}>
            <Input
              allowClear
              value={keyword}
              onChange={(e) => setKeyword(e.target.value)}
              prefix={<SearchOutlined style={{ color: 'rgba(0,0,0,0.35)' }} />}
              placeholder={t('搜索规则 id / 名称 / 说明')}
              style={{ width: 260 }}
            />
            <Select<Severity[]>
              mode="multiple"
              allowClear
              value={severities}
              onChange={setSeverities}
              placeholder={t('严重度')}
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
                {t('清除筛选')}
              </Button>
            ) : null}
          </Space>

          {filtered.length === 0 ? (
            <Empty description={t('没有匹配的规则')}>
              <Button size="small" onClick={clearFilters}>
                {t('清除筛选')}
              </Button>
            </Empty>
          ) : (
            <Collapse
              defaultActiveKey={categories}
              items={categories.map((cat) => {
                const list = byCat.get(cat)!;
                // 不适用的规则即便开着也不会跑，不能计进"启用 N"。
                const on = list.filter(
                  (r) => applicable(r) && (draftFor(r.id).enabled ?? r.enabled),
                ).length;
                // 本分类下各规则当前命中的违规总数。
                const hits = list.reduce((s, r) => s + (ruleCounts[r.id] ?? 0), 0);
                return {
                  key: cat,
                  label: (
                    <Space size={8}>
                      <span style={{ fontWeight: 600 }}>{catLabel(cat, lang)}</span>
                      <Tag>{list.length}</Tag>
                      <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                        {t('启用')} {on}
                      </Typography.Text>
                      <Tag color={hits > 0 ? 'volcano' : 'default'}>
                        {t('违规')} {hits}
                      </Tag>
                    </Space>
                  ),
                  // 阻止冒泡：否则点"整组启用"会顺手把分组折叠掉。
                  extra: (
                    <Tooltip title={t('只对当前筛选出的规则生效')}>
                      <Space size={4} onClick={(e) => e.stopPropagation()}>
                        <Button size="small" onClick={() => setCategoryEnabled(cat, true)}>
                          {t('全部启用')}
                        </Button>
                        <Button size="small" onClick={() => setCategoryEnabled(cat, false)}>
                          {t('全部停用')}
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
        message={t('规则是知识库驱动的：新增 / 修改 YAML 规则后前端无需改动，建图或点「刷新」即生效')}
        description={t('工程级覆盖只记「与全局默认不同的那部分」：恢复默认 = 删除覆盖行，规则随 YAML 演进')}
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
        okText={t('完成')}
        cancelText={t('取消')}
        onOk={() => setEditing(null)}
        // 「恢复默认」放在左侧：它是破坏性操作，不能和「完成/取消」混成一排随手点到。
        footer={(_, { OkBtn, CancelBtn }) => (
          <div style={{ display: 'flex', justifyContent: 'space-between' }}>
            <Button
              danger
              disabled={editing ? Object.keys(draftFor(editing.id).options).length === 0 : true}
              onClick={() => {
                if (!editing) return;
                const d = draftFor(editing.id);
                setDraft(editing.id, { ...d, options: {} });
                message.success(t('已恢复规则默认参数，点「保存并重跑」生效'));
              }}
            >
              {t('参数恢复默认')}
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
              {t('改动只进草稿：关掉这个窗口后，点右上角「保存并重跑」才会写库并重跑检查')}
            </Typography.Paragraph>
            {editing.params?.map((p) => renderParam(editing, p))}
          </>
        ) : null}
      </Modal>
    </>
  );
}
