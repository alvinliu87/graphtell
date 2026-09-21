import { http } from '@/shared/api/http';
import type { CheckReport, CheckRule, CheckSummary, Violation } from './model';

/** 合规检查的数据访问。 */
export const checkApi = {
  /** 全部已装载规则（供页面展示"能检查什么"）。 */
  rules: () => http.get<CheckRule[]>('/api/rules'),
  /** 跑一次检查；`ruleIds` 为空表示全部启用规则。 */
  check: (projectId: number, ruleIds?: string[]) =>
    http.post<CheckReport>(`/api/projects/${projectId}/check`, {
      rule_ids: ruleIds && ruleIds.length > 0 ? ruleIds : null,
    }),
  /** 读取上一次落库的违规（不重跑）。 */
  violations: (projectId: number, limit = 500) =>
    http.get<Violation[]>(`/api/projects/${projectId}/violations?limit=${limit}`),
  /** 上一次检查的严重度汇总（菜单角标用，不重跑规则）。 */
  summary: (projectId: number) =>
    http.get<CheckSummary>(`/api/projects/${projectId}/check/summary`),
};
