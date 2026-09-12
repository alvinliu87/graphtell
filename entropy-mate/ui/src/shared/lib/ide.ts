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
  const file = absolutePath(loc.file, projectRoot);
  switch (target) {
    case 'vscode':
      return `vscode://file/${file}:${loc.line}`;
    case 'vscode-insiders':
      return `vscode-insiders://file/${file}:${loc.line}`;
    case 'cursor':
      return `cursor://file/${file}:${loc.line}`;
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
  if (projectRoot) return `${projectRoot.replace(/\/$/, '')}/${file}`;
  return `/${file}`;
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

/** fallback：复制 `path:line`，任何环境都能用。 */
export async function copyPath(loc: SourceLocation, projectRoot?: string): Promise<void> {
  const text = `${loc.file}:${loc.line}${loc.symbol ? ` (${loc.symbol})` : ''}`;
  try {
    await navigator.clipboard.writeText(text);
    message.success(`已复制 ${text}`);
  } catch {
    message.info(text);
  }
}

/** 复制完整可粘贴的引用（供 issue / 报告内嵌）。 */
export async function copyReference(
  loc: SourceLocation,
  extra?: string,
): Promise<void> {
  const text = `${loc.file}:${loc.line}${extra ? ` — ${extra}` : ''}`;
  try {
    await navigator.clipboard.writeText(text);
    message.success(`已复制：${text}`);
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
