import { http } from '@/shared/api/http';
import type { RecallQuery, RecallResult } from './model';

/** 代码召回的数据访问。 */
export const recallApi = {
  recall: (projectId: number, q: RecallQuery) =>
    http.post<RecallResult>(`/api/projects/${projectId}/recall`, q),
};
