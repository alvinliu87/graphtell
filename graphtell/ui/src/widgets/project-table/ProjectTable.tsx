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
      locale={{ emptyText: t('No projects yet; click "New project" at top-right to start') }}
      columns={[
        {
          title: t('Name'),
          dataIndex: 'name',
          width: 200,
          ellipsis: true,
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
        // 根目录是**不定长**内容，让它吃掉剩余宽度（不设 width）；若把不设宽度的位置留给
        // 「名称」，固定表格布局下它会独占所有剩余空间，把路径挤成一小截 —— 正是「名字很宽、
        // 路径被截断」的成因。名称反而是短枚举，固定宽度 + ellipsis 更稳。
        { title: t('Root'), dataIndex: 'root_path', ellipsis: true },
        {
          title: t('Status'),
          dataIndex: 'status',
          width: 100,
          render: (s: Project['status']) => (
            <Tag color={STATUS_META[s]?.color ?? 'default'}>{t(STATUS_META[s]?.label ?? s)}</Tag>
          ),
        },
        {
          title: t('Full pipeline'),
          dataIndex: ['config', 'full_pipeline'],
          width: 90,
          render: (v: boolean) => (v ? t('Yes') : t('basic stages only')),
        },
        {
          title: t('Created'),
          dataIndex: 'created_at',
          width: 180,
          render: (v: number) => formatTime(v),
        },
        {
          title: t('Actions'),
          width: 140,
          render: (_, p) => (
            <Space size={4}>
              <Tooltip title={t('Code Graph')}>
                <Button
                  type="text"
                  icon={<ApartmentOutlined />}
                  onClick={() => navigate(`/projects/${p.id}/graph`)}
                />
              </Tooltip>
              {/*
                暂时注释：节点浏览（Explorer）入口已停用（与侧栏同步）。
                它的独特价值是「按名精确查 / 按类盘点」，但当前形态没兑现：
                `limit: 200` 硬顶且无排序 → 大盘点会漏；列里 fqn / 语言 / 阶段 / 置信度
                是造图内部字段；与提示词增强（语义检索）大量重叠。
                页面与路由都保留，恢复时把这一行（与侧栏那一行）解开即可。
              */}
              {/* <Tooltip title={t('Explorer')}>
                <Button
                  type="text"
                  icon={<AreaChartOutlined />}
                  onClick={() => navigate(`/projects/${p.id}/explorer`)}
                />
              </Tooltip> */}
              <DeleteProjectButton projectId={p.id} name={p.name} onDeleted={onDeleted} />
            </Space>
          ),
        },
      ]}
    />
  );
}
