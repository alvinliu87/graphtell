/** Presentation-layer formatting helpers (side-effect free, unit-testable). */

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

/** Take the last segment of an FQN for display. */
export function shortName(fqn: string | null | undefined): string {
  if (!fqn) return '-';
  const parts = fqn.split(/[\\/:]/);
  return parts[parts.length - 1] || fqn;
}

export function truncate(text: string, max = 60): string {
  const s = text == null ? '' : String(text);
  return s.length <= max ? s : `${s.slice(0, max - 1)}…`;
}

/**
 * Middle elision: keep head and tail, replace the middle with a single ellipsis — good for long
 * paths like `GET /v2/order/.../create`. Complements `truncate` (tail elision): the first half of a
 * path (verb + base path) and its last half (the leaf resource) usually carry the most information.
 *
 * `max` counts **visual width** (same as `units()` in `layout/types.ts`: CJK / full-width ≈ 1.8x a
 * Latin char), not character count. That way 40 Chinese characters (≈ 72 visual units) cannot burst
 * the pill, while a pure Latin name of 40 chars stays ≈ 40 wide — behaviour unchanged.
 */
const WIDE_CP = 0x2e7f;
function charUnits(s: string): number {
  let u = 0;
  for (const ch of s) u += (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
  return u;
}

export function truncateMiddle(text: string, max = 40): string {
  const s = text == null ? '' : String(text);
  if (charUnits(s) <= max) return s;
  const half = (max - 1) / 2; // half for the head, half for the tail, 1 unit reserved for the ellipsis
  let head = '';
  let hu = 0;
  for (const ch of s) {
    const cu = (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
    if (hu + cu > half) break;
    head += ch;
    hu += cu;
  }
  let tail = '';
  let tu = 0;
  for (let i = s.length - 1; i >= 0; i -= 1) {
    const ch = s[i];
    const cu = (ch.codePointAt(0) ?? 0) > WIDE_CP ? 1.8 : 1;
    if (tu + cu > half) break;
    tail = ch + tail;
    tu += cu;
  }
  return `${head}…${tail}`;
}
