import { Fragment } from 'react';
import { Button, Dropdown, Space, Tag, Tooltip, Typography } from 'antd';
import { CopyOutlined, DownOutlined, ExportOutlined } from '@ant-design/icons';
import type { SourceLocation } from '@/entities/view';
import {
  copyAllLocations,
  copyPath,
  driftHint,
  IDE_LABEL,
  IdeTarget,
  isSensitive,
  openInIde,
  preferredIde,
  setPreferredIde,
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
  ordered = false,
  showCopyAll = true,
}: {
  locations: SourceLocation[];
  kind?: string;
  projectRoot?: string;
  emptyHint?: string;
  /** 相邻位置之间显示"从上往下"箭头，用于边证据链等有序场景。 */
  ordered?: boolean;
  /** 是否在底部提供"复制全部位置"；逐跳链路里每跳只放一个位置，不必重复这个按钮。 */
  showCopyAll?: boolean;
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
          onClick: () => {
            setPreferredIde(t);
            void openInIde(t, loc, projectRoot);
          },
        }));
        return (
          <Fragment key={`${loc.file}:${loc.line}:${i}`}>
            <div
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
                <Typography.Link
                  onClick={() => void openInIde(preferredIde(), loc, projectRoot)}
                  title={`在 IDE 中打开（${IDE_LABEL[preferredIde()]}；右上角图标可换 IDE）`}
                  style={{ fontSize: 12, fontWeight: 600, wordBreak: 'break-all', display: 'inline-block' }}
                >
                  {loc.file}:{loc.line}
                </Typography.Link>
                <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.45)' }}>
                  {loc.note ?? (loc.symbol ? `符号 ${loc.symbol}` : '')}
                  {loc.symbol ? ` · ${driftHint(loc)}` : ''}
                </div>
                {loc.snippet ? (
                  <pre
                    style={{
                      margin: '6px 0 0',
                      padding: '6px 8px',
                      fontSize: 11,
                      fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
                      background: '#f1f5f9',
                      borderRadius: 6,
                      color: '#334155',
                      whiteSpace: 'pre-wrap',
                      wordBreak: 'break-all',
                    }}
                  >
                    {loc.snippet}
                  </pre>
                ) : null}
              </div>
              <Space size={4}>
                <Dropdown menu={{ items }} trigger={['click']}>
                  <Tooltip title="在 IDE 中打开（失败会自动复制路径）">
                    <Button size="small" type="text" icon={<ExportOutlined />} />
                  </Tooltip>
                </Dropdown>
                <Tooltip title="复制绝对 path:line（无 IDE 场景的兜底）">
                  <Button
                    size="small"
                    type="text"
                    icon={<CopyOutlined />}
                    onClick={() => void copyPath(loc, projectRoot)}
                  />
                </Tooltip>
              </Space>
            </div>
            {ordered && i < locations.length - 1 ? (
              <div style={{ display: 'flex', justifyContent: 'center', color: '#94a3b8', padding: '2px 0' }}>
                <DownOutlined />
              </div>
            ) : null}
          </Fragment>
        );
      })}
      {showCopyAll ? (
        <Button
          size="small"
          block
          onClick={() =>
            void copyAllLocations(locations, projectRoot, `${locations.length} 处共现位置`)
          }
        >
          复制全部位置（绝对路径）
        </Button>
      ) : null}
    </Space>
  );
}

/** 位置数量的角标。 */
export function LocationBadge({ count }: { count: number }) {
  if (count === 0) return <Tag>无位置</Tag>;
  if (count === 1) return <Tag color="blue">1 处位置</Tag>;
  return <Tag color="blue">{count} 处共现位置</Tag>;
}
