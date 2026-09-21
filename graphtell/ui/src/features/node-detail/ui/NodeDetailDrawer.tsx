import { Descriptions, Drawer, Empty, Table, Tag } from 'antd';
import { useNodeDetail, edgeColor, nodeColor } from '@/entities/graph';
import { shortName } from '@/shared/lib/format';
import { useLocale } from '@/shared/lib/i18n';

/** 节点详情抽屉：属性、标注、相邻边。 */
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
      title={node ? `${node.kind} · ${node.name}` : t('节点详情')}
      open={nodeId !== undefined}
      onClose={onClose}
      width={620}
      destroyOnClose
    >
      {!node && !loading ? <Empty description={t('未找到该节点')} /> : null}
      {node ? (
        <>
          <Descriptions column={1} size="small" bordered>
            <Descriptions.Item label={t('种类')}>
              <Tag color={nodeColor(node.kind)}>{node.kind}</Tag>
            </Descriptions.Item>
            <Descriptions.Item label={t('名称')}>{node.name}</Descriptions.Item>
            <Descriptions.Item label={t('完全限定名')}>{node.fqn ?? '-'}</Descriptions.Item>
            <Descriptions.Item label="Identity">{node.identity?.value ?? '-'}</Descriptions.Item>
            <Descriptions.Item label={t('位置')}>
              {node.file_id ? `file#${node.file_id}` : '-'}
              {node.start_line ? ` : ${node.start_line}-${node.end_line}` : ''}
            </Descriptions.Item>
            <Descriptions.Item label={t('语言')}>{node.language}</Descriptions.Item>
            <Descriptions.Item label={t('置信度')}>{node.confidence.toFixed(2)}</Descriptions.Item>
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

          <h4 style={{ marginTop: 20 }}>{t('标注')}</h4>
          <Table
            size="small"
            rowKey="id"
            dataSource={annotations}
            pagination={false}
            locale={{ emptyText: t('暂无标注') }}
            columns={[
              { title: t('通道'), dataIndex: 'channel', width: 110 },
              { title: t('种类'), dataIndex: 'kind', width: 130 },
              { title: t('子类型'), dataIndex: 'subkind', width: 110, render: (v?: string) => v ?? '-' },
              {
                title: t('置信度'),
                dataIndex: 'confidence',
                width: 90,
                render: (v: number) => v.toFixed(2),
              },
            ]}
          />

          <h4 style={{ marginTop: 20 }}>
            {t('相邻边（') + neighbors.length + t('）')}
          </h4>
          <Table
            size="small"
            rowKey="id"
            dataSource={neighbors}
            pagination={{ pageSize: 8 }}
            locale={{ emptyText: t('暂无边') }}
            columns={[
              {
                title: t('关系'),
                dataIndex: 'kind',
                width: 130,
                render: (k: string) => (
                  <Tag color={edgeColor(k)} style={{ color: '#fff' }}>
                    {k}
                  </Tag>
                ),
              },
              {
                title: t('方向'),
                width: 90,
                render: (_, e) => (e.from_id === node.id ? t('→ 出') : t('← 入')),
              },
              {
                title: t('对端'),
                render: (_, e) => String(e.from_id === node.id ? e.to_id : e.from_id),
              },
              { title: t('阶段'), dataIndex: 'phase', width: 120 },
              {
                title: t('置信度'),
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

/** 供列表复用：把 FQN 收短。 */
export { shortName };
