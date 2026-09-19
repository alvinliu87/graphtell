import {
  Alert,
  Button,
  Card,
  Col,
  Collapse,
  Drawer,
  Input,
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
// 暂时注释：IDE 打开入口已移除，无需再处理「后端根 → 本地根」映射（根模板 / WSL / 按工程覆盖）。
// 以后再考虑加回时恢复此 import。
// import {
//   effectiveTemplate,
//   getWslDistro,
//   getWslMode,
//   resolveProjectRoot,
// } from '@/shared/lib/ide';
import { formatNumber } from '@/shared/lib/format';
import { useLocale } from '@/shared/lib/i18n';
import { FullscreenOutlined, InfoCircleOutlined } from '@ant-design/icons';

/** 图视图页：两级筛选 → 单对象链路子图 → 可跳转的结论面板。 */
export function GraphPage() {
  const { projectId } = useParams();
  const id = Number(projectId);
  const navigate = useNavigate();
  const { t } = useLocale();
  const [params, setParams] = useSearchParams();
  const { project, reload: reloadProject } = useProject(id);
  const { run } = useRunStatus(id, project?.status);

  const { perspectives } = usePerspectives(id);

  /** 现场（URL 是唯一真源）。 */
  const [state, setState] = useState<ViewState>(() => decodeViewState(params.toString()));
  /** 面包屑：记录导航过的视角，可回退。 */
  const [trail, setTrail] = useState<BreadcrumbItem[]>([]);
  /** 上一个中心，切视角后保留为邻居并标记 `from`。 */
  const [origin, setOrigin] = useState<{ id: number; name: string } | null>(null);
  /** 候选列表连同它所属的视角一起存：切视角后必须立刻失效，
   *  否则会拿上一视角的列表来填空默认值 / 判断"节点是否存在"。 */
  const [candidateBundle, setCandidateBundle] = useState<{ p: string; q: string; list: Candidate[] }>({
    p: '',
    q: '',
    list: [],
  });
  /** 当前视角的候选是否已从后端返回：用于判断"未选节点"时是「还在加载」还是「确实没有任何候选」。 */
  const [candidatesLoaded, setCandidatesLoaded] = useState(false);
  /**
   * 当前视角下「后端推荐的默认对象」id——独立于下拉的自动补全缓存。
   *
   * 与 `candidateBundle` 解耦：默认对象来自后端对**全量候选**的"语义依赖价值"排序
   * （无搜索词那次 `candidates` 请求），是视图层派生语义；而 `candidateBundle` 是
   * 「自动补全查询」的客户端缓存。两者语义不同、不应混用——这里在排名列表到达时
   * 单独快照一次，后续搜索 / 展开下拉都不会改写它，彻底断开
   * "默认对象 = 自动补全缓存的第一项" 这种前后端语义混用。
   */
  const [defaultNode, setDefaultNode] = useState<{ p: string; id: number } | null>(null);
  /** 只有"属于当前视角"的候选才生效；切换视角的瞬间派生为空，等新视角候选到达后才有值。 */
  const candidates = candidateBundle.p === state.p ? candidateBundle.list : [];
  const [candidateSearch, setCandidateSearch] = useState('');
  /** 二级对象下拉是否展开：候选只在展开时才去后端取（按需加载）。
   *  后端 candidates 在无搜索词时会取全量候选做"语义依赖价值"排序，但已改为整图预加载后
   *  在内存里跑 BFS（不再逐节点查库），很快；有搜索词时直接按名称返回、不打分。
   *  这里仍按需加载：已选中节点且未展开下拉时绝不预取，避免每次切视角都无谓打一次。 */
  const [dropdownOpen, setDropdownOpen] = useState(false);
  /** 折叠模式下，点击节点后按需展开显示的语法子图（按节点 id 归集）。 */
  const [expanded, setExpanded] = useState<Record<number, { nodes: CanvasNode[]; edges: EdgeView[] }>>({});
  const [expandingId, setExpandingId] = useState<number | null>(null);
  const [inspectNode, setInspectNode] = useState<number | null>(state.i);
  const [inspectEdge, setInspectEdge] = useState<number | null>(state.e);
  /** 点击的那条边本身。合成边的折叠链（`via`）只存在于当次视图结果里，按 id 重查拿不到，
   *  所以必须点击时随身带入 Inspector，否则"途经 N 跳"就会变成一句空话。 */
  const [inspectEdgeView, setInspectEdgeView] = useState<EdgeView | null>(null);
  /** 右侧"结论/导航"面板：默认收起为抽屉浮层，不占用图的横向空间。 */
  const [drawerOpen, setDrawerOpen] = useState(false);
  const lastPushed = useRef<string>('');
  /**
   * 下一次写 URL 是否要**跳过**：state 刚从 URL 同步过来时不许回写。
   *
   * 两个 effect 的触发时机不同（`[params]` vs `[state]`），存在"URL 与 state 同帧各变一次"
   * 的窗口：此时 state→URL 用的还是**同步前**的旧 state，写回去就把刚到达的新 URL 覆盖掉，
   * 而覆盖后的 URL 又会被 URL→state 同步成旧 state —— 两者互踩、永远差一拍，
   * 表现就是切视角后 URL 在两个值之间无限来回跳。
   */
  const skipWrite = useRef(false);
  /**
   * 下一次写 URL 是否用 `replace`（不占历史记录）。
   *
   * 只有**用户主动导航**（切视角 / 选对象 / 点面包屑 / 点图）才配占一条历史记录；
   * 由状态自己"补出来"的更新（URL 缺 `p` 时的修正、未指定对象时自动选中默认对象）
   * 一律 replace —— 否则切一次视角就压两条记录（先 `?p=X` 再 `?p=X&n=Y`），
   * 返回键退到 `?p=X` 又会被重新推一次、再自动补 `n`，URL 看起来就在原地反复跳。
   */
  const derivedNav = useRef(false);

  const current = perspectives.find((p) => p.id === state.p) ?? null;
  const isAggregate = current?.mode === 'aggregate';
  /** 未选节点、且候选尚未就绪 / 即将自动选中第一个时：图区应显示 spinner 而非空态，
   *  否则首屏与切视角会先闪一下「该视角下暂无可展示的对象」。 */
  const pendingAutoSelect =
    !isAggregate && state.n === null && (candidates.length > 0 || !candidatesLoaded);

  // 图的语义内容标识：仅「切换视角 / 选中对象 / 切聚合视图」这类导航动作会改变它，
  // 用于触发 GraphCanvas 重新 fit。单节点就地展开、悬浮、手动缩放平移不计入。
  const fitKey = isAggregate ? `agg:${state.p}` : `obj:${state.p ?? ''}:${state.n ?? ''}`;
  /** 手动「适应屏幕」信号：每次 +1 即让 GraphCanvas 重置为整图 fit。 */
  const [fitSignal, setFitSignal] = useState(0);
  /** 子工程过滤：按 `sub_project_id` 多选显示（空数组 = 全部）。多个前端 / 后端各自成一类。 */
  const [subFilter, setSubFilter] = useState<number[]>([]);
  /** 子工程列表（id / name / role），供过滤器与画布着色 / 图例使用。 */
  const { data: subProjectsData } = useAsync(() => projectApi.subProjects(id), [id]);
  const subProjects: SubProject[] = subProjectsData ?? [];
  const TIER_LABEL: Record<string, string> = {
    frontend: t('前端'),
    backend: t('后端'),
    library: t('库'),
    unknown: t('未知'),
  };
  const KIND_LABEL: Record<string, string> = {
    admin: t('管理后台'),
    'mini-program': t('小程序'),
    mobile: t('移动端'),
    h5: t('H5'),
    api: t('API'),
    worker: t('任务/队列'),
    bff: t('BFF'),
    web: t('Web'),
  };
  const roleLabel = (r?: string | null) => {
    if (!r) return t('未知');
    const [tier, kind] = r.split(':');
    if (kind) return KIND_LABEL[kind] ?? kind;
    return TIER_LABEL[tier] ?? tier;
  };

  // 子项目作为「上层维度」：仅当选中「单一」子工程时，自动跳到它最相关的默认视角与对象；
  // 多选 / 空选只做画布过滤（不切视角），避免频繁切换打断浏览。
  const singleSubId = subFilter.length === 1 ? subFilter[0] : undefined;
  const singleSub = singleSubId !== undefined ? subProjects.find((s) => s.id === singleSubId) : undefined;
  useEffect(() => {
    if (!singleSub) return;
    const pid = defaultPerspectiveForRole(singleSub.role);
    if (!pid) return;
    setState((s) => ({ ...s, p: pid, n: null, i: null, e: null }));
    // 仅依赖 subFilter：切换视角（state.p 变化）不应再次触发，否则会循环。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [subFilter]);

  /**
   * 画布高度按**视口剩余空间**自适应。
   *
   * 图区上方那块（页头 + 筛选行 + 可能插入的提示条）高度是动态的，写死 720 会让
   * 首屏必须下滑才能看全图。这里量出图区在文档中的起始位置，把视口剩下的高度全给画布。
   * 每次渲染后重算一次（提示条出现 / 消失都会改变起始位置），窗口缩放时再补一次；
   * `setCanvasHeight` 值不变时 React 会自行跳过重渲染，不会自激。
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

  // 边面板关闭时同步丢弃随身边对象，避免下次打开残留上一条边的折叠链。
  useEffect(() => {
    if (inspectEdge === null) setInspectEdgeView(null);
  }, [inspectEdge]);

  // ---------------------------------------------------------- URL 同步
  // URL → state（前进 / 后退 / 外部链接）
  useEffect(() => {
    const next = decodeViewState(params.toString());
    // state 的来源变成 URL 了：本轮 state→URL 必须让路（见 `skipWrite` 的说明）。
    skipWrite.current = true;
    setState((prev) => (sameViewState(prev, next) ? prev : next));
  }, [params]);

  // state → URL（只写差异，避免污染历史栈）
  useEffect(() => {
    const search = encodeViewState(state);
    // 口径必须一致再比：`encodeViewState` 带前导 `?`，而 `params.toString()` 没有 ——
    // 直接拿两者相等去判断"URL 已经是这个状态"永远为假，于是每次（包括前进 / 后退
    // 刚同步过来的状态）都会再推一条历史记录，URL 就在原地反复变。
    const currentSearch = params.toString();
    const derived = derivedNav.current;
    derivedNav.current = false;
    if (search.replace(/^\?/, '') === currentSearch) return;
    // 这一帧 state 是 URL 同步来的：URL 才是真源，写回去只会把新 URL 覆盖成旧 state。
    if (skipWrite.current) return;
    if (search === lastPushed.current) return;
    lastPushed.current = search;
    setParams(new URLSearchParams(search), { replace: derived });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state]);
  // 标记只在「URL 刚变」的那一帧有效：帧末无条件清掉，绝不泄漏到下一次写入 ——
  // 否则会误伤随后真正需要写入的更新（如自动选中默认对象）。
  useEffect(() => {
    skipWrite.current = false;
  });

  // 视角未指定 / 非法 → 选第一个有数据的视角
  useEffect(() => {
    if (perspectives.length === 0) return;
    const fixed = reconcileViewState(
      state,
      perspectives.map((p) => ({ id: p.id, mode: p.mode, available: p.available })),
    );
    // 这是"URL 缺 / 错了 `p`"的修正，不是用户导航 —— 用 replace，不占历史记录。
    if (!sameViewState(fixed, state)) {
      derivedNav.current = true;
      setState(fixed);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [perspectives]);

  // 载入二级候选（服务端实时搜索，输入防抖 200ms）。这是**下拉建议的 UI 缓存**，
  // 缓存键是「视角 + 搜索词」：同键直接复用，换词就重新请求——否则首次加载后输入永
  // 远打不进后端（曾因此搜索失效）。它**只服务于下拉建议**，不决定默认对象（见 `defaultNode`）。
  // 按需加载：已选中节点、下拉未展开且无搜索词时不预取，
  // 把那次昂贵的全量打分推迟到用户真正要选对象时。
  useEffect(() => {
    if (!state.p || isAggregate) {
      setCandidateBundle({ p: state.p ?? '', q: '', list: [] });
      setCandidatesLoaded(false);
      // 视角失效时，旧视角的推荐默认对象也一并作废，避免切换瞬间误选上一个视角的节点。
      setDefaultNode(null);
      return;
    }
    // 切换视角时先清掉上一个视角的默认对象，等本次排名列表到达再重新快照。
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
            // 仅当这是「无搜索词的排名列表」时，快照本次视角推荐的默认对象。
            // 有搜索词的是自动补全结果，不能当成默认对象的来源。
            // 连视角 id 一起存：切视角那一帧"清默认对象"和"自动选中"是同一次提交里
            // 跑的两个 effect，只存 id 的话自动选中会读到**上一视角**的默认值，
            // 把别的视角的节点当成新视角的中心（曾导致 `?p=table&n=<路由节点>`）。
            if (query === '') setDefaultNode(list[0] ? { p: perspective, id: list[0].id } : null);
          }
        })
        .catch(() => {
          if (alive) setCandidatesLoaded(true);
        });
    };
    // 无搜索词且尚未选中节点：候选要用来「自动选中第一个对象」——尽快拿到，不防抖；
    // 其余情况（用户正在输入 / 展开下拉补拉）一律防抖，避免每个按键都打一次接口。
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

  // 对象视角：只在「完全没指定中心」时，用后端推荐的默认对象初始化中心。
  // 默认对象来自 `defaultNode`（排名列表到达时独立快照，带所属视角），与下拉的自动补全缓存无关；
  // 已经明确导航到某个节点时**绝不覆盖**——否则会把刚点进来的节点静默换成推荐项。
  /** 只有"属于当前视角"的默认对象才生效：切视角那一帧它必须立刻失效。 */
  const suggestedNodeId = defaultNode && defaultNode.p === state.p ? defaultNode.id : null;
  useEffect(() => {
    if (isAggregate || state.n !== null || suggestedNodeId === null) return;
    if (candidateSearch !== '') return; // 用户正在搜索时，不抢先替他选默认对象
    // 自动选中是"补默认值"，不是用户导航 —— 用 replace，避免切一次视角压两条历史记录。
    derivedNav.current = true;
    setState((s) => ({ ...s, n: suggestedNodeId }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [suggestedNodeId, isAggregate, candidateSearch, state.n]);

  // 视图恒为**折叠模式**：折叠时后端会把语法节点收进边的 `via` 链并内联每一跳的调用处，
  // 点边即可逐跳核对；而"展开全部语法节点"是**信息降级**——画了 Method/CallSite，
  // 却丢掉了 via 与每跳调用处，还把图撑成多层单行、要横向滚好几屏。
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

  // ---------------------------------------------------------- 导航
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

  /** 折叠模式下点击语义节点 → 拉取其局部语法调用子图并就地展开（再次点击收起）。 */
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
      // 同样取**折叠**子图：子图里每条边都带 `via` 与每跳调用处，点边即可展开链路。
      // 跳数沿用当前视角的 `state.d`（与中心主图同深度）：不再单开一个"展开跳数"，
      // 否则同一个页面里两套深度各说各话，用户也不知道该填几。
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
   * 点击节点：
   * * 该节点**有对应视角** → **一级视角切到它、二级对象设为该节点**（"点击即切"）；
   * * 没有对应视角 → 折叠模式下就地展开调用链，并只打开 Inspector。
   */
  const handleNodeClick = useCallback(
    (nodeId: number, _kind: string, ownView: string | null) => {
      if (!ownView) {
        // `ConfigKey` 等语义资产没有"单链路"视角：不切顶部筛选器，只打开 Inspector；
        // 顺带就地展开它的折叠子图（边带 via 链，点边可逐跳核对）
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
      // 关键：**一级视角也要切**。只设 `n` 的话该节点不属于当前视角的候选，
      // 会被 `reconcileViewState` 清空，导航实际失效。
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
      // 点**当前这一步**是空操作：既不关 Inspector、不清 `from` 标记，也不多压一条历史记录。
      // 更要紧的是 `n` —— 切视角压进来的那一步 `node` 是 `null`，照原样 set 会把中心
      // 重置成第一个候选，于是"点自己"看起来像跳到了别处。
      if (index === trail.length - 1 && item.perspective === state.p) return;
      setTrail((prev) => prev.slice(0, index + 1));
      setState((s) => ({ ...s, p: item.perspective, n: item.node, i: null, e: null }));
      setInspectNode(null);
      setInspectEdge(null);
      setOrigin(null);
    },
    [trail, state.p],
  );

  // 折叠模式下，把"按需展开的语法子图"合并进当前语义图（按锚点环号偏移，避免重排）。
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

  /** 端点 id → 名字：折叠链要把首尾两个语义节点也标出名字。 */
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
   * 二级筛选器的显示名兜底。
   *
   * 「点击图中的节点切视角」是**直接给节点 id**（不经候选列表），而候选是**按需加载**的
   * （已选中节点且未展开下拉时刻意不预取）—— 于是下拉里找不到匹配 `value` 的选项，
   * antd 会把 value 原样渲染成裸 id（如 `57601`），看起来像筛选器坏了。
   *
   * 名字只从**权威来源**取：① 面包屑历史（导航带来的节点名）② 当前视图中心
   * （真正加载成功的对象，含名字）。**不再用 `candidates.some` 推断"节点是否存在/叫什么"**——
   * 候选是带上限、按需加载的 UI 缓存，不是服务端权威；拿它当存在性判据正是之前"裸 id / 误判"的根源。
   * 候选里命中时 PerspectivePicker 本就会忽略 `nodeName`、用候选的完整 label，故这里无需特判。
   */
  const selectedNodeName = useMemo(() => {
    if (state.n === null) return null;
    // 点图导航后 `view` 仍是上一视角的数据（`useAsync` 保留旧值），所以先查面包屑再查中心。
    for (let i = trail.length - 1; i >= 0; i -= 1) {
      if (trail[i].node === state.n && trail[i].nodeName) return trail[i].nodeName;
    }
    return view?.center.id === state.n ? view.center.name : null;
  }, [state.n, trail, view]);

  // 首次进入时把当前位置压入面包屑
  useEffect(() => {
    if (!state.p || trail.length > 0) return;
    // 名字只从权威来源（当前视图中心）取，不回退到候选列表——候选是按需加载的 UI 缓存，
    // 不是节点存在/命名的权威；视图未就绪时留空，待 `view` 到达后由选中态正常显示。
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

  // 布局由 `views/perspectives.yaml` 按视角声明（路由=分层、资源=径向……），不再有手动覆盖。
  const layoutMode: LayoutMode = current?.layout ?? 'radial';
  // 暂时注释：本地根模板 / WSL 模式 / 按工程覆盖 三处设置只在「跳转 IDE」时才需要，
  // 跳转入口已移除，复制绝对路径直接用后端 root_path 即可（以后再考虑加回）。
  //
  // // 按工程覆盖：优先级高于全局根模板，用于模板表达不了的特例；只影响 IDE 跳转与复制，不改后端数据。
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
  // // 本地工程根只影响 IDE 跳转与复制，不改后端数据。
  // // 解析优先级：按工程覆盖（localRoot） > 全局根模板 / WSL 预设（设置页）> 后端 root_path。
  // const projectRoot = resolveProjectRoot(project?.root_path, localRoot, effectiveTemplate());
  // // WSL 模式开启时把 distro 透传给跳转 / 复制逻辑，生成正确的远程 scheme 与 UNC 前缀。
  // const wslDistro = getWslMode() ? getWslDistro() : undefined;
  // // 仅用于界面核对：本工程实际生效根来自哪一层（覆盖 > 模板 / WSL > 后端）。
  // const rootSource = localRoot
  //   ? t('按工程覆盖')
  //   : getWslMode()
  //     ? t('WSL 模式') + '（' + (getWslDistro() || 'Ubuntu') + '）'
  //     : effectiveTemplate()
  //       ? t('全局根模板')
  //       : t('后端 root_path');

  // 复制绝对路径用的本地根：暂不做模板 / WSL / 覆盖变换，直接用后端 root_path。
  const projectRoot = project?.root_path;
  // WSL 映射停用（见上）；Inspector 仍接收该 prop，留空即可，恢复时改回 getWslMode() 计算。
  const wslDistro: string | undefined = undefined;

  if (projectId === undefined || Number.isNaN(id)) {
    return <Alert type="error" message={t('缺少工程 ID')} />;
  }

  return (
    <>
      <PageHeader
        compact
        title={
          <>
            {project ? `${t('图视图')} · ${project.name}` : t('图视图')}
            {/* 使用说明收进 tooltip：这行字只有第一次看有用，不值得常驻一行 */}
            <Tooltip title={t('一级选视角、二级选对象；只渲染当前这一条链路，被省略的部分以计数与未解析记账呈现')}>
              <InfoCircleOutlined
                style={{ fontSize: 13, color: 'rgba(0,0,0,0.35)', marginLeft: 6, cursor: 'help' }}
              />
            </Tooltip>
          </>
        }
        extra={<RunPipelineButton projectId={id} onStarted={() => void reloadProject()} />}
      />

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
              {/* 图标化 + tooltip：省下来的宽度留给面包屑，避免这一行换行把画布往下推 */}
              <Tooltip title={t('重置缩放与平移，使整张图完整显示在当前视窗内')}>
                <Button size="small" icon={<FullscreenOutlined />} onClick={() => setFitSignal((s) => s + 1)} />
              </Tooltip>
              <Button size="small" type="primary" ghost onClick={() => setDrawerOpen(true)}>
                {t('结论 / 导航')}
              </Button>
              {Object.keys(expanded).length > 0 && (
                <Button size="small" onClick={() => setExpanded({})}>
                  {t('收起调用') + '（' + Object.keys(expanded).length + '）'}
                </Button>
              )}
              <Select
                mode="multiple"
                allowClear
                size="small"
                style={{ minWidth: 200 }}
                placeholder={t('全部子工程')}
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
        {/* 暂时注释：IDE 打开入口已移除，按工程覆盖本地根与「当前生效根」展示一并停用（以后再考虑加回）。
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
                  {t('高级 · 按工程覆盖本地根（特殊场景才需要）')}
                </Typography.Text>
              ),
              children: (
                <Space align="center" wrap>
                  <Tooltip title={t('本地工程根仅用于 IDE 跳转与复制，不改后端数据。优先级：此处「按工程覆盖」> 全局根模板（设置页）> 后端 root_path。留空即按后两者解析。')}>
                    <Typography.Text type="secondary" style={{ fontSize: 12, whiteSpace: 'nowrap' }}>
                      {t('本地工程根（覆盖）')}
                    </Typography.Text>
                  </Tooltip>
                  <Input
                    size="small"
                    style={{ width: 420 }}
                    placeholder={project?.root_path ?? t('按工程覆盖的本地绝对路径，留空则取全局模板 / 后端路径')}
                    value={localRoot}
                    onChange={(e) => updateLocalRoot(e.target.value)}
                  />
                  {localRoot ? (
                    <Button size="small" type="link" onClick={() => updateLocalRoot('')}>
                      {t('用全局 / 后端路径')}
                    </Button>
                  ) : null}
                  <Button size="small" type="link" onClick={() => navigate('/settings')}>
                    {t('全局根模板设置')}
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
          {t('本工程当前生效根（来源：') + rootSource + t('）：')}
          <Typography.Text code style={{ fontSize: 12 }}>
            {projectRoot || t('（无法解析，请检查后端 root_path 或上方覆盖）')}
          </Typography.Text>
        </Typography.Paragraph>
        */}
      </Card>

      {isAggregate && aggView?.notice ? (
        <Alert type="info" showIcon style={{ marginBottom: 16 }} message={aggView.notice} />
      ) : null}

      {/* 诚实性守门：这个对象在当前视角下取不到时，如实告知并给一条出路，
          而不是悄悄把中心换成第一个候选（那等于展示一张无关的图）。 */}
      {!isAggregate && state.n !== null && !loading && objectError ? (
        <Alert
          type="warning"
          showIcon
          style={{ marginBottom: 12 }}
          message={t('对象 #') + state.n + t(' 在「') + (current?.label ?? state.p) + t('」视角下取不到链路')}
          description={t('可能已被删除、或不属于该视角（') + errText(objectError) + t('）。请在左侧一级视角重新选择。')}
          action={
            candidates.length > 0 ? (
              <Button size="small" onClick={() => setState((s) => ({ ...s, n: suggestedNodeId ?? candidates[0]?.id }))}>
                {t('换第一个对象')}
              </Button>
            ) : null
          }
        />
      ) : null}

      <Row gutter={[16, 16]}>
        <Col xs={24} xl={24}>
          {/* 量高锚点：见上方 `recomputeCanvasHeight` */}
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
            />

          {/* 入口类视角（路由 / 定时任务）无链路时给出说明，避免"画面空了 = 坏了"的错觉 */}
          {view && view.conclusions['提示'] ? (
            <Alert
              type="info"
              showIcon
              style={{ marginTop: 16 }}
              message={String(view.conclusions['提示'])}
            />
          ) : null}

          {/* 诚实性守门：省略了什么、为什么省略。
              机制必须保留（绝不静默省略），但呈现压成一行 —— 整句模板每次一字不差，
              只有数字在变，看第三遍起就是噪声；数字直接取结构化字段，不再渲染后端模板句。 */}
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
                {t('已画 ') + view.hidden.shown + t(' 条边，折叠 ') + (view.hidden.total - view.hidden.shown) + t(' 个语法节点')}
              </span>
              {Object.entries(view.hidden.by_kind).map(([k, v]) => (
                <Tag key={k} style={{ marginInlineEnd: 0 }}>
                  {k} {v}
                </Tag>
              ))}
              <span>{t('单击任意边可查看它经由的每一跳及调用处')}</span>
            </div>
          ) : null}

          {view && view.unresolved.length > 0 ? (
            <Card
              variant="borderless"
              style={{ borderRadius: 14, marginTop: 16 }}
              size="small"
              title={t('未解析记账')}
              extra={<Tag color="orange">{view.unresolved.length}</Tag>}
            >
              <Table
                size="small"
                rowKey={(_, i) => String(i)}
                dataSource={view.unresolved}
                pagination={false}
                columns={[
                  { title: t('代码'), dataIndex: 'code', width: 170 },
                  { title: t('说明'), dataIndex: 'message' },
                  { title: t('位置'), dataIndex: 'location', width: 220, ellipsis: true },
                ]}
              />
            </Card>
          ) : null}
        </Col>
      </Row>

      <Drawer
        title={t('结论与导航')}
        placement="right"
        width={360}
        open={drawerOpen}
        onClose={() => setDrawerOpen(false)}
        styles={{ body: { padding: 16 } }}
      >
        {/* 「结论」不再单独立卡：入边/出边两个大数字在图上一眼可数（环上节点标题也写了），
             大数字卡占 ~150px 只为说两句话。压成环上节点卡的 extra + 底部一行，
             有增量信息的（标注 / schema 列数 / 路由表登记）才有资格出现。 */}
        <Card
          variant="borderless"
          size="small"
          title={t('环上节点')}
          extra={
            view ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('入边 ') + fmt(view.conclusions['入边']) + ' · ' + t('出边 ') + fmt(view.conclusions['出边'])}
              </Typography.Text>
            ) : aggView ? (
              <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                {t('分组数 ') + aggView.clusters.length}
              </Typography.Text>
            ) : null
          }
        >
          {view ? (
            <Space direction="vertical" size={8} style={{ width: '100%' }}>
              {(view?.rings ?? []).map((ring, i) => (
                <div key={i}>
                  <Typography.Text strong style={{ fontSize: 12 }}>
                    {t('环') + (i + 1) + '（' + ring.length + '）'}
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
              {Array.from(new Set(asArray(view.conclusions['标注']))).length > 0 ||
              view.conclusions['schema 列数'] !== undefined ||
              view.conclusions['路由表登记'] ? (
                <Space size={6} wrap style={{ marginTop: 4 }}>
                  {Array.from(new Set(asArray(view.conclusions['标注']))).map((a) => (
                    <Tag key={a} color="volcano" style={{ marginInlineEnd: 0 }}>
                      {a}
                    </Tag>
                  ))}
                  {view.conclusions['schema 列数'] !== undefined ? (
                    <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                      {t('schema 列数：') + String(view.conclusions['schema 列数'])}
                    </Typography.Text>
                  ) : null}
                  {view.conclusions['路由表登记'] ? (
                    <Typography.Text type="secondary" style={{ fontSize: 12 }}>
                      {t('路由表登记 handler：') + String(view.conclusions['路由表登记'])}
                    </Typography.Text>
                  ) : null}
                </Space>
              ) : null}
            </Space>
          ) : aggView ? (
            <Typography.Text type="secondary" style={{ fontSize: 12 }}>
              {aggView.matrix
                ? t('共 ') + formatNumber(aggView.matrix.cells.flat().reduce((a, b) => a + b, 0)) + t(' 个单元格取值')
                : t('选择一个对象后显示结论')}
            </Typography.Text>
          ) : (
            <Typography.Text type="secondary">{t('选择一个对象后显示结论')}</Typography.Text>
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
  /** 节点所属「端」：`frontend` / `backend`（由 FKB 标注）。用于图上区分前后端子工程。 */
  side?: string | null;
  /** 节点所属子工程 id（后端 `NodeView.sub_project_id`）。图着色 / 过滤以子工程为单位。 */
  sub_project_id?: number | null;
  /** 悬浮卡片要显示的信息；缺失时用空值兜底。 */
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

/// 子工程角色 → 默认视角：选了某子工程后自动跳过去。
/// 后端：worker → 计划任务视角，其余（api / bff / admin …）→ 路由视角；
/// 前端：暂用路由视角兜底（registry 暂无前端专属视角，见 perspectives.yaml 的 Page 视角为 MVP 暂挂）。
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
  return '请求失败';
}
