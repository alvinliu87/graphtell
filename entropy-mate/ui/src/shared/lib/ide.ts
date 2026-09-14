import { message } from 'antd';
import type { SourceLocation } from '@/entities/view';

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
export function ideUrl(target: IdeTarget, loc: SourceLocation, projectRoot?: string): string {
  const raw = absolutePath(loc.file, projectRoot);
  // scheme URL 里统一用正斜杠；Windows 反斜杠会被编码或误解。
  const file = raw.replace(/\\/g, '/');
  switch (target) {
    case 'vscode':
    case 'vscode-insiders':
    case 'cursor':
      // vscode URL: vscode://file/<path>，path 本身不能再带前导 '/'，否则成 'vscode://file//home/...'
      // 会被系统/IDE 解释为 UNC 路径 \\home\...，导致"Path does not exist"。
      const clean = file.startsWith('/') ? file.slice(1) : file;
      return `${target}://file/${clean}:${loc.line}`;
    default:
      // JetBrains 系：?line= 支持行号，symbol 追加在后面便于人工核对
      const symbol = loc.symbol ? `#${encodeURIComponent(loc.symbol)}` : '';
      return `jetbrains://${target}/navigate/reference?project=&path=${encodeURIComponent(
        file,
      )}:${loc.line}${symbol}`;
  }
}

function absolutePath(file: string, projectRoot?: string): string {
  if (file.startsWith('/') || /^[a-zA-Z]:[\\/]/.test(file)) return file;
  if (projectRoot) return `${projectRoot.replace(/[\\/]$/, '')}/${file}`;
  return `/${file}`;
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
): Promise<void> {
  const url = ideUrl(target, loc, projectRoot);
  try {
    // 用隐藏 iframe 触发 scheme，避免整页跳转（Tauri / 浏览器都适用）
    const iframe = document.createElement('iframe');
    iframe.style.display = 'none';
    iframe.src = url;
    document.body.appendChild(iframe);
    window.setTimeout(() => iframe.remove(), 1500);
    message.success(`已请求 ${IDE_LABEL[target]} 打开 ${loc.file}:${loc.line}`);
  } catch {
    await copyPath(loc, projectRoot);
  }
}

/** fallback：复制绝对 `path:line`，任何环境都能用（便于粘到终端 / go-to-file）。
 * 注意：不附加 symbol，因为用户复制后通常是 Ctrl+P / go-to-file 直接定位文件，
 * 括号里的符号会污染路径，导致 IDE 找不到。 */
export async function copyPath(loc: SourceLocation, projectRoot?: string): Promise<void> {
  const file = absolutePath(loc.file, projectRoot);
  const text = `${file}:${loc.line}`;
  try {
    await navigator.clipboard.writeText(text);
    message.success(`已复制 ${text}`);
  } catch {
    message.info(text);
  }
}

/** 一次性复制所有位置（绝对路径，每行一个），便于粘到终端 / IDE 的 go-to-file。
 * 不附加 symbol，理由同上。 */
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
    message.success(`已复制 ${locations.length} 处位置（绝对路径）`);
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

/** 行号漂移提示文案。 */
export function driftHint(loc: SourceLocation): string {
  return loc.symbol
    ? `若行号已漂移，请按符号 ${loc.symbol} 在该文件中重新定位`
    : '若行号已漂移，请按文件路径重新定位';
}
