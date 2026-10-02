import { Button, Space, Table, Tag, Tooltip } from 'antd';
import { AreaChartOutlined, ApartmentOutlined } from '@ant-design/icons';
import { useNavigate } from 'react-router-dom';
import { STATUS_META, type Project } from '@/entities/project';
import { useLocale } from '@/shared/lib/i18n';
import { DeleteProjectButton } from '@/features/delete-project';
import { formatTime } from '@/shared/lib/format';

/** Project list. */
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
        // The root directory is **variable-length** content, so let it take the remaining width (no width set); if the unsized slot were left to
        // "name", under a fixed table layout it would claim all remaining space and squeeze the path into a stub -- exactly the cause of
        // "very wide names, truncated paths". Name is instead a short enumeration, so a fixed width + ellipsis is steadier.
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
                Temporarily commented out: the node-browse (Explorer) entry is disabled (synced with the sidebar).
                Its unique value is "exact lookup by name / inventory by kind", but the current form doesn't deliver:
                `limit: 200` hard cap with no sorting -> a large inventory misses entries; the columns fqn / language / phase / confidence
                are internal graph-building fields; and it overlaps heavily with prompt augmentation (semantic search).
                The page and route are both kept; restoring just uncomments this line (and the sidebar one).
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
