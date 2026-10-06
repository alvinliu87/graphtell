// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { act } from 'react';
import { Routes, Route } from 'react-router-dom';
import { CoveragePage } from './CoveragePage';
import { mountWithRouter } from '@/test/render';

describe('CoveragePage', () => {
  it('renders the build-report shell', async () => {
    mountWithRouter(
      <Routes>
        <Route path="/projects/:projectId/coverage" element={<CoveragePage />} />
      </Routes>,
      '/projects/1/coverage',
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
    expect(document.body.textContent).toContain('Build Report');
    expect(document.body.textContent).toContain('Problem types');
  });
});
