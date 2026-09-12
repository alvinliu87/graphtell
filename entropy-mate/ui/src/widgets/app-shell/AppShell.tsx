import { Badge, Layout, Menu, Space, Tag, Tooltip, Typography } from 'antd';
import {
  ApartmentOutlined,
  DatabaseOutlined,
  DeploymentUnitOutlined,
  FundProjectionScreenOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import { Outlet, useLocation, useNavigate, useParams } from 'react-router-dom';
import { useHealth } from '@/entities/pipeline';

const { Sider, Content, Header } = Layout;

/** 应用外壳：侧边导航 + 顶栏 + 内容区。 */
export function AppShell() {
  const navigate = useNavigate();
  const location = useLocation();
  const { projectId } = useParams();
  const { health } = useHealth();

  const withProject = (path: string) => (projectId ? `/projects/${projectId}${path}` : '/');

  const items = [
    { key: '/', icon: <FundProjectionScreenOutlined />, label: '工程总览' },
    ...(projectId
      ? [
          { key: withProject('/graph'), icon: <ApartmentOutlined />, label: '图视图' },
          { key: withProject('/explorer'), icon: <DatabaseOutlined />, label: '节点浏览' },
          { key: withProject('/diagnostics'), icon: <WarningOutlined />, label: '诊断' },
        ]
      : []),
  ];

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <Sider
        theme="light"
        width={216}
        style={{ borderRight: '1px solid #eef0f4', paddingTop: 8 }}
        breakpoint="lg"
        collapsedWidth={64}
      >
        <div style={{ padding: '10px 20px 18px', display: 'flex', alignItems: 'center', gap: 10 }}>
          <div
            style={{
              width: 30,
              height: 30,
              borderRadius: 9,
              background: 'linear-gradient(135deg,#3d7eff,#7c5cff)',
              color: '#fff',
              display: 'grid',
              placeItems: 'center',
              fontWeight: 700,
            }}
          >
            EM
          </div>
          <div>
            <div style={{ fontWeight: 700, letterSpacing: '-0.02em' }}>EntropyMate</div>
            <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.4)' }}>代码库图化分析</div>
          </div>
        </div>
        <Menu
          mode="inline"
          selectedKeys={[location.pathname]}
          items={items}
          onClick={({ key }) => navigate(key)}
          style={{ borderInlineEnd: 'none' }}
        />
      </Sider>
      <Layout>
        <Header
          style={{
            background: '#fff',
            borderBottom: '1px solid #eef0f4',
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            paddingInline: 24,
            height: 56,
          }}
        >
          <Typography.Text type="secondary" style={{ fontSize: 13 }}>
            {projectId ? `当前工程 #${projectId}` : '选择或创建一个工程开始分析'}
          </Typography.Text>
          <Space size={10}>
            {health ? (
              <>
                <Tooltip title="已装载的框架知识数量">
                  <Tag icon={<DeploymentUnitOutlined />} color="blue">
                    FKB {health.frameworks}
                  </Tag>
                </Tooltip>
                <Tag color="green">{health.languages.map((l: string) => l.toUpperCase()).join(' / ')}</Tag>
                <Badge status={health.status === 'ok' ? 'success' : 'error'} text="后端在线" />
              </>
            ) : (
              <Badge status="error" text="后端未连接" />
            )}
          </Space>
        </Header>
        <Content style={{ padding: 24, background: '#f7f8fa' }}>
          <Outlet />
        </Content>
      </Layout>
    </Layout>
  );
}
