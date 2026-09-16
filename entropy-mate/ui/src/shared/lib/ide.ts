import { message } from 'antd';
import type { SourceLocation } from '@/entities/view';
import { translate as t } from '@/shared/lib/i18n';

/**
 * 跳转 IDE。
 *
 * 设计要点：
 * * 定位用 **path + symbol + line 三元组** —— 行号会漂移，符号不会
 * * `vscode://` / JetBrains scheme 只是"首选"，**必须配复制路径作为 fallback**
 *   （CI / Web / 无 IDE / 远程容器场景都打不开 scheme）
 * * 敏感节点（`SecretLocation`）只允许跳到**键名**位置，绝不显示值 ——
 *   在"源码高度开源"的前提下，分析工具不能成为泄露源
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

/** 构造 IDE URL（只用相对路径 + 行号，不暴露任何值）。 */
export function ideUrl(
  target: IdeTarget,
  loc: SourceLocation,
  projectRoot?: string,
  wslDistro?: string,
): string {
  const raw = absolutePath(loc.file, projectRoot);
  // scheme URL 里统一用正斜杠；Windows 反斜杠会被编码或误解。
  const file = raw.replace(/\\/g, '/');
  // WSL 模式：VS Code / Cursor 必须用专属远程 scheme 才能打开——
  // vscode://file/ 配 \\wsl$\ 前缀在 WSL 场景下打不开。
  if (wslDistro && (target === 'vscode' || target === 'vscode-insiders' || target === 'cursor')) {
    return `vscode://vscode-remote/wsl+${wslDistro}${file}:${loc.line}`;
  }
  switch (target) {
    case 'vscode':
    case 'vscode-insiders':
    case 'cursor':
      // vscode URL: vscode://file/<path>，path 本身不能再带前导 '/'，否则成 'vscode://file//home/...'
      // 会被系统/IDE 解释为 UNC 路径 \\home\...，导致"Path does not exist"。
      const clean = file.startsWith('/') ? file.slice(1) : file;
      return `${target}://file/${clean}:${loc.line}`;
    default:
      // WSL 模式下给 JetBrains 补 \\wsl$\<distro> UNC 前缀，使其能定位 WSL 文件。
      const jbFile = wslDistro ? `\\\\wsl$\\${wslDistro}${file}` : file;
      // JetBrains 系：?line= 支持行号，symbol 追加在后面便于人工核对
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

// ---- 工程根解析：全局「根模板」+ 按工程「覆盖」 ----
// 通用工具不内嵌任何特定场景（WSL / Docker / 远程）的假设：
// 用户用根模板描述"后端根 → 本地根"的通用变换，{root} 占位后端的 root_path；
// 按工程覆盖优先级最高，用于模板表达不了的特例（如本地与后端完全不同的盘符/目录）。

export const ROOT_TEMPLATE_KEY = 'em.rootTemplate';

/** 全局根模板（含 {root} 占位符），空串表示不启用。 */
export function getRootTemplate(): string {
  try {
    return localStorage.getItem(ROOT_TEMPLATE_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 保存全局根模板；传空串即清除。 */
export function setRootTemplate(template: string): void {
  try {
    const t = template.trim();
    if (t) localStorage.setItem(ROOT_TEMPLATE_KEY, t);
    else localStorage.removeItem(ROOT_TEMPLATE_KEY);
  } catch {
    /* ignore */
  }
}

// ---- WSL 快捷预设 ----
// 通用工具不内嵌 WSL 假设；这里只把它做成"一键预设"：开启时用 {root}（后端 Linux 路径）作根，
// 并把 distro 透传给 ideUrl / 复制逻辑，由它们生成正确的 VS Code 远程 scheme 与 JetBrains UNC 前缀。
// 注意：distro 不可省——\\wsl$\Ubuntu 与 \\wsl$\Debian 是不同的挂载点。

export const WSL_MODE_KEY = 'em.wslMode';
export const WSL_DISTRO_KEY = 'em.wslDistro';
export const WSL_CONFIGURED_KEY = 'em.wslConfigured';

/** 是否开启 WSL 模式。 */
export function getWslMode(): boolean {
  try {
    return localStorage.getItem(WSL_MODE_KEY) === '1';
  } catch {
    return false;
  }
}

/** 用户是否已手动配置过 WSL 设置（手动配置优先于后端自动探测）。 */
export function getWslConfigured(): boolean {
  try {
    return localStorage.getItem(WSL_CONFIGURED_KEY) === '1';
  } catch {
    return false;
  }
}

/** 保存 WSL 开关（'1' 表示开，清除即关），并标记为已手动配置。 */
export function setWslMode(on: boolean): void {
  try {
    if (on) localStorage.setItem(WSL_MODE_KEY, '1');
    else localStorage.removeItem(WSL_MODE_KEY);
    localStorage.setItem(WSL_CONFIGURED_KEY, '1');
  } catch {
    /* ignore */
  }
}

/** WSL 发行版名（默认 Ubuntu）。 */
export function getWslDistro(): string {
  try {
    return localStorage.getItem(WSL_DISTRO_KEY) ?? 'Ubuntu';
  } catch {
    return 'Ubuntu';
  }
}

/** 保存 WSL 发行版名，并标记为已手动配置。 */
export function setWslDistro(distro: string): void {
  try {
    localStorage.setItem(WSL_DISTRO_KEY, distro.trim() || 'Ubuntu');
    localStorage.setItem(WSL_CONFIGURED_KEY, '1');
  } catch {
    /* ignore */
  }
}

/**
 * 由后端环境自动套用 WSL 设置；不标记为"已手动配置"，
 * 以便后端环境变化时下次启动可重新探测。
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
 * 实际生效的根模板：WSL 开启时为 `{root}`（即后端 Linux 路径，具体 WSL 变换交给 ideUrl / 复制逻辑），
 * 否则用用户填的通用模板。按工程覆盖仍高于此结果。
 */
export function effectiveTemplate(): string {
  return getWslMode() ? '{root}' : getRootTemplate();
}

/**
 * 解析最终用于 IDE 跳转 / 复制的本地工程根。
 * 优先级：按工程覆盖 > 全局根模板（对后端根做 {root} 替换） > 后端 root_path。
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

/** 用户偏好的 IDE（持久化在 localStorage，默认 VS Code）。点路径跳转时用它。 */
export function preferredIde(): IdeTarget {
  try {
    const saved = localStorage.getItem(IDE_STORAGE_KEY);
    if (saved && saved in IDE_LABEL) return saved as IdeTarget;
  } catch {
    /* localStorage 不可用（隐私模式 / SSR）时静默降级 */
  }
  return 'vscode';
}

/** 记住用户最近选择的 IDE，下次直接用它。 */
export function setPreferredIde(target: IdeTarget): void {
  try {
    localStorage.setItem(IDE_STORAGE_KEY, target);
  } catch {
    /* ignore */
  }
}

/** 打开（失败时自动降级为复制路径）。 */
export async function openInIde(
  target: IdeTarget,
  loc: SourceLocation,
  projectRoot?: string,
  wslDistro?: string,
): Promise<void> {
  const url = ideUrl(target, loc, projectRoot, wslDistro);
  try {
    // 用隐藏 iframe 触发 scheme，避免整页跳转（Tauri / 浏览器都适用）
    const iframe = document.createElement('iframe');
    iframe.style.display = 'none';
    iframe.src = url;
    document.body.appendChild(iframe);
    window.setTimeout(() => iframe.remove(), 1500);
    message.success(t('已请求 ') + IDE_LABEL[target] + t(' 打开 ') + `${loc.file}:${loc.line}`);
  } catch {
    await copyPath(loc, projectRoot);
  }
}

/** fallback：复制绝对 `path:line`，任何环境都能用（便于粘到终端 / go-to-file）。
 * 不带 WSL / 远程前缀——复制的就是仓库内的真实相对路径拼上本地根，用户自行决定怎么用。
 * 注意：不附加 symbol，因为用户复制后通常是 Ctrl+P / go-to-file 直接定位文件，
 * 括号里的符号会污染路径，导致 IDE 找不到。 */
export async function copyPath(
  loc: SourceLocation,
  projectRoot?: string,
): Promise<void> {
  const file = absolutePath(loc.file, projectRoot);
  const text = `${file}:${loc.line}`;
  try {
    await navigator.clipboard.writeText(text);
    message.success(t('已复制 ') + text);
  } catch {
    message.info(text);
  }
}

/** 一次性复制所有位置（绝对路径，每行一个），便于粘到终端 / IDE 的 go-to-file。
 * 不附加 symbol，理由同上；同样不带 WSL / 远程前缀。 */
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
    message.success(t('已复制 ') + locations.length + t(' 处位置（绝对路径）'));
  } catch {
    message.info(text);
  }
}

/**
 * 判断是否为"只应暴露键名"的敏感位置。
 * 命中时不返回行内容，只返回 key 的位置。
 */
export function isSensitive(kind: string): boolean {
  return kind === 'SecretLocation';
}

