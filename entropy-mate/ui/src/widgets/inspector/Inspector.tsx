import { Alert, Descriptions, Drawer, Empty, Space, Tag, Typography } from 'antd';
import { useEffect, useState } from 'react';
import type { EdgeEvidence, NodeLocations, SourceLocation } from '@/entities/view';
import { viewApi } from '@/entities/view';
import { nodeColor } from '@/entities/graph';
import { useAsync } from '@/shared/lib/useAsync';
import { LocationBadge, LocationList } from './LocationList';

/**
 * 右侧 Inspector。
 *
 * 两类用途：
 * 1. **没有对应视角的节点**（`ConfigKey` / `KeyPattern` / `Component` / `SecretLocation`）
 *    —— 点它**不切顶部筛选器**，只在这里显示属性与"另有 N 处引用"
 * 2. 边 —— 显示证据链：实边单点、虚线边展开途经的每个 CallSite 位置
 */
export function Inspector({
  nodeId,
  edgeId,
  projectRoot,
  onClose,
  onJumpToReference,
}: {
  nodeId: number | null;
  edgeId: number | null;
  projectRoot?: string;
  onClose: () => void;
  onJumpToReference?: (nodeId: number) => void;
}) {
  const open = nodeId !== null || edgeId !== null;

  return (
    <Drawer
      title={nodeId !== null ? '节点详情' : '边证据链'}
      open={open}
      onClose={onClose}
      width={560}
      destroyOnClose
    >
      {nodeId !== null ? (
        <NodePanel
          nodeId={nodeId}
          projectRoot={projectRoot}
          onJumpToReference={onJumpToReference}
        />
      ) : null}
      {edgeId !== null ? <EdgePanel edgeId={edgeId} projectRoot={projectRoot} /> : null}
    </Drawer>
  );
}

function NodePanel({
  nodeId,
  projectRoot,
  onJumpToReference,
}: {
  nodeId: number;
  projectRoot?: string;
  onJumpToReference?: (nodeId: number) => void;
}) {
  const { data, loading } = useAsync<NodeLocations | null>(
    () => viewApi.nodeLocations(nodeId),
    [nodeId],
  );

  if (loading) return <Typography.Text type="secondary">加载中…</Typography.Text>;
  if (!data) return <Empty description="未找到该节点" />;

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label="种类">
          <Tag color={nodeColor(data.kind)} style={{ color: '#fff' }}>
            {data.kind}
          </Tag>
        </Descriptions.Item>
        <Descriptions.Item label="名称">{data.name}</Descriptions.Item>
        <Descriptions.Item label="节点类型">
          {data.synthetic ? '合成节点（语义对象）' : '语法节点'}
        </Descriptions.Item>
        <Descriptions.Item label="位置">
          <LocationBadge count={data.locations.length} />
        </Descriptions.Item>
        <Descriptions.Item label="引用">{data.reference_count} 条入边</Descriptions.Item>
      </Descriptions>

      {data.synthetic ? (
        <Alert
          type="info"
          showIcon
          message="这是合成节点：它由多处共现汇聚而成"
          description="下面列出全部出处，请按需逐条验证；这里不会替你挑一个'看起来像'的位置。"
        />
      ) : null}

      <LocationList locations={data.locations} kind={data.kind} projectRoot={projectRoot} />

      {data.reference_count > 0 ? (
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          另有 {data.reference_count} 处引用指向它。
        </Typography.Text>
      ) : null}
    </Space>
  );
}

function EdgePanel({ edgeId, projectRoot }: { edgeId: number; projectRoot?: string }) {
  const { data, loading } = useAsync<EdgeEvidence | null>(
    () => viewApi.edgeEvidence(edgeId),
    [edgeId],
  );
  const [hops, setHops] = useState<SourceLocation[]>([]);

  useEffect(() => {
    setHops([]);
  }, [edgeId]);

  if (loading) return <Typography.Text type="secondary">加载中…</Typography.Text>;
  if (!data) return <Empty description="未找到该边" />;

  const unresolved = !data.edge.resolved;
  const shown = hops.length > 0 ? hops : data.locations;

  return (
    <Space direction="vertical" size={16} style={{ width: '100%' }}>
      <Descriptions column={1} size="small" bordered>
        <Descriptions.Item label="关系">{data.edge.kind}</Descriptions.Item>
        <Descriptions.Item label="状态">
          {unresolved ? <Tag color="orange">待验证假设（虚线）</Tag> : <Tag color="green">已解析（实线）</Tag>}
        </Descriptions.Item>
        <Descriptions.Item label="置信度">{data.edge.confidence.toFixed(2)}</Descriptions.Item>
        {data.edge.hops !== null ? (
          <Descriptions.Item label="跳数">via {data.edge.hops} hops</Descriptions.Item>
        ) : null}
      </Descriptions>

      {data.reason ? <Alert type="warning" showIcon message={data.reason} /> : null}

      {unresolved ? (
        <Typography.Text type="secondary" style={{ fontSize: 12 }}>
          虚线边是推断结果：下面每个位置都是可亲自验证的落点，核对后再采信。
        </Typography.Text>
      ) : null}

      {data.via.length > 0 ? (
        <Space direction="vertical" size={4}>
          {data.via.map((v, i) => (
            <Tag key={i}>{v}</Tag>
          ))}
        </Space>
      ) : null}

      <LocationList
        locations={shown}
        projectRoot={projectRoot}
        emptyHint="这条边没有可跳转的证据位置（可能来自权威源推断）"
      />
    </Space>
  );
}
