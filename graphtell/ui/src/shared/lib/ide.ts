import { message } from 'antd';
import type { SourceLocation } from '@/entities/view';
import { translate as t } from '@/shared/lib/i18n';

/**
 * Jumping to an IDE.
 *
 * Design points:
 * * Locations use the **path + symbol + line triple** — line numbers drift, symbols do not
 * * `vscode://` / JetBrains schemes are only the *preferred* route; **copying the path must
 *   exist as a fallback** (CI / web / no IDE / remote container cannot open a scheme)
 * * Sensitive nodes (`SecretLocation`) may only jump to the **key name** position, never
 *   display the value — given how open source code is, an analysis tool must not become a leak
 */

export type IdeTarget = 'vscode' | 'vscode-insiders' | 'idea' | 'webstorm' | 'phpstorm' | 'cursor';

export const IDE_LABEL: Record<IdeTarget, string> = {
  vscode: 'VS Code',
  'vscode-insiders': 'VS Code Insiders',
  idea: 'IntelliJ IDEA',
  webstorm: 'WebStorm',
  phpstorm: 'PhpStorm',
  cursor: 'Cursor',
};

/** Build an IDE URL (relative path + line number only; never exposes a value). */
export function ideUrl(
  target: IdeTarget,
  loc: SourceLocation,
  projectRoot?: string,
  wslDistro?: string,
): string {
  const raw = absolutePath(loc.file, projectRoot);
  // Scheme URLs always use forward slashes; Windows backslashes get encoded or misread.
  const file = raw.replace(/\\/g, '/');
  // WSL mode: VS Code / Cursor must use their dedicated remote scheme —
  // vscode://file/ with a \\wsl$\ prefix does not open under WSL.
  if (wslDistro && (target === 'vscode' || target === 'vscode-insiders' || target === 'cursor')) {
    return `vscode://vscode-remote/wsl+${wslDistro}${file}:${loc.line}`;
  }
  switch (target) {
    case 'vscode':
    case 'vscode-insiders':
    case 'cursor':
      // vscode URL: vscode://file/<path>; the path itself must not carry a leading '/', otherwise it
      // becomes 'vscode://file//home/...' which the OS/IDE reads as the UNC path \\home\... and
      // reports "Path does not exist".
      const clean = file.startsWith('/') ? file.slice(1) : file;
      return `${target}://file/${clean}:${loc.line}`;
    default:
      // Under WSL mode, prepend the \\wsl$\<distro> UNC prefix for JetBrains so it can locate WSL files.
      const jbFile = wslDistro ? `\\\\wsl$\\${wslDistro}${file}` : file;
      // JetBrains family: ?line= carries the line number; the symbol is appended for manual checking.
      const symbol = loc.symbol ? `#${encodeURIComponent(loc.symbol)}` : '';
      return `jetbrains://${target}/navigate/reference?project=&path=${encodeURIComponent(
        jbFile,
      )}:${loc.line}${symbol}`;
  }
}

export function absolutePath(file: string, projectRoot?: string): string {
  if (file.startsWith('/') || /^[a-zA-Z]:[\\/]/.test(file)) return file;
  if (projectRoot) return `${projectRoot.replace(/[\\/]$/, '')}/${file}`;
  return `/${file}`;
}

// ---- Project root resolution: a global "root template" plus a per-project "override" ----
// The generic tool embeds no assumption about a specific scenario (WSL / Docker / remote):
// the user describes the generic "backend root → local root" transform with a template, where
// {root} is a placeholder for the backend root_path. The per-project override has the highest
// priority and covers cases the template cannot express (e.g. a completely different local drive/dir).

export const ROOT_TEMPLATE_KEY = 'em.rootTemplate';

/** Global root template (with the {root} placeholder); an empty string means disabled. */
export function getRootTemplate(): string {
  try {
    return localStorage.getItem(ROOT_TEMPLATE_KEY) ?? '';
  } catch {
    return '';
  }
}

/** Save the global root template; passing an empty string clears it. */
export function setRootTemplate(template: string): void {
  try {
    const t = template.trim();
    if (t) localStorage.setItem(ROOT_TEMPLATE_KEY, t);
    else localStorage.removeItem(ROOT_TEMPLATE_KEY);
  } catch {
    /* ignore */
  }
}

// ---- WSL quick preset ----
// The generic tool embeds no WSL assumption; this only makes WSL a "one-click preset": when enabled
// {root} (the backend Linux path) is used as the root and the distro is passed through to ideUrl /
// the copy logic, which generate the correct VS Code remote scheme and JetBrains UNC prefix.
// Note: the distro is mandatory — \\wsl$\Ubuntu and \\wsl$\Debian are different mount points.

export const WSL_MODE_KEY = 'em.wslMode';
export const WSL_DISTRO_KEY = 'em.wslDistro';
export const WSL_CONFIGURED_KEY = 'em.wslConfigured';

/** Whether WSL mode is on. */
export function getWslMode(): boolean {
  try {
    return localStorage.getItem(WSL_MODE_KEY) === '1';
  } catch {
    return false;
  }
}

/** Whether the user has configured WSL settings manually (manual config wins over backend auto-detection). */
export function getWslConfigured(): boolean {
  try {
    return localStorage.getItem(WSL_CONFIGURED_KEY) === '1';
  } catch {
    return false;
  }
}

/** Save the WSL switch ('1' = on, cleared = off) and mark it as manually configured. */
export function setWslMode(on: boolean): void {
  try {
    if (on) localStorage.setItem(WSL_MODE_KEY, '1');
    else localStorage.removeItem(WSL_MODE_KEY);
    localStorage.setItem(WSL_CONFIGURED_KEY, '1');
  } catch {
    /* ignore */
  }
}

/** WSL distro name (defaults to Ubuntu). */
export function getWslDistro(): string {
  try {
    return localStorage.getItem(WSL_DISTRO_KEY) ?? 'Ubuntu';
  } catch {
    return 'Ubuntu';
  }
}

/** Save the WSL distro name and mark it as manually configured. */
export function setWslDistro(distro: string): void {
  try {
    localStorage.setItem(WSL_DISTRO_KEY, distro.trim() || 'Ubuntu');
    localStorage.setItem(WSL_CONFIGURED_KEY, '1');
  } catch {
    /* ignore */
  }
}

/**
 * Apply WSL settings automatically from the backend environment; does *not* mark them as
 * "manually configured", so a later backend change can be re-detected on the next start.
 */
export function applyWslAuto(on: boolean, distro: string): void {
  try {
    if (on) {
      localStorage.setItem(WSL_MODE_KEY, '1');
      localStorage.setItem(WSL_DISTRO_KEY, distro.trim() || 'Ubuntu');
    } else {
      localStorage.removeItem(WSL_MODE_KEY);
    }
  } catch {
    /* ignore */
  }
}

/**
 * The root template actually in effect: `{root}` when WSL mode is on (i.e. the backend Linux path,
 * with the WSL transform itself handled by ideUrl / the copy logic), otherwise the user's generic
 * template. A per-project override still outranks this result.
 */
export function effectiveTemplate(): string {
  return getWslMode() ? '{root}' : getRootTemplate();
}

/**
 * Resolve the local project root finally used for IDE jumps / copying.
 * Priority: per-project override > global root template ({root} substitution on the backend root) >
 * backend root_path.
 */
export function resolveProjectRoot(
  backendRoot: string | undefined,
  override?: string,
  template?: string,
): string | undefined {
  if (override && override.trim()) return override.trim();
  const tpl = (template ?? '').trim();
  if (tpl) return backendRoot ? tpl.replace(/\{root\}/g, backendRoot) : tpl;
  return backendRoot || undefined;
}

const IDE_STORAGE_KEY = 'em.preferredIde';

/** The user's preferred IDE (persisted in localStorage, defaults to VS Code). Used when jumping to a path. */
export function preferredIde(): IdeTarget {
  try {
    const saved = localStorage.getItem(IDE_STORAGE_KEY);
    if (saved && saved in IDE_LABEL) return saved as IdeTarget;
  } catch {
    /* localStorage unavailable (privacy mode / SSR): degrade silently */
  }
  return 'vscode';
}

/** Remember the IDE the user picked most recently and use it directly next time. */
export function setPreferredIde(target: IdeTarget): void {
  try {
    localStorage.setItem(IDE_STORAGE_KEY, target);
  } catch {
    /* ignore */
  }
}

/** Open in the IDE (falls back to copying the path on failure). */
export async function openInIde(
  target: IdeTarget,
  loc: SourceLocation,
  projectRoot?: string,
  wslDistro?: string,
): Promise<void> {
  const url = ideUrl(target, loc, projectRoot, wslDistro);
  try {
    // Trigger the scheme through a hidden iframe so the whole page does not navigate (works in Tauri and browsers).
    const iframe = document.createElement('iframe');
    iframe.style.display = 'none';
    iframe.src = url;
    document.body.appendChild(iframe);
    window.setTimeout(() => iframe.remove(), 1500);
    message.success(t('Requested ') + IDE_LABEL[target] + t(' to open ') + `${loc.file}:${loc.line}`);
  } catch {
    await copyPath(loc, projectRoot);
  }
}

/** Fallback: copy the absolute `path:line`, usable in any environment (easy to paste into a terminal / go-to-file).
 * No WSL / remote prefix — what is copied is the real in-repo relative path joined with the local root, and the
 * user decides what to do with it. Note: no symbol is appended, because after copying the user usually goes
 * straight to the file via Ctrl+P / go-to-file, and a symbol in parentheses would pollute the path so the IDE
 * cannot find it. */
export async function copyPath(
  loc: SourceLocation,
  projectRoot?: string,
): Promise<void> {
  const file = absolutePath(loc.file, projectRoot);
  const text = `${file}:${loc.line}`;
  try {
    await navigator.clipboard.writeText(text);
    message.success(t('Copied ') + text);
  } catch {
    message.info(text);
  }
}

/** Copy all locations at once (absolute paths, one per line) for pasting into a terminal / IDE go-to-file.
 * No symbol, for the same reason; likewise no WSL / remote prefix. */
export async function copyAllLocations(
  locations: SourceLocation[],
  projectRoot?: string,
  extra?: string,
): Promise<void> {
  const lines = locations.map((loc) => {
    const file = absolutePath(loc.file, projectRoot);
    return `${file}:${loc.line}`;
  });
  if (extra) lines.push(`— ${extra}`);
  const text = lines.join('\n');
  try {
    await navigator.clipboard.writeText(text);
    message.success(t('Copied ') + locations.length + t(' locations (absolute paths)'));
  } catch {
    message.info(text);
  }
}

/**
 * Whether a location is "key name only" sensitive.
 * When it matches, no line content is returned — only the position of the key.
 */
export function isSensitive(kind: string): boolean {
  return kind === 'SecretLocation';
}
