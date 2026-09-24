/** 图实体：节点、边、标注、符号表、诊断。 */

export interface IdentityKey {
  kind: string;
  value: string;
}

export interface Node {
  id: number;
  project_id: number;
  sub_project_id: number | null;
  kind: string;
  name: string;
  fqn: string | null;
  identity: IdentityKey | null;
  file_id: number | null;
  start_line: number;
  end_line: number;
  start_byte: number;
  end_byte: number;
  language: string;
  phase: string;
  confidence: number;
  properties: Record<string, unknown> | null;
}

export interface Edge {
  id: number;
  project_id: number;
  kind: string;
  from_id: number;
  to_id: number;
  phase: string;
  confidence: number;
  properties: Record<string, unknown> | null;
}

export interface Annotation {
  id: number;
  node_id: number;
  channel: string;
  kind: string;
  subkind: string | null;
  confidence: number;
  evidence: Record<string, unknown> | null;
  phase: string;
}

export interface SymbolEntry {
  project_id: number;
  table: string;
  key: string;
  value: Record<string, unknown>;
}

export type Severity = 'info' | 'warning' | 'error' | 'critical';

export interface Diagnostic {
  project_id: number;
  phase: string;
  code: string;
  severity: Severity;
  message: string;
  location: string | null;
  payload: Record<string, unknown> | null;
}

/** 非规则诊断的严重度汇总（菜单角标用，排除 `rule:` 前缀以免与合规检查重复）。 */
export interface DiagnosticSummary {
  critical: number;
  error: number;
  warning: number;
  info: number;
  /** 暂无解析器的语言（`go` / `rust` …）：这些子工程只有文件结构，没有语义抽取。 */
  unsupported_languages?: string[];
}

export interface GraphStats {
  nodes: number;
  edges: number;
  annotations: number;
  by_kind: Record<string, number>;
}

/** 不同节点种类的配色（可视化用）。 */
export const NODE_COLORS: Record<string, string> = {
  Class: '#3d7eff',
  Interface: '#7c5cff',
  Trait: '#a855f7',
  Enum: '#ec4899',
  Method: '#0ea5e9',
  Function: '#06b6d4',
  Property: '#14b8a6',
  Const: '#22c55e',
  Namespace: '#64748b',
  CallSite: '#cbd5e1',
  File: '#94a3b8',
  Table: '#f59e0b',
  HttpContract: '#ef4444',
  ConfigKey: '#8b5cf6',
  I18nKey: '#eab308',
  // 进程外中介（Event / Queue / Cache / Topic）同色族，便于一眼识别。
  Cache: '#f97316',
  Event: '#f97316',
  Queue: '#f97316',
  Topic: '#f97316',
  // 事件 / 队列的消费者（监听器 / 消费者类）在画布上重标为此角色，与 Event / Queue 同族。
  EventHandler: '#fb923c',
  // 中间件：挂在路由上的守门人，与契约（HttpContract）同冷色族，读图时"入口 → 守卫 → 资源"
  // 的层次一眼可辨（守卫不属于业务资源，刻意不与 Table / Cache 的暖色混）。
  Middleware: '#0ea5e9',
  Unknown: '#9ca3af',
};

export function nodeColor(kind: string): string {
  return NODE_COLORS[kind] ?? NODE_COLORS.Unknown;
}

/** 边的配色。 */
export const EDGE_COLORS: Record<string, string> = {
  Extends: '#94a3b8',
  Implements: '#94a3b8',
  UsesTrait: '#c4b5fd',
  Calls: '#60a5fa',
  HasCallSite: '#e2e8f0',
  // 读/写库：冷暖对比，避免琥珀/橙混成一片。
  ReadsDb: '#3b82f6', // 读库：蓝色（冷、只读）
  WritesDb: '#ea580c', // 写库：深橙色（暖、变更）
  MapsTo: '#64748b', // 结构映射（Model→Table），移出"读/写库"暖色族，改为中性石板色
  ReadsConfig: '#a78bfa',
  ReadsCache: '#14b8a6', // 读缓存：青色，可见度足够且不与读配置紫冲突
  HandledBy: '#ef4444',
  CallsHttp: '#22c55e',
  Triggers: '#fb7185',
  PublishesTo: '#fdba74',
  Declares: '#e5e7eb',
  Contains: '#e5e7eb',
  ResolvesTo: '#38bdf8',
  // 中间件边：用中间件的天蓝；与 `HandledBy`（红 = 谁处理这个端点）区分开 ——
  // 一个是"路过谁"，一个是"落到谁"。
  PassesThrough: '#0ea5e9',
};

export function edgeColor(kind: string): string {
  return EDGE_COLORS[kind] ?? '#cbd5e1';
}
