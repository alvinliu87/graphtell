import { Alert, Button, Card, Col, Empty, Row, Space } from 'antd';
import { PlusOutlined, ReloadOutlined } from '@ant-design/icons';
import { useState } from 'react';
import { useProjects } from '@/entities/project';
import { CreateProjectModal } from '@/features/create-project';
import { ProjectTable } from '@/widgets/project-table';
import { PageHeader } from '@/shared/ui/PageHeader';
import { StatCard } from '@/shared/ui/StatCard';
import { useLocale } from '@/shared/lib/i18n';
import { DeploymentUnitOutlined, FolderOpenOutlined } from '@ant-design/icons';

/** Project overview page: CRUD entry point. */
export function ProjectsPage() {
  const { projects, loading, error, reload, building } = useProjects();
  const [open, setOpen] = useState(false);
  const { t } = useLocale();

  const ready = projects.filter((p) => p.status === 'ready').length;
  const indexing = projects.filter((p) => p.status === 'indexing').length;

  return (
    <>
      <PageHeader
        title={t('Projects')}
        subtitle={t('Adding a project auto-starts graphing: detect sub-projects → syntax graph → load framework knowledge → semantic synthesis → dynamic resolution')}
        extra={
          <Space>
            <Button icon={<ReloadOutlined />} onClick={() => void reload()} loading={loading}>
              {t('Refresh')}
            </Button>
            <Button type="primary" icon={<PlusOutlined />} onClick={() => setOpen(true)}>
              {t('New project')}
            </Button>
          </Space>
        }
      />

      {/* On backend failure / no network the error **must** be reported explicitly, not fall through to the "no projects yet" empty state below --
               otherwise it looks exactly like "genuinely 0 projects": the user thinks there's no data when the request actually failed
               (this spot swallowed errors for a long time: the error returned by useProjects() was never consumed). */}
      {!loading && error ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('Failed to load projects')}
          description={
            <div>
              <div>{error}</div>
              <div style={{ marginTop: 8 }}>
                {t('The backend may be down or unreachable. Click "Refresh" (top-right) to retry.')}
              </div>
            </div>
          }
        />
      ) : null}

      {!loading && projects.length === 0 && !error ? (
        <Empty
          style={{ marginTop: 96 }}
          description={t('No projects yet — create one to start graphing')}
        >
          <Button type="primary" icon={<PlusOutlined />} onClick={() => setOpen(true)}>
            {t('New project')}
          </Button>
        </Empty>
      ) : projects.length > 0 && !error ? (
        <>
          <Row gutter={[16, 16]} style={{ marginBottom: 20 }}>
            <Col xs={24} sm={8}>
              <StatCard
                title={t('Total projects')}
                value={projects.length}
                accent="#3d7eff"
                icon={<FolderOpenOutlined />}
              />
            </Col>
            <Col xs={24} sm={8}>
              <StatCard title={t('Ready')} value={ready} accent="#16a34a" icon={<DeploymentUnitOutlined />} />
            </Col>
            <Col xs={24} sm={8}>
              {/* The spinner only spins while it is **really tracking** (the list polls silently during a build) — it is not decoration */}
              <StatCard title={t('Indexing')} value={indexing} accent="#f59e0b" icon={<ReloadOutlined spin={building} />} />
            </Col>
          </Row>

          <Card
            variant="borderless"
            style={{ borderRadius: 14 }}
            title={t('All projects')}
          >
            <ProjectTable projects={projects} loading={loading} onDeleted={() => void reload()} />
          </Card>
        </>
      ) : null}

      <CreateProjectModal
        open={open}
        onClose={() => setOpen(false)}
        onCreated={() => void reload()}
      />
    </>
  );
}
