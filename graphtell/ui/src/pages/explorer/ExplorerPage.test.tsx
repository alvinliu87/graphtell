// @vitest-environment jsdom
import { describe, expect, it, vi } from 'vitest';

vi.mock('@/shared/api/http', async () => {
  const { createHttpMock } = await import('@/test/httpMock');
  return { http: createHttpMock() };
});

import { act } from 'react';
import { Routes, Route } from 'react-router-dom';
import { ExplorerPage } from './ExplorerPage';
import { mountWithRouter } from '@/test/render';

describe('ExplorerPage', () => {
  it('renders the node browser with its search controls', async () => {
    mountWithRouter(
      <Routes>
        <Route path="/projects/:projectId/explorer" element={<ExplorerPage />} />
      </Routes>,
      '/projects/1/explorer',
    );
    await act(async () => {
      await new Promise((r) => setTimeout(r, 60));
    });
    expect(document.body.textContent).toContain('Explorer');
    // The kind filter lists semantic node kinds.
    expect(document.body.textContent).toContain('Table');
  });
});
