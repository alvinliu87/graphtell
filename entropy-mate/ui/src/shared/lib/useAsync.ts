import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * 极简异步数据 hook。
 *
 * 不引入 react-query 等依赖：本项目的数据量小、刷新频率低，
 * 一个带 `reload` 的 hook 足够，也避免为了「状态管理」而增加心智负担。
 */
function depsEqual(a: unknown[], b: unknown[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (!Object.is(a[i], b[i])) return false;
  }
  return true;
}

export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fetching, setFetching] = useState(false);
  const mounted = useRef(true);
  // 最近一次「成功 fetch」所对应的 deps。loading 据此 + fetching 派生，
  // 完全不依赖「渲染期 setState」的时序，从根本上杜绝首屏 / 切换视角时的空态闪烁。
  const fetchedDeps = useRef<unknown[] | null>(null);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const run = useCallback(async () => {
    setFetching(true);
    setError(null);
    try {
      const result = await fn();
      if (mounted.current) setData(result);
      return result;
    } catch (e) {
      if (mounted.current) setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      if (mounted.current) {
        // 成败都标记本轮 deps 已结算：成功→数据就绪；失败→停止 spinner 并展示 error，避免永久转圈
        fetchedDeps.current = deps;
        setFetching(false);
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(() => {
    void run();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [run]);

  // 首帧（fetchedDeps 为 null）或依赖已变 → 数据尚未就绪 → loading 为 true（spinner）；
  // 与 fetching（主动拉取中）取或。同步派生，第一帧即为 true，绝不先露空态。
  const ready = fetchedDeps.current !== null && depsEqual(fetchedDeps.current, deps);
  const loading = !ready || fetching;

  return { data, loading, error, reload: run };
}

/** 轮询：用于建图进度。 */
export function usePolling(fn: () => Promise<unknown>, intervalMs: number, active: boolean) {
  const saved = useRef(fn);
  useEffect(() => {
    saved.current = fn;
  }, [fn]);

  useEffect(() => {
    if (!active) return;
    const id = window.setInterval(() => {
      void saved.current();
    }, intervalMs);
    void saved.current();
    return () => window.clearInterval(id);
  }, [intervalMs, active]);
}
