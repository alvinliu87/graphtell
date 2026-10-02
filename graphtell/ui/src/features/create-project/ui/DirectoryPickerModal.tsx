import { Alert, Button, Modal, Space, Spin, Typography } from 'antd';
import { useLocale } from '@/shared/lib/i18n';
import {
  ArrowLeftOutlined,
  FolderOpenOutlined,
  HomeOutlined,
} from '@ant-design/icons';
import { useEffect, useState } from 'react';
import { fsApi, type DirEntry } from '@/entities/fs';

/**
 * Directory picker: browse the host filesystem through the backend `/api/fs/browse`.
 *
 * The backend process runs on the host system (including WSL), so it can access mount paths like `/mnt/c`,
 * naturally supporting Windows disks under WSL.
 */
export function DirectoryPickerModal({
  open,
  onClose,
  onSelect,
}: {
  open: boolean;
  onClose: () => void;
  onSelect: (path: string) => void;
}) {
  const [path, setPath] = useState('/home');
  const [entries, setEntries] = useState<DirEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const { t } = useLocale();

  useEffect(() => {
    if (!open) return;
    let alive = true;
    setLoading(true);
    setError(null);
    fsApi
      .browse(path)
      .then((list) => {
        if (alive) setEntries(list);
      })
      .catch((e) => {
        if (alive) setError(e instanceof Error ? e.message : 'Read failed');
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [open, path]);

  const goUp = () => {
    const idx = path.lastIndexOf('/');
    setPath(idx <= 0 ? '/' : path.slice(0, idx));
  };

  return (
    <Modal
      title={t('Select codebase root')}
      open={open}
      onCancel={onClose}
      onOk={() => onSelect(path)}
      okText={t('Select this directory')}
      cancelText={t('Cancel')}
      width={560}
      destroyOnClose
    >
      <Space style={{ marginBottom: 12 }} wrap>
        <Button size="small" icon={<ArrowLeftOutlined />} onClick={goUp}>
          {t('Up')}
        </Button>
        <Button size="small" icon={<HomeOutlined />} onClick={() => setPath('/home')}>
          /home
        </Button>
        <Button size="small" onClick={() => setPath('/')}>
          /
        </Button>
        <Button size="small" onClick={() => setPath('/mnt')}>
          /mnt（{t('WSL disk')}）
        </Button>
      </Space>

      <Typography.Text
        type="secondary"
        style={{ display: 'block', marginBottom: 8, wordBreak: 'break-all' }}
      >
        {t('Current directory: ')}{path}
      </Typography.Text>

      {error && (
        <Alert type="error" showIcon message={error} style={{ marginBottom: 8 }} />
      )}

      <Spin spinning={loading}>
        <div
          style={{
            maxHeight: 320,
            overflow: 'auto',
            border: '1px solid #f0f0f0',
            borderRadius: 8,
          }}
        >
          {entries.map((e) => (
            <div
              key={e.path}
              onClick={() => setPath(e.path)}
              style={{
                padding: '8px 12px',
                cursor: 'pointer',
                borderBottom: '1px solid #f5f5f5',
              }}
              onMouseEnter={(ev) => (ev.currentTarget.style.background = '#f5f8ff')}
              onMouseLeave={(ev) => (ev.currentTarget.style.background = 'transparent')}
            >
              <Space>
                <FolderOpenOutlined />
                <span>{e.name}</span>
              </Space>
            </div>
          ))}
          {!loading && !error && entries.length === 0 && (
            <div style={{ padding: 16, color: '#999' }}>{t('No sub-directories in this directory')}</div>
          )}
        </div>
      </Spin>
    </Modal>
  );
}
