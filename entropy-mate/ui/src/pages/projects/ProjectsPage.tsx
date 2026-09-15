import { Button, Card, Col, Row, Space, Tag } from 'antd';
import { PlusOutlined, ReloadOutlined } from '@ant-design/icons';
import { useState } from 'react';
import { useProjects } from '@/entities/project';
import { CreateProjectModal } from '@/features/create-project';
import { ProjectTable } from '@/widgets/project-table';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';
import { useLocale } from '@/shared/lib/i18n';
import { DeploymentUnitOutlined, FolderOpenOutlined } from '@ant-design/icons';

/** 工程总览页：CRUD 入口。 */
export function ProjectsPage() {
  const { projects, loading, reload } = useProjects();
  const [open, setOpen] = useState(false);
  const { t } = useLocale();

  const ready = projects.filter((p) => p.status === 'ready').length;
  const indexing = projects.filter((p) => p.status === 'indexing').length;

  return (
    <>
      <PageHeader
        title={t('工程总览')}
        subtitle={t('添加一个工程后会自动开始建图：识别子工程 → 语法建图 → 装载框架知识 → 语义合成 → 动态解析')}
        extra={
          <Space>
            <Button icon={<ReloadOutlined />} onClick={() => void reload()} loading={loading}>
              {t('刷新')}
            </Button>
            <Button type="primary" icon={<PlusOutlined />} onClick={() => setOpen(true)}>
              {t('新建工程')}
            </Button>
          </Space>
        }
      />

      <Row gutter={[16, 16]} style={{ marginBottom: 20 }}>
        <Col xs={24} sm={8}>
          <StatCard
            title={t('工程总数')}
            value={projects.length}
            accent="#3d7eff"
            icon={<FolderOpenOutlined />}
          />
        </Col>
        <Col xs={24} sm={8}>
          <StatCard title={t('已就绪')} value={ready} accent="#16a34a" icon={<DeploymentUnitOutlined />} />
        </Col>
        <Col xs={24} sm={8}>
          <StatCard title={t('建图中')} value={indexing} accent="#f59e0b" icon={<ReloadOutlined spin={indexing > 0} />} />
        </Col>
      </Row>

      <Card
        variant="borderless"
        style={{ borderRadius: 14 }}
        title={t('全部工程')}
        extra={<Tag color="blue">{t('SQLite 持久化')}</Tag>}
      >
        <ProjectTable projects={projects} loading={loading} onDeleted={() => void reload()} />
      </Card>

      <CreateProjectModal
        open={open}
        onClose={() => setOpen(false)}
        onCreated={() => void reload()}
      />
    </>
  );
}
