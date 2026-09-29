import { Navigate, createBrowserRouter } from 'react-router-dom';
import { AppShell } from '@/widgets/app-shell';
import { ProjectsPage } from '@/pages/projects/ProjectsPage';
import { GraphPage } from '@/pages/graph/GraphPage';
import { ExplorerPage } from '@/pages/explorer/ExplorerPage';
import { CoveragePage } from '@/pages/coverage/CoveragePage';
import { CheckPage } from '@/pages/check/CheckPage';
import { RulesPage } from '@/pages/check/RulesPage';
import { RecallPage } from '@/pages/recall/RecallPage';

export const router = createBrowserRouter([
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
]);
