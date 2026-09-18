import { http } from '@/shared/api/http';
import { applyWslAuto, getWslConfigured } from './ide';

/** 后端运行环境（含 WSL 探测），由 `/api/health` 提供。 */
export interface BackendEnv {
  isWsl: boolean;
  wslDistro: string;
  /** 访问前端的客户端操作系统；只有 Windows 才需要 WSL 的 UNC / 远程 scheme 映射。 */
  clientPlatform: 'windows' | 'linux' | 'mac' | 'unknown';
}

let cached: BackendEnv | null = null;

export function getCachedBackendEnv(): BackendEnv | null {
  return cached;
}

interface HealthDto {
  status: string;
  version: string;
  languages: string[];
  frameworks: number;
  is_wsl: boolean;
  wsl_distro: string;
}

function detectClientPlatform(): BackendEnv['clientPlatform'] {
  const ua = navigator.userAgent.toLowerCase();
  const platform =
    (navigator as unknown as { userAgentData?: { platform?: string } }).userAgentData
      ?.platform ?? navigator.platform ?? '';
  const p = String(platform).toLowerCase();
  if (p.includes('win') || ua.includes('win')) return 'windows';
  if (p.includes('mac') || ua.includes('mac')) return 'mac';
  if (p.includes('linux') || ua.includes('x11') || ua.includes('wayland')) return 'linux';
  return 'unknown';
}

/**
 * 拉取后端环境（含 WSL 探测），缓存一次。
 *
 * 仅当 WSL 设置尚未被用户手动配置过时，才把探测结果自动套用：
 * 后端在 WSL 且客户端是 Windows 时启用 WSL 映射，否则不启用。
 * 这样既不覆盖用户显式选择，也让常驻 WSL + Windows 的用户零配置即可用。
 */
export async function fetchBackendEnv(): Promise<BackendEnv> {
  const clientPlatform = detectClientPlatform();
  let isWsl = false;
  let wslDistro = 'Ubuntu';
  try {
    const h = await http.get<HealthDto>('/api/health');
    isWsl = h.is_wsl;
    if (h.wsl_distro && h.wsl_distro.trim()) wslDistro = h.wsl_distro.trim();
  } catch {
    /* 健康接口不可达：保守当作非 WSL */
  }
  cached = { isWsl, wslDistro, clientPlatform };
  if (!getWslConfigured()) {
    applyWslAuto(isWsl && clientPlatform === 'windows', wslDistro);
  }
  return cached;
}
