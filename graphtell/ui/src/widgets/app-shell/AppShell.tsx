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
  // Temporarily commented out: the node-browse entry is disabled
  // DatabaseOutlined,
  DownOutlined,
  MenuFoldOutlined,
  MenuUnfoldOutlined,
  SafetyCertificateOutlined,
  // Temporarily commented out: the rule-set entry moved to the rule-check page
  // ProfileOutlined,
  SearchOutlined,
  UnorderedListOutlined,
  // Temporarily commented out: the settings entry is hidden
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

/** Max projects shown directly in the top-bar project dropdown; more go through the bottom "project overview" into the list page. */
const MAX_PROJECTS = 8;

/** App shell: side navigation + top bar + content area. */
export function AppShell() {
  const navigate = useNavigate();
  const location = useLocation();
  const { projectId } = useParams();
  const { health } = useHealth();
  const { lang, setLang, t } = useLocale();

  // Menu badge: the summary of violations persisted by the automatic check (run automatically after a build; here we only read, never rerun).
  const summaryRes = useAsync<CheckSummary | null>(
    () => (projectId ? checkApi.summary(Number(projectId)) : Promise.resolve(null)),
    [projectId],
  );
  const summary = summaryRes.data ?? null;

  // Note: **diagnostics do not fetch a summary here, and get no sidebar badge**.
  // Diagnostics are a report of "what this graph failed to build", and their subject is the graph -- their entry is in the ⓘ Popover beside the code-graph page title
  // (see the "graph coverage" row in GraphPage). A permanent orange sidebar badge would only say "something here needs your attention",
  // while the vast majority of diagnostics are engine limitations and expected -- that would be lying.

  // Top-bar project quick-select: fetch the project list; the current project name comes straight from the list (same criterion as the page title).
  const projectsRes = useAsync<Project[]>(() => projectApi.list(), []);
  const projects = projectsRes.data ?? [];
  const currentProject = projects.find((p) => String(p.id) === projectId) ?? null;
  const projectName = currentProject?.name ?? null;

  // Render the severity summary as a right-aligned menu badge: error/critical red, warning orange, nothing when all clear.
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

  // The rule-check menu label carries a severity badge, making the menu itself a quality dashboard.
  //
  // Only rule-check gets a badge: it counts "which rule the code violated", and the count is the to-do count;
  // diagnostics aren't (see the comment above), so they stay out of the sidebar.
  const checkLabel = (
    <span style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', gap: 8 }}>
      <span>{t('Rule inspection')}</span>
      {severityBadge(summary)}
    </span>
  );

  // Whether the left sidebar is collapsed: the code-graph route defaults to collapsed (maximize on entry, giving horizontal space to the graph),
  // other routes default to expanded. Reset by route only when pathname changes; manual collapse/expand within a page persists across that route.
  // The responsive auto-collapse at the lg breakpoint is kept too.
  const [collapsed, setCollapsed] = useState<boolean>(() =>
    /^\/projects\/\d+\/graph$/.test(location.pathname),
  );
  const toggleSider = () => setCollapsed((c) => !c);
  useEffect(() => {
    const isGraph = /^\/projects\/\d+\/graph$/.test(location.pathname);
    setCollapsed(isGraph);
  }, [location.pathname]);

  const withProject = (path: string) => (projectId ? `/projects/${projectId}${path}` : '/');

  // The sidebar holds only "in-project views"; project navigation (select / switch / overview) moved to the top-bar dropdown,
  // avoiding the hierarchy ambiguity of putting "project overview" at the same level as views. With no project the list is empty and a hint renders.
  /**
   * The sidebar keeps only **three flat items** -- no grouping, no group headings.
   *
   * Group headings ("explore" / "quality gate") are signposts for "a group of ≥3 similar items";
   * now there are only three items in total, each a different action (view graph / search code / view conclusions);
   * grouping would make "two groups of one or two" look like padding -- flat actually reads in one glance.
   *
   * The rule set **stays out of the menu**: it tunes "which rules are enabled and at what threshold" -- it is a **configuration** of rule checking,
   * not a parallel destination -- users want to tune rules after seeing conclusions, so the entry lives in
   * the rule-check page header (see the "rule set" button in CheckPage).
   */
  const items = [
    ...(projectId
      ? [
          { key: withProject('/graph'), icon: <ApartmentOutlined />, label: t('Code Graph') },
          // Temporarily commented out: the node-browse entry is disabled (the button in the project table is commented out too).
          // Its truly irreplaceable part is "exact lookup by name / inventory by kind", but the current form doesn't deliver:
          // `limit: 200` hard cap with no sorting (inventory misses entries), columns are internal graph-building fields (fqn / language / phase / confidence),
          // and it overlaps heavily with prompt augmentation (semantic search).
          // The route `/explorer` and the page are both kept -- restoring only needs uncommenting this line and that block in ProjectTable.
          // { key: withProject('/explorer'), icon: <DatabaseOutlined />, label: t('Explorer') },
          { key: withProject('/recall'), icon: <SearchOutlined />, label: t('Prompt augmentation') },
          {
            key: withProject('/check'),
            icon: <SafetyCertificateOutlined />,
            label: checkLabel,
          },
          // Temporarily commented out: the rule set is now entered from the rule-check page (see the comment above).
          // { key: withProject('/rules'), icon: <ProfileOutlined />, label: t('Rule Set') },
        ]
      : []),
    // Temporarily commented out: the settings page route is disabled and the nav entry hidden along with it (revisit later).
    // { key: '/settings', icon: <SettingOutlined />, label: t('Settings') },
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
                aria-label={collapsed ? t('Expand sidebar') : t('Collapse sidebar')}
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
              {/* The Header's built-in line-height:64px blows multi-line text past the 56px height, so the line height must be constrained explicitly */}
              <div style={{ lineHeight: 1.25, whiteSpace: 'nowrap' }}>
                <div style={{ fontWeight: 700, letterSpacing: '-0.02em', fontSize: 15 }}>
                  GraphTell
                </div>
                <div style={{ fontSize: 11, color: 'rgba(0,0,0,0.4)' }}>
                  {t('Codebase graph analysis')}
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
                    label: t('Projects'),
                  },
                ],
                onClick: ({ key }) => {
                  if (key === '__list__') navigate('/');
                  else navigate(`/projects/${key}/graph`);
                },
              }}
            >
              <Button>
                {projectName ?? t('Select project')} <DownOutlined />
              </Button>
            </Dropdown>
          </Space>
          {/* Right side: language switch + offline alert only. Static info like framework knowledge / languages was removed from the top bar (viewable in project details);
                   no hint while the backend is online (the app running at all means it's online); a red dot alert appears only on anomalies, avoiding daily noise. */}
          <Space size={10} align="center">
            <Tooltip title={t('UI language')}>
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
            {/* Temporarily commented out: the settings entry is hidden
            <Button
              type="text"
              aria-label={t('Settings')}
              icon={<SettingOutlined />}
              onClick={() => navigate('/settings')}
            />
            */}
            {health && health.status !== 'ok' && (
              <>
                <Divider type="vertical" style={{ marginInline: 2 }} />
                <Badge status="error" text={t('Backend disconnected')} />
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
