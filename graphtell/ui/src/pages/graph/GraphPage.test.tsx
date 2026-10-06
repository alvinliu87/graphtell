// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { act } from 'react';
import { Routes, Route } from 'react-router-dom';
import { GraphPage } from './GraphPage';
import { mountWithRouter } from '@/test/render';

describe('GraphPage', () => {
  it('renders the code-graph shell (header + perspective picker + canvas host) without crashing', async () => {
    mountWithRouter(
      <Routes>
        <Route path="/projects/:projectId/graph" element={<GraphPage />} />
      </Routes>,
      '/projects/1/graph',
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
    expect(document.body.textContent).toContain('Code Graph');
    // Perspective picker + conclusions drawer entry are part of the shell.
    expect(document.body.textContent).toContain('Conclusions');
  });
});
