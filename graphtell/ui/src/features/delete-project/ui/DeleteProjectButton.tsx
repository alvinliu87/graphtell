import { Button, Popconfirm, message } from 'antd';
import { useLocale } from '@/shared/lib/i18n';
import { DeleteOutlined } from '@ant-design/icons';
import { projectApi } from '@/entities/project';

/** Delete a project (along with all of its graph data). */
export function DeleteProjectButton({
  projectId,
  name,
  onDeleted,
}: {
  projectId: number;
  name: string;
  onDeleted?: () => void;
}) {
  const { t } = useLocale();
  const remove = async () => {
    try {
      await projectApi.remove(projectId);
      message.success(t('Deleted "') + name + t('」'));
      onDeleted?.();
    } catch (e) {
      message.error(e instanceof Error ? e.message : t('Delete failed'));
    }
  };

  return (
    <Popconfirm
      title={t('Delete project "') + name + t('」？')}
      description={t('All nodes, edges, annotations and symbol tables of this project will be cleared, unrecoverably.')}
      okText={t('Delete')}
      okButtonProps={{ danger: true }}
      cancelText={t('Cancel')}
      onConfirm={remove}
    >
      <Button type="text" danger icon={<DeleteOutlined />} />
    </Popconfirm>
  );
}
