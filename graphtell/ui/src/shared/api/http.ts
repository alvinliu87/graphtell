/**
 * HTTP client.
 *
 * Single responsibility: only "how to request" and "where the request goes"; it
 * knows nothing about business semantics (those live in `entities/*\/api.ts`).
 *
 * Base URL resolution order:
 * 1. Tauri desktop: the in-process backend gets a kernel-assigned port, read via the `api_port` command
 * 2. Environment variable `VITE_API_BASE` (when deployed to another machine)
 * 3. `localStorage` override (for debugging)
 * 4. Default `http://127.0.0.1:5177`
 */

import { translate as t } from '@/shared/lib/i18n';
import { notify } from '@/shared/lib/notify';

export interface ApiResponse<T> {
  ok: boolean;
  data: T | null;
  error: string | null;
}

export class ApiError extends Error {
  constructor(message: string, readonly status: number) {
    super(message);
    this.name = 'ApiError';
  }
}

const DEFAULT_BASE = 'http://127.0.0.1:5177';

let base: string = DEFAULT_BASE;

/** Internal bridge exposed by Tauri v2 (avoids depending on @tauri-apps/api). */
interface TauriInternals {
  invoke?: (cmd: string, args?: unknown) => Promise<unknown>;
}

async function detectBase(): Promise<string> {
  const internals = (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
  if (internals?.invoke) {
    try {
      const info = (await internals.invoke('api_port')) as { base_url?: string; port?: number } | null;
      if (info?.base_url) return info.base_url;
      if (info?.port) return `http://127.0.0.1:${info.port}`;
    } catch {
      /* Not a Tauri environment or the command is not registered: fall through below. */
    }
  }
  const fromEnv = import.meta.env.VITE_API_BASE as string | undefined;
  // When deployed same-origin as the backend (e.g. Docker single port), `same-origin` makes the
  // frontend use relative paths so no host name is baked in at build time; empty/false falls back
  // to the default below or the localStorage override.
  if (fromEnv === 'same-origin') return '';
  if (fromEnv) return fromEnv;
  try {
    return localStorage.getItem('graphtell.api-base') ?? DEFAULT_BASE;
  } catch {
    return DEFAULT_BASE;
  }
}

/** Called once before app start to resolve the backend base URL. */
export async function initApiBase(): Promise<string> {
  base = await detectBase();
  return base;
}

export function apiBase(): string {
  return base;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  let res: Response;
  try {
    res = await fetch(`${base}${path}`, {
      ...init,
      headers: {
        'Content-Type': 'application/json',
        ...(init?.headers ?? {}),
      },
    });
  } catch (e) {
    // Transport-level failure (backend down / wrong port / network gone): unlike each page's own
    // domain errors, the user would otherwise see nothing at all, so raise one global notification.
    // The error is then rethrown so each page's useAsync decides whether to draw an inline Alert
    // (ProjectsPage / ExplorerPage do; anywhere else at least gets this toast as a fallback).
    notify(t('Cannot reach the backend. Please make sure the server is running.'));
    throw e;
  }
  if (!res.ok) {
    throw new ApiError(`HTTP ${res.status} ${res.statusText}`, res.status);
  }
  const body = (await res.json()) as ApiResponse<T>;
  if (!body.ok || body.data === null) {
    throw new ApiError(body.error ?? t('Unknown error'), res.status);
  }
  return body.data;
}

export const http = {
  get: <T>(path: string) => request<T>(path),
  post: <T>(path: string, payload?: unknown) =>
    request<T>(path, { method: 'POST', body: payload ? JSON.stringify(payload) : undefined }),
  put: <T>(path: string, payload?: unknown) =>
    request<T>(path, { method: 'PUT', body: payload ? JSON.stringify(payload) : undefined }),
  del: <T>(path: string) => request<T>(path, { method: 'DELETE' }),
};
