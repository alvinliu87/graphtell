import { http } from '@/shared/api/http';
import type { DirEntry } from './model';

/** Filesystem browsing: list the sub-directories of a path (for the directory picker). */
export const fsApi = {
  browse: (path: string) =>
    http.get<DirEntry[]>(`/api/fs/browse?path=${encodeURIComponent(path)}`),
};
