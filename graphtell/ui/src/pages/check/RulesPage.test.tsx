// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { act } from 'react';
import { Routes, Route, Outlet } from 'react-router-dom';
import { RulesPage } from './RulesPage';
import { mountWithRouter } from '@/test/render';

describe('RulesPage', () => {
  it('renders the rule-set shell (context provided via Outlet)', async () => {
    mountWithRouter(
      <Routes>
        <Route
          path="/projects/:projectId"
          element={<Outlet context={{ refreshCheckSummary: () => {} }} />}
        >
          <Route path="rules" element={<RulesPage />} />
        </Route>
      </Routes>,
      '/projects/1/rules',
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
    expect(document.body.textContent).toContain('Rule Set');
    expect(document.body.textContent).toContain('Save & re-run');
  });
});
