/**
 * View state (the URL *is* the state).
 *
 * Why it has to live in the URL:
 * * **Refresh** — after F5 the user must land on the same graph, otherwise one stray refresh throws
 *   away several minutes of investigation
 * * **Back / forward** — switching perspective is a costly navigation action, so the browser back
 *   button must be able to undo it
 * * **Sharing / reporting** — one link is one conclusion ("look at this endpoint's link")
 *
 * Short keys keep the URL readable: `p` perspective, `n` centre, `d` depth, `i` Inspector.
 * (There used to be an `m` layout override: layout is declared per perspective in
 * `views/perspectives.yaml` and is no longer exposed to the user.)
 */
export interface ViewState {
  /** Level one: perspective id. */
  p: string | null;
  /** Level two: id of the centre object node. */
  n: number | null;
  /** Hop count. */
  d: number;
  /** Node selected in the Inspector (does not switch perspective). */
  i: number | null;
  /** Edge selected in the Inspector. */
  e: number | null;
}

export const EMPTY_STATE: ViewState = { p: null, n: null, d: 2, i: null, e: null };

export function encodeViewState(s: ViewState): string {
  const usp = new URLSearchParams();
  if (s.p) usp.set('p', s.p);
  if (s.n !== null) usp.set('n', String(s.n));
  if (s.d !== 2) usp.set('d', String(s.d));
  if (s.i !== null) usp.set('i', String(s.i));
  if (s.e !== null) usp.set('e', String(s.e));
  const str = usp.toString();
  return str ? `?${str}` : '';
}

export function decodeViewState(search: string): ViewState {
  const usp = new URLSearchParams(search);
  const num = (key: string): number | null => {
    const v = usp.get(key);
    if (v === null) return null;
    const n = Number(v);
    return Number.isFinite(n) ? n : null;
  };
  return {
    p: usp.get('p'),
    n: num('n'),
    d: num('d') ?? 2,
    i: num('i'),
    e: num('e'),
  };
}

/** Whether two states are equivalent (used to avoid writing duplicate history entries). */
export function sameViewState(a: ViewState, b: ViewState): boolean {
  return a.p === b.p && a.n === b.n && a.d === b.d && a.i === b.i && a.e === b.e;
}

/**
 * Reconcile the state: a URL can be hand-written or stale (e.g. the perspective was deleted).
 * This only repairs what can be repaired, and **never silently moves the user to an unrelated graph**.
 *
 * Special care: **do not use the candidate list to decide whether a node exists**. The candidate list
 * exists for the level-two picker (it has a `limit` and may be filtered by the backend); treating it as
 * an existence check would misjudge a node the user just clicked — one that happens to rank beyond the
 * first N — as "deleted", and the page would then auto-replace it with the first candidate. That is
 * precisely "silently showing an unrelated graph". Whether a node is really usable is answered by the
 * response to `/view/{p}?node=`; if it cannot be fetched, the page reports that honestly and lets the
 * user pick again.
 */
export function reconcileViewState(
  state: ViewState,
  perspectives: Array<{ id: string; mode: 'object' | 'aggregate'; available: number; depth?: number }>,
): ViewState {
  const exists = state.p ? perspectives.find((x) => x.id === state.p) : undefined;
  if (!exists) {
    const first = perspectives.find((x) => x.available > 0) ?? perspectives[0];
    return { ...EMPTY_STATE, p: first?.id ?? null, d: first?.depth ?? 2 };
  }
  if (exists.mode === 'aggregate') {
    // An aggregate perspective has no "single object"
    return { ...state, n: null };
  }
  return state;
}
