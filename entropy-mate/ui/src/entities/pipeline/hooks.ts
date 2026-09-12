import { useCallback, useEffect, useRef, useState } from 'react';
import { useAsync } from '@/shared/lib/useAsync';
import { pipelineApi } from './api';
import type { RunStatus } from './model';

/**
 * 建图进度：状态为 `indexing` 时轮询，结束后自动停止。
 */
export function useRunStatus(projectId: number | undefined, status?: string) {
  const [snapshot, setSnapshot] = useState<RunStatus | null>(null);
  const timer = useRef<number | null>(null);

  const fetchOnce = useCallback(async () => {
    if (projectId === undefined) return;
    try {
      const s = await pipelineApi.status(projectId);
      setSnapshot(s);
    } catch {
      /* 忽略轮询错误，避免打断 UI */
    }
  }, [projectId]);

  useEffect(() => {
    if (projectId === undefined) return;
    void fetchOnce();
    if (status !== 'indexing') {
      if (timer.current) window.clearInterval(timer.current);
      return;
    }
    timer.current = window.setInterval(() => void fetchOnce(), 800);
    return () => {
      if (timer.current) window.clearInterval(timer.current);
    };
  }, [projectId, status, fetchOnce]);

  return { run: snapshot, refresh: fetchOnce };
}

export function useHealth() {
  const { data, loading, error } = useAsync(() => pipelineApi.health(), []);
  return { health: data, loading, error };
}
