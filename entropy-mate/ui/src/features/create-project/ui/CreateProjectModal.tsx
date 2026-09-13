import { Form, Input, Modal, Switch, message } from 'antd';
import { useState } from 'react';
import { projectApi } from '@/entities/project';
import type { CreateProjectInput } from '@/entities/project';

/**
 * 创建工程。
 *
 * 创建成功后后端会**自动开始建图**（P0→P7），无需额外操作。
 */
export function CreateProjectModal({
  open,
  onClose,
  onCreated,
}: {
  open: boolean;
  onClose: () => void;
  onCreated?: (projectId: number) => void;
}) {
  const [form] = Form.useForm<CreateProjectInput & { full_pipeline: boolean }>();
  const [submitting, setSubmitting] = useState(false);

  const submit = async () => {
    const values = await form.validateFields();
    setSubmitting(true);
    try {
      const project = await projectApi.create({
        name: values.name,
        root_path: values.root_path,
        description: values.description,
        config: { full_pipeline: values.full_pipeline ?? true },
      });
      message.success(`已创建「${project.name}」，正在自动建图`);
      form.resetFields();
      onClose();
      onCreated?.(project.id);
    } catch (e) {
      message.error(e instanceof Error ? e.message : '创建失败');
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <Modal
      title="新建工程"
      open={open}
      onCancel={onClose}
      onOk={submit}
      okText="创建并开始建图"
      cancelText="取消"
      confirmLoading={submitting}
      destroyOnClose
    >
      <Form form={form} layout="vertical" initialValues={{ full_pipeline: true }} style={{ marginTop: 16 }}>
        <Form.Item name="name" label="工程名称" rules={[{ required: true, message: '请输入名称' }]}>
          <Input placeholder="例如：CRMEB" />
        </Form.Item>
        <Form.Item
          name="root_path"
          label="代码库根目录"
          rules={[{ required: true, message: '请输入绝对路径' }]}
          extra="将自动识别其中的子工程（composer.json / package.json / pom.xml 等）"
        >
          <Input placeholder="例如：/home/alvin/entropy-mate/samples/CRMEB-master" />
        </Form.Item>
        <Form.Item name="description" label="描述">
          <Input.TextArea rows={2} placeholder="可选" />
        </Form.Item>
        <Form.Item name="full_pipeline" label="执行全阶段流水线" valuePropName="checked">
          <Switch />
        </Form.Item>
      </Form>
    </Modal>
  );
}
