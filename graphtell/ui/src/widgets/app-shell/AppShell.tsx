import { useEffect, useState } from 'react';
import { Badge, Button, Divider, Layout, Menu, Segmented, Space, Typography } from 'antd';
import {
  ApartmentOutlined,
  DatabaseOutlined,
  FundProjectionScreenOutlined,
  MenuFoldOutlined,
  MenuUnfoldOutlined,
  SafetyCertificateOutlined,
  SearchOutlined,
  // 暂时注释：设置入口已隐藏
  // SettingOutlined,
  WarningOutlined,
} from '@ant-design/icons';
import { Outlet, useLocation, useNavigate, useParams } from 'react-router-dom';
import { useHealth } from '@/entities/pipeline';
import { checkApi, type CheckSummary } from '@/entities/check';
import { graphApi, type DiagnosticSummary } from '@/entities/graph';
import { useAsync } from '@/shared/lib/useAsync';
import { useLocale, type Lang } from '@/shared/lib/i18n';

type SeverityCounts = { critical: number; error: number; warning: number; info: number };

const { Sider, Content, Header } = Layout;

/** 应用外壳：侧边导航 + 顶栏 + 内容区。 */
export function AppShell() {
  const navigate = useNavigate();
  const location = useLocation();
  const { projectId } = useParams();
  const { health } = useHealth();
  const { lang, setLang, t } = useLocale();

  // 菜单角标：自动检查落库的违规汇总（建图后自动跑，这里只读不重跑）。
  const summaryRes = useAsync<CheckSummary | null>(
    () => (projectId ? checkApi.summary(Number(projectId)) : Promise.resolve(null)),
    [projectId],
  );
  const summary = summaryRes.data ?? null;

  // 诊断角标：非规则诊断（根缺失、断链、identity 冲突等）按严重度汇总。
  const diagRes = useAsync<DiagnosticSummary | null>(
    () => (projectId ? graphApi.diagnosticsSummary(Number(projectId)) : Promise.resolve(null)),
    [projectId],
  );
  const diagSummary = diagRes.data ?? null;

  // 把严重度汇总渲染成菜单右对齐角标：error/critical 红、warning 橙，全清则无。
  const severityBadge = (s: SeverityCounts | null) => {
    if (!s || s.critical + s.error + s.warning === 0) return null;
    const alarm = s.critical + s.error;
    return (
      <Badge
        count={s.critical + s.error + s.warning}
        color={alarm > 0 ? '#ff4d4f' : '#fa8c16'}
        overflowCount={99}
        style={{ boxShadow: 'none' }}
      />
    );
  };

  // 合规检查 / 诊断的菜单标签：带严重度角标，让菜单本身成为质量仪表盘。
  const checkLabel = (
    <span style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
      <span>{t('合规检查')}</span>
      {severityBadge(summary)}
    </span>
  );
  const diagnosticsLabel = (
    <span style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
      <span>{t('诊断')}</span>
      {severityBadge(diagSummary)}
    </span>
  );

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
    { key: '/', icon: <FundProjectionScreenOutlined />, label: t('工程总览') },
    ...(projectId
      ? [
          {
            type: 'group' as const,
            key: 'group-explore',
            label: t('探索'),
            children: [
              { key: withProject('/graph'), icon: <ApartmentOutlined />, label: t('图视图') },
              { key: withProject('/explorer'), icon: <DatabaseOutlined />, label: t('节点浏览') },
              { key: withProject('/recall'), icon: <SearchOutlined />, label: t('代码召回') },
            ],
          },
          {
            type: 'group' as const,
            key: 'group-quality',
            label: t('质量门禁'),
            children: [
              {
                key: withProject('/check'),
                icon: <SafetyCertificateOutlined />,
                label: checkLabel,
              },
              {
                key: withProject('/diagnostics'),
                icon: <WarningOutlined />,
                label: diagnosticsLabel,
              },
            ],
          },
        ]
      : []),
    // 暂时注释：设置页路由已停用，导航入口一并隐藏（以后再考虑加回）。
    // { key: '/settings', icon: <SettingOutlined />, label: t('设置') },
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
              <div style={{ fontWeight: 700, letterSpacing: '-0.02em' }}>GraphTell</div>
              <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.4)' }}>{t('代码库图化分析')}</div>
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
              aria-label={collapsed ? t('展开侧边栏') : t('收起侧边栏')}
              icon={collapsed ? <MenuUnfoldOutlined /> : <MenuFoldOutlined />}
              onClick={toggleSider}
            />
            <Typography.Text type="secondary" style={{ fontSize: 13 }}>
              {projectId ? `${t('当前工程')} #${projectId}` : t('选择或创建一个工程开始分析')}
            </Typography.Text>
          </Space>
          {/* 右侧：仅语言切换 + 离线告警。框架知识 / 语言等静态信息已从顶栏移除（可在工程详情查看），
              后端在线时无提示（应用能跑即代表在线），仅在异常时冒出红点告警，避免日常噪音。 */}
          <Space size={10} align="center">
            <Segmented
              size="small"
              value={lang}
              onChange={(v) => setLang(v as Lang)}
              options={[
                { label: '中文', value: 'zh-CN' },
                { label: 'EN', value: 'en-US' },
              ]}
            />
            {/* 暂时注释：设置入口已隐藏
            <Button
              type="text"
              aria-label={t('设置')}
              icon={<SettingOutlined />}
              onClick={() => navigate('/settings')}
            />
            */}
            {health && health.status !== 'ok' && (
              <>
                <Divider type="vertical" style={{ marginInline: 2 }} />
                <Badge status="error" text={t('后端未连接')} />
              </>
            )}
          </Space>
        </Header>
        <Content style={{ padding: 24, background: '#f7f8fa' }}>
          <Outlet context={{ refreshCheckSummary: summaryRes.silentReload }} />
        </Content>
      </Layout>
    </Layout>
  );
}
