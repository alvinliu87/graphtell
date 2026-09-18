import { Button, message } from 'antd';
import { useLocale } from '@/shared/lib/i18n';
import { ThunderboltOutlined } from '@ant-design/icons';
import { pipelineApi } from '@/entities/pipeline';

/** 触发建图（后台线程执行，前端轮询进度）。 */
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
      message.success(t('已开始建图'));
      onStarted?.();
    } catch (e) {
      message.error(e instanceof Error ? e.message : t('启动失败'));
    }
  };

  return (
    <Button type="primary" icon={<ThunderboltOutlined />} onClick={run}>
      {t('重新建图')}
    </Button>
  );
}
