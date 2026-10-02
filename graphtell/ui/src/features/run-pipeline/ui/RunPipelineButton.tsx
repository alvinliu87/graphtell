import { Button, message } from 'antd';
import { useLocale } from '@/shared/lib/i18n';
import { ThunderboltOutlined } from '@ant-design/icons';
import { pipelineApi } from '@/entities/pipeline';

/** Trigger a graph build (runs on a background thread; the frontend polls progress). */
export function RunPipelineButton({
  projectId,
  onStarted,
}: {
  projectId: number;
  onStarted?: () => void;
}) {
  const { t } = useLocale();
  const run = async () => {
    try {
      await pipelineApi.run(projectId);
      message.success(t('Graphing started'));
      onStarted?.();
    } catch (e) {
      message.error(e instanceof Error ? e.message : t('Failed to start'));
    }
  };

  return (
    <Button type="primary" icon={<ThunderboltOutlined />} onClick={run}>
      {t('Rebuild graph')}
    </Button>
  );
}
