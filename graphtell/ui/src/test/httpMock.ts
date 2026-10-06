// Shared `http` mock that returns sensible *empty* shapes per endpoint so page components
// render their empty/loading states without crashing during smoke tests.
// (Components read `body.data`-style structures; returning the wrong type — e.g. an object
// where an array is expected — would throw during render, so each endpoint gets a typed empty.)
import { vi } from 'vitest';

export function createHttpMock() {
  const get = (url: string): unknown => {
    if (url.includes('/violations')) return []; // Violation[]
    if (url.includes('/check/summary')) return { critical: 0, error: 0, warning: 0, info: 0 };
    if (url === '/api/rules') return []; // CheckRule[]
    if (url.includes('/rules/config')) return {}; // Record<string, ProjectRuleConfig>
    if (url.includes('/sub-projects')) return []; // SubProject[]
    if (url.includes('/perspectives')) return []; // Perspective[]
    if (url.includes('/nodes')) return []; // Node[]
    if (url.includes('/diagnostics/summary'))
      return { critical: 0, error: 0, warning: 0, info: 0, by_code: [], unsupported_languages: [] };
    if (url.includes('/diagnostics')) return []; // Diagnostic[]
    // Object / aggregate views: return null so the `view` stays undefined (empty graph).
    if (url.includes('/view/') || url.includes('/aggregate/')) return null;
    if (url.includes('/health')) return {};
    if (url.split('?')[0] === '/api/projects') return []; // project list
    return {};
  };

  return {
    get: vi.fn((u: string) => get(u)),
    post: vi.fn().mockResolvedValue({}),
    put: vi.fn().mockResolvedValue({}),
    del: vi.fn().mockResolvedValue({}),
  };
}
