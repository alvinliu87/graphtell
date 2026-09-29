import { useEffect, useState } from 'react';
import {
  Badge,
  Button,
  Divider,
  Dropdown,
  Layout,
  Menu,
  Segmented,
  Space,
  Tooltip,
  Typography,
} from 'antd';
import {
  ApartmentOutlined,
  // 暂时注释：节点浏览入口已停用
  // DatabaseOutlined,
  DownOutlined,
  MenuFoldOutlined,
  MenuUnfoldOutlined,
  SafetyCertificateOutlined,
  // 暂时注释：规则集入口移到规则检验页
  // ProfileOutlined,
  SearchOutlined,
  UnorderedListOutlined,
  // 暂时注释：设置入口已隐藏
  // SettingOutlined,
} from '@ant-design/icons';
import { Outlet, useLocation, useNavigate, useParams } from 'react-router-dom';
import { useHealth } from '@/entities/pipeline';
import { projectApi, type Project } from '@/entities/project';
import { checkApi, type CheckSummary } from '@/entities/check';
import { useAsync } from '@/shared/lib/useAsync';
import { useLocale, type Lang } from '@/shared/lib/i18n';

type SeverityCounts = { critical: number; error: number; warning: number; info: number };

const { Sider, Content, Header } = Layout;

/** 顶栏工程下拉最多直接展示的工程数，更多走底部「工程总览」进列表页。 */
const MAX_PROJECTS = 8;

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

  // 注意：**诊断不在这里取汇总，也不在侧栏挂角标**。
  // 诊断是"这张图少建了什么"的报告，主体是图 —— 它的入口在代码图页标题旁 ⓘ 的 Popover 里
  // （见 GraphPage 的「图覆盖」行）。侧栏一个常驻橙色角标只会表达"有东西欠你处理"，
  // 而诊断里绝大多数是引擎局限与预期内，那是在骗人。

  // 顶栏工程快捷选择：拉工程列表，当前工程名直接从列表里取（与页面标题同一口径）。
  const projectsRes = useAsync<Project[]>(() => projectApi.list(), []);
  const projects = projectsRes.data ?? [];
  const currentProject = projects.find((p) => String(p.id) === projectId) ?? null;
  const projectName = currentProject?.name ?? null;

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

  // 规则检验的菜单标签：带严重度角标，让菜单本身成为质量仪表盘。
  //
  // 只有规则检验带角标：它统计的是"代码违反了哪条规则"，条数就是待办数；
  // 诊断不是（见上方注释），所以不进侧栏。
  const checkLabel = (
    <span style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
      <span>{t('规则检验')}</span>
      {severityBadge(summary)}
    </span>
  );

  // 左侧栏是否收起：代码图路由默认收起（进入即最大化，让出横向空间给图），
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

  // 侧栏只放「工程内视图」；工程导航（选择/切换/总览）移到顶栏下拉，
  // 避免「工程总览」与视图平级带来的上下级歧义。无工程时列表为空，渲染提示。
  /**
   * 侧栏只留**三个平铺项**，不再分组、也不要组标题。
   *
   * 组标题（"探索" / "质量门禁"）是给"一组 ≥3 个相似项"做路标的；
   * 现在总共就三项、且各自是不同动作（看图 / 搜代码 / 看结论），
   * 分组只会让"两个组各一两项"看起来像在凑数 —— 平铺反而一眼看完。
   *
   * 规则集**不进菜单**：它调的是"哪些规则启用、阈值多少"，是规则检验的**配置项**，
   * 不是并列的目的地 —— 用户是在看了结论之后才想去调规则，所以入口放在
   * 规则检验页页头（见 CheckPage 的「规则集」按钮）。
   */
  const items = [
    ...(projectId
      ? [
          { key: withProject('/graph'), icon: <ApartmentOutlined />, label: t('代码图') },
          // 暂时注释：节点浏览入口已停用（工程表格里那个按钮也一并注释了）。
          // 它真正不可替代的是「按名精确查 / 按类盘点」，但当前形态没兑现：
          // `limit: 200` 硬顶且无排序（盘点会漏）、列是造图内部字段（fqn / 语言 / 阶段 / 置信度）、
          // 与提示词增强（语义检索）大量重叠。
          // 路由 `/explorer` 与页面都保留着 —— 恢复只需解开这一行与 ProjectTable 里那一段。
          // { key: withProject('/explorer'), icon: <DatabaseOutlined />, label: t('节点浏览') },
          { key: withProject('/recall'), icon: <SearchOutlined />, label: t('提示词增强') },
          {
            key: withProject('/check'),
            icon: <SafetyCertificateOutlined />,
            label: checkLabel,
          },
          // 暂时注释：规则集改为从规则检验页进入（见上方注释）。
          // { key: withProject('/rules'), icon: <ProfileOutlined />, label: t('规则集') },
        ]
      : []),
    // 暂时注释：设置页路由已停用，导航入口一并隐藏（以后再考虑加回）。
    // { key: '/settings', icon: <SettingOutlined />, label: t('设置') },
  ];

  return (
    <Layout style={{ minHeight: '100vh' }}>
      {projectId && (
        <Sider
          theme="light"
          width={216}
          collapsed={collapsed}
          collapsedWidth={64}
          breakpoint="lg"
          onBreakpoint={(broken) => setCollapsed(broken)}
          style={{ borderRight: '1px solid #eef0f4', paddingTop: 8 }}
        >
          <Menu
            mode="inline"
            selectedKeys={[location.pathname]}
            items={items}
            onClick={({ key }) => navigate(key)}
            style={{ borderInlineEnd: 'none' }}
          />
        </Sider>
      )}
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
            lineHeight: 'normal',
          }}
        >
          <Space size={12}>
            {projectId && (
              <Button
                type="text"
                aria-label={collapsed ? t('展开侧边栏') : t('收起侧边栏')}
                icon={collapsed ? <MenuUnfoldOutlined /> : <MenuFoldOutlined />}
                onClick={toggleSider}
              />
            )}
            <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexShrink: 0 }}>
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
                  lineHeight: 1,
                }}
              >
                GT
              </div>
              {/* Header 自带 line-height:64px 会把多行文字撑爆 56px 高度，必须显式收敛行高 */}
              <div style={{ lineHeight: 1.25, whiteSpace: 'nowrap' }}>
                <div style={{ fontWeight: 700, letterSpacing: '-0.02em', fontSize: 15 }}>
                  GraphTell
                </div>
                <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.4)' }}>
                  {t('代码库图化分析')}
                </div>
              </div>
            </div>
            <Dropdown
              trigger={['click']}
              menu={{
                selectedKeys: projectId ? [projectId] : [],
                items: [
                  ...projects.slice(0, MAX_PROJECTS).map((p) => ({
                    key: String(p.id),
                    label: p.name,
                  })),
                  { type: 'divider' },
                  {
                    key: '__list__',
                    icon: <UnorderedListOutlined />,
                    label: t('工程总览'),
                  },
                ],
                onClick: ({ key }) => {
                  if (key === '__list__') navigate('/');
                  else navigate(`/projects/${key}/graph`);
                },
              }}
            >
              <Button>
                {projectName ?? t('选择工程')} <DownOutlined />
              </Button>
            </Dropdown>
          </Space>
          {/* 右侧：仅语言切换 + 离线告警。框架知识 / 语言等静态信息已从顶栏移除（可在工程详情查看），
              后端在线时无提示（应用能跑即代表在线），仅在异常时冒出红点告警，避免日常噪音。 */}
          <Space size={10} align="center">
            <Tooltip title={t('界面语言')}>
              <Segmented
                size="small"
                value={lang}
                onChange={(v) => setLang(v as Lang)}
                options={[
                  { label: '中', value: 'zh-CN' },
                  { label: 'EN', value: 'en-US' },
                ]}
              />
            </Tooltip>
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
