import { Navigate, createBrowserRouter, createHashRouter } from 'react-router-dom';
import { AppShell } from '@/widgets/app-shell';
import { ProjectsPage } from '@/pages/projects/ProjectsPage';
import { GraphPage } from '@/pages/graph/GraphPage';
import { ExplorerPage } from '@/pages/explorer/ExplorerPage';
import { CoveragePage } from '@/pages/coverage/CoveragePage';
import { CheckPage } from '@/pages/check/CheckPage';
import { RulesPage } from '@/pages/check/RulesPage';
import { RecallPage } from '@/pages/recall/RecallPage';
import { SettingsPage } from '@/pages/settings/SettingsPage';

const routes = [
  {
    path: '/',
    element: <AppShell />,
    children: [
      { index: true, element: <ProjectsPage /> },
      { path: 'projects', element: <Navigate to="/" replace /> },
      { path: 'projects/:projectId/graph', element: <GraphPage /> },
      // The explorer entry is disabled (commented out in both the sidebar and the project table),
      // but the route and page are **kept**: direct URLs still work, so restoring the entry only
      // means uncommenting that one sidebar line.
      { path: 'projects/:projectId/explorer', element: <ExplorerPage /> },
      { path: 'projects/:projectId/coverage', element: <CoveragePage /> },
      { path: 'projects/:projectId/check', element: <CheckPage /> },
      { path: 'projects/:projectId/rules', element: <RulesPage /> },
      { path: 'projects/:projectId/recall', element: <RecallPage /> },
      { path: 'settings', element: <SettingsPage /> },
    ],
  },
];

// The static demo (GitHub Pages) must use HashRouter: the site is served from a **sub-path**
// like `https://<user>.github.io/<repo>/`, where BrowserRouter matches `/projects/1/graph`
// against the site root — it never matches, and a hard refresh 404s (Pages has no backend
// rewrite rules). Hash routing is independent of the sub-path; a refresh just re-requests index.html.
//
// Switched only when `VITE_STATIC_DEMO=1`; normal deployment (backend ServeDir SPA fallback) is unchanged.
export const router =
  import.meta.env.VITE_STATIC_DEMO === '1' ? createHashRouter(routes) : createBrowserRouter(routes);
