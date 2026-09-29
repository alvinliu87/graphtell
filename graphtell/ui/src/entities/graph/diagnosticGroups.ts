/**
 * 建图期诊断的归类：把「N 条条目」讲成「N 类问题」。
 *
 * 为什么必须归类：诊断天生是**长尾重复**的 —— 一条引擎诊断会在几百个文件上各触发一次
 * （CRMEB 上 `IdentityUnresolved` 就有 349 条），一条 `UnresolvedLink` 也只会说同一句
 * "指向的目标不在图里"。平铺成表格时，用户读到的是"349 个问题"，而真相是
 * "1 类问题发生了 349 次" —— 这个数量级差别直接决定了要不要管它。
 *
 * 计数取自后端 `DiagnosticSummary.by_code`（全量 `GROUP BY`），**不是**数明细窗口：
 * 明细有读取上限（见 `DIAGNOSTIC_LIMIT`），拿它分组会得到偏小的计数。
 * 老后端没有 `by_code` 时才退回数窗口，并在页面上说明。
 *
 * 放在 `entities/graph` 而不是页面下：诊断页（归类视图）与侧栏角标（按类计数）都要用它，
 * 而"什么算一类问题、哪类要管"必须只有一份判定。
 * 它跨实体引用 `entities/check` 的 `SEVERITY_RANK`：严重度档位与顺序的真相在那边，
 * 宁可跨引用也不要在这里复制一份（复制出来的那份总有一天会和check页漂移）。
 */
import { SEVERITY_RANK, type Severity } from '@/entities/check';
import type { Diagnostic, DiagnosticCodeCount } from './model';

/**
 * 一类诊断「要不要你管」的粗分类。
 *
 * 这是本页最想传达的一件事：诊断条目数 ≠ 待办数。三类的处置完全不同，
 * 混在一张表里只能靠用户自己读 code 猜。
 */
export type DiagnosticCategory =
  /** 引擎 / 框架知识局限：图少建了一块，你的代码本身没问题。 */
  | 'engine'
  /** 预期内：目标在 vendor 或被 Ingest 排除，属设计如此。 */
  | 'expected'
  /** 值得看一眼：可能指向真实的代码问题（死路由、事件没注册）。 */
  | 'actionable';

/**
 * 已知 code 的分类（与 `diag.<Code>.*` 的词条一一对应）。
 *
 * 未收录的 code（FKB 新增）不猜含义，按严重度兜底：warning 以上归「值得看一眼」，
 * 提示级归「预期内」—— 宁可多提醒，也不要替用户断言"这个不用管"。
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

/** 后端下发的严重度字符串 → 本前端的 `Severity`（未知一律按最轻的「提示」）。 */
export function asSeverity(raw: string): Severity {
  return (SEVERITIES as string[]).includes(raw) ? (raw as Severity) : 'info';
}

export function categoryOf(code: string, severity: Severity): DiagnosticCategory {
  return (
    CATEGORY_OF_CODE[code] ??
    (SEVERITY_RANK[severity] <= SEVERITY_RANK.warning ? 'actionable' : 'expected')
  );
}

/** 一类诊断的聚合结果。 */
export interface DiagnosticGroup {
  code: string;
  /** 该类条目数（全量口径）。 */
  count: number;
  /** 该类在各严重度上的分布，如 `{ warning: 349 }`。 */
  bySeverity: Partial<Record<Severity, number>>;
  /** 该类里最严重的档位：决定排序位置与配色。 */
  severity: Severity;
  category: DiagnosticCategory;
  /** 明细窗口里属于该类的条目（按严重度排序），可能少于 `count`（读取上限所致）。 */
  samples: Diagnostic[];
}

/** 严重度分布里最重的一档。 */
function worst(bySeverity: Partial<Record<Severity, number>>): Severity {
  return SEVERITIES.find((s) => (bySeverity[s] ?? 0) > 0) ?? 'info';
}

/**
 * 按 code 分组。
 *
 * `byCode` 为空（老后端 / 汇总接口失败）时退回数明细窗口：计数会偏小、类别齐全，
 * 页面据此提示"计数按当前读取窗口"，而不是白屏或显示 0。
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

/** 展示顺序：严重度优先 → 条数降序 → code 字典序（保证同一份数据每次顺序一致）。 */
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

/** 分类展示顺序（按"要不要你管"从重到轻）。 */
export const CATEGORY_ORDER: DiagnosticCategory[] = ['actionable', 'engine', 'expected'];

/** 分类配色（antd 语义色名；engine / expected 刻意用中性色，避免看起来像告警）。 */
export const CATEGORY_COLOR: Record<DiagnosticCategory, string> = {
  actionable: 'gold',
  engine: 'default',
  expected: 'default',
};

/**
 * 「值得看一眼」的条目数 = `actionable` 类的条目数。
 *
 * 侧栏角标与页面统计卡共用同一份判定，避免两处各定一套"什么算问题"。
 */
export function actionableCount(groups: DiagnosticGroup[]): number {
  return groups.filter((g) => g.category === 'actionable').reduce((n, g) => n + g.count, 0);
}
