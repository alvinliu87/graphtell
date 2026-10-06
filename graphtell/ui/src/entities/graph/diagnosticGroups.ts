/**
 * Grouping of build-time diagnostics: turning "N entries" into "N problem types".
 *
 * Why grouping is mandatory: diagnostics are inherently **long-tail and
 * repetitive** — one engine diagnostic fires once per file in hundreds of files
 * (on a real project `IdentityUnresolved` alone has 349 entries), and a single
 * `UnresolvedLink` only ever says the same thing: "the target is not in the
 * graph". Laid out flat as a table the user reads "349 problems" while the truth
 * is "1 problem type occurred 349 times" — an order-of-magnitude difference that
 * decides whether it is worth attention at all.
 *
 * Counts come from the backend `DiagnosticSummary.by_code` (a full `GROUP BY`),
 * **not** from counting the detail window: the detail list has a read cap (see
 * `DIAGNOSTIC_LIMIT`), so grouping over it yields counts that are too small. Only
 * when an older backend lacks `by_code` do we fall back to counting the window,
 * and the page says so.
 *
 * Lives in `entities/graph` rather than under a page: both the diagnostics page
 * (grouped view) and the sidebar badge (per-type counts) use it, and "what counts
 * as one problem type / which types need attention" must have exactly one
 * definition. It cross-references `SEVERITY_RANK` from `entities/check` because
 * the truth about severity tiers and their order lives there — better to cross
 * an entity boundary than to copy the list here (a copy drifts from the check
 * page sooner or later).
 */
import { SEVERITY_RANK, type Severity } from '@/entities/check';
import type { Diagnostic, DiagnosticCodeCount } from './model';

/**
 * Coarse bucket for "does this diagnostic type need you".
 *
 * This is the single most important thing the page conveys: diagnostic entry
 * count ≠ number of todos. The three buckets are handled completely differently,
 * and mixing them in one table leaves the user guessing from raw codes.
 */
export type DiagnosticCategory =
  /** Engine / framework-knowledge limit: a piece of the graph was not built; your code itself is fine. */
  | 'engine'
  /** Expected: the target lives in vendor or was excluded by Ingest — by design. */
  | 'expected'
  /** Worth a look: may point at a real code problem (dead route, unregistered event). */
  | 'actionable';

/**
 * Category per known code (one-to-one with the `diag.<Code>.*` entries).
 *
 * Unlisted codes (added by FKB) are not guessed at; they fall back on severity:
 * warning and above go to "worth a look", info level to "expected" — better to
 * over-remind than to assert on the user's behalf that "this needs nothing".
 */
const CATEGORY_OF_CODE: Record<string, DiagnosticCategory> = {
  IdentityUnresolved: 'engine',
  NoParserForLanguage: 'engine',
  RootRuleUnresolved: 'engine',
  AnnotateTargetMissing: 'engine',
  AliasTargetMissing: 'expected',
  EventListenNoConsumer: 'expected',
  UnresolvedLink: 'actionable',
  EventTriggerUnresolved: 'actionable',
  EventListenUnresolved: 'actionable',
  EventListenTargetMissing: 'actionable',
};

const SEVERITIES: Severity[] = ['critical', 'error', 'warning', 'info'];

/** Backend severity string → this frontend's `Severity` (unknown values become the lightest, "info"). */
export function asSeverity(raw: string): Severity {
  return (SEVERITIES as string[]).includes(raw) ? (raw as Severity) : 'info';
}

export function categoryOf(code: string, severity: Severity): DiagnosticCategory {
  return (
    CATEGORY_OF_CODE[code] ??
    (SEVERITY_RANK[severity] <= SEVERITY_RANK.warning ? 'actionable' : 'expected')
  );
}

/** Aggregation result for one diagnostic type. */
export interface DiagnosticGroup {
  code: string;
  /** Number of entries of this type (full scope). */
  count: number;
  /** Distribution across severities, e.g. `{ warning: 349 }`. */
  bySeverity: Partial<Record<Severity, number>>;
  /** The most severe tier present: decides sort position and colour. */
  severity: Severity;
  category: DiagnosticCategory;
  /** Entries of this type inside the detail window (sorted by severity); may be fewer than `count` because of the read cap. */
  samples: Diagnostic[];
}

/** The most severe tier present in a severity distribution. */
function worst(bySeverity: Partial<Record<Severity, number>>): Severity {
  return SEVERITIES.find((s) => (bySeverity[s] ?? 0) > 0) ?? 'info';
}

/**
 * Group by code.
 *
 * When `byCode` is empty (older backend / summary endpoint failure) we fall back
 * to counting the detail window: counts are then too small but every type is
 * present, and the page says "counts follow the current read window" instead of
 * blanking out or showing 0.
 */
export function groupDiagnostics(
  byCode: DiagnosticCodeCount[] | undefined,
  items: Diagnostic[],
): DiagnosticGroup[] {
  const groups = new Map<string, DiagnosticGroup>();
  const ensure = (code: string): DiagnosticGroup => {
    const found = groups.get(code);
    if (found) return found;
    const created: DiagnosticGroup = {
      code,
      count: 0,
      bySeverity: {},
      severity: 'info',
      category: 'expected',
      samples: [],
    };
    groups.set(code, created);
    return created;
  };

  const counts = byCode ?? [];
  if (counts.length) {
    for (const c of counts) {
      const sev = asSeverity(c.severity);
      const g = ensure(c.code);
      g.count += c.count;
      g.bySeverity[sev] = (g.bySeverity[sev] ?? 0) + c.count;
    }
  } else {
    for (const d of items) {
      const g = ensure(d.code);
      g.count += 1;
      g.bySeverity[d.severity] = (g.bySeverity[d.severity] ?? 0) + 1;
    }
  }

  for (const d of items) ensure(d.code).samples.push(d);

  const list = [...groups.values()];
  for (const g of list) {
    g.severity = worst(g.bySeverity);
    g.category = categoryOf(g.code, g.severity);
    g.samples.sort((a, b) => SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity]);
  }
  return sortGroups(list);
}

/** Display order: severity first → entry count descending → code lexicographic (so the same data always sorts the same). */
export function sortGroups(groups: DiagnosticGroup[]): DiagnosticGroup[] {
  return groups
    .slice()
    .sort(
      (a, b) =>
        SEVERITY_RANK[a.severity] - SEVERITY_RANK[b.severity] ||
        b.count - a.count ||
        a.code.localeCompare(b.code),
    );
}

/** Category display order (from "needs you" to "does not"). */
export const CATEGORY_ORDER: DiagnosticCategory[] = ['actionable', 'engine', 'expected'];

/** Category colours (antd semantic names; engine / expected deliberately neutral so they do not look like alerts). */
export const CATEGORY_COLOR: Record<DiagnosticCategory, string> = {
  actionable: 'gold',
  engine: 'default',
  expected: 'default',
};

/**
 * "Worth a look" entry count = entries in the `actionable` category.
 *
 * The sidebar badge and the page stat card share this one definition so the two
 * places cannot each invent their own notion of "what counts as a problem".
 */
export function actionableCount(groups: DiagnosticGroup[]): number {
  return groups.filter((g) => g.category === 'actionable').reduce((n, g) => n + g.count, 0);
}
