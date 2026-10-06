// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const ide = { getWslConfigured: vi.fn(), applyWslAuto: vi.fn() };
vi.mock('@/shared/lib/ide', () => ({
  getWslConfigured: (...a: unknown[]) => ide.getWslConfigured(...a),
  applyWslAuto: (...a: unknown[]) => ide.applyWslAuto(...a),
}));

vi.mock('@/shared/api/http', () => ({ http: { get: vi.fn() } }));

import { http } from '@/shared/api/http';
import { fetchBackendEnv, getCachedBackendEnv } from './backendEnv';

function setUserAgent(ua: string, platform: string) {
  Object.defineProperty(window.navigator, 'userAgent', { value: ua, configurable: true });
  Object.defineProperty(window.navigator, 'platform', { value: platform, configurable: true });
}

describe('backendEnv', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    ide.getWslConfigured.mockReturnValue(false);
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('detects a windows client from the user agent', async () => {
    setUserAgent('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32');
    vi.mocked(http.get).mockResolvedValue({ is_wsl: false, wsl_distro: '' });
    const env = await fetchBackendEnv();
    expect(env.clientPlatform).toBe('windows');
  });

  it('detects a mac client', async () => {
    setUserAgent('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15)', 'MacIntel');
    vi.mocked(http.get).mockResolvedValue({ is_wsl: false, wsl_distro: '' });
    const env = await fetchBackendEnv();
    expect(env.clientPlatform).toBe('mac');
  });

  it('detects a linux client', async () => {
    setUserAgent('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64');
    vi.mocked(http.get).mockResolvedValue({ is_wsl: false, wsl_distro: '' });
    const env = await fetchBackendEnv();
    expect(env.clientPlatform).toBe('linux');
  });

  it('caches the result and auto-applies WSL mapping on Windows + WSL backend (when not manually configured)', async () => {
    setUserAgent('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32');
    vi.mocked(http.get).mockResolvedValue({ is_wsl: true, wsl_distro: 'Debian' });

    const env = await fetchBackendEnv();

    expect(env.isWsl).toBe(true);
    expect(env.wslDistro).toBe('Debian');
    expect(ide.applyWslAuto).toHaveBeenCalledWith(true, 'Debian');
    expect(getCachedBackendEnv()?.clientPlatform).toBe('windows');
  });

  it('does not auto-apply WSL mapping when the user has configured it manually', async () => {
    setUserAgent('Mozilla/5.0 (Windows NT 10.0; Win64; x64)', 'Win32');
    ide.getWslConfigured.mockReturnValue(true);
    vi.mocked(http.get).mockResolvedValue({ is_wsl: true, wsl_distro: 'Ubuntu' });

    await fetchBackendEnv();
    expect(ide.applyWslAuto).not.toHaveBeenCalled();
  });

  it('conservatively treats the backend as non-WSL when health is unreachable', async () => {
    setUserAgent('Mozilla/5.0 (X11; Linux x86_64)', 'Linux x86_64');
    vi.mocked(http.get).mockRejectedValue(new Error('down'));

    const env = await fetchBackendEnv();
    expect(env.isWsl).toBe(false);
    expect(env.wslDistro).toBe('Ubuntu');
  });
});
