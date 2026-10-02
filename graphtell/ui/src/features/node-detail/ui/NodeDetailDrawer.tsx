import { Descriptions, Drawer, Empty, Table, Tag } from 'antd';
import { useNodeDetail, edgeColor, nodeColor } from '@/entities/graph';
import { shortName } from '@/shared/lib/format';
import { useLocale } from '@/shared/lib/i18n';

/** Node detail drawer: properties, annotations, adjacent edges. */
export function NodeDetailDrawer({
  nodeId,
  onClose,
}: {
  nodeId: number | undefined;
  onClose: () => void;
}) {
  const { node, neighbors, annotations, loading } = useNodeDetail(nodeId);
  const { t } = useLocale();

  return (
    <Drawer
      title={node ? `${node.kind} · ${node.name}` : t('Node details')}
      open={nodeId !== undefined}
      onClose={onClose}
      width={620}
      destroyOnClose
    >
      {!node && !loading ? <Empty description={t('Node not found')} /> : null}
      {node ? (
        <>
          <Descriptions column={1} size="small" bordered>
            <Descriptions.Item label={t('Kind')}>
              <Tag color={nodeColor(node.kind)}>{node.kind}</Tag>
            </Descriptions.Item>
            <Descriptions.Item label={t('Name')}>{node.name}</Descriptions.Item>
            <Descriptions.Item label={t('Fully qualified name')}>{node.fqn ?? '-'}</Descriptions.Item>
            <Descriptions.Item label="Identity">{node.identity?.value ?? '-'}</Descriptions.Item>
            <Descriptions.Item label={t('Location')}>
              {node.file_id ? `file#${node.file_id}` : '-'}
              {node.start_line ? ` : ${node.start_line}-${node.end_line}` : ''}
            </Descriptions.Item>
            <Descriptions.Item label={t('Language')}>{node.language}</Descriptions.Item>
            <Descriptions.Item label={t('Confidence')}>{node.confidence.toFixed(2)}</Descriptions.Item>
          </Descriptions>

          {node.properties ? (
            <pre
              style={{
                marginTop: 16,
                padding: 12,
                background: '#f8fafc',
                borderRadius: 8,
                fontSize: 12,
                maxHeight: 220,
                overflow: 'auto',
              }}
            >
              {JSON.stringify(node.properties, null, 2)}
            </pre>
          ) : null}

          <h4 style={{ marginTop: 20 }}>{t('Annotations')}</h4>
          <Table
            size="small"
            rowKey="id"
            dataSource={annotations}
            pagination={false}
            locale={{ emptyText: t('No annotations') }}
            columns={[
              { title: t('Channel'), dataIndex: 'channel', width: 110 },
              { title: t('Kind'), dataIndex: 'kind', width: 130 },
              { title: t('Subtype'), dataIndex: 'subkind', width: 110, render: (v?: string) => v ?? '-' },
              {
                title: t('Confidence'),
                dataIndex: 'confidence',
                width: 90,
                render: (v: number) => v.toFixed(2),
              },
            ]}
          />

          <h4 style={{ marginTop: 20 }}>
            {t('Adjacent edges (') + neighbors.length + t('）')}
          </h4>
          <Table
            size="small"
            rowKey="id"
            dataSource={neighbors}
            pagination={{ pageSize: 8 }}
            locale={{ emptyText: t('No edges') }}
            columns={[
              {
                title: t('Relation'),
                dataIndex: 'kind',
                width: 130,
                render: (k: string) => (
                  <Tag color={edgeColor(k)} style={{ color: '#fff' }}>
                    {k}
                  </Tag>
                ),
              },
              {
                title: t('Direction'),
                width: 90,
                render: (_, e) => (e.from_id === node.id ? t('→ out') : t('← in')),
              },
              {
                title: t('Opposite'),
                render: (_, e) => String(e.from_id === node.id ? e.to_id : e.from_id),
              },
              { title: t('Phase'), dataIndex: 'phase', width: 120 },
              {
                title: t('Confidence'),
                dataIndex: 'confidence',
                width: 90,
                render: (v: number) => v.toFixed(2),
              },
            ]}
          />
        </>
      ) : null}
    </Drawer>
  );
}

/** Reused by lists: shorten an FQN. */
export { shortName };
