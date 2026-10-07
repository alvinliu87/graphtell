/** Graph entities: nodes, edges, annotations, symbol table, diagnostics. */

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

/** Entry count for one diagnostic type (same `code` + same severity). */
export interface DiagnosticCodeCount {
  code: string;
  severity: Severity;
  count: number;
}

/** Severity rollup for non-rule diagnostics (menu badge; excludes the `rule:` prefix so it does not double-count compliance). */
export interface DiagnosticSummary {
  critical: number;
  error: number;
  warning: number;
  info: number;
  /** Languages with no parser yet (`go` / `rust` …): those sub-projects have file structure only, no semantic extraction. */
  unsupported_languages?: string[];
  /**
   * Entry counts broken down by **problem type** (full scope, unaffected by the
   * list read cap).
   *
   * Both the sidebar badge and the diagnostics page rely on it to turn "445
   * entries" into "6 problem types" — when one engine diagnostic fires in
   * hundreds of places, reporting only the total makes "the same thing happened
   * 349 times" read as "349 problems". Older backends may omit it, hence optional.
   */
  by_code?: DiagnosticCodeCount[];
}

export interface GraphStats {
  nodes: number;
  edges: number;
  annotations: number;
  by_kind: Record<string, number>;
}

/** Palette per node kind (visualisation only). */
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
  // Out-of-process mediators (Event / Queue / Cache / Topic) share one colour family so they are recognisable at a glance.
  Cache: '#f97316',
  Event: '#f97316',
  Queue: '#f97316',
  Topic: '#f97316',
  // Consumers of events / queues (listeners / consumer classes) are relabelled to this role on the canvas, same family as Event / Queue.
  EventHandler: '#fb923c',
  // Middleware: the gatekeeper attached to a route, sharing the cool family with contracts (HttpContract) so the
  // "entry → guard → resource" layering is obvious when reading the graph (a guard is not a business resource,
  // deliberately kept out of the warm Table / Cache colours).
  Middleware: '#0ea5e9',
  Unknown: '#9ca3af',
};

export function nodeColor(kind: string): string {
  return NODE_COLORS[kind] ?? NODE_COLORS.Unknown;
}

/** Palette per edge kind. */
export const EDGE_COLORS: Record<string, string> = {
  Extends: '#94a3b8',
  Implements: '#94a3b8',
  UsesTrait: '#c4b5fd',
  Calls: '#60a5fa',
  HasCallSite: '#e2e8f0',
  // DB read/write: a cool/warm contrast so amber and orange do not blur together.
  ReadsDb: '#3b82f6', // reads DB: blue (cool, read-only)
  WritesDb: '#ea580c', // writes DB: deep orange (warm, mutating)
  MapsTo: '#64748b', // structural mapping (Model→Table), moved out of the warm read/write-DB family to a neutral slate
  ReadsConfig: '#a78bfa',
  ReadsCache: '#14b8a6', // reads cache: teal, visible enough and not clashing with the config-read purple
  HandledBy: '#ef4444',
  CallsHttp: '#22c55e',
  Triggers: '#fb7185',
  PublishesTo: '#fdba74',
  Declares: '#e5e7eb',
  Contains: '#e5e7eb',
  ResolvesTo: '#38bdf8',
  // Cross-sub-project contract bridge: the front end's contract node -> the back end's declaration of the same
  // endpoint. Same hue family as `CallsHttp` (both are "the front end reaches this endpoint") but in cyan, so it
  // reads as "the two halves of one endpoint" rather than as another caller.
  ResolvesToContract: '#06b6d4',
  // Middleware edge: the middleware sky blue, kept apart from `HandledBy` (red = who handles this endpoint) —
  // one is "who you pass through", the other is "where you land".
  PassesThrough: '#0ea5e9',
};

export function edgeColor(kind: string): string {
  return EDGE_COLORS[kind] ?? '#cbd5e1';
}
