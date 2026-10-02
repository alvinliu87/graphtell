/**
 * Minimal global notification bridge.
 *
 * A module-level `notify()` lets non-React code (such as the fetch wrapper in
 * `shared/api/http.ts`) raise antd's `message` — but antd's `message` must come
 * from the `<App>` context (`App.useApp()`), and the fetch wrapper is a pure
 * function with no access to hooks. So this decouples the two with a registered
 * callback: `App` registers `message.error` from the bridge component on mount
 * and clears it on unmount.
 *
 * Why it only reports "transport-level failure": HTTP error statuses (4xx/5xx)
 * are domain errors, rendered by each page's own `useAsync.error` as an inline
 * `Alert`; this only covers "the backend is unreachable at all" (fetch threw a
 * network error), so it does not flood the screen alongside the inline alerts.
 */
type Notifier = (msg: string) => void;

let notifier: Notifier | null = null;
let lastAt = 0;

/** Registered by the `App` bridge component on mount, cleared on unmount. */
export function setNotifier(fn: Notifier | null) {
  notifier = fn;
}

/** Raise one global error toast (deduplicated within 5s so a down backend cannot spam the screen). */
export function notify(msg: string) {
  const now = Date.now();
  if (now - lastAt < 5000) return;
  lastAt = now;
  notifier?.(msg);
}
