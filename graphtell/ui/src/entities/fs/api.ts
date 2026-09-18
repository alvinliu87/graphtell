import { http } from '@/shared/api/http';
import type { DirEntry } from './model';

/** 文件系统浏览：列出某路径下的子目录（供目录选择器使用）。 */
export const fsApi = {
  browse: (path: string) =>
    http.get<DirEntry[]>(`/api/fs/browse?path=${encodeURIComponent(path)}`),
};
