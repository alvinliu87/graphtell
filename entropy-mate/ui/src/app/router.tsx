import { Navigate, createBrowserRouter } from 'react-router-dom';
import { AppShell } from '@/widgets/app-shell';
import { ProjectsPage } from '@/pages/projects/ProjectsPage';
import { GraphPage } from '@/pages/graph/GraphPage';
import { ExplorerPage } from '@/pages/explorer/ExplorerPage';
import { DiagnosticsPage } from '@/pages/diagnostics/DiagnosticsPage';

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
    ],
  },
]);
