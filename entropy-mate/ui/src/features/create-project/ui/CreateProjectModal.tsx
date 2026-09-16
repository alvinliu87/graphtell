import {
  Button,
  Card,
  Form,
  Input,
  Modal,
  Result,
  Space,
  Switch,
  Typography,
  message,
} from 'antd';
import { EyeOutlined, FolderOpenOutlined } from '@ant-design/icons';
import { useEffect, useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { projectApi } from '@/entities/project';
import type { CreateProjectInput } from '@/entities/project';
import { DirectoryPickerModal } from './DirectoryPickerModal';
import { useLocale } from '@/shared/lib/i18n';

type Phase = 'editing' | 'submitting' | 'building' | 'ready' | 'failed';

/**
 * 代码库根目录字段：只读输入框 +「选择目录」按钮。
 *
 * 作为 `Form.Item` 的直接子节点，由 antd 注入 `value` / `onChange`，
 * 避免把 `Space.Compact` 当成表单控件导致回填失效。
 */
function RootPathField({
  value,
  onChange,
  onPick,
  disabled,
}: {
  value?: string;
  onChange?: (v: string) => void;
  onPick: () => void;
  disabled?: boolean;
}) {
  const { t } = useLocale();
  return (
    <Space.Compact style={{ width: '100%' }}>
      <Input
        readOnly
        value={value}
        placeholder={t('点击右侧按钮选择目录')}
        onChange={(e) => onChange?.(e.target.value)}
      />
      <Button icon={<FolderOpenOutlined />} onClick={onPick} disabled={disabled}>
        {t('选择目录')}
      </Button>
    </Space.Compact>
  );
}

/**
 * 创建工程。
 *
 * 创建成功后后端会**自动开始建图**（P0→P7）。本弹窗不会立即关闭，而是进入「建图中」
 * 状态：表单置灰不可编辑、提交按钮显示「建图中…」；轮询建图状态，建好后将提交按钮
 * 变为「关闭」，表单内容变为「点击查看图」。
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
  const { t } = useLocale();
  const navigate = useNavigate();
  const [form] = Form.useForm<CreateProjectInput & { full_pipeline: boolean }>();
  const [phase, setPhase] = useState<Phase>('editing');
  const [createdId, setCreatedId] = useState<number | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const pollRef = useRef<number | null>(null);

  const locked = phase === 'building' || phase === 'submitting';

  const stopPolling = () => {
    if (pollRef.current !== null) {
      clearTimeout(pollRef.current);
      pollRef.current = null;
    }
  };

  const startPolling = (id: number) => {
    stopPolling();
    const tick = async () => {
      try {
        const p = await projectApi.get(id);
        if (p.status === 'ready') {
          setPhase('ready');
          stopPolling();
          onCreated?.(id);
          return;
        }
        if (p.status === 'failed') {
          setPhase('failed');
          stopPolling();
          message.error(`工程「${p.name}」建图失败，请检查代码库根目录`);
          return;
        }
      } catch {
        /* 网络抖动等：继续轮询 */
      }
      pollRef.current = window.setTimeout(tick, 2000);
    };
    pollRef.current = window.setTimeout(tick, 1500);
  };

  useEffect(() => () => stopPolling(), []);

  // 每次打开重置为初始态
  useEffect(() => {
    if (open) {
      stopPolling();
      setPhase('editing');
      setCreatedId(null);
      form.resetFields();
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const handleClose = () => {
    stopPolling();
    setPhase('editing');
    setCreatedId(null);
    form.resetFields();
    onClose();
  };

  const submit = async () => {
    const values = await form.validateFields();
    setPhase('submitting');
    try {
      const project = await projectApi.create({
        name: values.name,
        root_path: values.root_path,
        description: values.description,
        config: { full_pipeline: values.full_pipeline ?? true },
      });
      setCreatedId(project.id);
      setPhase('building');
      onCreated?.(project.id);
      startPolling(project.id);
    } catch (e) {
      message.error(e instanceof Error ? e.message : '创建失败');
      setPhase('editing');
    }
  };

  const handleOk = () => {
    if (phase === 'editing' || phase === 'submitting') {
      void submit();
    } else {
      handleClose();
    }
  };

  const okText =
    phase === 'ready' || phase === 'failed'
      ? '关闭'
      : phase === 'building'
        ? '建图中…'
        : '创建并开始建图';

  return (
    <>
      <Modal
        title={t('新建工程')}
        open={open}
        onCancel={handleClose}
        onOk={handleOk}
        okText={okText}
        cancelText={t('取消')}
        okButtonProps={{ disabled: phase === 'building', loading: phase === 'submitting' }}
        cancelButtonProps={{ disabled: locked }}
        maskClosable={!locked}
        closable={!locked}
        destroyOnClose
      >
        {phase === 'ready' ? (
          <Space direction="vertical" style={{ width: '100%' }} size={16}>
            <Result
              status="success"
              title={t('建图完成')}
              subTitle={t('工程已就绪，可查看代码结构图')}
            />
            <Card
              hoverable
              onClick={() => {
                if (createdId !== null) navigate(`/projects/${createdId}/graph`);
                handleClose();
              }}
              style={{
                textAlign: 'center',
                borderRadius: 12,
                borderColor: '#3d7eff',
                cursor: 'pointer',
              }}
            >
              <EyeOutlined style={{ fontSize: 24, color: '#3d7eff' }} />
              <div style={{ marginTop: 8, fontSize: 15, fontWeight: 600, color: '#3d7eff' }}>
                {t('点击查看图')}
              </div>
            </Card>
          </Space>
        ) : (
          <Form
            form={form}
            layout="vertical"
            initialValues={{ full_pipeline: true }}
            style={{ marginTop: 16 }}
            disabled={locked}
          >
            <Form.Item
              name="name"
              label={t('工程名称')}
              rules={[{ required: true, message: t('请输入名称') }]}
            >
              <Input placeholder={t('例如：CRMEB')} />
            </Form.Item>
            <Form.Item
              name="root_path"
              label={t('代码库根目录')}
              rules={[{ required: true, message: t('请选择目录') }]}
              extra={t('将自动识别其中的子工程（composer.json / package.json / pom.xml 等）')}
            >
              <RootPathField onPick={() => setPickerOpen(true)} disabled={locked} />
            </Form.Item>
            <Form.Item name="description" label={t('描述')}>
              <Input.TextArea rows={2} placeholder={t('可选')} />
            </Form.Item>
            <Form.Item name="full_pipeline" label={t('执行完整建图流程')} valuePropName="checked">
              <Switch />
            </Form.Item>
            {phase === 'failed' && (
              <Typography.Text type="danger">
                {t('建图失败，可关闭后重试或检查代码库根目录。')}
              </Typography.Text>
            )}
          </Form>
        )}
      </Modal>

      <DirectoryPickerModal
        open={pickerOpen}
        onClose={() => setPickerOpen(false)}
        onSelect={(p) => {
          form.setFieldsValue({ root_path: p });
          setPickerOpen(false);
        }}
      />
    </>
  );
}
