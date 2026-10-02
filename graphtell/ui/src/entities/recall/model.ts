/**
 * Prompt-augmentation entities (code recall): prompt → related code.
 *
 * Mirrors `gt-application::recall_service` one-to-one. The key fields are
 * `direct` and `hop`: they separate "matched the keyword directly" from "pulled
 * in by graph expansion" — exactly what distinguishes recall from full-text
 * search. The UI must show that difference, otherwise the user cannot tell why a
 * given result is here at all.
 */

/** Recall request. */
export interface RecallQuery {
  query: string;
  limit?: number;
  hops?: number;
  kinds?: string[];
  with_snippets?: boolean;
}

/** A seed (a node that matched a keyword directly). */
export interface SeedInfo {
  node_id: number;
  kind: string;
  name: string;
  score: number;
}

/** One recall hit. */
export interface RecallHit {
  node_id: number;
  kind: string;
  name: string;
  fqn?: string | null;
  score: number;
  /** Hops from the seed (0 = the seed itself). */
  hop: number;
  /** Name of the originating seed. */
  seed: string;
  matched_terms: string[];
  /** Whether the keyword was matched directly. */
  direct: boolean;
  file?: string | null;
  line?: number | null;
  snippet?: string | null;
  relations: string[];
}

/**
 * Request to compose a prompt.
 *
 * `query` and `intent` are two different things: `query` is the **search text
 * used to recall code**, `intent` is **this task / the prompt** (telling the LLM
 * what to do). They are separate because they serve different purposes — the
 * same task may need several phrasings before recall hits the right code, and
 * conversely the same batch of code can support different tasks.
 */
export interface ComposePromptRequest {
  /** Search text used to recall code (natural language mixed with identifiers is fine). */
  query: string;
  /** This task / prompt; when omitted the composed result asks the LLM to infer it from the context. */
  intent?: string;
  limit?: number;
  hops?: number;
  with_snippets?: boolean;
}

/** Result of composing a prompt. */
export interface ComposePromptResult {
  /** The complete prompt, ready to paste into an LLM (task + code context + quality constraints). */
  prompt: string;
  /** Raw recall context (markdown) so it can be trimmed by hand. */
  markdown: string;
  seed_count: number;
  hit_count: number;
  /** Rough token estimate for the prompt. */
  approx_tokens: number;
  /**
   * The full recall result (seeds / hits / query terms / quality tier).
   *
   * The backend returns it in one shot instead of the frontend calling `/recall`
   * again: recall is the most expensive step, and running it twice is both
   * wasteful and potentially inconsistent (the graph may be rebuilt in between).
   */
  recall: RecallResult;
}

/** Recall result. */
export interface RecallResult {
  project_id: number;
  query: string;
  terms: string[];
  kind_hints: string[];
  seeds: SeedInfo[];
  hits: RecallHit[];
  /** Context pack ready to paste into an LLM. */
  markdown: string;
  truncated: boolean;

  /**
   * Recall confidence (0~1), plus the three quality fields below: recall quality
   * varies enormously (some queries put the right answer in the top two, others
   * miss both intents and fill the top rows with generic noise) yet the results
   * look identical. Without reporting quality explicitly the user trusts them
   * equally, and "silent failure" becomes the worst failure mode.
   * These are hints only, they do not filter: hits are returned as usual.
   */
  confidence: number;
  /** Quality tier (see the backend `RecallQuality`). */
  quality: 'high' | 'medium' | 'low';
  /** Reason for the tier, in plain language, ready to display. */
  quality_reason: string;
  /** Unmatched feature concepts — rendered as clickable chips that re-run recall with that word. */
  missing_terms: string[];
}
