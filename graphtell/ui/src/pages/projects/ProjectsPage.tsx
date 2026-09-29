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

/** 工程总览页：CRUD 入口。 */
export function ProjectsPage() {
  const { projects, loading, error, reload, building } = useProjects();
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

      {/* 后端故障 / 网络不通时**必须**显式报错，不能落到下面的「还没有工程」空态 ——
          否则会和"确实 0 个工程"长得一模一样，用户以为没数据，实则请求挂了
          （这一处长期吞错：useProjects() 返回的 error 此前从未被消费）。 */}
      {!loading && error ? (
        <Alert
          type="error"
          showIcon
          style={{ marginBottom: 16 }}
          message={t('工程列表加载失败')}
          description={
            <div>
              <div>{error}</div>
              <div style={{ marginTop: 8 }}>
                {t('可能是后端未启动或网络不通。点击右上角「刷新」重试。')}
              </div>
            </div>
          }
        />
      ) : null}

      {!loading && projects.length === 0 && !error ? (
        <Empty
          style={{ marginTop: 96 }}
          description={t('还没有工程 —— 新建一个开始图化分析')}
        >
          <Button type="primary" icon={<PlusOutlined />} onClick={() => setOpen(true)}>
            {t('新建工程')}
          </Button>
        </Empty>
      ) : projects.length > 0 && !error ? (
        <>
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
              {/* 转圈只在**真在跟进**时转（列表在建图期间静默轮询），不是装饰 */}
              <StatCard title={t('建图中')} value={indexing} accent="#f59e0b" icon={<ReloadOutlined spin={building} />} />
            </Col>
          </Row>

          <Card
            variant="borderless"
            style={{ borderRadius: 14 }}
            title={t('全部工程')}
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
