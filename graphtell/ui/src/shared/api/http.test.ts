// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import { ApiError, apiBase, http, initApiBase } from './http';

const DEFAULT_BASE = 'http://127.0.0.1:5177';

describe('http client — base URL resolution', () => {
  beforeEach(() => {
    localStorage.clear();
    delete (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
    vi.unstubAllEnvs();
  });
  afterEach(() => {
    vi.unstubAllEnvs();
    vi.unstubAllGlobals();
  });

  it('prefers the Tauri-provided base_url', async () => {
    (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {
      invoke: vi.fn().mockResolvedValue({ base_url: 'http://tauri:1234' }),
    };
    const base = await initApiBase();
    expect(base).toBe('http://tauri:1234');
    expect(apiBase()).toBe('http://tauri:1234');
  });

  it('falls back to Tauri port when base_url is absent', async () => {
    (window as unknown as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ = {
      invoke: vi.fn().mockResolvedValue({ port: 9999 }),
    };
    await initApiBase();
    expect(apiBase()).toBe('http://127.0.0.1:9999');
  });

  it('treats VITE_API_BASE=same-origin as a relative (empty) base', async () => {
    vi.stubEnv('VITE_API_BASE', 'same-origin');
    await initApiBase();
    expect(apiBase()).toBe('');
  });

  it('uses an explicit VITE_API_BASE URL', async () => {
    vi.stubEnv('VITE_API_BASE', 'http://remote:8080');
    await initApiBase();
    expect(apiBase()).toBe('http://remote:8080');
  });

  it('falls back to a localStorage override', async () => {
    localStorage.setItem('graphtell.api-base', 'http://ls:7000');
    await initApiBase();
    expect(apiBase()).toBe('http://ls:7000');
  });

  it('defaults to the well-known localhost port when nothing else is configured', async () => {
    await initApiBase();
    expect(apiBase()).toBe(DEFAULT_BASE);
  });
});

describe('http client — request + response unwrapping', () => {
  beforeEach(async () => {
    localStorage.clear();
    localStorage.setItem('graphtell.api-base', 'http://test:5177');
    await initApiBase();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('returns data on a successful {ok,data} envelope', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      statusText: 'OK',
      json: async () => ({ ok: true, data: { hello: 'world' } }),
    });
    vi.stubGlobal('fetch', fetchMock);

    const data = await http.get<{ hello: string }>('/api/foo');
    expect(fetchMock).toHaveBeenCalledWith(
      'http://test:5177/api/foo',
      expect.objectContaining({ headers: { 'Content-Type': 'application/json' } }),
    );
    expect(data).toEqual({ hello: 'world' });
  });

  it('sends a JSON body on POST', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      statusText: 'OK',
      json: async () => ({ ok: true, data: 42 }),
    });
    vi.stubGlobal('fetch', fetchMock);

    const data = await http.post<number>('/api/bar', { a: 1 });
    const [url, init] = fetchMock.mock.calls[0] as [string, RequestInit];
    expect(url).toBe('http://test:5177/api/bar');
    expect(init.method).toBe('POST');
    expect(init.body).toBe(JSON.stringify({ a: 1 }));
    expect(data).toBe(42);
  });

  it('throws ApiError when the envelope reports ok:false', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      statusText: 'OK',
      json: async () => ({ ok: false, error: 'boom' }),
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(http.get('/api/x')).rejects.toMatchObject({ message: 'boom' });
  });

  it('throws ApiError on a non-2xx HTTP status', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: false,
      status: 404,
      statusText: 'Not Found',
      json: async () => ({ ok: false, error: null }),
    });
    vi.stubGlobal('fetch', fetchMock);

    const err = await http.get('/api/missing').catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(404);
  });

  it('rethrows (and surfaces a transport error) when fetch itself fails', async () => {
    const fetchMock = vi.fn().mockRejectedValue(new Error('network down'));
    vi.stubGlobal('fetch', fetchMock);

    await expect(http.get('/api/y')).rejects.toThrow('network down');
  });
});
