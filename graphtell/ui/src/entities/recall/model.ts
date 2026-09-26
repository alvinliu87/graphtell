/**
 * 代码召回实体：提示词 → 相关代码。
 *
 * 与后端 `gt-application::recall_service` 一一对应。
 * 关键字段是 `direct` 与 `hop`：它们把「关键词直接命中」与「靠图扩展带出来」
 * 区分开 —— 这正是召回区别于全文检索的地方，UI 必须把这个区别显示出来，
 * 否则用户无法判断一条结果为什么会出现在这里。
 */

/** 召回请求。 */
export interface RecallQuery {
  query: string;
  limit?: number;
  hops?: number;
  kinds?: string[];
  with_snippets?: boolean;
}

/** 种子（直接命中关键词的节点）。 */
export interface SeedInfo {
  node_id: number;
  kind: string;
  name: string;
  score: number;
}

/** 一条召回命中。 */
export interface RecallHit {
  node_id: number;
  kind: string;
  name: string;
  fqn?: string | null;
  score: number;
  /** 距种子的跳数（0 = 种子本身）。 */
  hop: number;
  /** 来源种子名。 */
  seed: string;
  matched_terms: string[];
  /** 是否直接命中关键词。 */
  direct: boolean;
  file?: string | null;
  line?: number | null;
  snippet?: string | null;
  relations: string[];
}

/** 召回结果。 */
export interface RecallResult {
  project_id: number;
  query: string;
  terms: string[];
  kind_hints: string[];
  seeds: SeedInfo[];
  hits: RecallHit[];
  /** 可直接粘给 LLM 的上下文包。 */
  markdown: string;
  truncated: boolean;

  /**
   * 召回置信度（0~1）。
   * 以及下面三个质量字段：召回质量方差极大（有的查询正解在前二，有的两个意图
   * 都落空、前排全是泛词噪声），但结果长得一模一样 —— 不把质量显式报出来，
   * 用户会同等信任，于是「静默失败」成了最坏的失败模式。
   * 只做提示，不过滤结果：命中照常返回。
   */
  confidence: number;
  /** 质量档位（见后端 `RecallQuality`）。 */
  quality: 'high' | 'medium' | 'low';
  /** 档位判定依据（人话，可直接展示）。 */
  quality_reason: string;
  /** 未命中的特征概念 —— UI 上渲染成可点击 chip，点一下即用该词重新召回。 */
  missing_terms: string[];
}
