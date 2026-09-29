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
  // 部署到与后端同域时（如 Docker 单端口），用 `same-origin` 让前端走相对路径，
  // 无需在构建期写死主机名；留空/false 时回落到下方的默认或 localStorage 覆盖。
  if (fromEnv === 'same-origin') return '';
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
    // 传输层失败（后端没起来 / 端口不对 / 网络断了）：和各页自己的领域错误不同，这里
    // 用户什么都看不到，所以走全局 notify 弹一次。再原样抛出，让各页的 useAsync 决定要不要
    // 画内联 Alert（ProjectsPage / ExplorerPage 会画；没画的地方至少也有这条 toast 兜底）。
    notify(t('无法连接后端，请确认服务已启动'));
    throw e;
  }
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
