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
 * Location list.
 *
 * A synthesized node (`Table:user` and the like) **necessarily comes from multiple co-occurrences**: both the `crmeb.sql` CREATE TABLE statement
 * and the Model's `$table` definition. Always give a **multi-location list** here; never fabricate a single location.
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
  /** WSL distro name; when non-empty, jump / copy is handled as WSL. */
  wslDistro?: string;
  emptyHint?: string;
  /** Show a "top to bottom" arrow between adjacent locations, for ordered scenarios like an edge evidence chain. */
  ordered?: boolean;
  /** Whether to offer "copy all locations" at the bottom; in a per-hop chain each hop holds one location, so this button needn't repeat. */
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

/** Badge with the location count. */
export function LocationBadge({ count }: { count: number }) {
  const { t } = useLocale();
  if (count === 0) return <Tag>{t('No location')}</Tag>;
  if (count === 1) return <Tag color="blue">{1 + t(' locations')}</Tag>;
  return <Tag color="blue">{count + t(' source locations')}</Tag>;
}
