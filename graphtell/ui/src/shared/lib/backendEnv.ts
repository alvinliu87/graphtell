import { http } from '@/shared/api/http';
import { applyWslAuto, getWslConfigured } from './ide';

/** Backend runtime environment (including WSL detection), provided by `/api/health`. */
export interface BackendEnv {
  isWsl: boolean;
  wslDistro: string;
  /** Operating system of the client visiting the frontend; only Windows needs the WSL UNC / remote scheme mapping. */
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
 * Fetch the backend environment (including WSL detection) and cache it once.
 *
 * The detection result is auto-applied only when WSL settings have not been configured manually:
 * WSL mapping is enabled when the backend runs in WSL and the client is Windows, otherwise not.
 * That never overrides an explicit user choice, while users on WSL + Windows work with zero setup.
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
    /* Health endpoint unreachable: conservatively treat as non-WSL. */
  }
  cached = { isWsl, wslDistro, clientPlatform };
  if (!getWslConfigured()) {
    applyWslAuto(isWsl && clientPlatform === 'windows', wslDistro);
  }
  return cached;
}
