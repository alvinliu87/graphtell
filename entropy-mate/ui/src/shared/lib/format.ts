/** 展示层格式化工具（无副作用、可单元测试）。 */

export function formatNumber(n: number): string {
  if (!Number.isFinite(n)) return '-';
  if (Math.abs(n) >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (Math.abs(n) >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1000);
  return `${m}m${s}s`;
}

export function formatTime(epochMillis: number): string {
  if (!epochMillis) return '-';
  return new Date(epochMillis).toLocaleString('zh-CN', { hour12: false });
}

/** 把 FQN 的最后一段取出来做展示。 */
export function shortName(fqn: string | null | undefined): string {
  if (!fqn) return '-';
  const parts = fqn.split(/[\\/:]/);
  return parts[parts.length - 1] || fqn;
}

export function truncate(text: string, max = 60): string {
  return text.length <= max ? text : `${text.slice(0, max - 1)}…`;
}
