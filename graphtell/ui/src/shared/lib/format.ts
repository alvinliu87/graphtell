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

/**
 * 中间省略：保留头尾，中间以单个省略号代替，适合 `GET /v2/order/.../create` 这类长路径。
 * 与 `truncate`（尾部省略）互补：路径前半（动词 + 基路径）和后半（末级资源）往往最有信息量。
 *
 * `max` 按**视觉宽度**计（与 `layout/types.ts` 的 `units()` 一致：CJK / 全角 ≈ 拉丁的 1.8 倍宽），
 * 而非字符数。这样 40 个汉字（≈ 72 视觉位）不会把药丸顶破——纯拉丁名 40 字符 ≈ 40 宽，行为不变。
 */
const WIDE_CP = 0x2e7f;
function charUnits(s: string): number {
  let u = 0;
  for (const ch of s) u += (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
  return u;
}

export function truncateMiddle(text: string, max = 40): string {
  if (charUnits(text) <= max) return text;
  const half = (max - 1) / 2; // 头尾各半，中间留 1 位给省略号
  let head = '';
  let hu = 0;
  for (const ch of text) {
    const cu = (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
    if (hu + cu > half) break;
    head += ch;
    hu += cu;
  }
  let tail = '';
  let tu = 0;
  for (let i = text.length - 1; i >= 0; i -= 1) {
    const ch = text[i];
    const cu = (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
    if (tu + cu > half) break;
    tail = ch + tail;
    tu += cu;
  }
  return `${head}…${tail}`;
}
