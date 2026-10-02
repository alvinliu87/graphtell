import { Fragment } from 'react';
import { Button, Dropdown, Space, Tag, Tooltip, Typography } from 'antd';
import { CopyOutlined, DownOutlined, ExportOutlined } from '@ant-design/icons';
import type { SourceLocation } from '@/entities/view';
import {
  copyAllLocations,
  copyPath,
  IDE_LABEL,
  IdeTarget,
  isSensitive,
  openInIde,
  preferredIde,
  setPreferredIde,
} from '@/shared/lib/ide';
import { useLocale } from '@/shared/lib/i18n';

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
  wslDistro,
  emptyHint = 'This node has no usable source location (possibly a pure semantic synthetic object)',
  ordered = false,
  showCopyAll = true,
}: {
  locations: SourceLocation[];
  kind?: string;
  projectRoot?: string;
  /** WSL 发行版名；非空时跳转 / 复制按 WSL 处理。 */
  wslDistro?: string;
  emptyHint?: string;
  /** 相邻位置之间显示"从上往下"箭头，用于边证据链等有序场景。 */
  ordered?: boolean;
  /** 是否在底部提供"复制全部位置"；逐跳链路里每跳只放一个位置，不必重复这个按钮。 */
  showCopyAll?: boolean;
}) {
  const { t } = useLocale();

  if (locations.length === 0) {
    return <Typography.Text type="secondary">{t(emptyHint)}</Typography.Text>;
  }

  const sensitive = kind ? isSensitive(kind) : false;

  return (
    <Space direction="vertical" size={8} style={{ width: '100%' }}>
      {sensitive ? (
        <Typography.Text type="warning" style={{ fontSize: 12 }}>
          {t('Sensitive location: jump only to the key-name line, no values shown')}
        </Typography.Text>
      ) : null}
      {locations.map((loc, i) => {
        const items = (Object.keys(IDE_LABEL) as IdeTarget[]).map((it) => ({
          key: it,
          label: IDE_LABEL[it],
          onClick: () => {
            setPreferredIde(it);
            void openInIde(it, loc, projectRoot, wslDistro);
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
                <Typography.Text
                  style={{ fontSize: 12, fontWeight: 600, wordBreak: 'break-all', display: 'inline-block' }}
                >
                  {loc.file}:{loc.line}
                </Typography.Text>
                <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.45)' }}>
                  {loc.note ?? (loc.symbol ? t('symbol ') + loc.symbol : '')}
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
                {/*
                <Dropdown menu={{ items }} trigger={['click']}>
                  <Tooltip title={t('Open in IDE (falls back to copying path on failure)')}>
                    <Button size="small" type="text" icon={<ExportOutlined />} />
                  </Tooltip>
                </Dropdown>
                */}
                <Tooltip title={t('Copy absolute path:line')}>
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
            void copyAllLocations(locations, projectRoot, `${locations.length}${t(' source locations')}`)
          }
        >
          {t('Copy all locations (absolute paths)')}
        </Button>
      ) : null}
    </Space>
  );
}

/** 位置数量的角标。 */
export function LocationBadge({ count }: { count: number }) {
  const { t } = useLocale();
  if (count === 0) return <Tag>{t('No location')}</Tag>;
  if (count === 1) return <Tag color="blue">{1 + t(' locations')}</Tag>;
  return <Tag color="blue">{count + t(' source locations')}</Tag>;
}
