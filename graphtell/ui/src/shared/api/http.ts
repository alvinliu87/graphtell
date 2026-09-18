/**
 * HTTP 客户端。
 *
 * 单一职责：只负责「怎么请求」与「请求打到哪」，不关心业务语义
 * （业务在 `entities/*\/api.ts`）。
 *
 * 基地址解析顺序：
 * 1. Tauri 桌面端：进程内后端由内核分配端口，通过 `api_port` 命令获取
 * 2. 环境变量 `VITE_API_BASE`（部署到别的机器时）
 * 3. `localStorage` 覆盖（调试用）
 * 4. 默认 `http://127.0.0.1:5177`
 */

import { translate as t } from '@/shared/lib/i18n';

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

/** Tauri v2 暴露的内部桥接（无需引入 @tauri-apps/api 依赖）。 */
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
      /* 非 Tauri 环境或命令未注册：走下面的兜底 */
    }
  }
  const fromEnv = import.meta.env.VITE_API_BASE as string | undefined;
  if (fromEnv) return fromEnv;
  try {
    return localStorage.getItem('graphtell.api-base') ?? DEFAULT_BASE;
  } catch {
    return DEFAULT_BASE;
  }
}

/** 应用启动前调用一次，确定后端基地址。 */
export async function initApiBase(): Promise<string> {
  base = await detectBase();
  return base;
}

export function apiBase(): string {
  return base;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`${base}${path}`, {
    ...init,
    headers: {
      'Content-Type': 'application/json',
      ...(init?.headers ?? {}),
    },
  });
  if (!res.ok) {
    throw new ApiError(`HTTP ${res.status} ${res.statusText}`, res.status);
  }
  const body = (await res.json()) as ApiResponse<T>;
  if (!body.ok || body.data === null) {
    throw new ApiError(body.error ?? t('未知错误'), res.status);
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
