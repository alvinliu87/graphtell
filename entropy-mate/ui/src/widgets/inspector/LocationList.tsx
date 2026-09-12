import { Button, Dropdown, Space, Tag, Tooltip, Typography } from 'antd';
import { CopyOutlined, ExportOutlined } from '@ant-design/icons';
import type { SourceLocation } from '@/entities/view';
import {
  copyPath,
  copyReference,
  driftHint,
  IDE_LABEL,
  IdeTarget,
  isSensitive,
  openInIde,
} from '@/shared/lib/ide';

/**
 * 位置列表。
 *
 * 合成节点（`Table:user` 之类）**必然来自多处共现**：既有 `crmeb.sql` 的建表语句，
 * 也有 Model 的 `$table` 定义。这里一律给出**多位置列表**，绝不编造单一位置。
 */
export function LocationList({
  locations,
  kind,
  projectRoot,
  emptyHint = '该节点没有可用的源码位置（可能是纯语义合成对象）',
}: {
  locations: SourceLocation[];
  kind?: string;
  projectRoot?: string;
  emptyHint?: string;
}) {
  if (locations.length === 0) {
    return <Typography.Text type="secondary">{emptyHint}</Typography.Text>;
  }

  const sensitive = kind ? isSensitive(kind) : false;

  return (
    <Space direction="vertical" size={8} style={{ width: '100%' }}>
      {sensitive ? (
        <Typography.Text type="warning" style={{ fontSize: 12 }}>
          敏感位置：只跳到键名所在行，不展示任何值
        </Typography.Text>
      ) : null}
      {locations.map((loc, i) => {
        const items = (Object.keys(IDE_LABEL) as IdeTarget[]).map((t) => ({
          key: t,
          label: IDE_LABEL[t],
          onClick: () => void openInIde(t, loc, projectRoot),
        }));
        return (
          <div
            key={`${loc.file}:${loc.line}:${i}`}
            style={{
              display: 'flex',
              alignItems: 'flex-start',
              justifyContent: 'space-between',
              gap: 10,
              padding: '8px 10px',
              borderRadius: 8,
              background: '#f8fafc',
            }}
          >
            <div style={{ minWidth: 0 }}>
              <div style={{ fontSize: 12, fontWeight: 600, wordBreak: 'break-all' }}>
                {loc.file}:{loc.line}
              </div>
              <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.45)' }}>
                {loc.note ?? (loc.symbol ? `符号 ${loc.symbol}` : '')}
                {loc.symbol ? ` · ${driftHint(loc)}` : ''}
              </div>
            </div>
            <Space size={4}>
              <Dropdown menu={{ items }} trigger={['click']}>
                <Tooltip title="在 IDE 中打开（失败会自动复制路径）">
                  <Button size="small" type="text" icon={<ExportOutlined />} />
                </Tooltip>
              </Dropdown>
              <Tooltip title="复制 path:line（无 IDE 场景的兜底）">
                <Button
                  size="small"
                  type="text"
                  icon={<CopyOutlined />}
                  onClick={() => void copyPath(loc, projectRoot)}
                />
              </Tooltip>
            </Space>
          </div>
        );
      })}
      <Button
        size="small"
        block
        onClick={() =>
          void copyReference(
            locations[0],
            `${locations.length} 处共现位置`,
          )
        }
      >
        复制全部位置（报告内嵌 path:line）
      </Button>
    </Space>
  );
}

/** 位置数量的角标。 */
export function LocationBadge({ count }: { count: number }) {
  if (count === 0) return <Tag>无位置</Tag>;
  if (count === 1) return <Tag color="blue">1 处位置</Tag>;
  return <Tag color="blue">{count} 处共现位置</Tag>;
}
