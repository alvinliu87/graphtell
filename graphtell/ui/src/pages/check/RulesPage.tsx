import { useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  Alert,
  Button,
  Card,
  Collapse,
  Empty,
  Input,
  InputNumber,
  Popconfirm,
  Select,
  Space,
  Switch,
  Tag,
  Tooltip,
  Typography,
  message,
} from 'antd';
import { PlayCircleOutlined, QuestionCircleOutlined, UndoOutlined } from '@ant-design/icons';
import { useNavigate, useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { checkApi } from '@/entities/check';
import {
  SEVERITY_COLOR,
  SEVERITY_RANK,
  type CheckRule,
  type ProjectRuleConfig,
  type RuleParam,
  type Severity,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';

const SEVERITY_LABEL: Record<Severity, string> = {
  critical: '严重',
  error: '错误',
  warning: '警告',
  info: '提示',
};

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

  const rules = useAsync(() => checkApi.rules(), []);
  const configs = useAsync(() => checkApi.ruleConfigs(id), [id]);

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

  // 按分类聚合（保持 CATEGORY_ORDER 顺序，未知分类追加其后）。
  const { categories, byCat } = useMemo(() => {
    const map = new Map<string, CheckRule[]>();
    for (const r of sorted) {
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
      <Space key={p.key} size={8} wrap align="center">
        <Space size={4}>
          <span style={{ fontSize: 13 }}>{p.label}</span>
          {p.description ? (
            <Tooltip title={p.description}>
              <QuestionCircleOutlined style={{ color: 'rgba(0,0,0,0.45)' }} />
            </Tooltip>
          ) : null}
        </Space>
        {control}
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {t('默认')} {String(p.default) === '' ? t('（空）') : String(p.default)}
        </Typography.Text>
        {overridden ? (
          <Tag color="blue" style={{ marginInlineEnd: 0 }}>
            {t('已覆盖')}
          </Tag>
        ) : null}
      </Space>
    );
  };

  const renderRule = (r: CheckRule): ReactNode => {
    const d = draftFor(r.id);
    const effective = d.enabled ?? r.enabled;
    const overridden = d.enabled !== null || Object.keys(d.options).length > 0;
    return (
      <Card
        key={r.id}
        variant="borderless"
        style={{ borderRadius: 14, opacity: effective ? 1 : 0.6 }}
        title={
          <Space size={8} wrap>
            <Switch size="small" checked={effective} onChange={(v) => setEnabled(r.id, v)} />
            <Tag color={SEVERITY_COLOR[r.severity]}>{SEVERITY_LABEL[r.severity]}</Tag>
            <span style={{ fontWeight: 600 }}>{r.title}</span>
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {r.id}
            </Typography.Text>
            {d.enabled === null ? (
              <Tag>
                {t('继承默认')}：{r.enabled ? t('启用') : t('停用')}
              </Tag>
            ) : (
              <Tag color={d.enabled ? 'green' : 'default'}>
                {t('工程覆盖')}：{d.enabled ? t('启用') : t('停用')}
              </Tag>
            )}
          </Space>
        }
        extra={
          <Space size={8}>
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

          {r.params && r.params.length > 0 ? (
            <div style={{ marginTop: 12, display: 'grid', gap: 10 }}>
              <Typography.Text strong style={{ fontSize: 13 }}>
                {t('可调参数')}
              </Typography.Text>
              {r.params.map((p) => renderParam(r, p))}
            </div>
          ) : (
            <div style={{ marginTop: 10 }}>
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('这条规则没有可调参数')}
              </Typography.Text>
            </div>
          )}
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
        <Collapse
          defaultActiveKey={categories}
          items={categories.map((cat) => {
            const list = byCat.get(cat)!;
            const on = list.filter((r) => draftFor(r.id).enabled ?? r.enabled).length;
            return {
              key: cat,
              label: (
                <Space size={8}>
                  <span style={{ fontWeight: 600 }}>{catLabel(cat, lang)}</span>
                  <Tag>{list.length}</Tag>
                  <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                    {t('启用')} {on}
                  </Typography.Text>
                </Space>
              ),
              // 阻止冒泡：否则点"整组启用"会顺手把分组折叠掉。
              extra: (
                <Space size={4} onClick={(e) => e.stopPropagation()}>
                  <Button size="small" onClick={() => setCategoryEnabled(cat, true)}>
                    {t('全部启用')}
                  </Button>
                  <Button size="small" onClick={() => setCategoryEnabled(cat, false)}>
                    {t('全部停用')}
                  </Button>
                </Space>
              ),
              children: <div style={{ display: 'grid', gap: 16 }}>{list.map(renderRule)}</div>,
            };
          })}
        />
      )}

      <Alert
        type="info"
        showIcon
        style={{ marginTop: 16 }}
        message={t('规则是知识库驱动的：新增 / 修改 YAML 规则后前端无需改动，建图或点「刷新」即生效')}
        description={t('工程级覆盖只记「与全局默认不同的那部分」：恢复默认 = 删除覆盖行，规则随 YAML 演进')}
      />
    </>
  );
}
