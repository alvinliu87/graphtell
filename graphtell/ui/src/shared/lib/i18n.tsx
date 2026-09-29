import {
  createContext,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';

/**
 * 极简 i18n：以 `kind`（节点 / 边种类，开放字符串）为键做本地化。
 *
 * 为什么放前端、以 `kind` 为键：
 * * `kind` 是稳定且有限的「已知种类」键（`gt-domain::model::kinds`），
 *   后端只下发 `kind`，前端按语言映射成可读谓语（如 `PublishesTo` → `投递到`），
 *   新增语言**无需重编译 Rust 内核**（项目核心原则是 OCP）。
 * * 未知种类（FKB 动态引入）`t` 直接回退为原键，不会白屏。
 * * 未包 `LocaleProvider` 时（如单测）`t` 返回原键，保证既有断言仍成立。
 */

export type Lang = 'zh-CN' | 'en-US';

export const LANGS: Lang[] = ['zh-CN', 'en-US'];

const STORAGE_KEY = 'em.lang';

const dict: Record<Lang, Record<string, string>> = {
  'zh-CN': {
    // ---- 边谓语 ----
    'edge.Contains': '包含',
    'edge.Declares': '声明',
    'edge.Extends': '继承',
    'edge.Implements': '实现',
    'edge.UsesTrait': '使用 trait',
    'edge.Calls': '调用',
    'edge.HasCallSite': '含调用点',
    'edge.Imports': '导入',
    'edge.HandledBy': '由…处理',
    'edge.CallsHttp': '调用 HTTP',
    'edge.Triggers': '触发事件',
    'edge.PublishesTo': '投递到',
    'edge.ReadsDb': '读库',
    'edge.WritesDb': '写库',
    'edge.MapsTo': '映射到',
    'edge.ForeignKey': '外键',
    'edge.ReadsConfig': '读配置',
    // 中间件边：读作"经过"而不是"守卫" —— 中间件里有会拒绝请求的守卫，也有只加响应头 /
    // 记日志的旁路，统一叫"守卫"等于替后者过度声明。鉴不鉴权看 `Capability` 标注。
    'edge.PassesThrough': '经过',
    'edge.ReadsCache': '读缓存',
    'edge.WritesCache': '写缓存',
    'edge.Mutates': '改变状态',
    'edge.NavigatesTo': '跳转页面',
    'edge.Emits': '发射事件',
    'edge.ListensTo': '监听事件',
    'edge.ResolvesTo': '解析为',
    'edge.Unknown': '未知关系',
    // 组合谓语：同一处**既读又写**（见 `edgeKindLabel`，键 = 种类按字典序用 + 连接）。
    'edge.ReadsDb+WritesDb': '读写库',
    'edge.ReadsCache+WritesCache': '读写缓存',
    // ---- 节点种类 ----
    'node.File': '文件',
    'node.Directory': '目录',
    'node.Namespace': '命名空间',
    'node.Class': '类',
    'node.Interface': '接口',
    'node.Trait': 'trait',
    'node.Enum': '枚举',
    'node.EnumCase': '枚举项',
    'node.Method': '方法',
    'node.Function': '函数',
    'node.Property': '属性',
    'node.Const': '常量',
    'node.CallSite': '调用点',
    'node.Table': '表',
    'node.HttpContract': 'HTTP 契约',
    'node.ConfigKey': '配置键',
    'node.I18nKey': '国际化键',
    'node.Event': '事件',
    'node.Schedule': '计划任务',
    'node.Queue': '队列',
    'node.Cache': '缓存',
    'node.Topic': '主题',
    'node.Page': '页面',
    'node.EventBus': '事件总线',
    'node.EventHandler': '事件处理器',
    'node.Middleware': '中间件',
    'node.Unknown': '未知',
    // ---- 建图期诊断（`code` → 人话）----
    //
    // 为什么放前端、以 code 为键：诊断 code 是引擎侧的封闭集合（`ws.diagnose(...)` 的调用点），
    // 与节点 / 边 kind 同类 —— 前端按语言映射成人话，页面就不必再显示 `IdentityUnresolved`
    // 这种用户读不懂的标识符。未收录的 code 回退为原 code 本身（见 `diagnosticTitle`），
    // 不会白屏，也不需要改 Rust 内核。
    'diag.IdentityUnresolved.title': '规则算不出对象的唯一标识',
    'diag.IdentityUnresolved.what':
      '合成语义对象（事件 / 配置键 / HTTP 契约等）时算不出它的身份键，于是「这一处不产生语义节点」，只剩文件结构。图因此少一块，相关召回会变弱 —— 但代码本身没问题。',
    'diag.NoParserForLanguage.title': '这种语言还没有解析器',
    'diag.NoParserForLanguage.what':
      '文件被扫进来了，但该语言没注册解析器：只有文件节点，没有类 / 函数 / 调用。子工程的图只有结构层，这里是「看不见」，不是「没有调用」。',
    'diag.RootRuleUnresolved.title': '框架知识的某项没能解析',
    'diag.RootRuleUnresolved.what':
      '框架知识声明的值（如 ThinkPHP 的 db_prefix）在工程里没解析出来。依赖它的规则会整体失效，通常连带出现「表名对不上」这类下游现象。',
    'diag.AnnotateTargetMissing.title': '规则的标注目标没能解析',
    'diag.AnnotateTargetMissing.what':
      '规则本应给对象打标注 / 建边，但目标没解析出来，这一步被跳过。依赖该标注的规则会「静默归零」：看着像「没问题」，其实是判据不成立。',
    'diag.AliasTargetMissing.title': '父类 / 门面目标不在图内',
    'diag.AliasTargetMissing.what':
      '继承或门面调用的目标被 Ingest 排除（vendor 等），已记占位节点。这是「预期内」的：第三方代码不进图。',
    'diag.EventListenNoConsumer.title': 'Event::listen 没写监听器',
    'diag.EventListenNoConsumer.what':
      'event.php 里 `Event::listen(...)` 没有带监听器参数，注册信息不完整，事件挂不到具体消费者上。',
    'diag.UnresolvedLink.title': '某条关系找不到目标',
    'diag.UnresolvedLink.what':
      '调用 / 路由 / 门面指向的目标不在图里。警告级：路由指向不存在的 handler，线上会 500，值得核对；提示级：门面或变量类型推断不出来，目标多半在 vendor，属预期。',
    'diag.EventTriggerUnresolved.title': "event('…') 找不到已注册的事件",
    'diag.EventTriggerUnresolved.what':
      "某处触发了 event('…')，但 event.php 里没有这个键。若该键被写成空数组，监听器会被整体误判成死代码 —— 这是「事件系统看起来没接上」的常见原因。",
    'diag.EventListenUnresolved.title': "Event::listen('…') 找不到对应事件",
    'diag.EventListenUnresolved.what':
      '监听的目标事件没在 event.php 注册。监听器仍可经 `listener` 标注识别，但无法精确挂到具体事件上。',
    'diag.EventListenTargetMissing.title': 'Event::listen 的监听器不在图内',
    'diag.EventListenTargetMissing.what':
      '注册的监听器类不在图里（被排除或没被解析），事件挂不上去。',
    // ---- 通用 ----
    'status.resolved': '已解析',
    'status.unverified': '待验证',
    'indirect.tooltip':
      '起点自身并未执行该动作；由调用链下游某处传播而来。事实成立，可在下方「调用链」中逐跳核对。',
  },
  'en-US': {
    'edge.Contains': 'contains',
    'edge.Declares': 'declares',
    'edge.Extends': 'extends',
    'edge.Implements': 'implements',
    'edge.UsesTrait': 'uses trait',
    'edge.Calls': 'calls',
    'edge.HasCallSite': 'has call site',
    'edge.Imports': 'imports',
    'edge.HandledBy': 'handled by',
    'edge.CallsHttp': 'calls HTTP',
    'edge.Triggers': 'triggers',
    'edge.PublishesTo': 'publishes to',
    'edge.ReadsDb': 'reads DB',
    'edge.WritesDb': 'writes DB',
    'edge.MapsTo': 'maps to',
    'edge.ForeignKey': 'foreign key',
    'edge.ReadsConfig': 'reads config',
    'edge.PassesThrough': 'passes through',
    'edge.ReadsCache': 'reads cache',
    'edge.WritesCache': 'writes cache',
    'edge.Mutates': 'mutates',
    'edge.NavigatesTo': 'navigates to',
    'edge.Emits': 'emits',
    'edge.ListensTo': 'listens to',
    'edge.ResolvesTo': 'resolves to',
    'edge.Unknown': 'unknown relation',
    'edge.ReadsDb+WritesDb': 'read+write DB',
    'edge.ReadsCache+WritesCache': 'read+write cache',
    'node.File': 'File',
    'node.Directory': 'Directory',
    'node.Namespace': 'Namespace',
    'node.Class': 'Class',
    'node.Interface': 'Interface',
    'node.Trait': 'Trait',
    'node.Enum': 'Enum',
    'node.EnumCase': 'Enum case',
    'node.Method': 'Method',
    'node.Function': 'Function',
    'node.Property': 'Property',
    'node.Const': 'Const',
    'node.CallSite': 'Call site',
    'node.Table': 'Table',
    'node.HttpContract': 'HTTP contract',
    'node.ConfigKey': 'Config key',
    'node.I18nKey': 'I18n key',
    'node.Event': 'Event',
    'node.Schedule': 'Schedule',
    'node.Queue': 'Queue',
    'node.Cache': 'Cache',
    'node.Topic': 'Topic',
    'node.Page': 'Page',
    'node.EventBus': 'Event bus',
    'node.EventHandler': 'Event handler',
    'node.Middleware': 'Middleware',
    'node.Unknown': 'Unknown',
    // ---- Build-time diagnostics (code → plain language) ----
    'diag.IdentityUnresolved.title': 'A rule cannot derive an object identity',
    'diag.IdentityUnresolved.what':
      'While synthesizing a semantic object (event / config key / HTTP contract…), the rule could not derive its identity key, so no semantic node is created here — only the file structure remains. The graph is missing a piece and recall weakens, but your code itself is fine.',
    'diag.NoParserForLanguage.title': 'No parser for this language yet',
    'diag.NoParserForLanguage.what':
      'Files were scanned, but no parser is registered for this language: file nodes only, no classes / functions / calls. This sub-project has structure only — it is "invisible", not "no calls".',
    'diag.RootRuleUnresolved.title': 'A framework-knowledge value could not be resolved',
    'diag.RootRuleUnresolved.what':
      'A value declared in framework knowledge (e.g. ThinkPHP db_prefix) was not resolved in this project. Rules depending on it fail as a whole, usually surfacing downstream as unmatched table names.',
    'diag.AnnotateTargetMissing.title': 'A rule could not resolve its annotation target',
    'diag.AnnotateTargetMissing.what':
      'A rule should annotate an object or create an edge, but the target did not resolve and the step was skipped. Rules depending on that annotation silently go to zero hits: it looks like "no problem" while the criterion is actually invalid.',
    'diag.AliasTargetMissing.title': 'Inherited / facade target is not in the graph',
    'diag.AliasTargetMissing.what':
      'The target of an inheritance or facade call was excluded by Ingest (vendor etc.) and a placeholder node was recorded. This is expected: third-party code is not ingested.',
    'diag.EventListenNoConsumer.title': 'Event::listen has no listener',
    'diag.EventListenNoConsumer.what':
      'In event.php, `Event::listen(...)` is registered without a listener argument; the registration is incomplete and the event cannot be attached to a concrete consumer.',
    'diag.UnresolvedLink.title': 'A relation has no resolvable target',
    'diag.UnresolvedLink.what':
      'The target of a call / route / facade is not in the graph. Warning level: a route pointing to a non-existent handler returns 500 in production — worth checking. Info level: facade or variable type could not be inferred, the target is likely in vendor — expected.',
    'diag.EventTriggerUnresolved.title': "event('…') has no registered event",
    'diag.EventTriggerUnresolved.what':
      "Something triggers event('…') but the key does not exist in event.php. If that key is an empty array, listeners get misjudged as dead code — a common cause of \"the event system looks disconnected\".",
    'diag.EventListenUnresolved.title': "Event::listen('…') has no matching event",
    'diag.EventListenUnresolved.what':
      'The target event is not registered in event.php. The listener is still detectable via the `listener` annotation, but cannot be attached to a concrete event.',
    'diag.EventListenTargetMissing.title': 'Event::listen listener is not in the graph',
    'diag.EventListenTargetMissing.what':
      'The registered listener class is not in the graph (excluded or unresolved), so the event cannot be attached.',
    'status.resolved': 'resolved',
    'status.unverified': 'unverified',
    'indirect.tooltip':
      'The focal node itself did not perform this action; it was propagated from downstream along the call chain. The fact holds — verify it hop by hop in the chain below.',

    // ---- 界面文案（中文即键；zh-CN 回退为原串，en-US 提供英译）----
    '工程总览': 'Projects',
    '质量门禁': 'Quality Gate',
    '合规检查': 'Compliance',
    '规则集': 'Rule Set',
    '代码图': 'Code Graph',
    '节点浏览': 'Explorer',
    '诊断': 'Diagnostics',
    '建图报告': 'Build Report',
    '查看建图报告': 'View build report',
    // ---- 诊断页（建图期诊断）----
    '根节点没解析、路由指向不存在的 handler、identity 算不出来…… 说的都是「图少建了一块」。':
      'Unresolved roots, routes pointing to non-existent handlers, underivable identities — all of them mean "a piece of the graph was not built".',
    '读法：先看「问题类型」有几类、哪类要管；同一类在几百个文件上重复触发时，条数不代表问题数。':
      'How to read it: start from the problem types — how many there are and which one matters. When one type fires in hundreds of files, the entry count is not the problem count.',
    '代码是否违反规则见': 'For rule violations of the code itself, see ',
    '去质量门禁查看': 'Open Quality Gate',
    '问题类型': 'Problem types',
    '这一页是「建图报告」：记录图没建全的地方，不是你的代码违反了规则':
      'This page is the "Build Report": where the graph is incomplete — not a verdict on your code.',
    '根节点缺失、路由指向不存在的 handler、identity 冲突等 —— 这些记录本身就是分析结论':
      'Missing root nodes, routes pointing to non-existent handlers, identity conflicts, etc. — these records are the conclusions.',
    '暂无建图报告 —— 图没报出任何未解析 / 缺失。':
      'No build report — the graph reported nothing unresolved or missing.',
    '暂无建图报告条目': 'No report entries',
    '条记录，当前列出了': 'records, currently listing',
    ' 类': ' types',
    ' 条': ' entries',
    '共 ': 'Total ',
    '错误': 'Errors',
    '警告': 'Warnings',
    '提示': 'Hints',
    '严重': 'Critical',
    '类型': 'Type',
    '已复制定位': 'Location copied',
    // 诊断页的分组视图
    '这些语言还没有解析器：': 'No parser for these languages: ',
    ' —— 对应子工程只有文件结构，语义召回在这里为空。':
      ' — those sub-projects have file structure only; semantic recall is empty there.',
    ' 类问题': ' problem types',
    '值得看一眼': 'Worth a look',
    '引擎 / 知识局限': 'Engine / knowledge limits',
    '预期内': 'Expected',
    '可能指向真实的代码问题（死路由 / 事件没注册），值得核对。':
      'May point to a real code problem (dead route / unregistered event) — worth checking.',
    '引擎或框架知识没能建出这一块：代码本身没问题，但图在这里是缺的，相关召回会弱。':
      'The engine or framework knowledge could not build this piece: your code is fine, but the graph is missing here and recall weakens.',
    '目标在 vendor 或被排除，属设计如此，不用管。':
      'The target is in vendor or excluded by design — nothing to do.',
    '这一页没有需要你处理的：全部是预期内 / 引擎局限。':
      'Nothing here needs your attention: all of it is expected or an engine limitation.',
    '其余是引擎 / 知识局限与预期内：不改变召回结论，除非你要查的正是那一块。':
      'The rest are engine / knowledge limits and expected cases: they do not change recall conclusions unless that is exactly the part you are looking into.',
    '按问题类型': 'By type',
    '逐条明细': 'Entries',
    '按类型的计数取自全量汇总，不受明细读取上限影响':
      'Per-type counts come from the full aggregate, unaffected by the entry read cap.',
    '按类型的计数取自当前读取窗口（汇总接口未返回按类型计数），可能偏小。':
      'Per-type counts are derived from the current read window (the summary API returned no per-type counts) and may be too small.',
    '这一类共 ': 'This type has ',
    '查看明细': 'View entries',
    '样例位置': 'Sample locations',
    '处同类，点「查看明细」按此类型筛选':
      ' more of the same type; use "View entries" to filter by it.',
    '明细已被读取上限截断': 'Entries truncated by the read limit',
    '本工程共': 'This project has',
    '条（明细有读取上限）：按类型的计数取自全量汇总，仍然准确。':
      ' entries (the entry list is capped): per-type counts come from the full aggregate and stay accurate.',
    '全部类型': 'All types',
    // 代码图页的「图覆盖」提示（诊断页入口）
    '图覆盖': 'Graph coverage',
    ' 处': ' places',
    '（多为引擎局限与预期内，不改变召回结论）':
      '— mostly engine limits and expected cases; recall conclusions are unaffected.',
    '回到代码图': 'Back to code graph',
    '返回合规检查': 'Back to compliance',
    '建图后自动跑出的规则结论（持久化）；哪些规则启用、阈值多少在「规则集」里调':
      'Rule conclusions produced automatically after the build (persisted); which rules are enabled and their thresholds are tuned in "Rule Set".',
    '只画当前这一条链路；其余以计数与未解析记账呈现。':
      'Draws one link at a time; the rest is shown as counts and unresolved records.',
    '设置': 'Settings',
    '设置项暂未启用': 'Settings are temporarily disabled',
    '本地根模板 / WSL 模式 / 默认 IDE 等设置仅服务于「跳转 IDE」；该入口已移除，相关设置暂时停用。':
      'The local root template / WSL mode / default IDE only served "jump to IDE", which has been removed; these settings are temporarily disabled.',
    '代码库图化分析': 'Codebase graph analysis',
    '展开侧边栏': 'Expand sidebar',
    '收起侧边栏': 'Collapse sidebar',
    '当前工程': 'Current project',
    '选择或创建一个工程开始分析': 'Select or create a project to start',
    '已装载的框架知识数量': 'Loaded framework knowledge count',
    '检测到的语言': 'Detected languages',
    '框架知识': 'Framework knowledge',
    '后端在线': 'Backend online',
    '后端未连接': 'Backend disconnected',

    '一级选视角、二级选对象；只渲染当前这一条链路，被省略的部分以计数与未解析记账呈现':
      'Pick a perspective, then an object; only this one link is rendered. Omitted parts are shown as counts and an unresolved tally.',
    '结论 / 导航': 'Conclusions / Navigation',
    '收起调用': 'Collapse calls',
    '高级 · 按工程覆盖本地根（特殊场景才需要）': 'Advanced · Per-project local root override (only for special cases)',
    '本地工程根（覆盖）': 'Local project root (override)',
    '本地工程根仅用于 IDE 跳转与复制，不改后端数据。优先级：此处「按工程覆盖」> 全局根模板（设置页）> 后端 root_path。留空即按后两者解析。':
      'Local project root is only for IDE jumping and copying, not for changing backend data. Priority: this "per-project override" > global root template (Settings) > backend root_path. Empty falls back to the latter two.',
    '按工程覆盖的本地绝对路径，留空则取全局模板 / 后端路径': 'Absolute local path override; empty uses global template / backend path',
    '用全局 / 后端路径': 'Use global / backend path',
    '全局根模板设置': 'Global root template settings',
    '本工程当前生效根（来源：': 'Effective root for this project (source: ',
    '）：': ':',
    '（无法解析，请检查后端 root_path 或上方覆盖）': '(unresolvable; check backend root_path or the override above)',
    '对象 #': 'Object #',
    ' 在「': ' in perspective ',
    '」视角下取不到链路': ' has no link under this perspective',
    '可能已被删除、或不属于该视角（': 'It may have been deleted, or not belong to this perspective (',
    '）。请在左侧一级视角重新选择。': '). Please re-select from the left perspective.',
    '换第一个对象': 'Use first object',
    '未解析记账': 'Unresolved tally',
    '已画 ': 'Drawn: ',
    ' 条边，折叠 ': ' edges; folded ',
    ' 个语法节点': ' syntax nodes',
    '单击任意边可查看它经由的每一跳及调用处':
      'Click any edge to inspect every hop and call site along its path',
    // 孤儿直连访问记账：不占画布（语法节点信息量低），但必须给出条目与位置。
    '另有直连访问 ': 'Also ',
    ' 处找不到语义入口': ' direct accesses with no semantic entry (CLI / cron / event) — listed here, not drawn',
    '其它直连访问 ': 'orphan access: ',
    '代码': 'Code',
    '说明': 'Description',
    '位置': 'Location',
    '结论': 'Conclusions',
    '入边': 'In-edges',
    '出边': 'Out-edges',
    '标注': 'Annotations',
    'schema 列数：': 'Schema columns: ',
    '经过中间件：': 'Middleware: ',
    '路由表登记 handler：': 'Route-registered handler: ',
    '分组数': 'Groups',
    ' 个单元格取值': ' cell values',
    '选择一个对象后显示结论': 'Select an object to see conclusions',
    '环上节点': 'Nodes on rings',
    '环': 'Ring',
    '缺少工程 ID': 'Missing project ID',

    '节点详情': 'Node details',
    '边证据链': 'Edge evidence chain',
    '加载中…': 'Loading…',
    '未找到该节点': 'Node not found',
    '种类': 'Kind',
    '名称': 'Name',
    '节点类型': 'Node type',
    '合成节点（语义对象）': 'Synthetic node (semantic object)',
    '语法节点': 'Syntax node',
    '引用': 'References',
    ' 条入边': ' in-edges',
    '这是合成节点：它由多处来源汇聚而成': 'This is a synthetic node: aggregated from multiple sources',
    '下面列出全部出处，请按需逐条验证；这里不会替你挑一个「看起来像」的位置。':
      'All sources are listed below; verify each as needed. We will not pick a "looks-like" location for you.',
    '另有 ': ' plus ',
    ' 处引用指向它。': ' references point to it.',
    '合成边（折叠汇总）': 'Synthetic edge (collapsed aggregate)',
    '这条边是把多条调用链聚合后归纳出的语义边，没有与之对应的单一源码位置；下面是被它折叠的中间节点（自起点到终点），可据此逐跳核对。':
      'This edge is a semantic edge aggregated from multiple call chains; there is no single corresponding source location. Below are the intermediate nodes it collapsed (start to end); verify hop by hop.',
    '这条边是把多条调用链聚合后归纳出的语义边，图里没有与之对应的单条直接边，也没有可定位的触发点，因此没有逐跳证据可查。':
      'This edge is a semantic edge aggregated from multiple call chains; there is no single corresponding direct edge in the graph and no locatable trigger point, so there is no hop-by-hop evidence.',
    '未找到该边': 'Edge not found',
    '关系': 'Relation',
    '状态': 'Status',
    '待验证假设（虚线）': 'Unverified hypothesis (dashed)',
    '已解析（实线）': 'Resolved (solid)',
    '置信度': 'Confidence',
    '跳数': 'Hops',
    '途经 ': 'via ',
    ' 跳': ' hops',
    '未解析的边是推断结果：下面每个位置都是可亲自验证的落点，核对后再采信。':
      'Unresolved edges are inferences: each location below is verifiable; verify before trusting.',
    '证据位置': 'Evidence locations',
    '这条边没有可跳转的证据位置（可能来自权威源推断）':
      'This edge has no jumpable evidence location (may come from authoritative-source inference)',
    '定义处': 'Definition',
    '调用处': 'Call site',
    '定义': 'Definition',
    '全部出处': 'All occurrences',
    '未解析到调用语句': 'Call statement not resolved',
    '该跳不是直接的 Calls 边（如 路由→handler 的绑定，或调用未解析），后端未给出「调用处」':
      'This hop is not a direct Calls edge (e.g. a route→handler binding, or an unresolved call), so the backend provides no call site.',
    '在主图中以该节点为中心重绘': 'Re-center the main graph on this node',
    '折叠掉的调用链': 'Collapsed call chain',
    '调用链': 'Call chain',
    '（': ' (',
    ' 条路径': ' paths',
    '起止各 1 个 + 中间 ': ' one start + one end + ',
    '路径 ': 'Path ',
    '完全限定名': 'Fully qualified name',
    'Identity': 'Identity',

    '语言': 'Language',
    '暂无标注': 'No annotations',
    '通道': 'Channel',
    '子类型': 'Subtype',
    '相邻边（': 'Adjacent edges (',
    '）': ')',
    '暂无边': 'No edges',
    '方向': 'Direction',
    '→ 出': '→ out',
    '← 入': '← in',
    '对端': 'Opposite',
    '该节点没有可用的源码位置（可能是纯语义合成对象）':
      'This node has no usable source location (possibly a pure semantic synthetic object)',
    '敏感位置：只跳到键名所在行，不展示任何值': 'Sensitive location: jump only to the key-name line, no values shown',
    '在 IDE 中打开（': 'Open in IDE (',
    '；右上角图标可换 IDE）': '; icon at top-right to switch IDE)',
    '复制绝对 path:line（无 IDE 场景的兜底）': 'Copy absolute path:line (fallback when no IDE)',
    '在 IDE 中打开（失败会自动复制路径）': 'Open in IDE (falls back to copying path on failure)',
    ' 处来源位置': ' source locations',
    '复制全部位置（绝对路径）': 'Copy all locations (absolute paths)',
    '无位置': 'No location',
    ' 处位置': ' locations',
    '符号 ': 'symbol ',

    '还没有工程，点击右上角「新建工程」开始': 'No projects yet; click "New project" at top-right to start',
    '根目录': 'Root',
    '完整流程': 'Full pipeline',
    '是': 'Yes',
    '仅基础阶段': 'basic stages only',
    '创建时间': 'Created',
    '操作': 'Actions',
    '刷新': 'Refresh',
    '重置': 'Reset',
    '点击显隐此类节点': 'Click to show/hide this node type',
    '点击显隐此类边': 'Click to show/hide this edge type',
    '（连到它的边一并收起）': ' (edges touching it collapse too)',
    '（只经由它相连的点一并收起）': ' (nodes reachable only via it collapse too)',
    '关系类型': 'Relation type',
    '子工程': 'Sub-project',
    '仅配色，不参与筛选': 'Color key only — not clickable',
    '隐藏某类节点时，连到它的边一并收起': 'Hiding a node type also collapses the edges touching it',
    '隐藏某类关系时，只经由它相连的点一并收起':
      'Hiding a relation type also collapses nodes reachable only through it',
    '连带收起 {{n}} 点': '{{n}} node(s) collapsed',
    '这些点只经由被隐藏的关系相连，已一并收起':
      'These nodes were only reachable through hidden relations, so they are collapsed too',
    '新建工程': 'New project',
    '工程总数': 'Total projects',
    '已就绪': 'Ready',
    '建图中': 'Indexing',
    '全部工程': 'All projects',
    'SQLite 持久化': 'SQLite persisted',
    '添加一个工程后会自动开始建图：识别子工程 → 语法建图 → 装载框架知识 → 语义合成 → 动态解析':
      'Adding a project auto-starts graphing: detect sub-projects → syntax graph → load framework knowledge → semantic synthesis → dynamic resolution',
    '检索图上的任意节点，并查看它的标注与相邻边': 'Search any node on the graph and view its annotations and adjacent edges',
    '节点种类': 'Node kind',
    '按名称过滤': 'Filter by name',
    '没有匹配的节点': 'No matching nodes',
    '完全限定名 / Identity': 'FQN / Identity',
    '详情': 'Details',
    '根节点缺失、路由指向不存在的 handler、identity 冲突等 —— 诊断本身就是分析结论':
      'Missing root nodes, routes pointing to non-existent handlers, identity conflicts, etc. — diagnostics are the conclusions',
    '严重度': 'Severity',
    '阶段': 'Phase',

    'WSL 快捷配置': 'WSL quick config',
    '启用 WSL 模式': 'Enable WSL mode',
    '发行版（distro）': 'Distro',
    'WSL 磁盘': 'WSL disk',
    'WSL（distro=Ubuntu）': 'WSL (distro=Ubuntu)',
    'Docker 挂载 /host': 'Docker mount /host',
    '远程 dev server': 'Remote dev server',
    '示例跳转 URL：': 'Example jump URL: ',
    'WSL（': 'WSL (',
    '如 ': 'e.g. ',
    '开启后无需手填模板：各工程的后端 Linux 路径会自动按 WSL 处理——VS Code / Cursor 走':
      'After enabling, no manual template needed: each project’s backend Linux path is auto-handled as WSL — VS Code / Cursor use ',
    '远程 scheme，JetBrains 与「复制路径」补': ' remote scheme; JetBrains and "copy path" add ',
    '前缀。': ' prefix.',
    '后端探测：': 'Backend detected: ',
    '非 WSL': 'Non-WSL',
    '，客户端：': ', client: ',
    '恢复自动探测': 'Restore auto-detect',
    '已自动套用': 'Auto-applied',
    '通用工具不内嵌任何场景（WSL / Docker / 远程）的假设。用「根模板」描述':
      'Generic tools embed no scenario assumptions (WSL / Docker / remote). Use "root template" to describe ',
    '的变换，': ' transformation; ',
    '会被替换为后端工程根；留空则不启用，回退到后端路径。WSL 模式开启时此模板被忽略。':
      ' is replaced with the backend project root; empty disables it and falls back to backend path. Ignored when WSL mode is on.',
    '全局根模板（非 WSL 场景 / 高级）': 'Global root template (non-WSL / advanced)',
    '例如 \\\\wsl$\\Ubuntu{root}（留空则不启用）': 'e.g. \\\\wsl$\\Ubuntu{root} (empty disables)',
    '示例一键填入：': 'Click to fill example: ',
    '解析后的本地根：': 'Resolved local root: ',
    '（无，将使用后端路径）': '(none; backend path will be used)',
    '示例文件 ': 'Example file ',
    ' →': ' →',
    '默认 IDE': 'Default IDE',
    '当前默认：': 'Current default: ',
    '点击任意位置跳转时会优先用它': 'Used first when jumping to any location',
    '保存设置': 'Save settings',
    '清除模板': 'Clear template',
    '按工程特例': 'Per-project exceptions',
    '若某个工程的本地根无法用模板表达（盘符 / 目录完全不同），可在其代码图页的「本地工程根（覆盖）」单独填写，覆盖全局设置。':
      'If a project’s local root can’t be expressed by the template (different drive / dir), fill "Local project root (override)" on its code-graph page to override global settings.',
    '存储键：': 'Storage keys: ',
    '（浏览器 localStorage，仅本机生效）': '(browser localStorage, local only)',

    '已开始建图': 'Graphing started',
    '启动失败': 'Failed to start',
    '重新建图': 'Rebuild graph',
    '点击右侧按钮选择目录': 'Click the button on the right to select a directory',
    '选择目录': 'Select directory',
    '建图完成': 'Graphing complete',
    '总耗时 ': 'Total ',
    '工程已就绪，可查看代码结构图': 'Project is ready; you can view the code structure graph',
    '点击查看图': 'Click to view graph',
    '工程名称': 'Project name',
    '请输入名称': 'Please enter a name',
    '例如：CRMEB': 'e.g. CRMEB',
    '代码库根目录': 'Codebase root',
    '请选择目录': 'Please select a directory',
    '将自动识别其中的子工程（composer.json / package.json / pom.xml 等）':
      'Sub-projects are auto-detected (composer.json / package.json / pom.xml, etc.)',
    '描述': 'Description',
    '可选': 'Optional',
    '执行完整建图流程': 'Run full pipeline',
    '建图失败，可关闭后重试或检查代码库根目录。': 'Graphing failed; close and retry, or check the codebase root.',
    '关闭': 'Close',
    '建图中…': 'Graphing…',
    '创建并开始建图': 'Create and start graphing',
    '取消': 'Cancel',
    '创建失败': 'Creation failed',
    '工程「': 'Project "',
    '」建图失败，请检查代码库根目录': '" graphing failed; check the codebase root.',
    '选择代码库根目录': 'Select codebase root',
    '选择此目录': 'Select this directory',
    '上级目录': 'Up',
    '当前目录：': 'Current directory: ',
    '该目录下没有子目录': 'No sub-directories in this directory',
    '读取失败': 'Read failed',

    '聚合视角没有"单个对象"': 'Aggregate perspective has no single object',
    '加载候选…': 'Loading candidates…',
    '选择一个对象': 'Select an object',
    '回退：': 'Back: ',

    '正在执行 ': 'Running ',
    '节点 ': 'Nodes ',
    ' · 边 ': ' · edges ',
    ' · 标注 ': ' · annotations ',
    ' · 别名 ': ' · aliases ',
    '暂无运行记录': 'No run records',

    '已删除「': 'Deleted "',
    '」': '"',
    '删除工程「': 'Delete project "',
    '」？': '"?',
    '该工程的全部节点、边、标注与符号表都会被清除，且不可恢复。':
      'All nodes, edges, annotations and symbol tables of this project will be cleared, unrecoverably.',
    '删除': 'Delete',
    '删除失败': 'Delete failed',

    '待建图': 'Pending',
    '失败': 'Failed',
    '摄取': 'Ingest',
    '语法建图': 'Syntax graph',
    '知识装载': 'Knowledge load',
    '源码标注': 'Source annotation',
    '语义合成': 'Semantic synthesis',
    '汇聚标注': 'Aggregate annotation',
    '动态解析': 'Dynamic resolve',
    '识别子工程与待分析文件，排除依赖目录与静态资源':
      'Detect sub-projects and files to analyze, excluding dependency dirs and static assets',
    '从语言语法创建 Class / Method / Property / CallSite 节点与继承边':
      'Create Class / Method / Property / CallSite nodes and inheritance edges from language syntax',
    '装载框架知识，解析 AppRoot、容器绑定、事件表、数据库 schema 等权威源':
      'Load framework knowledge; resolve authoritative sources like AppRoot, container bindings, event tables, DB schema',

    '该视角下暂无可展示的对象': 'No displayable objects under this perspective',
    '后端 root_path → 本地根': 'backend root_path → local root',
    '加载视图…': 'Loading view…',
    '类别 ': 'Category ',
    '单击查看证据链': 'Click to view the evidence chain',
    '虚线 = 经调用链间接；实线 = 直接调用': 'Dashed = indirect via call chain; solid = direct call',
    '滚轮缩放 · 拖拽平移 · 左键单击切视角 · 右键打开位置':
      'Scroll to zoom · drag to pan · left-click to switch perspective · right-click to open location',
    ' 个成员': ' members',
    ' ·经 ': ' ·via ',

    '已请求 ': 'Requested ',
    ' 打开 ': ' to open ',
    '已复制 ': 'Copied ',
    ' 处位置（绝对路径）': ' locations (absolute paths)',
    '未知错误': 'Unknown error',

    '径向布局：中心为当前对象，同心环表示跳数（环 1 = 直接关联）。环半径按各环药丸数量自适应，避免重叠。':
      'Radial layout: center is the current object; concentric rings denote hops (ring 1 = direct). Ring radius adapts to the number of pills per ring to avoid overlap.',
    '分层布局：自上而下分层（层 = 跳数），层间用 90° 正交折线，适合看调用链下钻。':
      'Layered layout: top-down layers (layer = hops) with 90° orthogonal links between layers; good for drilling a call chain.',
    'Spine 布局：把最长的一条链排成主轴，便于数据流 / 风险取证逐跳核对。':
      'Spine layout: the longest chain becomes the main axis for step-by-step data-flow / risk forensics.',
    '聚类布局：每个框是一个分组，框上只给计数与样例成员，不是单链路。':
      'Cluster layout: each box is a group; the box shows only a count and sample members, not a single link.',
    '矩阵布局：行 × 列两维度，单元格颜色深浅表示数量，0 表示该组合确实没有产出。':
      'Matrix layout: rows × columns; cell shade shows quantity, 0 means that combination truly has no output.',
    'ER 布局：表与表之间用 90° 正交连线，用于看同事务关联。':
      'ER layout: tables linked by 90° orthogonal lines; for viewing same-transaction relations.',

    // ---- 规则集页：按工程覆盖启用态与可调参数 ----
    '规则由后端 YAML 声明，前端只渲染；可在本工程内覆盖启用态与阈值，保存后自动重跑':
      'Rules are declared in backend YAML and only rendered here; you can override enabled state and thresholds per project — saving triggers a re-check.',
    '放弃修改': 'Discard changes',
    '保存并重跑': 'Save & re-run',
    '将对 {{n}} 条规则写入覆盖，并重跑一次全量检查':
      'Will write overrides for {{n}} rule(s) and re-run a full check',
    '已保存并重跑：命中 {{n}} 条违规（跑 {{run}}/{{total}} 条规则）':
      'Saved & re-run: {{n}} violation(s) ({{run}}/{{total}} rules executed)',
    '已放弃未保存的修改': 'Unsaved changes discarded',
    '只跑这条规则': 'Run this rule only',
    '恢复默认': 'Reset to default',
    '清除本工程的覆盖，回到 YAML 全局默认':
      'Clear this project override and fall back to the YAML global default',
    '继承默认': 'Inherits default',
    '工程覆盖': 'Project override',
    '启用': 'On',
    '停用': 'Off',
    '可调参数': 'Tunable parameters',
    '默认': 'Default',
    '（空）': '(empty)',
    '已覆盖': 'Overridden',
    '留空 = 不过滤': 'Empty = no filter',
    '参数设置': 'Parameters',
    '已改': 'changed',
    '参数恢复默认': 'Reset parameters',
    '已恢复规则默认参数，点「保存并重跑」生效':
      'Parameters reset to rule defaults; click "Save & re-run" to apply',
    '改动只进草稿：关掉这个窗口后，点右上角「保存并重跑」才会写库并重跑检查':
      'Changes stay in the draft: after closing, click "Save & re-run" at the top-right to persist and re-run',
    '完成': 'Done',
    '搜索规则 id / 名称 / 说明': 'Search rule id / name / description',
    '全部': 'All',
    '已启用': 'Enabled',
    '已停用': 'Disabled',
    '本工程改过': 'Overridden here',
    '清除筛选': 'Clear filters',
    '没有匹配的规则': 'No matching rules',
    '只对当前筛选出的规则生效': 'Applies only to the currently filtered rules',
    '不适用': 'N/A here',
    '需要': 'requires',
    '条违规': 'violations',
    '违规': 'violations',
    '计数随严重度': 'counts follow severity',
    '本工程不是该规则的适用环境，检查时会被自动跳过':
      'This project is not in the rule’s target environment; the check will skip it automatically',
    '工程级覆盖只记「与全局默认不同的那部分」：恢复默认 = 删除覆盖行，规则随 YAML 演进':
      'Project overrides only store what differs from the global default: "reset to default" deletes the override row, so rules keep evolving with YAML.',
  },
};

function baseLang(): Lang {
  try {
    const v = localStorage.getItem(STORAGE_KEY);
    if (v === 'en-US' || v === 'zh-CN') return v;
  } catch {
    /* ignore */
  }
  return 'zh-CN';
}

export interface LocaleApi {
  lang: Lang;
  setLang: (l: Lang) => void;
  /** 未知键原样回退，保证不白屏。 */
  t: (key: string) => string;
}

const noProvider: LocaleApi = {
  lang: 'zh-CN',
  setLang: () => {},
  t: (k: string) => k,
};

/** 最近一次生效的语言；供非组件上下文（工具函数 / 模块级）做一次性翻译。 */
let currentLang: Lang = baseLang();

/** 非组件上下文（如 `ide.ts`、`http.ts` 的工具函数）用的翻译；读取最近一次生效的语言。 */
export function translate(k: string): string {
  return dict[currentLang][k] ?? k;
}

const LocaleContext = createContext<LocaleApi>(noProvider);

export function LocaleProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(baseLang);
  useEffect(() => {
    try {
      localStorage.setItem(STORAGE_KEY, lang);
    } catch {
      /* ignore */
    }
  }, [lang]);

  const api = useMemo<LocaleApi>(
    () => {
      currentLang = lang;
      return {
        lang,
        setLang: setLangState,
        t: (k: string) => dict[lang][k] ?? k,
      };
    },
    [lang],
  );

  return <LocaleContext.Provider value={api}>{children}</LocaleContext.Provider>;
}

export function useLocale(): LocaleApi {
  return useContext(LocaleContext);
}

/**
 * 边谓语标签。
 *
 * 折叠视图里一个使用者对同一资源只画**一条**边（写 > 读择优），被压掉的另一半
 * 由后端记在 `also_kinds` 里（`WritesDb` + `['ReadsDb']` = 这处既读又写）。
 * 此时必须用组合谓语（「读写库」）而不是单边谓语 —— 否则屏幕上只报读或只报写，
 * 两种都是失真。
 *
 * 组合键按**种类字典序**拼接（`ReadsDb+WritesDb`），与 `also_kinds` 的排列无关；
 * 缺少对应词条时回退到单边标签，不会白屏。
 */
export function edgeKindLabel(
  t: (k: string) => string,
  kind: string,
  also: string[] = [],
): string {
  const single = `edge.${kind}`;
  if (!also.length) {
    return t(single);
  }
  const both = `edge.${[kind, ...also].sort().join('+')}`;
  const v = t(both);
  return v === both ? t(single) : v;
}
