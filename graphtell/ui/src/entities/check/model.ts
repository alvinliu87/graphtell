/**
 * 合规检查实体：规则（CheckRule）与违规（Violation）。
 *
 * 与后端 `gt-domain::model::rules` 一一对应 —— 规则声明在 YAML 里，
 * 前端只是渲染，不认识任何具体规则（后端加一条规则，前端无需改动）。
 */

export type Severity = 'info' | 'warning' | 'error' | 'critical';

/** 规则作用的节点范围。 */
export interface RuleScope {
  kinds: string[];
  name_contains?: string | null;
  limit: number;
  /** 适用语言白名单（php / java / javascript / typescript）；为空 = 跨语言通用。 */
  languages?: string[];
  /** 适用框架白名单（thinkphp6 / spring-boot …）；为空 = 不限框架。 */
  frameworks?: string[];
}

/** 一条检查规则（来自 `rules/*.yaml`）。 */
export interface CheckRule {
  id: string;
  title: string;
  description?: string | null;
  severity: Severity;
  category: string;
  enabled: boolean;
  applies_to: RuleScope;
  message: string;
  remediation?: string | null;
}

/** 一次命中的违规。 */
export interface Violation {
  project_id: number;
  rule_id: string;
  title: string;
  category: string;
  severity: Severity;
  node_id: number;
  node_name: string;
  node_kind: string;
  message: string;
  remediation?: string | null;
  file?: string | null;
  line?: number | null;
}

/** 一次检查的报告。 */
export interface CheckReport {
  project_id: number;
  rules_total: number;
  rules_run: number;
  violations: Violation[];
  by_severity: Record<string, number>;
  by_rule: Record<string, number>;
  /**
   * 跑了但一条都没命中的规则。
   *
   * 必须显式展示：规则最危险的失效方式不是误报，而是**静默归零** ——
   * 判据用了一个图上不存在的标注/边，于是永远匹配不上。此时"0 条违规"
   * 会被读成"代码没问题"，比误报危险得多。
   */
  rules_silent: string[];
  /**
   * 环境不匹配而跳过的规则（声明了 languages / frameworks，本工程没有该栈）。
   * 这是**预期行为**，不是故障 —— PHP 专属规则不该在纯 Java 工程上跑。
   */
  rules_not_applicable: string[];
  /**
   * 判据不成立而停用的规则：判据提到的边/标注/能力在本工程图上一个都没有。
   * 这时跑规则只会产出**恒真误报**（`no_incoming: X` 在 X 不存在时对所有节点成立）。
   */
  rules_unavailable: string[];
  duration_ms: number;
}

/** 严重度排序权重（列表默认 error 在前）。 */
export const SEVERITY_RANK: Record<Severity, number> = {
  critical: 0,
  error: 1,
  warning: 2,
  info: 3,
};

export const SEVERITY_COLOR: Record<Severity, string> = {
  critical: 'magenta',
  error: 'red',
  warning: 'orange',
  info: 'blue',
};
