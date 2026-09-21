import { useMemo, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Col,
  Collapse,
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
import {
  CopyOutlined,
  PlayCircleOutlined,
  SafetyCertificateOutlined,
} from '@ant-design/icons';
import { useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';
import { checkApi } from '@/entities/check';
import {
  SEVERITY_COLOR,
  SEVERITY_RANK,
  type CheckReport,
  type Severity,
  type Violation,
} from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';

const SEVERITY_LABEL: Record<Severity, string> = {
  critical: '严重',
  error: '错误',
  warning: '警告',
  info: '提示',
};

/**
 * 合规检查页：在图上按规则给出的结论。
 *
 * 设计要点：
 * * 规则集来自后端 YAML —— 页面不认识任何具体规则，后端加规则前端无需改动；
 * * 「运行检查」可反复执行，后端保证重跑会清空上一轮违规，UI 看到的永远是
 *   **当前代码**的结论，而不是历史堆积；
 * * 每条违规都给出 `path:line`，可一键复制去 IDE 定位 —— 结论必须可验证。
 */
export function CheckPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  // 手动跑完后刷新侧边栏「合规检查」角标（建图自动跑时由菜单自身读取，无需此处）。
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [report, setReport] = useState<CheckReport | null>(null);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);
  const [severity, setSeverity] = useState<Severity | 'all'>('all');
  const [ruleFilter, setRuleFilter] = useState<string | 'all'>('all');
  const [limit] = useState(20);

  const rules = useAsync(() => checkApi.rules(), []);
  const stored = useAsync(() => checkApi.violations(id, 500), [id]);

  const violations = report?.violations ?? stored.data ?? [];

  const run = async () => {
    setRunning(true);
    setRunError(null);
    try {
      const r = await checkApi.check(id, []);
      setReport(r);
      refreshCheckSummary();
      // 有静默规则时不能报"没问题" —— 那会把"规则瞎了"说成"代码干净"。
      if (r.violations.length === 0 && r.rules_silent.length === 0) {
        message.success(t('检查完成，没有命中任何违规'));
      }
    } catch (e) {
      setRunError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  const runOne = async (ruleId: string) => {
    setRunning(true);
    setRunError(null);
    try {
      const r = await checkApi.check(id, [ruleId]);
      setReport(r);
      refreshCheckSummary();
      setRuleFilter(ruleId);
    } catch (e) {
      setRunError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  const counts = useMemo(() => {
    const c: Record<string, number> = { critical: 0, error: 0, warning: 0, info: 0 };
    for (const v of violations) c[v.severity] = (c[v.severity] ?? 0) + 1;
    return c;
  }, [violations]);

  const filtered = useMemo(() => {
    return violations
      .filter((v) => severity === 'all' || v.severity === severity)
      .filter((v) => ruleFilter === 'all' || v.rule_id === ruleFilter)
      .slice()
      .sort(
        (a, b) =>
          SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] ||
          a.rule_id.localeCompare(b.rule_id),
      );
  }, [violations, severity, ruleFilter]);

  const copyLocation = (v: Violation) => {
    const loc = v.file ? `${v.file}${v.line ? `:${v.line}` : ''}` : '';
    if (!loc) return;
    void navigator.clipboard?.writeText(loc);
    message.success(t('已复制定位'));
  };

  const ruleItems = (rules.data ?? []).map((r) => ({
    key: r.id,
    label: (
      <Space size={8} wrap>
        <Tag color={SEVERITY_COLOR[r.severity]}>{SEVERITY_LABEL[r.severity]}</Tag>
        <span style={{ fontWeight: 600 }}>{r.title}</span>
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          {r.category} · {r.id}
        </Typography.Text>
      </Space>
    ),
    children: (
      <div style={{ color: 'rgba(0,0,0,0.65)', fontSize: 13 }}>
        <div>{r.description ?? t('（无说明）')}</div>
        <div style={{ marginTop: 8 }}>
          <Typography.Text type="secondary">
            {t('作用范围')}：{r.applies_to.kinds.join(', ')}
          </Typography.Text>
        </div>
        <div style={{ marginTop: 6 }}>
          <Typography.Text type="secondary">
            {t('适用环境')}：
            {r.applies_to.languages?.length
              ? r.applies_to.languages.join(', ')
              : t('跨语言通用')}
            {r.applies_to.frameworks?.length
              ? ` · ${r.applies_to.frameworks.join(', ')}`
              : ''}
          </Typography.Text>
        </div>
        {r.remediation ? (
          <div style={{ marginTop: 6 }}>
            <Typography.Text type="secondary">
              {t('处理建议')}：{r.remediation}
            </Typography.Text>
          </div>
        ) : null}
        <Button
          size="small"
          style={{ marginTop: 10 }}
          icon={<PlayCircleOutlined />}
          loading={running}
          onClick={() => void runOne(r.id)}
        >
          {t('只跑这条规则')}
        </Button>
      </div>
    ),
  }));

  return (
    <>
      <PageHeader
        title={t('合规检查')}
        subtitle={t(
          '按 YAML 声明的规则在图上检测违规 —— 规则由后端知识库驱动，新增规则不需要改代码',
        )}
        extra={
          <Space>
            <Button
              type="primary"
              icon={<SafetyCertificateOutlined />}
              loading={running}
              onClick={() => void run()}
            >
              {t('运行检查')}
            </Button>
          </Space>
        }
      />

      {runError ? (
        <Alert type="error" showIcon message={runError} style={{ marginBottom: 16 }} />
      ) : null}

      {report && report.rules_silent.length > 0 ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 16 }}
          message={`${t('有')} ${report.rules_silent.length} ${t('条规则跑了但 0 命中')}`}
          description={
            <div>
              <div>
                {t(
                  '规则最危险的失效方式不是误报，而是静默归零：判据用了一个图上不存在的标注或边，于是永远匹配不上。在排除「代码真干净」之前，先怀疑规则瞎了。',
                )}
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
          message={`${t('有')} ${report.rules_unavailable.length} ${t('条规则判据不成立，已停用')}`}
          description={
            <div>
              <div>
                {t(
                  '判据提到的边/标注在本工程图上一个都没有，跑下去只会产出恒真误报（例如「没有 X 入边」在 X 不存在时对每个节点都成立）。宁可不跑，也不要报一堆假的。',
                )}
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
          message={`${t('有')} ${report.rules_not_applicable.length} ${t('条规则不适用于本工程技术栈')}`}
          description={
            <ul style={{ marginBlock: 8, paddingLeft: 20 }}>
              {report.rules_not_applicable.map((s) => (
                <li key={s}>{s}</li>
              ))}
            </ul>
          }
        />
      ) : null}

      <Row gutter={[16, 16]} style={{ marginBottom: 16 }}>
        <Col xs={12} md={6}>
          <StatCard
            title={t('已装载规则')}
            value={report?.rules_total ?? rules.data?.length ?? 0}
            accent="#7c5cff"
          />
        </Col>
        <Col xs={12} md={6}>
          <StatCard title={t('错误')} value={counts.error ?? 0} accent="#ff4d4f" />
        </Col>
        <Col xs={12} md={6}>
          <StatCard title={t('警告')} value={counts.warning ?? 0} accent="#fa8c16" />
        </Col>
        <Col xs={12} md={6}>
          <StatCard title={t('提示')} value={counts.info ?? 0} accent="#3d7eff" />
        </Col>
      </Row>

      <Row gutter={[16, 16]}>
        <Col xs={24} lg={9}>
          <Card
            variant="borderless"
            title={t('规则集')}
            style={{ borderRadius: 14 }}
            styles={{ body: { paddingTop: 8 } }}
          >
            {rules.loading ? (
              <Typography.Text type="secondary">{t('加载中…')}</Typography.Text>
            ) : ruleItems.length === 0 ? (
              <Empty description={t('没有装载任何规则')} />
            ) : (
              <Collapse ghost items={ruleItems} />
            )}
          </Card>
        </Col>
        <Col xs={24} lg={15}>
          <Card
            variant="borderless"
            style={{ borderRadius: 14 }}
            title={t('违规')}
            extra={
              report ? (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('耗时')} {report.duration_ms}ms · {t('跑了')} {report.rules_run}{' '}
                  {t('条规则')}
                </Typography.Text>
              ) : (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('显示上一次检查的结果，点「运行检查」可重新检测')}
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
                  { label: t('全部'), value: 'all' },
                  { label: t('错误'), value: 'error' },
                  { label: t('警告'), value: 'warning' },
                  { label: t('提示'), value: 'info' },
                ]}
              />
              <Select
                size="small"
                style={{ minWidth: 220 }}
                value={ruleFilter}
                onChange={setRuleFilter}
                options={[
                  { label: t('全部规则'), value: 'all' },
                  ...(rules.data ?? []).map((r) => ({ label: r.title, value: r.id })),
                ]}
              />
            </Space>
            <Table<Violation>
              rowKey={(v) => `${v.rule_id}-${v.node_id}`}
              loading={stored.loading && !report}
              dataSource={filtered}
              pagination={{ pageSize: limit }}
              locale={{ emptyText: t('没有命中的违规') }}
              columns={[
                {
                  title: t('严重度'),
                  dataIndex: 'severity',
                  width: 92,
                  render: (s: Severity) => (
                    <Tag color={SEVERITY_COLOR[s]}>{SEVERITY_LABEL[s]}</Tag>
                  ),
                },
                { title: t('规则'), dataIndex: 'rule_id', width: 190, ellipsis: true },
                {
                  title: t('对象'),
                  dataIndex: 'node_name',
                  width: 220,
                  ellipsis: true,
                  render: (name: string, v) => (
                    <Tooltip title={`${v.node_kind} · ${name}`}>
                      <span>{name}</span>
                    </Tooltip>
                  ),
                },
                { title: t('说明'), dataIndex: 'message' },
                {
                  title: t('位置'),
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
        </Col>
      </Row>
    </>
  );
}
