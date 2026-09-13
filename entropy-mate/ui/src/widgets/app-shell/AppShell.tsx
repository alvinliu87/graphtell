import { useEffect, useState } from 'react';
import { Badge, Button, Layout, Menu, Space, Tag, Tooltip, Typography } from 'antd';
import {
  ApartmentOutlined,
  DatabaseOutlined,
  DeploymentUnitOutlined,
  FundProjectionScreenOutlined,
  MenuFoldOutlined,
  MenuUnfoldOutlined,
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

  // 左侧栏是否收起：图视图路由默认收起（进入即最大化，让出横向空间给图），
  // 其余路由默认展开。仅在 pathname 变化时按路由重置；页面内的手动折叠/展开在路由内持续有效。
  // 同时保留 lg 断点的响应式自动收起。
  const [collapsed, setCollapsed] = useState<boolean>(() =>
    /^\/projects\/\d+\/graph$/.test(location.pathname),
  );
  const toggleSider = () => setCollapsed((c) => !c);
  useEffect(() => {
    const isGraph = /^\/projects\/\d+\/graph$/.test(location.pathname);
    setCollapsed(isGraph);
  }, [location.pathname]);

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
        collapsed={collapsed}
        collapsedWidth={64}
        breakpoint="lg"
        onBreakpoint={(broken) => setCollapsed(broken)}
        style={{ borderRight: '1px solid #eef0f4', paddingTop: 8 }}
      >
        <div
          style={{
            padding: collapsed ? '10px 17px 18px' : '10px 20px 18px',
            display: 'flex',
            alignItems: 'center',
            gap: 10,
          }}
        >
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
          {!collapsed && (
            <div>
              <div style={{ fontWeight: 700, letterSpacing: '-0.02em' }}>EntropyMate</div>
              <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.4)' }}>代码库图化分析</div>
            </div>
          )}
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
          <Space size={12}>
            <Button
              type="text"
              aria-label={collapsed ? '展开侧边栏' : '收起侧边栏'}
              icon={collapsed ? <MenuUnfoldOutlined /> : <MenuFoldOutlined />}
              onClick={toggleSider}
            />
            <Typography.Text type="secondary" style={{ fontSize: 13 }}>
              {projectId ? `当前工程 #${projectId}` : '选择或创建一个工程开始分析'}
            </Typography.Text>
          </Space>
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
