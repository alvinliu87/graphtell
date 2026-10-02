/** Graph-building pipeline entities. */

export interface PhaseReport {
  phase: string;
  nodes_created: number;
  edges_created: number;
  annotations_created: number;
  aliases_created: number;
  duration_ms: number;
  diagnostics: number;
}

export interface RunStatus {
  project_id: number;
  current_phase: string | null;
  phases: PhaseReport[];
  status: string;
}

export interface RunAccepted {
  project_id: number;
  accepted: boolean;
}

/** Phase order (matches the backend P0→P7 sequence). */
export const PHASE_ORDER = [
  'Ingest',
  'CfAst',
  'Prepare',
  'AnnotatePre',
  'Synthesize',
  'AnnotatePost',
  'Resolve',
];

/**
 * Phase display names.
 *
 * English is the source language: these values are passed through `t()` at the
 * call site and the `zh-CN` dictionary carries the Chinese overrides.
 */
export const PHASE_LABEL: Record<string, string> = {
  Ingest: 'Ingest',
  CfAst: 'Syntax graph',
  Prepare: 'Knowledge load',
  AnnotatePre: 'Source annotation',
  Synthesize: 'Semantic synthesis',
  AnnotatePost: 'Aggregate annotation',
  Resolve: 'Dynamic resolve',
};

export const PHASE_HINT: Record<string, string> = {
  Ingest: 'Detect sub-projects and files to analyze, excluding dependency dirs and static assets',
  CfAst: 'Create Class / Method / Property / CallSite nodes and inheritance edges from language syntax',
  Prepare: 'Load framework knowledge; resolve authoritative sources like AppRoot, container bindings, event tables, DB schema',
  AnnotatePre: 'Mark call sites with safety flags and framework semantic tags per framework-knowledge rules',
  Synthesize: 'Synthesize Table / HttpContract / ConfigKey / Event semantic nodes and merge them idempotently',
  AnnotatePost: 'Tag aggregated results with privacy fields, criticality, config mutability and register aliases',
  Resolve: 'Resolve containers / events / facades / routes through a funnel and build dynamic edges',
};

export interface HealthDto {
  status: string;
  version: string;
  languages: string[];
  frameworks: number;
}
