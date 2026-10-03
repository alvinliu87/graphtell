/**
 * Compliance-check entities: rules (`CheckRule`) and violations (`Violation`).
 *
 * Mirrors `gt-domain::model::rules` one-to-one — rules are declared in YAML and
 * the frontend only renders them; it knows no concrete rule (adding a rule on
 * the backend needs no frontend change).
 */

export type Severity = 'info' | 'warning' | 'error' | 'critical';

/** The set of node kinds a rule applies to. */
export interface RuleScope {
  kinds: string[];
  name_contains?: string | null;
  /** Candidate cap: a literal number, or `"$paramKey"` referencing a tunable param. */
  limit: number | string;
  /** Applicable language allowlist (php / java / javascript / typescript); empty = language-agnostic. */
  languages?: string[];
  /** Applicable framework allowlist (thinkphp / spring-boot …); empty = any framework. */
  frameworks?: string[];
}

/** Kind of a tunable parameter. */
export type RuleParamKind = 'number' | 'string' | 'enum' | 'bool';

/** A tunable parameter exposed to the user (declared under YAML `params:`). */
export interface RuleParam {
  key: string;
  label: string;
  description?: string | null;
  kind: RuleParamKind;
  /** Default value (a JSON scalar matching `kind`). */
  default: unknown;
  min?: number | null;
  max?: number | null;
  choices?: string[];
}

/** One check rule (from `rules/*.yaml`). */
export interface CheckRule {
  id: string;
  title: string;
  description?: string | null;
  severity: Severity;
  category: string;
  /** The **global default** enabled state from YAML; project overrides live in `ProjectRuleConfig`. */
  enabled: boolean;
  applies_to: RuleScope;
  /** Tunable parameter declarations; empty means this rule has nothing to tune. */
  params?: RuleParam[];
  message: string;
  remediation?: string | null;
}

/**
 * Per-project override of a single rule's configuration.
 *
 * `enabled === null/undefined` means **inherit** the YAML global default;
 * `options` only carries keys that were actually overridden — the rest fall back
 * to the `params` defaults.
 */
export interface ProjectRuleConfig {
  project_id: number;
  rule_id: string;
  enabled?: boolean | null;
  options?: Record<string, unknown> | null;
}

/** Config write request (omitted fields = leave that item unchanged). */
export interface RuleConfigPatch {
  rule_id: string;
  enabled?: boolean | null;
  options?: Record<string, unknown> | null;
}

/** One matched violation. */
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
  /** Sub-project owning the matched node; `null` means a shared resource (e.g. a cross-project table / queue). */
  sub_project_id?: number | null;
}

/** Report for one check run. */
export interface CheckReport {
  project_id: number;
  rules_total: number;
  rules_run: number;
  violations: Violation[];
  by_severity: Record<string, number>;
  by_rule: Record<string, number>;
  /**
   * Rules that ran but matched nothing.
   *
   * Must be shown explicitly: the most dangerous way a rule fails is not a false
   * positive but a **silent zero** — its criteria reference an annotation or edge
   * that does not exist on the graph, so it can never match. "0 violations" is
   * then read as "the code is fine", which is far more dangerous than a false
   * positive.
   */
  rules_silent: string[];
  /**
   * Rules skipped because the environment does not match (they declare
   * languages / frameworks this project does not use). This is **expected
   * behaviour**, not a fault — a PHP-only rule should not run on a pure Java
   * project.
   */
  rules_not_applicable: string[];
  /**
   * Rules disabled because their criteria do not hold: the edges / annotations /
   * capabilities they mention are entirely absent from this project's graph.
   * Running them would only produce **vacuously-true false positives**
   * (`no_incoming: X` holds for every node when X does not exist).
   */
  rules_unavailable: string[];
  duration_ms: number;
}

/** Severity sort weight (lists put errors first by default). */
export const SEVERITY_RANK: Record<Severity, number> = {
  critical: 0,
  error: 1,
  warning: 2,
  info: 3,
};

/**
 * Severity display names: the table column tag, the stat cards and the filter
 * all share **this single source**.
 *
 * Writing these tiers twice — once in CheckPage and once in RulesPage — makes
 * them drift: the stat cards list 4 tiers while the list's severity filter lists
 * only the last 3, making "critical" visible only under "All" and unfilterable.
 * The truth about how many tiers exist lives in the
 * `Severity` type plus this map; any second list is a future inconsistency.
 */
export const SEVERITY_LABEL: Record<Severity, string> = {
  critical: 'Critical',
  error: 'Error',
  warning: 'Warning',
  info: 'Info',
};

/**
 * Severity display order (derived from `SEVERITY_RANK`, not a second array).
 *
 * Table sorting, stat cards and the filter are therefore in the same order by
 * construction — writing the order three times is the same class of drift.
 */
export const SEVERITY_ORDER: Severity[] = (Object.keys(SEVERITY_RANK) as Severity[]).sort(
  (a, b) => SEVERITY_RANK[a] - SEVERITY_RANK[b],
);

export const SEVERITY_COLOR: Record<Severity, string> = {
  critical: 'magenta',
  error: 'red',
  warning: 'orange',
  info: 'blue',
};

/**
 * Rule category (slug) → display name (English source; translated via `t()`).
 *
 * Category slugs come from the rule-set YAML (`category` in `rules/**\/*.yaml`);
 * they are a **stable, finite** enumeration, but showing `api-hygiene` /
 * `architecture` raw is jargon to the user — so this is the single place holding
 * the "slug → plain language" truth, with i18n handling the Chinese switch.
 *
 * Why centralised: both the inspection page (filter group headings) and the
 * rule-set page (group headings / group toggles) use it; two copies is exactly
 * how "the same category is named differently on two pages" happens.
 */
export const RULE_CATEGORY_LABEL: Record<string, string> = {
  architecture: 'Architecture',
  security: 'Security',
  contract: 'Contract',
  deadcode: 'Dead Code',
  performance: 'Performance',
  'api-hygiene': 'API hygiene',
};

/** Display name for a category slug: unknown slugs fall back to the slug itself (FKB-introduced ones never blank out). */
export function ruleCategoryLabel(cat: string): string {
  return RULE_CATEGORY_LABEL[cat] ?? cat;
}

/** Severity rollup for compliance checks (menu badge; aggregated by the backend per code prefix). */
export interface CheckSummary {
  critical: number;
  error: number;
  warning: number;
  info: number;
}
