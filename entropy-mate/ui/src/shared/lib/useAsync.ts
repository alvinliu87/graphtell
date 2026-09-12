import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * 极简异步数据 hook。
 *
 * 不引入 react-query 等依赖：本项目的数据量小、刷新频率低，
 * 一个带 `reload` 的 hook 足够，也避免为了「状态管理」而增加心智负担。
 */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const run = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const result = await fn();
      if (mounted.current) setData(result);
      return result;
    } catch (e) {
      if (mounted.current) setError(e instanceof Error ? e.message : String(e));
      return null;
    } finally {
      if (mounted.current) setLoading(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(() => {
    void run();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

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
