import { Alert, Card, Input, Select, Space, Table, Tag } from 'antd';
import { useState } from 'react';
import { useParams } from 'react-router-dom';
import { useNodes } from '@/entities/graph';
import { nodeColor } from '@/entities/graph';
import { NodeDetailDrawer } from '@/features/node-detail';
import { PageHeader } from '@/shared/ui/PageHeader';
import { useLocale } from '@/shared/lib/i18n';
import { truncate } from '@/shared/lib/format';

/** Node browse page: search by kind / name and inspect a single node. */
export function ExplorerPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const [kind, setKind] = useState<string | undefined>('Table');
  const [name, setName] = useState<string>('');
  const [selected, setSelected] = useState<number | undefined>();
  const { t } = useLocale();

  const { nodes, loading, error, reload } = useNodes(id, { kind, name: name || undefined, limit: 200 });

  return (
    <>
      <PageHeader title={t('Explorer')} subtitle={t('Search any node on the graph and view its annotations and adjacent edges')} />

      {/* After a failed search data is empty -> the table shows "no matching nodes", indistinguishable from "genuinely 0 nodes";
               report the error explicitly here, so a backend failure isn't disguised as an empty result. */}
      {!loading && error ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('Failed to load nodes')}
          description={
            <div>
              <div>{error}</div>
              <div style={{ marginTop: 8 }}>
                {t('The node query failed — the backend may be down or unreachable. Retry to try again.')}
              </div>
            </div>
          }
        />
      ) : null}
      <Card
        variant="borderless"
        style={{ borderRadius: 14 }}
        extra={
          <Space>
            <Select
              allowClear
              placeholder={t('Node kind')}
              style={{ width: 180 }}
              value={kind}
              onChange={setKind}
              options={[
                'Table',
                'HttpContract',
                'ConfigKey',
                'I18nKey',
                'Cache',
                'Event',
                'Queue',
                'Class',
                'Method',
                'Property',
                'CallSite',
                'Namespace',
              ].map((k) => ({ label: k, value: k }))}
            />
            <Input.Search
              allowClear
              placeholder={t('Filter by name')}
              style={{ width: 260 }}
              value={name}
              onChange={(e) => setName(e.target.value)}
              onSearch={() => void reload()}
            />
          </Space>
        }
      >
        <Table
          rowKey="id"
          loading={loading}
          dataSource={nodes}
          scroll={{ x: 900 }}
          pagination={{ pageSize: 15, showSizeChanger: false }}
          locale={{ emptyText: t('No matching nodes') }}
          columns={[
            {
              title: t('Kind'),
              dataIndex: 'kind',
              width: 130,
              render: (k: string) => (
                <Tag color={nodeColor(k)} style={{ color: '#fff' }}>
                  {k}
                </Tag>
              ),
            },
            { title: t('Name'), dataIndex: 'name', width: 220, ellipsis: true },
            {
              title: t('FQN / Identity'),
              width: 340,
              render: (_, n) => truncate(n.fqn ?? n.identity?.value ?? '-', 64),
            },
            { title: t('Language'), dataIndex: 'language', width: 90 },
            { title: t('Phase'), dataIndex: 'phase', width: 120 },
            {
              title: 'Confidence',
              dataIndex: 'confidence',
              width: 90,
              render: (v: number) => v.toFixed(2),
            },
            {
              title: t('Actions'),
              width: 90,
              render: (_, n) => <a onClick={() => setSelected(n.id)}>{t('Details')}</a>,
            },
          ]}
        />
      </Card>
      <NodeDetailDrawer nodeId={selected} onClose={() => setSelected(undefined)} />
    </>
  );
}
