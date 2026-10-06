// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { act } from 'react';
import { Routes, Route, Outlet } from 'react-router-dom';
import { CheckPage } from './CheckPage';
import { mountWithRouter } from '@/test/render';

describe('CheckPage', () => {
  it('renders the rule-inspection shell (context provided via Outlet)', async () => {
    mountWithRouter(
      <Routes>
        <Route
          path="/projects/:projectId"
          element={<Outlet context={{ refreshCheckSummary: () => {} }} />}
        >
          <Route path="check" element={<CheckPage />} />
        </Route>
      </Routes>,
      '/projects/1/check',
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
    expect(document.body.textContent).toContain('Rule inspection');
    expect(document.body.textContent).toContain('Rule Set');
  });
});
