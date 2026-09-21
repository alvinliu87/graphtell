import { Alert, Button, Card, Space, Table, Tag } from 'antd';
import { useNavigate, useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { graphApi, type Diagnostic } from '@/entities/graph';
import { checkApi } from '@/entities/check';
import { useLocale } from '@/shared/lib/i18n';
import { PageHeader } from '@/shared/ui/PageHeader';

const SEVERITY_COLOR: Record<string, string> = {
  info: 'blue',
  warning: 'orange',
  error: 'red',
  critical: 'magenta',
};

/**
 * 诊断页：**建图期**诊断（冲突、缺失、未解析链接）—— 这些本身就是有价值的发现。
 *
 * 合规检查的结论**刻意不在这里**：两者虽然都存在诊断表里，但性质不同
 * （"图没建全" vs "代码违反了规则"），混在一张表里只会让两类结论都读不懂。
 * 这里给一个显式入口指过去，避免用户以为"一条错误都没有"。
 */
export function DiagnosticsPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { t } = useLocale();
  const navigate = useNavigate();
  const { data, loading } = useAsync<Diagnostic[]>(() => graphApi.diagnostics(id), [id]);
  const summary = useAsync(() => checkApi.summary(id), [id]);

  const s = summary.data;
  const total = s ? s.critical + s.error + s.warning + s.info : 0;

  return (
    <>
      <PageHeader
        title={t('诊断')}
        subtitle={t('根节点缺失、路由指向不存在的 handler、identity 冲突等 —— 诊断本身就是分析结论')}
      />

      {total > 0 ? (
        <Alert
          type={s && s.error + s.critical > 0 ? 'warning' : 'info'}
          showIcon
          style={{ marginBottom: 12 }}
          message={t('本页只列建图期诊断；合规检查的结论不在这里')}
          description={
            <Space size={8} wrap>
              <span>
                {t('合规检查另有')} {total} {t('条')}（
                {t('错误')} {s?.error ?? 0} · {t('警告')} {s?.warning ?? 0} · {t('提示')}{' '}
                {s?.info ?? 0}）
              </span>
              <Button size="small" type="primary" onClick={() => navigate(`/projects/${id}/check`)}>
                {t('去质量门禁查看')}
              </Button>
            </Space>
          }
        />
      ) : null}
      <Card variant="borderless" style={{ borderRadius: 14 }}>
        <Table<Diagnostic>
          rowKey={(d, i) => `${d.code}-${d.location ?? ''}-${i ?? 0}`}
          loading={loading}
          dataSource={data ?? []}
          pagination={{ pageSize: 20 }}
          locale={{ emptyText: t('暂无诊断') }}
          columns={[
            {
              title: t('严重度'),
              dataIndex: 'severity',
              width: 100,
              render: (s: string) => <Tag color={SEVERITY_COLOR[s] ?? 'default'}>{s}</Tag>,
            },
            { title: t('代码'), dataIndex: 'code', width: 200 },
            { title: t('阶段'), dataIndex: 'phase', width: 130 },
            { title: t('位置'), dataIndex: 'location', width: 300, ellipsis: true },
            { title: t('说明'), dataIndex: 'message' },
          ]}
        />
      </Card>
    </>
  );
}
