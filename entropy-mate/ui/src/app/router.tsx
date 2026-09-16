import { Navigate, createBrowserRouter } from 'react-router-dom';
import { AppShell } from '@/widgets/app-shell';
import { ProjectsPage } from '@/pages/projects/ProjectsPage';
import { GraphPage } from '@/pages/graph/GraphPage';
import { ExplorerPage } from '@/pages/explorer/ExplorerPage';
import { DiagnosticsPage } from '@/pages/diagnostics/DiagnosticsPage';
// 暂时注释：设置页停用（IDE 打开入口已移除，相关设置无处可用），以后再考虑加回。
// import { SettingsPage } from '@/pages/settings/SettingsPage';

export const router = createBrowserRouter([
  {
    path: '/',
    element: <AppShell />,
    children: [
      { index: true, element: <ProjectsPage /> },
      { path: 'projects', element: <Navigate to="/" replace /> },
      { path: 'projects/:projectId/graph', element: <GraphPage /> },
      { path: 'projects/:projectId/explorer', element: <ExplorerPage /> },
      { path: 'projects/:projectId/diagnostics', element: <DiagnosticsPage /> },
      // { path: 'settings', element: <SettingsPage /> },
    ],
  },
]);
