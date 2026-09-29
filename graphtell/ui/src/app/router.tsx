import { Navigate, createBrowserRouter, createHashRouter } from 'react-router-dom';
import { AppShell } from '@/widgets/app-shell';
import { ProjectsPage } from '@/pages/projects/ProjectsPage';
import { GraphPage } from '@/pages/graph/GraphPage';
import { ExplorerPage } from '@/pages/explorer/ExplorerPage';
import { CoveragePage } from '@/pages/coverage/CoveragePage';
import { CheckPage } from '@/pages/check/CheckPage';
import { RulesPage } from '@/pages/check/RulesPage';
import { RecallPage } from '@/pages/recall/RecallPage';

const routes = [
  {
    path: '/',
    element: <AppShell />,
    children: [
      { index: true, element: <ProjectsPage /> },
      { path: 'projects', element: <Navigate to="/" replace /> },
      { path: 'projects/:projectId/graph', element: <GraphPage /> },
      // 节点浏览入口已停用（侧栏 / 工程表格都注释掉了），但路由与页面**保留**：
      // 直接访问仍可用，恢复入口时只要解开侧栏那一行。
      { path: 'projects/:projectId/explorer', element: <ExplorerPage /> },
      { path: 'projects/:projectId/coverage', element: <CoveragePage /> },
      { path: 'projects/:projectId/check', element: <CheckPage /> },
      { path: 'projects/:projectId/rules', element: <RulesPage /> },
      { path: 'projects/:projectId/recall', element: <RecallPage /> },
    ],
  },
];

// 静态 demo（GitHub Pages）必须走 HashRouter：站点挂在
// `https://<user>.github.io/<repo>/` 这种**子路径**下，BrowserRouter 的
// `/projects/1/graph` 会按站点根去匹配 —— 匹配不到，直接刷新还会 404（Pages
// 没有后端重写规则）。Hash 路由与子路径无关，刷新也只是重新请求 index.html。
//
// 仅在 `VITE_STATIC_DEMO=1` 时切换；正常部署（后端 ServeDir 兜底 SPA）行为不变。
export const router =
  import.meta.env.VITE_STATIC_DEMO === '1' ? createHashRouter(routes) : createBrowserRouter(routes);
