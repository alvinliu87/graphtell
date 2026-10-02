import { http } from '@/shared/api/http';
import type {
  CheckReport,
  CheckRule,
  CheckSummary,
  ProjectRuleConfig,
  RuleConfigPatch,
  Violation,
} from './model';

/** Data access for compliance checks. */
export const checkApi = {
  /** All loaded rules (so the page can show "what can be checked"). */
  rules: () => http.get<CheckRule[]>('/api/rules'),
  /** Run one check; an empty `ruleIds` means every enabled rule. */
  check: (projectId: number, ruleIds?: string[]) =>
    http.post<CheckReport>(`/api/projects/${projectId}/check`, {
      rule_ids: ruleIds && ruleIds.length > 0 ? ruleIds : null,
    }),
  /** Read the last persisted violations (without re-running). Non-empty `subProjectIds` filters by sub-project. */
  violations: (projectId: number, limit = 500, subProjectIds?: number[]) =>
    http.get<Violation[]>(
      `/api/projects/${projectId}/violations?limit=${limit}` +
        (subProjectIds && subProjectIds.length
          ? `&sub_project_id=${subProjectIds.join(',')}`
          : ''),
    ),
  /** Severity rollup of the last check (menu badge; does not re-run rules). */
  summary: (projectId: number) =>
    http.get<CheckSummary>(`/api/projects/${projectId}/check/summary`),
  /** Rule config overrides for this project (key = rule_id). */
  ruleConfigs: (projectId: number) =>
    http.get<Record<string, ProjectRuleConfig>>(`/api/projects/${projectId}/rules/config`),
  /** Write config for a single rule (omitted fields keep their current value). */
  putRuleConfig: (projectId: number, patch: RuleConfigPatch) =>
    http.put<boolean>(`/api/projects/${projectId}/rules/config`, patch),
  /** Batch write (enabling / disabling a whole group or category). */
  batchRuleConfig: (projectId: number, items: RuleConfigPatch[]) =>
    http.post<boolean>(`/api/projects/${projectId}/rules/config/batch`, { items }),
  /** Reset a single rule's project override, falling back to the YAML global default. */
  resetRuleConfig: (projectId: number, ruleId: string) =>
    http.del<boolean>(`/api/projects/${projectId}/rules/config/${encodeURIComponent(ruleId)}`),
};
