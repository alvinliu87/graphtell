import { Card, Table, Tag } from 'antd';
import { useParams } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { graphApi, type Diagnostic } from '@/entities/graph';
import { PageHeader } from '@/shared/ui/PageHeader';

const SEVERITY_COLOR: Record<string, string> = {
  info: 'blue',
  warning: 'orange',
  error: 'red',
  critical: 'magenta',
};

/** 诊断页：冲突、缺失、未解析链接 —— 这些本身就是有价值的发现。 */
export function DiagnosticsPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const { data, loading } = useAsync<Diagnostic[]>(() => graphApi.diagnostics(id), [id]);

  return (
    <>
      <PageHeader
        title="诊断"
        subtitle="根节点缺失、路由指向不存在的 handler、identity 冲突等 —— 诊断本身就是分析结论"
      />
      <Card variant="borderless" style={{ borderRadius: 14 }}>
        <Table<Diagnostic>
          rowKey={(d, i) => `${d.code}-${d.location ?? ''}-${i ?? 0}`}
          loading={loading}
          dataSource={data ?? []}
          pagination={{ pageSize: 20 }}
          locale={{ emptyText: '暂无诊断' }}
          columns={[
            {
              title: '严重度',
              dataIndex: 'severity',
              width: 100,
              render: (s: string) => <Tag color={SEVERITY_COLOR[s] ?? 'default'}>{s}</Tag>,
            },
            { title: '代码', dataIndex: 'code', width: 200 },
            { title: '阶段', dataIndex: 'phase', width: 130 },
            { title: '位置', dataIndex: 'location', width: 300, ellipsis: true },
            { title: '说明', dataIndex: 'message' },
          ]}
        />
      </Card>
    </>
  );
}
