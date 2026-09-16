import { Button, Space, Table, Tag, Tooltip } from 'antd';
import { AreaChartOutlined, ApartmentOutlined } from '@ant-design/icons';
import { useNavigate } from 'react-router-dom';
import { STATUS_META, type Project } from '@/entities/project';
import { useLocale } from '@/shared/lib/i18n';
import { DeleteProjectButton } from '@/features/delete-project';
import { formatTime } from '@/shared/lib/format';

/** 工程列表。 */
export function ProjectTable({
  projects,
  loading,
  onDeleted,
  onSelect,
}: {
  projects: Project[];
  loading: boolean;
  onDeleted?: () => void;
  onSelect?: (project: Project) => void;
}) {
  const navigate = useNavigate();
  const { t } = useLocale();

  return (
    <Table<Project>
      rowKey="id"
      loading={loading}
      dataSource={projects}
      pagination={false}
      locale={{ emptyText: t('还没有工程，点击右上角「新建工程」开始') }}
      columns={[
        {
          title: t('名称'),
          dataIndex: 'name',
          render: (_, p) => (
            <a
              onClick={() => {
                onSelect?.(p);
                navigate(`/projects/${p.id}/graph`);
              }}
              style={{ fontWeight: 600 }}
            >
              {p.name}
            </a>
          ),
        },
        { title: t('根目录'), dataIndex: 'root_path', ellipsis: true, width: 340 },
        {
          title: t('状态'),
          dataIndex: 'status',
          width: 100,
          render: (s: Project['status']) => (
            <Tag color={STATUS_META[s]?.color ?? 'default'}>{t(STATUS_META[s]?.label ?? s)}</Tag>
          ),
        },
        {
          title: t('完整流程'),
          dataIndex: ['config', 'full_pipeline'],
          width: 90,
          render: (v: boolean) => (v ? t('是') : t('仅基础阶段')),
        },
        {
          title: t('创建时间'),
          dataIndex: 'created_at',
          width: 180,
          render: (v: number) => formatTime(v),
        },
        {
          title: t('操作'),
          width: 140,
          render: (_, p) => (
            <Space size={4}>
              <Tooltip title={t('图视图')}>
                <Button
                  type="text"
                  icon={<ApartmentOutlined />}
                  onClick={() => navigate(`/projects/${p.id}/graph`)}
                />
              </Tooltip>
              <Tooltip title={t('节点浏览')}>
                <Button
                  type="text"
                  icon={<AreaChartOutlined />}
                  onClick={() => navigate(`/projects/${p.id}/explorer`)}
                />
              </Tooltip>
              <DeleteProjectButton projectId={p.id} name={p.name} onDeleted={onDeleted} />
            </Space>
          ),
        },
      ]}
    />
  );
}
