import { Button, Popconfirm, message } from 'antd';
import { DeleteOutlined } from '@ant-design/icons';
import { projectApi } from '@/entities/project';

/** 删除工程（连带删除其全部图数据）。 */
export function DeleteProjectButton({
  projectId,
  name,
  onDeleted,
}: {
  projectId: number;
  name: string;
  onDeleted?: () => void;
}) {
  const remove = async () => {
    try {
      await projectApi.remove(projectId);
      message.success(`已删除「${name}」`);
      onDeleted?.();
    } catch (e) {
      message.error(e instanceof Error ? e.message : '删除失败');
    }
  };

  return (
    <Popconfirm
      title={`删除工程「${name}」？`}
      description="该工程的全部节点、边、标注与符号表都会被清除，且不可恢复。"
      okText="删除"
      okButtonProps={{ danger: true }}
      cancelText="取消"
      onConfirm={remove}
    >
      <Button type="text" danger icon={<DeleteOutlined />} />
    </Popconfirm>
  );
}
