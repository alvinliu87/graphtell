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
  /** 候选集上限：字面量数字，或 `"$paramKey"` 形式引用可调参数。 */
  limit: number | string;
  /** 适用语言白名单（php / java / javascript / typescript）；为空 = 跨语言通用。 */
  languages?: string[];
  /** 适用框架白名单（thinkphp6 / spring-boot …）；为空 = 不限框架。 */
  frameworks?: string[];
}

/** 可调参数的种类。 */
export type RuleParamKind = 'number' | 'string' | 'enum' | 'bool';

/** 一条规则暴露给用户的可调参数（在 YAML `params:` 下声明）。 */
export interface RuleParam {
  key: string;
  label: string;
  description?: string | null;
  kind: RuleParamKind;
  /** 默认值（与 kind 对应的 JSON 标量）。 */
  default: unknown;
  min?: number | null;
  max?: number | null;
  choices?: string[];
}

/** 一条检查规则（来自 `rules/*.yaml`）。 */
export interface CheckRule {
  id: string;
  title: string;
  description?: string | null;
  severity: Severity;
  category: string;
  /** YAML 里的**全局默认**启用态；工程级覆盖见 `ProjectRuleConfig`。 */
  enabled: boolean;
  applies_to: RuleScope;
  /** 可调参数声明；为空表示这条规则没有可调项。 */
  params?: RuleParam[];
  message: string;
  remediation?: string | null;
}

/**
 * 工程级对单条规则的配置覆盖。
 *
 * `enabled === null/undefined` 表示**继承** YAML 全局默认；
 * `options` 只放被覆盖过的键，未覆盖的取 `params` 的默认。
 */
export interface ProjectRuleConfig {
  project_id: number;
  rule_id: string;
  enabled?: boolean | null;
  options?: Record<string, unknown> | null;
}

/** 配置写入请求（省略的字段 = 不改该项）。 */
export interface RuleConfigPatch {
  rule_id: string;
  enabled?: boolean | null;
  options?: Record<string, unknown> | null;
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

/** 合规检查的严重度汇总（菜单角标用，来自后端按 code 前缀的聚合计数）。 */
export interface CheckSummary {
  critical: number;
  error: number;
  warning: number;
  info: number;
}
