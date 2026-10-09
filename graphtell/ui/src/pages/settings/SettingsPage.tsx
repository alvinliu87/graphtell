import { useCallback, useEffect, useRef, useState } from 'react';
import {
  Alert,
  Button,
  Card,
  Input,
  Progress,
  Radio,
  Space,
  Spin,
  Tag,
  Typography,
} from 'antd';
import { DownloadOutlined, ReloadOutlined } from '@ant-design/icons';
import { useAsync } from '@/shared/lib/useAsync';
import { useLocale } from '@/shared/lib/i18n';
import { modelApi, type ModelStatus, type SetBackendRequest } from '@/entities/model/api';

const { Text, Paragraph, Title } = Typography;

type Mode = SetBackendRequest['mode'];

const MODE_OPTIONS: { value: Mode; label: string; hint: string }[] = [
  { value: 'auto', label: 'Auto', hint: 'Use local bge-m3 if weights are present, otherwise fall back to offline hashing.' },
  { value: 'local', label: 'Local bge-m3', hint: 'Load bge-m3 from models/bge-m3-safetensors (requires the one-click download or manual setup).' },
  { value: 'url', label: 'Remote service', hint: 'Bring your own embedding service (OpenAI / TEI compatible). No local weights needed.' },
  { value: 'hash', label: 'Offline only', hint: 'Lexical / fast-vector matching only, no neural semantic vectors.' },
];

/** Settings page: embedding backend selection + one-click weight download. */
export function SettingsPage() {
  const { t } = useLocale();
  const statusRes = useAsync<ModelStatus>(() => modelApi.status(), []);
  const status = statusRes.data;

  const [mode, setMode] = useState<Mode>('auto');
  const [url, setUrl] = useState('');
  const [saving, setSaving] = useState(false);
  const [downloading, setDownloading] = useState(false);
  const [saveMsg, setSaveMsg] = useState<{ ok: boolean; text: string } | null>(null);

  // Seed the form from the loaded status.
  useEffect(() => {
    if (status) {
      setMode((status.backend_mode as Mode) || 'auto');
      setUrl(status.backend_url ?? '');
    }
  }, [status]);

  // Poll while a download is active.
  const active = status?.download.active ?? false;
  const pollRef = useRef(statusRes.silentReload);
  pollRef.current = statusRes.silentReload;
  useEffect(() => {
    if (!active) return;
    setDownloading(true);
    const id = window.setInterval(() => pollRef.current(), 1000);
    return () => {
      window.clearInterval(id);
      setDownloading(false);
    };
  }, [active]);

  const onSaveBackend = useCallback(async () => {
    setSaving(true);
    setSaveMsg(null);
    try {
      const res = await modelApi.setBackend({ mode, url: mode === 'url' ? url : undefined });
      setSaveMsg({ ok: true, text: `Backend set to "${res.backend_mode}". ${res.embedding_backend}` });
      // The embedding backend may have changed; refetch status.
      await statusRes.reload();
    } catch (e) {
      setSaveMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
    } finally {
      setSaving(false);
    }
  }, [mode, url, statusRes]);

  const onDownload = useCallback(async () => {
    setDownloading(true);
    setSaveMsg(null);
    try {
      await modelApi.download();
      // Kick off polling via the effect (active becomes true on next status fetch).
      await statusRes.reload();
    } catch (e) {
      setSaveMsg({ ok: false, text: e instanceof Error ? e.message : String(e) });
      setDownloading(false);
    }
  }, [statusRes]);

  const selectedHint = MODE_OPTIONS.find((m) => m.value === mode)?.hint ?? '';
  const pct = status?.download.progress && status.download.progress >= 0
    ? Math.round(status.download.progress * 100)
    : undefined;

  return (
    <div style={{ maxWidth: 760, margin: '0 auto' }}>
      <Title level={3} style={{ marginBottom: 4 }}>{t('Model & Embedding')}</Title>
      <Text type="secondary">{t('Choose how GraphTell turns code into vectors for prompt augmentation.')}</Text>

      <Card style={{ marginTop: 16 }} title={t('Embedding backend')}>
        {statusRes.loading && !status ? (
          <Spin />
        ) : (
          <Space direction="vertical" size={14} style={{ width: '100%' }}>
            <Radio.Group
              value={mode}
              onChange={(e) => setMode(e.target.value as Mode)}
              optionType="button"
              buttonStyle="solid"
            >
              {MODE_OPTIONS.map((m) => (
                <Radio.Button key={m.value} value={m.value}>{t(m.label)}</Radio.Button>
              ))}
            </Radio.Group>
            <Text type="secondary">{t(selectedHint)}</Text>

            {mode === 'url' && (
              <Input
                placeholder="http://localhost:8080  (OpenAI /embeddings or TEI /embed)"
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                addonBefore={t('URL')}
              />
            )}

            <Space>
              <Button type="primary" loading={saving} onClick={onSaveBackend}>
                {t('Apply (no restart needed)')}
              </Button>
              <Button icon={<ReloadOutlined />} onClick={() => statusRes.reload()}>
                {t('Refresh')}
              </Button>
            </Space>

            <Space size={8} wrap>
              <Text type="secondary">{t('Active backend:')}</Text>
              <Tag color={status?.local_weights_present ? 'green' : 'default'}>
                {status?.embedding_backend ?? '—'}
              </Tag>
              <Text type="secondary">{t('dim')} {status?.embedding_dim ?? '—'}</Text>
              <Text type="secondary">|</Text>
              <Text type="secondary">{t('local weights:')}</Text>
              <Tag color={status?.local_weights_present ? 'green' : 'red'}>
                {status?.local_weights_present ? t('present') : t('missing')}
              </Tag>
            </Space>
          </Space>
        )}
      </Card>

      <Card style={{ marginTop: 16 }} title={t('Local semantic model (bge-m3)')}>
        <Space direction="vertical" size={12} style={{ width: '100%' }}>
          <Paragraph type="secondary" style={{ marginBottom: 0 }}>
            {t('Download a pre-built bge-m3 bundle directly from GitHub Releases and unzip it on this host. No Python or pip install needed — just network access. Override the source with GT_BGE_DOWNLOAD_URL.')}
          </Paragraph>

          <Button
            type="primary"
            icon={<DownloadOutlined />}
            loading={downloading}
            disabled={status?.download.active}
            onClick={onDownload}
          >
            {t('Download & enable bge-m3')}
          </Button>

          {status?.download && (status.download.active || status.download.log || status.download.error) && (
            <div>
              {status.download.phase && (
                <Space size={8} style={{ marginBottom: 8 }}>
                  <Text strong>{t('Stage:')}</Text>
                  <Tag>{status.download.phase}</Tag>
                  {pct !== undefined && <Progress percent={pct} style={{ width: 200 }} />}
                </Space>
              )}
              {status.download.log && (
                <Paragraph>
                  <pre
                    style={{
                      maxHeight: 280,
                      overflow: 'auto',
                      background: '#0b0f17',
                      color: '#d6e1ff',
                      padding: 12,
                      borderRadius: 8,
                      fontSize: 12,
                      whiteSpace: 'pre-wrap',
                      wordBreak: 'break-word',
                      margin: 0,
                    }}
                  >
                    {status.download.log}
                  </pre>
                </Paragraph>
              )}
            </div>
          )}
        </Space>
      </Card>

      {saveMsg && (
        <Alert
          style={{ marginTop: 16 }}
          type={saveMsg.ok ? 'success' : 'error'}
          showIcon
          message={saveMsg.text}
        />
      )}
      {status?.download.error && (
        <Alert
          style={{ marginTop: 16 }}
          type="error"
          showIcon
          message={t('Download failed')}
          description={status.download.error}
        />
      )}
    </div>
  );
}
