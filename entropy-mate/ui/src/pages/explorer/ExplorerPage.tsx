import { Card, Input, Select, Space, Table, Tag } from 'antd';
import { useState } from 'react';
import { useParams } from 'react-router-dom';
import { useNodes } from '@/entities/graph';
import { nodeColor } from '@/entities/graph';
import { NodeDetailDrawer } from '@/features/node-detail';
import { PageHeader } from '@/shared/ui/PageHeader';
import { truncate } from '@/shared/lib/format';

/** 节点浏览页：按种类/名称检索并查看单个节点。 */
export function ExplorerPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const [kind, setKind] = useState<string | undefined>('Table');
  const [name, setName] = useState<string>('');
  const [selected, setSelected] = useState<number | undefined>();

  const { nodes, loading, reload } = useNodes(id, { kind, name: name || undefined, limit: 200 });

  return (
    <>
      <PageHeader title="节点浏览" subtitle="检索图上的任意节点，并查看它的标注与相邻边" />
      <Card
        variant="borderless"
        style={{ borderRadius: 14 }}
        extra={
          <Space>
            <Select
              allowClear
              placeholder="节点种类"
              style={{ width: 180 }}
              value={kind}
              onChange={setKind}
              options={[
                'Table',
                'HttpContract',
                'ConfigKey',
                'ExternalSystem',
                'I18nKey',
                'Class',
                'Method',
                'Property',
                'CallSite',
                'Namespace',
              ].map((k) => ({ label: k, value: k }))}
            />
            <Input.Search
              allowClear
              placeholder="按名称过滤"
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
          locale={{ emptyText: '没有匹配的节点' }}
          columns={[
            {
              title: '种类',
              dataIndex: 'kind',
              width: 130,
              render: (k: string) => (
                <Tag color={nodeColor(k)} style={{ color: '#fff' }}>
                  {k}
                </Tag>
              ),
            },
            { title: '名称', dataIndex: 'name', width: 220, ellipsis: true },
            {
              title: '完全限定名 / Identity',
              width: 340,
              render: (_, n) => truncate(n.fqn ?? n.identity ?? '-', 64),
            },
            { title: '语言', dataIndex: 'language', width: 90 },
            { title: '阶段', dataIndex: 'phase', width: 120 },
            {
              title: '置信度',
              dataIndex: 'confidence',
              width: 90,
              render: (v: number) => v.toFixed(2),
            },
            {
              title: '操作',
              width: 90,
              render: (_, n) => <a onClick={() => setSelected(n.id)}>详情</a>,
            },
          ]}
        />
      </Card>
      <NodeDetailDrawer nodeId={selected} onClose={() => setSelected(undefined)} />
    </>
  );
}
