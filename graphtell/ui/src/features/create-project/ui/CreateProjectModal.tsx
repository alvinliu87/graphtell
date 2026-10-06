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
 * Codebase root field: a read-only input plus a “Select directory” button.
 *
 * It is a direct child of `Form.Item` so antd injects `value` / `onChange`, which avoids treating
 * `Space.Compact` as the form control and breaking the back-fill.
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
        placeholder={t('Click the button on the right to select a directory')}
        onChange={(e) => onChange?.(e.target.value)}
      />
      <Button icon={<FolderOpenOutlined />} onClick={onPick} disabled={disabled}>
        {t('Select directory')}
      </Button>
    </Space.Compact>
  );
}

/**
 * Create a project.
 *
 * After successful creation the backend **automatically starts building** (P0→P7). This modal doesn't close immediately; it enters the "building"
 * state: the form is greyed out and uneditable, and the submit button shows "building…"; it polls the build status, and once done turns the submit
 * button into "close" and the form content into "click to view the graph".
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
          message.error(`Graphing failed for project “${p.name}” — please check the codebase root.`);
          return;
        }
      } catch {
        /* Transient issues like network jitter: keep polling */
      }
      pollRef.current = window.setTimeout(tick, 2000);
    };
    pollRef.current = window.setTimeout(tick, 1500);
  };

  useEffect(() => () => stopPolling(), []);

  // Reset to the initial state every time it opens
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
      message.error(e instanceof Error ? e.message : 'Creation failed');
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
      ? t('Close')
      : phase === 'building'
        ? t('Graphing…')
        : t('Create and start graphing');

  return (
    <>
      <Modal
        title={t('New project')}
        open={open}
        onCancel={handleClose}
        onOk={handleOk}
        okText={okText}
        cancelText={t('Cancel')}
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
              title={t('Graphing complete')}
              subTitle={t('Project is ready; you can view the code structure graph')}
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
                {t('Click to view graph')}
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
              label={t('Project name')}
              rules={[{ required: true, message: t('Please enter a name') }]}
            >
              <Input placeholder={t('e.g. my-project')} />
            </Form.Item>
            <Form.Item
              name="root_path"
              label={t('Codebase root')}
              rules={[{ required: true, message: t('Please select a directory') }]}
              extra={t('Sub-projects are auto-detected (composer.json / package.json / pom.xml, etc.)')}
            >
              <RootPathField onPick={() => setPickerOpen(true)} disabled={locked} />
            </Form.Item>
            <Form.Item name="description" label={t('Description')}>
              <Input.TextArea rows={2} placeholder={t('Optional')} />
            </Form.Item>
            <Form.Item name="full_pipeline" label={t('Run full pipeline')} valuePropName="checked">
              <Switch />
            </Form.Item>
            {phase === 'failed' && (
              <Typography.Text type="danger">
                {t('Graphing failed; close and retry, or check the codebase root.')}
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
