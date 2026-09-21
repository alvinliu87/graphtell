import { useState } from 'react';
import { Alert, Button, Card, Collapse, Empty, Space, Tag, Typography, message } from 'antd';
import { PlayCircleOutlined } from '@ant-design/icons';
import { useNavigate, useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { checkApi } from '@/entities/check';
import { SEVERITY_COLOR, SEVERITY_RANK, type CheckRule, type Severity } from '@/entities/check';
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
 * 规则集页：只读参考，按分类展示「这个工程会被哪些规则检查」。
 *
 * 与结果页分离 —— 规则是知识库驱动的声明，本页只说明判据/适用范围，
 * 不掺结果；想验证单条规则可点「只跑这条规则」，会跳到结果页。
 */
export function RulesPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t, lang } = useLocale();
  const navigate = useNavigate();
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [running, setRunning] = useState<string | null>(null);

  const rules = useAsync(() => checkApi.rules(), []);

  const sorted = [...(rules.data ?? [])].sort(
    (a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] || a.id.localeCompare(b.id),
  );

  // 按分类聚合（保持 CATEGORY_ORDER 顺序，未知分类追加其后）。
  const byCat = new Map<string, CheckRule[]>();
  for (const r of sorted) {
    const arr = byCat.get(r.category);
    if (arr) arr.push(r);
    else byCat.set(r.category, [r]);
  }
  const categories = [
    ...CATEGORY_ORDER.filter((c) => byCat.has(c)),
    ...[...byCat.keys()].filter((c) => !CATEGORY_ORDER.includes(c)),
  ];

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

  const renderRule = (r: CheckRule) => (
    <Card
      key={r.id}
      variant="borderless"
      style={{ borderRadius: 14 }}
      title={
        <Space size={8} wrap>
          <Tag color={SEVERITY_COLOR[r.severity]}>{SEVERITY_LABEL[r.severity]}</Tag>
          <span style={{ fontWeight: 600 }}>{r.title}</span>
          <Typography.Text type="secondary" style={{ fontSize: 12 }}>
            {r.id}
          </Typography.Text>
        </Space>
      }
      extra={
        <Button
          size="small"
          icon={<PlayCircleOutlined />}
          loading={running === r.id}
          onClick={() => void runOne(r.id)}
        >
          {t('只跑这条规则')}
        </Button>
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

  return (
    <>
      <PageHeader
        title={t('规则集')}
        subtitle={t('规则由后端 YAML 声明，前端只渲染；按分类聚合，判据/适用范围见每条说明')}
      />

      {rules.loading ? (
        <Typography.Text type="secondary">{t('加载中…')}</Typography.Text>
      ) : sorted.length === 0 ? (
        <Empty description={t('没有装载任何规则')} />
      ) : (
        <Collapse
          defaultActiveKey={categories}
          items={categories.map((cat) => ({
            key: cat,
            label: (
              <Space size={8}>
                <span style={{ fontWeight: 600 }}>{catLabel(cat, lang)}</span>
                <Tag>{byCat.get(cat)!.length}</Tag>
              </Space>
            ),
            children: <div style={{ display: 'grid', gap: 16 }}>{byCat.get(cat)!.map(renderRule)}</div>,
          }))}
        />
      )}

      <Alert
        type="info"
        showIcon
        style={{ marginTop: 16 }}
        message={t('规则是知识库驱动的：新增 / 修改 YAML 规则后前端无需改动，建图或点「刷新」即生效')}
      />
    </>
  );
}
