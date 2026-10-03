import {
  Alert,
  Badge,
  Button,
  Card,
  Col,
  Collapse,
  Divider,
  Drawer,
  Input,
  Popover,
  Row,
  Select,
  Space,
  Table,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { useParams, useSearchParams, useNavigate } from 'react-router-dom';
import { useAsync } from '@/shared/lib/useAsync';
import { useProject, projectApi, type SubProject } from '@/entities/project';
import {
  actionableCount,
  graphApi,
  groupDiagnostics,
  type DiagnosticSummary,
} from '@/entities/graph';
import {
  useAggregateView,
  useObjectView,
  usePerspectives,
  viewApi,
  type Candidate,
  type EdgeView,
  type LayoutMode,
  type SourceLocation,
} from '@/entities/view';
import { GraphCanvas, type CanvasCluster, type CanvasNode, type CanvasMatrix } from '@/widgets/graph-canvas';
import { Inspector } from '@/widgets/inspector';
import { PageHeader } from '@/shared/ui/PageHeader';
import { PipelineProgress } from '@/widgets/pipeline-progress';
import { RunPipelineButton } from '@/features/run-pipeline';
import { useRunStatus } from '@/entities/pipeline';
import { PerspectivePicker, type BreadcrumbItem } from '@/features/perspective-picker';
import {
  decodeViewState,
  encodeViewState,
  reconcileViewState,
  sameViewState,
  type ViewState,
} from '@/shared/lib/urlState';
// Temporarily commented out: the IDE-open entry has been removed, so there's no need to handle the "backend root -> local root" mapping (root template / WSL / per-project override).
// Restore this import if we ever bring it back.
// import {
//   effectiveTemplate,
//   getWslDistro,
//   getWslMode,
//   resolveProjectRoot,
// } from '@/shared/lib/ide';
import { formatNumber } from '@/shared/lib/format';
import { edgeKindLabel, useLocale } from '@/shared/lib/i18n';
import { FullscreenOutlined, InfoCircleOutlined } from '@ant-design/icons';

/** Code graph page: two-level filtering -> single-object link subgraph -> a jumpable conclusions panel. */
export function GraphPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const navigate = useNavigate();
  const { t } = useLocale();
  const [params, setParams] = useSearchParams();
  const { project, reload: reloadProject } = useProject(id);
  const { run } = useRunStatus(id, project?.status);

  const { perspectives } = usePerspectives(id);

  /** Scene (the URL is the single source of truth). */
  const [state, setState] = useState<ViewState>(() => decodeViewState(params.toString()));
  /** Breadcrumb: records the perspectives navigated, allows going back. */
  const [trail, setTrail] = useState<BreadcrumbItem[]>([]);
  /** The previous center, kept as a neighbor and marked `from` after switching perspective. */
  const [origin, setOrigin] = useState<{ id: number; name: string } | null>(null);
  /** Store the candidate list together with the perspective it belongs to: it must be invalidated immediately when switching perspective,
   *  otherwise it would use the previous perspective's list to fill the default value / judge "does the node exist". */
  const [candidateBundle, setCandidateBundle] = useState<{ p: string; q: string; list: Candidate[] }>({
    p: '',
    q: '',
    list: [],
  });
  /** Whether the current perspective's candidates have come back from the backend: tells "no node selected" apart as "still loading" vs "truly no candidates". */
  const [candidatesLoaded, setCandidatesLoaded] = useState(false);
  /**
   * The id of the "backend-recommended default object" under the current perspective -- independent of the dropdown autocomplete cache.
   *
   * Decoupled from `candidateBundle`: the default object comes from the backend's "semantic dependency value" ranking over **all candidates**
   * (that one `candidates` request without a search term) -- view-layer derived semantics; while `candidateBundle` is a client cache
   * of "autocomplete queries". The two differ in meaning and must not be mixed -- here we snapshot once when the ranking
   * list arrives; later searches / opening the dropdown never rewrite it,
   * fully severing the "default object = first item of the autocomplete cache" conflation.
   */
  const [defaultNode, setDefaultNode] = useState<{ p: string; id: number } | null>(null);
  /** Only candidates "belonging to the current perspective" apply; the moment you switch perspective it derives to empty until the new perspective's candidates arrive. */
  const candidates = candidateBundle.p === state.p ? candidateBundle.list : [];
  const [candidateSearch, setCandidateSearch] = useState('');
  /** Whether the level-2 object dropdown is open: candidates are only fetched from the backend when it is open (loaded on demand).
   *  without a search term it runs BFS in memory over the whole preloaded graph
   *  (no per-node DB lookups) and is fast; with a search term it matches by name, no scoring.
   *  Still loaded on demand here: never prefetch when a node is selected and the dropdown is closed, so a perspective switch makes no pointless call. */
  const [dropdownOpen, setDropdownOpen] = useState(false);
  /** In collapsed mode, the syntactic subgraph shown on demand after clicking a node (grouped by node id). */
  const [expanded, setExpanded] = useState<Record<number, { nodes: CanvasNode[]; edges: EdgeView[] }>>({});
  const [expandingId, setExpandingId] = useState<number | null>(null);
  const [inspectNode, setInspectNode] = useState<number | null>(state.i);
  const [inspectEdge, setInspectEdge] = useState<number | null>(state.e);
  /** The edge that was clicked. A synthetic edge's collapsed chain (`via`) exists only in this view result and can't be re-queried by id,
   *  so it must be carried into the Inspector on click, otherwise "passing through N hops" becomes an empty claim. */
  const [inspectEdgeView, setInspectEdgeView] = useState<EdgeView | null>(null);
  /** The right-hand "conclusions / navigation" panel: collapsed by default into a floating drawer, so it doesn't eat the graph's horizontal space. */
  const [drawerOpen, setDrawerOpen] = useState(false);
  /** Whether the orphan direct-access list is expanded (collapsed by default, showing only a one-line count). */
  const [orphansOpen, setOrphansOpen] = useState(false);
  const lastPushed = useRef<string>('');
  /**
   * Whether the **next** URL write should be **skipped**: state that was just synced from the URL must not be written back.
   *
   * The two effects fire at different times (`[params]` vs `[state]`), leaving a window where "the URL and state each change
   * once in the same frame": then state->URL still uses the **pre-sync** old state, so writing back overwrites the new URL that just arrived,
   * and that overwritten URL is in turn synced back into state as the old state -- the two step on each other, always one beat apart,
   * which shows up as the URL bouncing endlessly between two values after a perspective switch.
   */
  const skipWrite = useRef(false);
  /**
   * Whether the next URL write uses `replace` (consuming no history entry).
   *
   * Only **user-initiated navigation** (switch perspective / select object / click breadcrumb / click graph) earns a history entry;
   * updates "filled in" by state itself (correcting a missing `p` in the URL, auto-selecting a default object when none is specified)
   * always use replace -- otherwise one perspective switch pushes two entries (`?p=X` then `?p=X&n=Y`),
   * and Back to `?p=X` gets pushed again and auto-fills `n`, so the URL looks like it bounces in place.
   */
  const derivedNav = useRef(false);

  const current = perspectives.find((p) => p.id === state.p) ?? null;
  const isAggregate = current?.mode === 'aggregate';
  /** When no node is selected and candidates aren't ready / the first one is about to be auto-selected: the graph area should show a spinner rather than an empty state,
   *  otherwise the first screen and perspective switches flash "no object to show under this perspective". */
  const pendingAutoSelect =
    !isAggregate && state.n === null && (candidates.length > 0 || !candidatesLoaded);

  // Semantic-content identity of the graph: only navigation actions like "switch perspective / select object / switch aggregate view" change it,
  // used to trigger a re-fit in GraphCanvas. In-place single-node expansion, hovering, and manual zoom/pan don't count.
  const fitKey = isAggregate ? `agg:${state.p}` : `obj:${state.p ?? ''}:${state.n ?? ''}`;
  /** Manual "fit to screen" signal: each +1 makes GraphCanvas reset to whole-graph fit. */
  const [fitSignal, setFitSignal] = useState(0);
  /** Sub-project filter: multi-select display by `sub_project_id` (empty array = all). Multiple frontends / backends each form their own category. */
  const [subFilter, setSubFilter] = useState<number[]>([]);
  /**
   * The legend is the filter: hidden node / edge kinds. Empty array = hide nothing.
   * Synced to the URL (see the effect below), so a view with "all table nodes turned off" can be shared.
   */
  const [hiddenNodeKinds, setHiddenNodeKinds] = useState<string[]>([]);
  const [hiddenEdgeKinds, setHiddenEdgeKinds] = useState<string[]>([]);
  const toggleNodeKind = useCallback((k: string) => {
    setHiddenNodeKinds((prev) => (prev.includes(k) ? prev.filter((x) => x !== k) : [...prev, k]));
  }, []);
  const toggleEdgeKind = useCallback((k: string) => {
    setHiddenEdgeKinds((prev) => (prev.includes(k) ? prev.filter((x) => x !== k) : [...prev, k]));
  }, []);
  const resetLegendFilters = useCallback(() => {
    setHiddenNodeKinds([]);
    setHiddenEdgeKinds([]);
  }, []);
  /** Sub-project list (id / name / role), used by the filter and for canvas coloring / legend. */
  const { data: subProjectsData } = useAsync(() => projectApi.subProjects(id), [id]);
  const subProjects: SubProject[] = subProjectsData ?? [];
  /**
   * Languages with no parser yet (the `unsupported_languages` symbol table written by P2, returned structured via the diagnostic summary).
   * The top of the code graph needs a banner from this: these sub-projects have file structure only, no semantic extraction;
   * without saying so, a user facing a near-empty graph will think "the project itself has nothing", not "the tool doesn't support it".
   */
  const { data: diagSummary } = useAsync<DiagnosticSummary | null>(
    () => (Number.isNaN(id) ? Promise.resolve(null) : graphApi.diagnosticsSummary(id)),
    [id],
  );
  const unsupportedLangs = diagSummary?.unsupported_languages ?? [];

  /**
   * Data for the "graph coverage" hint: the build report's results grouped by type.
   *
   * The entry sits in the ⓘ beside the title (Popover, opens on click): the report answers "what did this graph fail to build",
   * and the subject is exactly this graph, so it belongs where the graph is explained; it's rarely needed, so **it must not take a page line** --
   * a permanent "445 occurrences" line would be pretending that 78% engine-limitation content is a to-do.
   *
   * The criterion shares `groupDiagnostics` / `actionableCount` with the build-report page; no separate "what counts as a problem".
   * Purely informational types aren't counted: a missing vendor target is by design, not a to-do.
   */
  const diagGroups = useMemo(() => groupDiagnostics(diagSummary?.by_code, []), [diagSummary]);
  const diagTypes = diagGroups.filter((g) => g.severity !== 'info').length;
  const diagTotal = diagSummary
    ? diagSummary.critical + diagSummary.error + diagSummary.warning + diagSummary.info
    : 0;
  const diagActionable = actionableCount(diagGroups);
  const TIER_LABEL: Record<string, string> = {
    frontend: t('Frontend'),
    backend: t('Backend'),
    library: t('DB'),
    unknown: t('Unknown'),
  };
  const KIND_LABEL: Record<string, string> = {
    admin: t('Admin'),
    'mini-program': t('Mini program'),
    mobile: t('Mobile'),
    h5: t('H5'),
    api: t('API'),
    worker: t('Jobs / queues'),
    bff: t('BFF'),
    web: t('Web'),
  };
  const roleLabel = (r?: string | null) => {
    if (!r) return t('Unknown');
    const [tier, kind] = r.split(':');
    if (kind) return KIND_LABEL[kind] ?? kind;
    return TIER_LABEL[tier] ?? tier;
  };

  // Sub-projects act as an "upper dimension": only when a **single** sub-project is selected do we auto-jump to its most relevant default perspective and object;
  // multi-select / empty only filters the canvas (no perspective switch), avoiding frequent switches that interrupt browsing.
  const singleSubId = subFilter.length === 1 ? subFilter[0] : undefined;
  const singleSub = singleSubId !== undefined ? subProjects.find((s) => s.id === singleSubId) : undefined;
  useEffect(() => {
    if (!singleSub) return;
    const pid = defaultPerspectiveForRole(singleSub.role);
    if (!pid) return;
    setState((s) => ({ ...s, p: pid, n: null, i: null, e: null }));
    // Depend only on subFilter: switching perspective (state.p change) must not re-trigger it, otherwise it loops.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subFilter]);

  /**
   * Canvas height adapts to the **remaining viewport space**.
   *
   * The block above the graph (page header + filter row + any inserted hint bar) has dynamic height; hardcoding 720 would
   * force scrolling on the first screen to see the whole graph. Here we measure the graph area's start position in the document and give the canvas all the remaining viewport height.
   * Recomputed after every render (a hint bar appearing / disappearing changes the start position), plus once on window resize;
   * React skips the re-render by itself when `setCanvasHeight` gets an unchanged value, so it can't self-excite.
   */
  const graphHostRef = useRef<HTMLDivElement>(null);
  const [canvasHeight, setCanvasHeight] = useState(560);
  const recomputeCanvasHeight = useCallback(() => {
    const el = graphHostRef.current;
    if (!el) return;
    const top = el.getBoundingClientRect().top + window.scrollY;
    setCanvasHeight(Math.max(380, Math.round(window.innerHeight - top - 24)));
  }, []);
  useEffect(recomputeCanvasHeight);
  useEffect(() => {
    window.addEventListener('resize', recomputeCanvasHeight);
    return () => window.removeEventListener('resize', recomputeCanvasHeight);
  }, [recomputeCanvasHeight]);

  // When the edge panel closes, also drop the companion object so the next open doesn't carry the previous edge's collapsed chain.
  useEffect(() => {
    if (inspectEdge === null) setInspectEdgeView(null);
  }, [inspectEdge]);

  // ---------------------------------------------------------- URL sync
  // URL -> state (forward / back / external link)
  useEffect(() => {
    const next = decodeViewState(params.toString());
    // state now originates from the URL: this round's state->URL must yield (see `skipWrite`'s note).
    skipWrite.current = true;
    setState((prev) => (sameViewState(prev, next) ? prev : next));
  }, [params]);

  // state -> URL (write only the diff, to avoid polluting the history stack)
  useEffect(() => {
    const search = encodeViewState(state);
    // Compare only after normalizing: `encodeViewState` carries a leading `?` while `params.toString()` doesn't --
    // comparing the two directly for "the URL is already this state" is always false, so every time (including the state just
    // synced over from forward / back) pushes another history entry and the URL keeps changing in place.
    const currentSearch = params.toString();
    const derived = derivedNav.current;
    derivedNav.current = false;
    if (search.replace(/^\?/, '') === currentSearch) return;
    // This frame's state came from the URL: the URL is the source of truth, writing back would only overwrite the new URL with the old state.
    if (skipWrite.current) return;
    if (search === lastPushed.current) return;
    lastPushed.current = search;
    setParams(new URLSearchParams(search), { replace: derived });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);
  // The flag is valid only for the frame where "the URL just changed": it's cleared unconditionally at frame end and never leaks into the next write --
  // otherwise it would break subsequent writes that are genuinely needed (e.g. auto-selecting the default object).
  useEffect(() => {
    skipWrite.current = false;
  });

  // Legend filter -> URL: write the hidden node / edge kinds into `hnk` / `hek`, so a refresh / shared link still restores them.
  useEffect(() => {
    const hnk = hiddenNodeKinds.join(',');
    const hek = hiddenEdgeKinds.join(',');
    setParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (hnk) next.set('hnk', hnk);
        else next.delete('hnk');
        if (hek) next.set('hek', hek);
        else next.delete('hek');
        return next;
      },
      { replace: true },
    );
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hiddenNodeKinds, hiddenEdgeKinds]);

  // URL -> legend filter (external link / forward-back): read once on mount only.
  useEffect(() => {
    const hnk = params.get('hnk');
    const hek = params.get('hek');
    if (hnk) setHiddenNodeKinds(hnk.split(',').filter(Boolean));
    if (hek) setHiddenEdgeKinds(hek.split(',').filter(Boolean));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Perspective unspecified / invalid -> pick the first perspective that has data
  useEffect(() => {
    if (perspectives.length === 0) return;
    const fixed = reconcileViewState(
      state,
      perspectives.map((p) => ({ id: p.id, mode: p.mode, available: p.available })),
    );
    // This is a correction for "the URL is missing / has a wrong `p`", not user navigation -- use replace so it doesn't consume a history entry.
    if (!sameViewState(fixed, state)) {
      derivedNav.current = true;
      setState(fixed);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [perspectives]);

  // Load level-2 candidates (server-side live search, 200ms debounce on input). This is a **UI cache for dropdown suggestions**;
  // the cache key is "perspective + search term": the same key is reused, a new term re-requests -- otherwise after the first load, typing would
  // never reach the backend (this once broke search). It **only serves dropdown suggestions** and doesn't decide the default object (see `defaultNode`).
  // Loaded on demand: don't prefetch when a node is already selected, the dropdown is closed, and there's no search term --
  // that expensive full scoring pass is deferred until the user actually picks an object.
  useEffect(() => {
    if (!state.p || isAggregate) {
      setCandidateBundle({ p: state.p ?? '', q: '', list: [] });
      setCandidatesLoaded(false);
      // When the perspective is invalidated, the previous perspective's recommended default object is invalidated too, avoiding a wrong pick at the switch moment.
      setDefaultNode(null);
      return;
    }
    // When switching perspective, first clear the previous perspective's default object, then re-snapshot once this ranking list arrives.
    if (candidateBundle.p !== state.p) setDefaultNode(null);
    if (candidateBundle.p === state.p && candidateBundle.q === candidateSearch) return;
    if (state.n !== null && !dropdownOpen && candidateSearch === '') return;
    const perspective = state.p;
    const query = candidateSearch;
    let alive = true;
    setCandidatesLoaded(false);
    const start = () => {
      void viewApi
        .candidates(id, perspective, 300, query, singleSubId)
        .then((list) => {
          if (alive) {
            setCandidateBundle({ p: perspective, q: query, list });
            setCandidatesLoaded(true);
            // Snapshot the default object recommended by this perspective only when this is a "no-search-term ranking list".
            // One with a search term is an autocomplete result and can't be the source of the default object.
            // Store the perspective id along with it: in the frame of switching perspective, "clear default object" and "auto-select" are two effects
            // running in the same commit; storing only the id makes auto-select read the **previous perspective's** default value,
            // treating a node from another perspective as the new perspective's center (this once produced `?p=table&n=<route node>`).
            if (query === '') setDefaultNode(list[0] ? { p: perspective, id: list[0].id } : null);
          }
        })
        .catch(() => {
          if (alive) setCandidatesLoaded(true);
        });
    };
    // No search term and no node selected yet: candidates are needed to "auto-select the first object" -- get them ASAP, no debounce;
    // all other cases (the user is typing / opening the dropdown to fetch more) are debounced, so we don't hit the API on every keystroke.
    if (query === '' && state.n === null) {
      start();
      return () => {
        alive = false;
      };
    }
    const timer = setTimeout(start, 200);
    return () => {
      alive = false;
      clearTimeout(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.p, isAggregate, id, candidateSearch, dropdownOpen, state.n, candidateBundle.p, candidateBundle.q, singleSubId]);

  // Object perspective: initialize the center with the backend-recommended default object only when "no center is specified at all".
  // The default object comes from `defaultNode` (snapshotted independently when the ranking list arrives, with its owning perspective), unrelated to the dropdown's autocomplete cache;
  // never override when the user has explicitly navigated to a node -- otherwise the node just clicked would be silently swapped for a recommendation.
  /** Only the default object "belonging to the current perspective" applies: it must be invalidated the moment the perspective switches. */
  const suggestedNodeId = defaultNode && defaultNode.p === state.p ? defaultNode.id : null;
  useEffect(() => {
    if (isAggregate || state.n !== null || suggestedNodeId === null) return;
    if (candidateSearch !== '') return; // While the user is searching, don't pre-emptively pick a default object for them
    // Auto-select is "filling in a default", not user navigation -- use replace, to avoid pushing two history entries per perspective switch.
    derivedNav.current = true;
    setState((s) => ({ ...s, n: suggestedNodeId }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [suggestedNodeId, isAggregate, candidateSearch, state.n]);

  // The view is always in **collapsed mode**: when collapsed, the backend folds syntactic nodes into the edge's `via` chain and inlines each hop's call site,
  // so clicking an edge verifies hop by hop; whereas "expanding all syntactic nodes" is **information degradation** -- it draws Method / CallSite
  // but loses `via` and each hop's call site, and blows the graph into multi-layer single rows needing several horizontal scrolls.
  const { view, loading, error: objectError } = useObjectView(
    id,
    state.p ?? undefined,
    state.n ?? undefined,
    state.d,
  );
  const { view: aggView, loading: aggLoading } = useAggregateView(
    id,
    isAggregate ? (state.p ?? undefined) : undefined,
    12,
  );

  // ---------------------------------------------------------- navigation
  const pushTrail = useCallback(
    (p: string, node: number | null, nodeName: string) => {
      const label = perspectives.find((x) => x.id === p)?.label ?? p;
      setTrail((prev) => {
        const next = [...prev, { perspective: p, label, node, nodeName }];
        return next.slice(-8);
      });
    },
    [perspectives],
  );

  /** In collapsed mode, clicking a semantic node -> fetch its local syntactic call subgraph and expand it in place (click again to collapse). */
  const toggleExpand = useCallback(
    (nodeId: number) => {
      if (expanded[nodeId]) {
        setExpanded((prev) => {
          const nxt = { ...prev };
          delete nxt[nodeId];
          return nxt;
        });
        return;
      }
      setExpandingId(nodeId);
      // Take the **collapsed** subgraph as well: every edge in it carries `via` and each hop's call site, so clicking an edge expands the chain.
      // Hop count follows the current perspective's `state.d` (same depth as the center's main graph): no separate "expansion hop count",
      // otherwise one page would have two depths disagreeing and the user wouldn't know which to set.
      void viewApi
        .object(id, state.p ?? 'route', nodeId, state.d)
        .then((ov) => {
          setExpanded((prev) => ({
            ...prev,
            [nodeId]: {
              nodes: [ov.center, ...ov.rings.flat()].map(toCanvas),
              edges: ov.edges,
            },
          }));
        })
        .catch(() => {})
        .finally(() => setExpandingId((cur) => (cur === nodeId ? null : cur)));
    },
    [expanded, id, state.p, state.d],
  );

  /**
   * Clicking a node:
   * * if the node **has a matching perspective** -> **switch the level-1 perspective to it and set the level-2 object to that node** ("click to switch");
   * * if it has no matching perspective -> in collapsed mode expand the call chain in place, and only open the Inspector.
   */
  const handleNodeClick = useCallback(
    (nodeId: number, _kind: string, ownView: string | null) => {
      if (!ownView) {
        // Semantic assets like `ConfigKey` have no "single-chain" perspective: don't switch the top filter, just open the Inspector;
        // and also expand its collapsed subgraph in place (edges carry the via chain, click an edge to verify hop by hop)
        toggleExpand(nodeId);
        setInspectNode(nodeId);
        setInspectEdge(null);
        setState((s) => ({ ...s, i: nodeId, e: null }));
        return;
      }
      if (state.n !== null && state.n !== nodeId) {
        const center = view?.center;
        if (center) setOrigin({ id: center.id, name: center.name });
      }
      const nodeName =
        view?.rings.flat().find((n) => n.id === nodeId)?.name ??
        (view?.center.id === nodeId ? view.center.name : `#${nodeId}`);
      pushTrail(ownView, nodeId, nodeName);
      // Key: **the level-1 perspective must switch too**. Setting only `n` leaves that node outside the current perspective's candidates,
      // so `reconcileViewState` clears it and the navigation silently fails.
      setState((s) => ({ ...s, p: ownView, n: nodeId, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
      setExpanded({});
    },
    [state.n, view, pushTrail, toggleExpand],
  );

  const handleEdgeClick = useCallback((edge: EdgeView) => {
    setInspectEdge(edge.id);
    setInspectEdgeView(edge);
    setInspectNode(null);
  }, []);

  const onTrailClick = useCallback(
    (index: number) => {
      const item = trail[index];
      if (!item) return;
      // Clicking **the current step** is a no-op: don't close the Inspector, don't clear the `from` mark, don't push another history entry.
      // More importantly `n`: the step pushed in when switching perspective has `node` = `null`; setting it as-is would reset the center
      // to the first candidate, so "clicking itself" would look like jumping somewhere else.
      if (index === trail.length - 1 && item.perspective === state.p) return;
      setTrail((prev) => prev.slice(0, index + 1));
      setState((s) => ({ ...s, p: item.perspective, n: item.node, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
      setOrigin(null);
    },
    [trail, state.p],
  );

  // In collapsed mode, merge the "on-demand expanded syntactic subgraph" into the current semantic graph (offset by the anchor ring number to avoid reordering).
  const merged = useMemo(() => {
    if (!view) return { center: null as CanvasNode | null, rings: [] as CanvasNode[][], edges: [] as EdgeView[] };
    const base: CanvasNode[] = [view.center, ...view.rings.flat()].map(toCanvas);
    const ringOf = new Map(base.map((n) => [n.id, n.ring]));
    const nodes: CanvasNode[] = [...base];
    const edges: EdgeView[] = [...view.edges];
    const seen = new Set(nodes.map((n) => n.id));
    for (const [anchorId, ctx] of Object.entries(expanded)) {
      const anchorRing = ringOf.get(Number(anchorId)) ?? 0;
      for (const n of ctx.nodes) {
        if (seen.has(n.id)) continue;
        seen.add(n.id);
        nodes.push({ ...n, ring: anchorRing + n.ring });
      }
      for (const e of ctx.edges) edges.push(e);
    }
    const maxRing = nodes.reduce((m, n) => Math.max(m, n.ring), 0);
    const rings: CanvasNode[][] = Array.from({ length: maxRing + 1 }, () => []);
    for (const n of nodes) if (n.ring > 0) rings[n.ring].push(n);
    return { center: toCanvas(view.center), rings, edges };
  }, [view, expanded]);

  /** Endpoint id -> name: the collapsed chain must also name the first and last semantic nodes. */
  const nodeNameOf = useCallback(
    (nid: number) => {
      const c = merged.center;
      if (c && c.id === nid) return c.name;
      for (const ring of merged.rings) {
        const hit = ring.find((n) => n.id === nid);
        if (hit) return hit.name;
      }
      return `#${nid}`;
    },
    [merged],
  );

  /**
   * Fallback display name for the level-2 filter.
   *
   * "Clicking a node in the graph to switch perspective" **passes the node id directly** (not via the candidate list), while candidates are **loaded on demand**
   * (deliberately not prefetched when a node is selected and the dropdown is closed) -- so the dropdown has no option matching `value`,
   * and antd renders the raw value as a bare id (e.g. `57601`), looking like the filter is broken.
   *
   * The name comes only from **authoritative sources**: ① breadcrumb history (node names brought by navigation) ② the current view center
   * (the object actually loaded successfully, name included). **No longer infer "does the node exist / what's it called" from `candidates.some`** --
   * candidates are a capped, lazily loaded UI cache, not server authority; using them as an existence criterion is exactly the root of the earlier "bare id / misjudgment".
   * When a candidate matches, PerspectivePicker already ignores `nodeName` and uses the candidate's full label, so no special case is needed here.
   */
  const selectedNodeName = useMemo(() => {
    if (state.n === null) return null;
    // After graph navigation `view` is still the previous perspective's data (`useAsync` keeps the old value), so consult the breadcrumb before the center.
    for (let i = trail.length - 1; i >= 0; i -= 1) {
      if (trail[i].node === state.n && trail[i].nodeName) return trail[i].nodeName;
    }
    return view?.center.id === state.n ? view.center.name : null;
  }, [state.n, trail, view]);

  // Push the current position into the breadcrumb on first entry
  useEffect(() => {
    if (!state.p || trail.length > 0) return;
    // Take the name only from the authoritative source (the current view's center), never fall back to the candidate list -- candidates are a lazily loaded UI cache,
    // not an authority on a node's existence / naming; leave it empty until the view is ready, then the selected state displays it normally once `view` arrives.
    const name = view?.center?.name ?? '';
    pushTrail(state.p, state.n, name);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.p, state.n, view]);

  const clusters: CanvasCluster[] = useMemo(
    () =>
      (aggView?.clusters ?? []).map((c) => ({
        key: c.key,
        label: c.label,
        count: c.count,
        members: c.members.map((m) => ({ id: m.id, kind: m.kind, name: m.name, ring: 1 })),
      })),
    [aggView],
  );
  const matrix: CanvasMatrix | undefined = aggView?.matrix
    ? { rows: aggView.matrix.rows, cols: aggView.matrix.cols, cells: aggView.matrix.cells }
    : undefined;

  // Layout is declared per perspective in `views/perspectives.yaml` (routes = layered, resources = radial …); there is no manual override.
  const layoutMode: LayoutMode = current?.layout ?? 'radial';
  // Temporarily commented out: the local root template / WSL mode / per-project override settings are only needed for "jump to IDE",
  // and that entry has been removed; copying an absolute path can just use the backend root_path (revisit later if we bring it back).
  //
  // // Per-project override: higher priority than the global root template, for cases a template can't express; affects only IDE jump and copy, not backend data.
  // const rootStorageKey = `em.projectRootOverride.${id}`;
  // const [localRoot, setLocalRoot] = useState<string>(() => {
  //   try {
  //     return localStorage.getItem(rootStorageKey) ?? '';
  //   } catch {
  //     return '';
  //   }
  // });
  // const updateLocalRoot = (value: string) => {
  //   const trimmed = value.trim();
  //   setLocalRoot(trimmed);
  //   try {
  //     if (trimmed) localStorage.setItem(rootStorageKey, trimmed);
  //     else localStorage.removeItem(rootStorageKey);
  //   } catch {
  //     /* ignore */
  //   }
  // };
  // // A local project root affects only IDE jump and copy, not backend data.
  // // Resolution priority: per-project override (localRoot) > global root template / WSL preset (settings page) > backend root_path.
  // const projectRoot = resolveProjectRoot(project?.root_path, localRoot, effectiveTemplate());
  // // When WSL mode is on, pass the distro through to the jump / copy logic to generate the right remote scheme and UNC prefix.
  // const wslDistro = getWslMode() ? getWslDistro() : undefined;
  // // For UI verification only: which layer this project's effective root actually comes from (override > template / WSL > backend).
  // const rootSource = localRoot
  //   ? t('Per-project override')
  //   : getWslMode()
  //     ? t('WSL mode') + '（' + (getWslDistro() || 'Ubuntu') + '）'
  //     : effectiveTemplate()
  //       ? t('Global root template')
  //       : t('backend root_path');

  // Local root used for copying absolute paths: no template / WSL / override transform for now, just use the backend root_path directly.
  const projectRoot = project?.root_path;
  // WSL mapping disabled (see above); Inspector still accepts the prop, just leave it empty, and switch back to getWslMode() when re-enabling.
  const wslDistro: string | undefined = undefined;

  if (projectId === undefined || Number.isNaN(id)) {
    return <Alert type="error" message={t('Missing project ID')} />;
  }

  return (
    <>
      <PageHeader
        compact
        title={
          <>
            {project ? `${t('Code Graph')} · ${project.name}` : t('Code Graph')}
            {/*
              Usage instructions + graph coverage + build-report entry, all gathered into this ⓘ (Popover, opens on click).

              Why a Popover and not a Tooltip: it needs to hold a **clickable link**,
              a Tooltip is hover-triggered, and moving the mouse from ⓘ sideways into the overlay easily closes it;
              whereas a "click to open" overlay stays put, so the link is reachable and no hovering in mid-air is needed.

              Why hide it in an overlay instead of a permanent line: 78% of this page's content is engine limitations and expected
              behavior (the same diagnostic fires once per hundreds of files); a permanent line announces "something here needs your attention".
              But it shouldn't be fully hidden either -- so only when there's genuinely "something worth a look" do we dot the ⓘ with a small gold dot:
              the dot only says "there's something in here", takes no layout space, and doesn't lie about severity.
            */}
            <Popover
              trigger="click"
              placement="bottomLeft"
              content={
                <div style={{ maxWidth: 320, fontSize: 12 }}>
                  <div style={{ color: 'rgba(0,0,0,0.65)' }}>
                    {t('Pick a perspective, then an object; only this one link is rendered. Omitted parts are shown as counts and an unresolved tally.')}
                  </div>
                  {diagTypes > 0 ? (
                    <>
                      <Divider style={{ margin: '8px 0' }} />
                      <div style={{ color: 'rgba(0,0,0,0.65)' }}>
                        {t('Graph coverage')}：{diagTypes} {t(' problem types')} · {t('Total ')}
                        {diagTotal}
                        {t(' places')}
                      </div>
                      {diagActionable > 0 ? (
                        <Tag color="gold" style={{ marginTop: 6, marginInlineEnd: 0 }}>
                          {t('Worth a look')} {diagActionable}
                        </Tag>
                      ) : (
                        <div style={{ marginTop: 4, color: 'rgba(0,0,0,0.45)' }}>
                          {t('— mostly engine limits and expected cases; recall conclusions are unaffected.')}
                        </div>
                      )}
                      <div style={{ marginTop: 8 }}>
                        <Button
                          type="link"
                          size="small"
                          style={{ paddingInline: 0, fontSize: 12, height: 'auto' }}
                          onClick={() => navigate(`/projects/${id}/coverage`)}
                        >
                          {t('View build report')} →
                        </Button>
                      </div>
                    </>
                  ) : null}
                </div>
              }
            >
              {/* Light up only when there's "something worth a look": a dot is cheaper than text and more honest than a "red badge" --
                       it only means "there's something inside", not "it's severe" */}
              <Badge dot={diagActionable > 0} color="#faad14" offset={[-2, 4]}>
                <InfoCircleOutlined
                  style={{ fontSize: 13, color: 'rgba(0,0,0,0.35)', marginLeft: 6, cursor: 'pointer' }}
                />
              </Badge>
            </Popover>
          </>
        }
        extra={<RunPipelineButton projectId={id} onStarted={() => void reloadProject()} />}
      />

      {/* Degradation note for unsupported languages: these sub-projects have file structure only, no semantic extraction.
              Without saying so, a user facing a near-empty graph will think "the project itself has nothing", not "the tool doesn't support it". */}
      {unsupportedLangs.length > 0 && (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 10 }}
          message={t('This project contains languages with no parser yet')}
          description={t('For these languages only file structure is built — no class / function / call extraction') + `：${unsupportedLangs.join('、')}`}
        />
      )}

      {/* The graph-coverage row is **not put on the page**: it now lives in the ⓘ Popover beside the title (see `PageHeader` above).
              Only the "languages with no parser" line stays here -- that one genuinely makes people misread the graph as empty, so it must be stated outright. */}

      <Card
        variant="borderless"
        style={{ borderRadius: 14, marginBottom: 10 }}
        styles={{ body: { padding: '8px 12px' } }}
      >
        <PerspectivePicker
          perspectives={perspectives}
          perspective={state.p}
          onPerspectiveChange={(p) => {
            pushTrail(p, null, '');
            setCandidateSearch('');
            setState((s) => ({ ...s, p, n: null, i: null, e: null }));
            setOrigin(null);
          }}
          candidates={candidates}
          node={state.n}
          onNodeChange={(n) => {
            const name = candidates.find((c) => c.id === n)?.name ?? '';
            pushTrail(state.p ?? '', n, name);
            setCandidateSearch('');
            setState((s) => ({ ...s, n, i: null, e: null }));
          }}
          onSearch={setCandidateSearch}
          onOpenChange={setDropdownOpen}
          searchText={candidateSearch}
          nodeName={selectedNodeName}
          trail={trail}
          onTrailClick={onTrailClick}
          loading={loading && candidates.length === 0}
          extra={
            <>
              {/* Iconified + tooltip: the saved width goes to the breadcrumb, so this row doesn't wrap and push the canvas down */}
              <Tooltip title={t('Reset zoom and pan so the whole graph fits the current viewport')}>
                <Button size="small" icon={<FullscreenOutlined />} onClick={() => setFitSignal((s) => s + 1)} />
              </Tooltip>
              <Button size="small" type="primary" ghost onClick={() => setDrawerOpen(true)}>
                {t('Conclusions / Navigation')}
              </Button>
              {Object.keys(expanded).length > 0 && (
                <Button size="small" onClick={() => setExpanded({})}>
                  {t('Collapse calls') + '（' + Object.keys(expanded).length + '）'}
                </Button>
              )}
              <Select
                mode="multiple"
                allowClear
                size="small"
                style={{ minWidth: 200 }}
                placeholder={t('All sub-projects')}
                value={subFilter}
                onChange={(v) => setSubFilter(v ?? [])}
                options={subProjects.map((s) => ({
                  label: `${s.name}（${roleLabel(s.role)}）`,
                  value: s.id,
                }))}
              />
            </>
          }
        />

        {/*
          The "only draws one chain" sentence moves **one step further out** from the ⓘ overlay beside the title.

          Why: a newcomer's biggest first question isn't "what did graph coverage miss", but
          "my project is huge, why does the graph have only these few nodes?" -- that question must be answered where it's asked,
          i.e. **right under the control that decides what this graph draws**, not hidden in a hover/click overlay.
          The ⓘ still keeps the full explanation (including the "level 1 = perspective, level 2 = object" steps and the coverage entry);
          only the one-line conclusion stays here, to avoid repeating the same whole paragraph in two places.

          Aggregate perspectives don't show it: they draw clusters / matrices, not "one chain", so saying it would mislead.
        */}
        {!isAggregate ? (
          <Typography.Text type="secondary" style={{ display: 'block', marginTop: 6, fontSize: 12 }}>
            {t('Draws one link at a time; the rest is shown as counts and unresolved records.')}
          </Typography.Text>
        ) : null}

        {/* Temporarily commented out: the IDE-open entry has been removed; the per-project local root override and the "currently effective root" display are disabled along with it (revisit later).
        <Collapse
          ghost
          bordered={false}
          defaultActiveKey={[]}
          style={{ marginTop: 8, maxWidth: 760 }}
          items={[
            {
              key: 'override',
              label: (
                <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                  {t('Advanced · Per-project local root override (only for special cases)')}
                </Typography.Text>
              ),
              children: (
                <Space align="center" wrap>
                  <Tooltip title={t('Local project root is only for IDE jumping and copying, not for changing backend data. Priority: this "per-project override" > global root template (Settings) > backend root_path. Empty falls back to the latter two.')}>
                    <Typography.Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                      {t('Local project root (override)')}
                    </Typography.Text>
                  </Tooltip>
                  <Input
                    size="small"
                    style={{ width: 420 }}
                    placeholder={project?.root_path ?? t('Absolute local path override; empty uses global template / backend path')}
                    value={localRoot}
                    onChange={(e) => updateLocalRoot(e.target.value)}
                  />
                  {localRoot ? (
                    <Button size="small" type="link" onClick={() => updateLocalRoot('')}>
                      {t('Use global / backend path')}
                    </Button>
                  ) : null}
                  <Button size="small" type="link" onClick={() => navigate('/settings')}>
                    {t('Global root template settings')}
                  </Button>
                </Space>
              ),
            },
          ]}
        />
        <Typography.Paragraph
          type="secondary"
          style={{ fontSize: 12, marginTop: 4, marginBottom: 0 }}
        >
          {t('Effective root for this project (source: ') + rootSource + t('）：')}
          <Typography.Text code style={{ fontSize: 12 }}>
            {projectRoot || t('(unresolvable; check backend root_path or the override above)')}
          </Typography.Text>
        </Typography.Paragraph>
        */}
      </Card>

      {isAggregate && aggView?.notice ? (
        <Alert type="info" showIcon style={{ marginBottom: 16 }} message={aggView.notice} />
      ) : null}

      {/* Honesty gate: when this object can't be fetched under the current perspective, say so and offer a way out,
               rather than silently swapping the center to the first candidate (that would be showing an unrelated graph). */}
      {!isAggregate && state.n !== null && !loading && objectError ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          message={t('Object #') + state.n + t(' in perspective ') + (current?.label ?? state.p) + t(' has no link under this perspective')}
          description={t('It may have been deleted, or not belong to this perspective (') + errText(objectError) + t('). Please re-select from the left perspective.')}
          action={
            candidates.length > 0 ? (
              <Button size="small" onClick={() => setState((s) => ({ ...s, n: suggestedNodeId ?? candidates[0]?.id }))}>
                {t('Use first object')}
              </Button>
            ) : null
          }
        />
      ) : null}

      <Row gutter={[16, 16]}>
        <Col xs={24} xl={24}>
          {/* Measure the high anchor: see `recomputeCanvasHeight` above */}
          <div ref={graphHostRef} />
          <GraphCanvas
            height={canvasHeight}
            mode={layoutMode}
            center={merged.center}
            rings={merged.rings}
            edges={merged.edges}
            clusters={isAggregate ? clusters : undefined}
            matrix={isAggregate ? matrix : undefined}
            loading={loading || aggLoading || expandingId !== null || pendingAutoSelect}
            originId={origin?.id ?? null}
            selectedId={state.n}
            onNodeClick={handleNodeClick}
            onNodeContextMenu={(nodeId) => {
              setInspectNode(nodeId);
              setInspectEdge(null);
              setState((s) => ({ ...s, i: nodeId, e: null }));
            }}
            onEdgeClick={handleEdgeClick}
            showEdgeLabels
            fitKey={fitKey}
            fitSignal={fitSignal}
            subProjects={subProjects}
            subFilter={subFilter}
            hiddenNodeKinds={hiddenNodeKinds}
            hiddenEdgeKinds={hiddenEdgeKinds}
            onToggleNodeKind={toggleNodeKind}
            onToggleEdgeKind={toggleEdgeKind}
            onResetLegendFilters={resetLegendFilters}
            />

          {/* Entry-type perspectives (routes / scheduled tasks) explain when there's no chain, avoiding the "blank screen = broken" impression */}
          {view && view.conclusions['hint'] ? (
            <Alert
              type="info"
              showIcon
              style={{ marginTop: 16 }}
              message={String(view.conclusions['hint'])}
            />
          ) : null}

          {/* Honesty gate: what was omitted, and why.
                   The mechanism must stay (never omit silently), but the presentation is compressed to one line -- the full template sentence
                   is identical every time and only the number changes, so from the third read on it's noise; the number comes straight from the structured field. */}
          {view ? (
            <div
              style={{
                marginTop: 12,
                fontSize: 12,
                color: 'rgba(0,0,0,0.45)',
                display: 'flex',
                gap: 8,
                flexWrap: 'wrap',
                alignItems: 'center',
              }}
            >
              <span>
                {t('Drawn: ') + view.hidden.shown + t(' edges; folded ') + (view.hidden.total - view.hidden.shown) + t(' syntax nodes')}
              </span>
              {Object.entries(view.hidden.by_kind).map(([k, v]) => (
                <Tag key={k} style={{ marginInlineEnd: 0 }}>
                  {k} {v}
                </Tag>
              ))}
              <span>{t('Click any edge to inspect every hop and call site along its path')}</span>
            </div>
          ) : null}

          {/* Orphan direct-access accounting: the accessor is a syntactic node and tracing up finds no semantic entry (CLI / scheduled /
                   event handlers are the norm). It **doesn't occupy the canvas** -- syntactic node names aren't addressable, don't answer "who triggered it",
                   and would eat the canvas budget meant for semantic nodes; but it is **never silently omitted**: a separate count + per-item locations,
                   click a name to open Inspector and keep verifying. */}
          {view && (view.orphans?.length ?? 0) > 0 ? (
            <div style={{ marginTop: 6, fontSize: 12, color: 'rgba(0,0,0,0.45)' }}>
              <Button
                type="link"
                size="small"
                style={{ padding: 0, height: 'auto', fontSize: 12 }}
                onClick={() => setOrphansOpen((v) => !v)}
              >
                {t('Also ') + (view.orphans?.length ?? 0) + t(' direct accesses with no semantic entry (CLI / cron / event) — listed here, not drawn')}
                {orphansOpen ? ' ▾' : ' ▸'}
              </Button>
              {orphansOpen ? (
                <div style={{ marginTop: 6, display: 'flex', flexDirection: 'column', gap: 2 }}>
                  {(view.orphans ?? []).map((o) => (
                    <div key={o.id} style={{ display: 'flex', gap: 6, alignItems: 'baseline' }}>
                      <Typography.Text
                        style={{ fontSize: 12, cursor: 'pointer' }}
                        onClick={() => {
                          // Pull out a local variable first: accessing `o.edge` inside a `setState` callback,
                          // TS narrowing doesn't cross into the closure (property narrowing is lost inside the callback), so it reports a possible null.
                          const oe = o.edge;
                          if (oe) {
                            // This direct access is itself a clickable, expandable semantic edge (e.g. an event trigger point):
                            // open the edge evidence-chain drawer and verify the call process hop by hop, rather than only opening node details.
                            setInspectNode(null);
                            setInspectEdge(oe.id);
                            setInspectEdgeView(oe);
                            setState((s) => ({ ...s, i: null, e: oe.id }));
                          } else {
                            setInspectNode(o.id);
                            setInspectEdge(null);
                            setState((s) => ({ ...s, i: o.id, e: null }));
                          }
                        }}
                      >
                        {o.name || `#${o.id}`}
                      </Typography.Text>
                      <Tag style={{ marginInlineEnd: 0 }}>{edgeKindLabel(t, o.edge_kind)}</Tag>
                      {o.location ? (
                        <Typography.Text type="secondary" style={{ fontSize: 11 }}>
                          {`${o.location.file}:${o.location.line}`}
                        </Typography.Text>
                      ) : null}
                    </div>
                  ))}
                </div>
              ) : null}
            </div>
          ) : null}

          {view && view.unresolved.length > 0 ? (
            <Card
              variant="borderless"
              style={{ borderRadius: 14, marginTop: 16 }}
              size="small"
              title={t('Unresolved tally')}
              extra={<Tag color="orange">{view.unresolved.length}</Tag>}
            >
              <Table
                size="small"
                rowKey={(_, i) => String(i)}
                dataSource={view.unresolved}
                pagination={false}
                columns={[
                  { title: t('Code'), dataIndex: 'code', width: 170 },
                  { title: t('Description'), dataIndex: 'message' },
                  { title: t('Location'), dataIndex: 'location', width: 220, ellipsis: true },
                ]}
              />
            </Card>
          ) : null}
        </Col>
      </Row>

      <Drawer
        title={t('Conclusions / navigation')}
        placement="right"
        width={360}
        open={drawerOpen}
        onClose={() => setDrawerOpen(false)}
        styles={{ body: { padding: 16 } }}
      >
        {/* "Conclusions" gets no card of its own: the two big numbers (in-edges / out-edges) are countable at a glance on the graph
                  (the ring node titles state them too); a big-number card costs ~150px just to say two sentences. Compressed into the ring
                  node card's extra + one bottom line; only things with incremental information (annotations / schema column count / route-table registration) qualify. */}
        <Card
          variant="borderless"
          size="small"
          title={t('Nodes on rings')}
          extra={
            view ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('In-edges ') +
                  fmt(view.conclusions['in_edges']) +
                  ' · ' +
                  t('Out-edges ') +
                  fmt(view.conclusions['out_edges']) +
                  // Orphan direct accesses: they don't occupy the canvas, but keep a checkable number in the "conclusions" section.
                  (view.conclusions['other_direct_access']
                    ? ' · ' + t('orphan access: ') + fmt(view.conclusions['other_direct_access'])
                    : '')}
              </Typography.Text>
            ) : aggView ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('Groups ') + aggView.clusters.length}
              </Typography.Text>
            ) : null
          }
        >
          {view ? (
            <Space direction="vertical" size={8} style={{ width: '100%' }}>
              {(view?.rings ?? []).map((ring, i) => (
                <div key={i}>
                  <Typography.Text strong style={{ fontSize: 12 }}>
                    {t('Ring') + (i + 1) + '（' + ring.length + '）'}
                  </Typography.Text>
                  <div style={{ display: 'flex', flexWrap: 'wrap', gap: 4, marginTop: 4 }}>
                    {ring.slice(0, 12).map((n) => (
                      <Tag
                        key={n.id}
                        color={n.has_own_view ? 'blue' : 'default'}
                        style={{ cursor: 'pointer' }}
                        onClick={() => handleNodeClick(n.id, n.kind, n.own_view)}
                      >
                        {n.name.slice(0, 24)}
                      </Tag>
                    ))}
                    {ring.length > 12 ? <Tag>+{ring.length - 12}</Tag> : null}
                  </div>
                </div>
              ))}
              {Array.from(new Set(asArray(view.conclusions['annotations']))).length > 0 ||
              view.conclusions['schema_columns'] !== undefined ||
              view.conclusions['middleware'] ||
              view.conclusions['route_registered'] ? (
                <Space size={6} wrap style={{ marginTop: 4 }}>
                  {Array.from(new Set(asArray(view.conclusions['annotations']))).map((a) => (
                    <Tag key={a} color="volcano" style={{ marginInlineEnd: 0 }}>
                      {a}
                    </Tag>
                  ))}
                  {view.conclusions['schema_columns'] !== undefined ? (
                    <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                      {t('Schema columns: ') + String(view.conclusions['schema_columns'])}
                    </Typography.Text>
                  ) : null}
                  {view.conclusions['middleware'] ? (
                    <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                      {t('Middleware: ') + String(view.conclusions['middleware'])}
                    </Typography.Text>
                  ) : null}
                  {view.conclusions['route_registered'] ? (
                    <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                      {t('Route-registered handler: ') + String(view.conclusions['route_registered'])}
                    </Typography.Text>
                  ) : null}
                </Space>
              ) : null}
            </Space>
          ) : aggView ? (
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {aggView.matrix
                ? t('Total ') + formatNumber(aggView.matrix.cells.flat().reduce((a, b) => a + b, 0)) + t(' cell values')
                : t('Select an object to see conclusions')}
            </Typography.Text>
          ) : (
            <Typography.Text type="secondary">{t('Select an object to see conclusions')}</Typography.Text>
          )}
        </Card>

        <div style={{ marginTop: 16 }}>
          <PipelineProgress run={run} indexing={project?.status === 'indexing'} />
        </div>
      </Drawer>

      <Inspector
        nodeId={inspectNode}
        edgeId={inspectEdge}
        edgeView={inspectEdgeView}
        nodeNameOf={nodeNameOf}
        projectRoot={projectRoot}
        wslDistro={wslDistro}
        onClose={() => {
          setInspectNode(null);
          setInspectEdge(null);
          setState((s) => ({ ...s, i: null, e: null }));
        }}
      />
    </>
  );
}

function toCanvas(n: {
  id: number;
  kind: string;
  name: string;
  ring: number;
  category?: string | null;
  own_view?: string | null;
  /** The "side" the node belongs to: `frontend` / `backend` (annotated by FKB). Used to tell frontend / backend sub-projects apart on the graph. */
  side?: string | null;
  /** The sub-project id the node belongs to (backend `NodeView.sub_project_id`). Graph coloring / filtering is per sub-project. */
  sub_project_id?: number | null;
  /** Info to show in the hover card; fall back to empty values when missing. */
  fqn?: string | null;
  locations?: SourceLocation[];
  annotations?: string[];
  metrics?: { fan_in?: number; fan_out?: number } | null;
}) {
  return {
    id: n.id,
    kind: n.kind,
    category: n.category ?? null,
    own_view: n.own_view ?? null,
    side: n.side ?? null,
    sub_project_id: n.sub_project_id ?? null,
    name: n.name,
    ring: n.ring,
    fqn: n.fqn ?? null,
    locations: n.locations ?? [],
    annotations: n.annotations ?? [],
    metrics: n.metrics ?? null,
  };
}

/// /// Sub-project role -> default perspective: after picking a sub-project, jump there automatically.
/// /// Backend: worker -> scheduled-task perspective, everything else (api / bff / admin …) -> route perspective;
/// /// Frontend: fall back to the route perspective for now (the registry has no frontend-specific perspective yet; see the Page perspective in perspectives.yaml, parked for the MVP).
function defaultPerspectiveForRole(role?: string | null): string | undefined {
  if (!role) return undefined;
  if (role.startsWith('backend:worker')) return 'schedule';
  if (role.startsWith('backend')) return 'route';
  if (role.startsWith('frontend')) return 'route';
  return undefined;
}

function fmt(v: unknown): string {
  return typeof v === 'number' ? formatNumber(v) : String(v ?? '-');
}

function asArray(v: unknown): string[] {
  return Array.isArray(v) ? v.map(String) : [];
}

function errText(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  return 'Request failed';
}
