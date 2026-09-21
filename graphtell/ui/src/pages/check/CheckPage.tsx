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
import { CopyOutlined, ReloadOutlined, SafetyCertificateOutlined } from '@ant-design/icons';
import { useOutletContext, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';
import { checkApi } from '@/entities/check';
import { projectApi, type SubProject } from '@/entities/project';
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

const SUB_ROLE_LABEL: Record<string, string> = {
  frontend: '前端',
  backend: '后端',
};

/**
 * 合规检查结果页：建图后自动跑出的结论（来自持久化诊断表）。
 *
 * 设计要点：
 * * 进入页面即直接显示**上一次自动检查**的落库结果，无需手动触发；
 * * 顶栏「刷新」用于回填/重算：对从未跑过的工程（如本功能上线前建好的工程）
 *   一键重跑并写回，新工程建图已自动完成这步；
 * * 规则集拆到独立的「规则集」页，本页只谈结论。
 */
export function CheckPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  // 手动重跑/刷新后刷新侧边栏「合规检查」角标。
  const { refreshCheckSummary } = useOutletContext<{ refreshCheckSummary: () => void }>();

  const [report, setReport] = useState<CheckReport | null>(null);
  const [running, setRunning] = useState(false);
  const [runError, setRunError] = useState<string | null>(null);
  const [severity, setSeverity] = useState<Severity | 'all'>('all');
  const [ruleFilter, setRuleFilter] = useState<string | 'all'>('all');
  const [subFilter, setSubFilter] = useState<number[]>([]);
  const [limit] = useState(20);

  // 子工程列表（供子项目筛选器）。
  const { data: subsData } = useAsync(() => projectApi.subProjects(id), [id]);
  const subs: SubProject[] = subsData ?? [];

  // 进入即加载上一次落库结果（自动检查已写入），不重跑；按子项目筛选时服务端已过滤。
  const stored = useAsync(
    () => checkApi.violations(id, 500, subFilter.length ? subFilter : undefined),
    [id, subFilter],
  );
  // 规则列表用于严重度筛选下拉与「已装载规则」计数。
  const rules = useAsync(() => checkApi.rules(), []);

  const violations = report?.violations ?? stored.data ?? [];
  // 子项目筛选：命中任一所选子工程，或归属为空（共享资源，如图视图「共享节点始终显示」）。
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

  const refresh = async () => {
    setRunning(true);
    setRunError(null);
    try {
      const r = await checkApi.check(id, []);
      setReport(r);
      refreshCheckSummary();
      if (r.violations.length === 0 && r.rules_silent.length === 0) {
        message.success(t('刷新完成，没有命中任何违规'));
      }
    } catch (e) {
      setRunError(e instanceof Error ? e.message : String(e));
    } finally {
      setRunning(false);
    }
  };

  const counts = useMemo(() => {
    const c: Record<string, number> = { critical: 0, error: 0, warning: 0, info: 0 };
    for (const v of scoped) c[v.severity] = (c[v.severity] ?? 0) + 1;
    return c;
  }, [scoped]);

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
    message.success(t('已复制定位'));
  };

  return (
    <>
      <PageHeader
        title={t('合规检查')}
        subtitle={t(
          '建图后自动跑出的规则结论（持久化），规则集见「规则集」页 —— 新增规则无需改前端',
        )}
        extra={
          <Space>
            <Button
              icon={<ReloadOutlined />}
              loading={running}
              onClick={() => void refresh()}
            >
              {t('刷新')}
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

      {!hasResults && !stored.loading ? (
        <Empty
          description={t('还没有检查结果 —— 点右上角「刷新」运行一次（新工程建图会自动跑）')}
          style={{ marginBlock: 48 }}
        />
      ) : null}

      {hasResults ? (
        <>
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

          <Card
            variant="borderless"
            style={{ borderRadius: 14 }}
            title={t('违规')}
            extra={
              report ? (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('耗时')} {report.duration_ms}ms · {t('跑了')} {report.rules_run} {t('条规则')}
                </Typography.Text>
              ) : (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('显示上一次自动检查的结果，点「刷新」可重算')}
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
              <Select
                size="small"
                mode="multiple"
                allowClear
                style={{ minWidth: 200 }}
                placeholder={t('全部子工程')}
                value={subFilter}
                onChange={(v) => setSubFilter(v ?? [])}
                options={subs.map((s) => ({
                  label: `${s.name}（${SUB_ROLE_LABEL[s.role] ?? s.role}）`,
                  value: s.id,
                }))}
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
        </>
      ) : null}
    </>
  );
}
