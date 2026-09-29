/**
 * 极简全局通知桥。
 *
 * 模块级的 `notify()` 让非 React 层的代码（如 `shared/api/http.ts` 的 fetch 封装）
 * 也能弹出 antd 的 `message` —— 但 antd 的 `message` 必须来自 `<App>` 上下文
 * （`App.useApp()`），而 fetch 封装是纯函数、拿不到 hook。所以这里用"注册回调"解耦：
 * `App` 挂载后在桥组件里把 `message.error` 注册进来，卸载时撤掉。
 *
 * 为什么只报"传输层失败"：HTTP 错误状态码（4xx/5xx）是领域错误，由各页自己的
 * `useAsync.error` 走内联 `Alert` 呈现；这里只兜底"后端根本连不上"（fetch 抛网络错误），
 * 避免和页面内联告警重复刷屏。
 */
type Notifier = (msg: string) => void;

let notifier: Notifier | null = null;
let lastAt = 0;

/** `App` 桥组件在挂载时注册、卸载时清空。 */
export function setNotifier(fn: Notifier | null) {
  notifier = fn;
}

/** 弹出一条全局错误提示（5 秒内同类不重复，防止后端宕机时瞬间刷一堆）。 */
export function notify(msg: string) {
  const now = Date.now();
  if (now - lastAt < 5000) return;
  lastAt = now;
  notifier?.(msg);
}
