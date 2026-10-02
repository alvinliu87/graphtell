/** View entities: aligned with the backend `gt-domain::model::view`. */

export type ViewMode = 'object' | 'aggregate';

export type LayoutMode = 'radial' | 'layered' | 'spine' | 'compound' | 'matrix' | 'er';

export interface Perspective {
  id: string;
  label: string;
  mode: ViewMode;
  layout: LayoutMode;
  depth: number;
  description?: string | null;
  available: number;
}

export interface SourceLocation {
  file: string;
  line: number;
  symbol: string | null;
  note: string | null;
  /** Source snippet for this location (e.g. the call statement); shown under the location for verification when provided. */
  snippet?: string | null;
}

export interface NodeView {
  id: number;
  kind: string;
  /** Category of a semantic node (currently identical to `kind`); first-class semantic nodes equal their kind, syntax nodes are null. */
  category: string | null;
  name: string;
  fqn: string | null;
  ring: number;
  sub_project_id: number | null;
  /** Whether this node kind has a matching perspective — decides whether a single click switches perspective. */
  has_own_view: boolean;
  /** Perspective id for this node; a single click switches level one to it and sets level two to this node. */
  own_view: string | null;
  /** Which "side" the node belongs to: `frontend` / `backend` (from the FKB `side` annotation). Used to tell the two ends apart in the UI. */
  side?: string | null;
  locations: SourceLocation[];
  annotations: string[];
  metrics: { fan_in?: number; fan_out?: number } | null;
}

/** An intermediate node folded into an edge (one link of the call chain). */
export interface ViaNode {
  id: number;
  kind: string;
  name: string;
  /** The "call site" of this hop (the CallSite location where the previous hop calls this node). The start node carries none. */
  call_site?: SourceLocation | null;
}

export interface EdgeView {
  id: number;
  kind: string;
  from: number;
  to: number;
  /** Whether this is a traceable, real semantic dependency (false only for non-semantic / synthetic edges). Dashed vs solid expresses directness via `indirect`. */
  resolved: boolean;
  confidence: number;
  hops: number | null;
  /**
   * Intermediate nodes folded into this edge (ordered from start to end).
   * In the folded view semantic nodes look directly connected, but they were
   * actually "pulled up" — this records the syntax nodes passed through.
   */
  via?: ViaNode[];
  /** The "call site" of the final hop (the CallSite location from the last `via` hop to `to`). */
  to_call_site?: SourceLocation | null;
  /**
   * Whether this is an **indirect, propagated** edge: the source node itself did
   * not perform the action; somewhere downstream on its call chain did (P8
   * replicates along Calls).
   *
   * Example: route A's handler calls a shared service that reads config K, so A
   * is marked `--ReadsConfig--> K`: the fact holds and both the call chain and
   * the contact point are verifiable. The UI only marks it with a dashed line and
   * a gold "indirect" tag to say "not a direct action of the source" — it is not
   * demoted to an unverified hypothesis.
   */
  indirect?: boolean;
  /**
   * Other access modes that hold **simultaneously** at the same place
   * (contact point → target).
   *
   * A user and a resource are joined by a single edge (write wins over read) and
   * the suppressed fact is recorded here: `kind: 'WritesDb'` +
   * `also_kinds: ['ReadsDb']` means "this place both reads and writes", so the
   * frontend shows "read+write DB" instead of reporting only one of them.
   */
  also_kinds?: string[];
  /**
   * Locations for **every** node on this link (start → intermediate hops → end),
   * inlined by the backend when building the view.
   *
   * Links in the folded view are pulled up on the fly, so intermediate hops
   * cannot be re-queried by edge id; the backend therefore returns everything at
   * once and the frontend never needs a per-node `/nodes/{id}/locations` call.
   * Empty means not inlined (non-folded views etc.) and the frontend falls back
   * to the original endpoint.
   */
  node_locations?: NodeLocationEntry[];
}

/** Location of one node on a link (start / intermediate hop / end), returned with the edge. */
export interface NodeLocationEntry {
  id: number;
  /** Synthetic node: its "all occurrences" do not all belong to this link, so the UI must annotate it differently. */
  synthetic: boolean;
  locations: SourceLocation[];
}

export interface HiddenInfo {
  total: number;
  shown: number;
  by_kind: Record<string, number>;
  note: string;
}

export interface UnresolvedInfo {
  code: string;
  message: string;
  location: string | null;
}

export interface Candidate {
  id: number;
  name: string;
  badge: string | null;
}

/**
 * A **direct** access that cannot be attributed to any semantic entry point
 * (orphan access).
 *
 * When the accessor is a syntax node and walking up the call chain finds no
 * semantic initiator (route / contract / scheduled task), it can neither be drawn
 * as a semantic user nor appear in the `via` of any pulled-up edge.
 *
 * The handling is **degradation, not omission**: it does not take canvas space
 * (syntax nodes carry little information and would eat the budget meant for
 * semantic nodes), but it is accounted for honestly and its contact point is
 * given — silently omitting it would contradict "N semantic in-edges" next to an
 * empty canvas.
 */
export interface OrphanAccess {
  id: number;
  /** Kind of the accessor node (usually `Method` / `Function`). */
  kind: string;
  name: string;
  /** What it does to the central resource (`ReadsDb` / `WritesCache`…). */
  edge_kind: string;
  location: SourceLocation | null;
  /**
   * Optional: when this "direct access" is itself **a clickable, expandable
   * semantic edge** (e.g. the `Triggers` firing point in the event perspective),
   * this carries the folded edge view (including the `via` call chain). The
   * frontend then opens the edge evidence drawer instead of just the node detail.
   * Semantic nodes (consumers) do not go through here — they are already promoted
   * to visible nodes on the canvas.
   */
  edge?: EdgeView | null;
}

export interface ObjectView {
  project_id: number;
  perspective: string;
  layout: LayoutMode;
  center: NodeView;
  rings: NodeView[][];
  edges: EdgeView[];
  hidden: HiddenInfo;
  /** Direct accesses with no semantic entry point (orphans): not drawn, but must be accounted for and verifiable one by one. */
  orphans?: OrphanAccess[];
  unresolved: UnresolvedInfo[];
  conclusions: Record<string, unknown>;
}

export interface Cluster {
  key: string;
  label: string;
  count: number;
  members: NodeView[];
}

export interface MatrixView {
  rows: string[];
  cols: string[];
  cells: number[][];
  row_totals: number[];
  col_totals: number[];
}

export interface AggregateView {
  project_id: number;
  perspective: string;
  layout: LayoutMode;
  clusters: Cluster[];
  matrix: MatrixView | null;
  hidden: HiddenInfo;
  unresolved: UnresolvedInfo[];
  conclusions: Record<string, unknown>;
  notice: string | null;
}

export interface EdgeEvidence {
  edge: EdgeView;
  reason: string | null;
  locations: SourceLocation[];
  via: string[];
}

export interface NodeLocations {
  id: number;
  kind: string;
  name: string;
  /** Synthetic node: locations necessarily come from several co-occurrences, so the UI must show a list rather than a single point. */
  synthetic: boolean;
  locations: SourceLocation[];
  reference_count: number;
}
