//! 框架知识库（FKB）的领域模型。
//!
//! FKB 是「预置框架知识」的可序列化形式：ThinkPHP 6、Uni-app、Laravel、CRMEB…
//! 每个框架一份 YAML，描述
//! * 如何**识别**该框架（[`Detector`]）
//! * 如何**解析框架根**（[`RootRule`]，如 `composer.json` 的 `autoload.psr-4`）
//! * P3 要装载哪些**权威符号表**（[`LoaderSpec`]）
//! * 各阶段执行的**规则**（[`Rule`] = 选择器 + 绑定）
//!
//! 全部数据驱动，内核不认识任何具体框架 —— 这是 **开闭原则** 与
//! **依赖倒置** 的落点：新增框架只需加一份 YAML。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::graph::{MergeStrategy, Span};
use super::kinds::{AnnotationChannel, EdgeKind, NodeKind, Phase, SynthesizedKind};
use crate::model::kinds::Language;

/// 一份框架知识。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameworkKnowledge {
    pub id: String,
    pub display_name: String,
    pub language: Language,
    /// 适用版本提示，仅用于展示。
    pub version_hint: Option<String>,
    /// 识别信号。
    pub detectors: Vec<Detector>,
    /// 框架根 / 关键路径解析规则。
    pub root_rules: Vec<RootRule>,
    /// P3 权威符号表装载器。
    pub loaders: Vec<LoaderSpec>,
    /// 分阶段规则。
    pub rules: Vec<Rule>,
    /// 能力接口声明（库 / 框架 → 能力映射）：按 `capability` 名查**跨语言的能力模板**
    /// （`capability_templates`，由装载器从核心配置载入），摊平成 `rules`。
    ///
    /// 这是「框架 / 库知识」（分语言、可用户扩展），与跨语言的能力模板（识别机制）分离：
    /// 模板只写一次，各语言 / 各库只声明「谁暴露了什么能力」。
    #[serde(default)]
    pub capability_interfaces: Vec<CapabilityInterface>,
    /// P7 动态解析声明：哪些调用是容器解析 / 事件触发 / 门面调用。
    pub resolvers: Vec<ResolverSpec>,
    /// 缺省排除目录（叠加在工程/语言默认规则之上）。
    pub exclude_globs: Vec<String>,
    /// 知识库作用域：框架级（默认）vs 项目级。
    ///
    /// * `Framework`：通用框架知识（如 `thinkphp6` / `laravel`），被任意使用该框架的工程加载；
    /// * `Project`：项目专有知识（如 `crmeb`），**仅当工程被识别为该项目时才加载**，
    ///   避免把项目约定（如 CRMEB 的 crontab 路由）串味到其它同框架工程。
    pub scope: KnowledgeScope,
    /// 路由 handler 的解析规则：如何把 `Route::get` 的第二实参还原成「类 + 方法」。
    ///
    /// 这是**框架知识**而非内核知识 —— 见 [`HandlerSpec`]。
    #[serde(default)]
    pub handler: Option<HandlerSpec>,
    /// 魔法方法委派：类用 `@method getList(...)` 声明、由 `__call` 转发到某个属性。
    ///
    /// PHP 生态里"注解声明 + `__call` 转发"很常见（CRMEB 的 `BaseServices` 把 20 多个
    /// `get*` / `count*` / `delete*` 转发给 `$this->dao`），但**转发给谁**是项目约定，
    /// 内核不该猜 —— 由 FKB 指明属性名即可，其余（注解解析、类型来源、继承回溯）都是通用能力。
    #[serde(default)]
    pub magic_delegation: Option<MagicDelegationSpec>,
    /// 数据模型 CRUD 动词 → 读 / 写分类（与 `MapsTo` 配合）。
    ///
    /// 「模型映射到表」是**静态结构**；`$model->save()` / `$model->find()` 才是
    /// **动作**。哪些方法名算读、哪些算写是框架 API 约定（ThinkPHP 的 `save` /
    /// `find`、Laravel 的 `create`…），由 FKB 声明后，P7 就能把
    /// `入口 → 模型 → 表` 的边标成真正的 `WritesDb` / `ReadsDb`，
    /// 而不是一路传播含糊的 `MapsTo`。
    #[serde(default)]
    pub db_verbs: Option<DbVerbsSpec>,
    /// **外部系统调用**的 callee 名单（HTTP / 短信 / 邮件 / RPC）：`curl_exec`、
    /// `Http::get` … 由 FKB 声明，供「循环内外部调用」判定（一次网络往返比一次
    /// 查询贵得多，放进循环里比 N+1 更容易拖垮接口）。
    #[serde(default)]
    pub external_calls: Vec<String>,
    /// **「中间件类 → 能力」映射**：某个中间件带什么能力（鉴权 / 限流 …）。
    ///
    /// 这份名单**属于框架知识**（什么叫鉴权中间件、项目自己给它起了什么名字），
    /// 所以放在 FKB 而不是内核 —— 内核不认识任何一个中间件名字。判定锚点是
    /// **中间件自身的确凿身份**（类名），而不是"这个端点看起来要不要登录"。
    #[serde(default)]
    pub middleware_capabilities: Vec<MiddlewareCapability>,
    /// **路由守卫识别规则**：怎么从调用图里认出「哪些中间件守着哪条路由」。
    ///
    /// 这是**框架知识而非内核知识**——`Route::get()->middleware(X)` 是 ThinkPHP/Laravel
    /// 的链式写法，`app.get(path, mw, handler)` 是 Express 的位置参数写法，
    /// `@UseGuards(X)` / `@login_required` / `@PreAuthorize` 是 NestJS/Python/Spring 的
    /// 装饰器 / 注解写法。内核不认识任何一种，全部由 FKB 声明，通用提取器按声明去图上
    /// 收，再写进 `route_list` 符号表（键与 P5 合成的 `HttpContract.name` 同形），
    /// 供 P14 把守卫类晋升为 `Middleware` 并连 `PassesThrough` 边。
    ///
    /// 新增语言 / 框架支持中间件 = 加一段 `route_guards`，**不改 Rust**。
    #[serde(default)]
    pub route_guards: Option<RouteGuardSpec>,
    /// **事务边界标记**：`transaction` / `startTrans` / `beginTransaction` …
    /// 供「同一方法多次写库但未识别到事务」判定（部分成功会留下脏数据）。
    #[serde(default)]
    pub tx_calls: Vec<String>,
    /// 「消费入口方法名」候选：连向一个类时，优先连到它的哪个方法。
    ///
    /// 各框架约定不同：Laravel/队列 Job 是 `handle`、Symfony 是 `__invoke`、
    /// ThinkPHP/CRMEB 的 Job 是 `doJob`、TP5 行为类是 `run`。
    /// 由 FKB 声明，避免新框架为了一个方法名去改内核。
    /// 未声明时回退到内核内置的**跨框架常见入口名**默认集。
    #[serde(default)]
    pub entry_methods: Vec<String>,
    /// 本 FKB 引入的**第一类语义节点种类**（在 [`NodeKind::SYNTHESIZED`] 之外追加）。
    ///
    /// 「哪些 kind 算语义节点」此前只写在 `kinds.rs` 的常量清单里 —— 于是每加一种
    /// 语义节点（前端的 `Store`、页面视角的 `Page`…）都要动内核，违反 OCP。
    /// 现在 FKB 可以自己声明：`semantic_kinds: [Store, Page]`，加载时登记进
    /// [`crate::model::kinds::register_semantic_kinds`]，折叠视图随即按语义节点渲染。
    #[serde(default)]
    pub semantic_kinds: Vec<String>,
    /// 本 FKB 引入的**第一类语义边种类**（在 [`EdgeKind::SEMANTIC`] 内置清单之外追加）。
    ///
    /// 与 `semantic_kinds`（节点）同构：新增一种语义边不该以改内核为代价。
    /// 例：某框架发明了 `SendsWebhook` 边，声明 `semantic_edge_kinds: [SendsWebhook]`
    /// 后它就像 `ReadsDb` 一样被当语义边计数 / 绘制，无需改 `kinds.rs`。
    #[serde(default)]
    pub semantic_edge_kinds: Vec<String>,
    /// 本 FKB 引入的**桥边种类**（在 [`EdgeKind::BRIDGE`] 内置清单之外追加）。
    #[serde(default)]
    pub bridge_edge_kinds: Vec<String>,
    /// 未识别到本框架时，是否仍应用其规则（默认 **false**）。
    ///
    /// 框架级规则带有强烈的框架假设（`Db::name` 是表名、`Route::get` 的第二个实参是
    /// handler……）。若对**同语言但不同框架**的工程无条件套用，就会用 A 框架的知识
    /// 去解释 B 框架的代码，产出**看似合理实则不可信**的图 —— 实测把 ThinkPHP 规则
    /// 套到 Laravel 工程上会凭空造出上百个 `Table` / `HttpContract` 节点。
    ///
    /// 因此默认只在 detector 命中时生效；仅当某份知识确实是「该语言的通用兜底」
    /// （不含具体框架假设）时才显式打开。
    #[serde(default)]
    pub apply_without_detection: bool,
}

/// 知识库作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeScope {
    /// 框架知识：随框架识别加载，适用于所有使用该框架的工程。
    #[default]
    Framework,
    /// 项目知识：随项目识别加载，仅适用于被识别为该项目（其 detectors 命中）的工程。
    Project,
}

/// 路由 handler 的解析规则（**框架知识，不写死在内核**）。
///
/// 「哪个类/方法处理这个请求」在各框架里是完全不同的形态：
///
/// | 框架 | handler 形态 |
/// |------|-------------|
/// | ThinkPHP | `'admin.Login/login'`（点号表层级，斜杠分隔方法） |
/// | Laravel | `[LoginController::class, 'login']` / `'Ctrl@login'` |
/// | Symfony | `App\Controller\LoginController::login` |
/// | Rails | `'login#index'` |
///
/// 因此「用什么符号分隔方法」「类名怎么拼」「应用段有哪些」全部由 FKB 声明；
/// 内核只负责按声明展开候选并查表。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HandlerSpec {
    /// handler 串里 controller 与 method 的分隔符（按序尝试，取第一个能拆开的）。
    pub method_separators: Vec<String>,
    /// controller 内部表示命名空间层级的字符（会被替换成该语言的命名空间分隔符）。
    ///
    /// ThinkPHP 的 `v1.agent.AgentManage` → `v1\agent\AgentManage`。
    pub hierarchy_separators: Vec<String>,
    /// 类名候选模板，`{app}` 与 `{controller}` 为占位符。
    pub class_templates: Vec<String>,
    /// `{app}` 的候选值（模板 × 段 逐个展开）。
    pub app_segments: Vec<String>,
    /// 从路由文件路径推断 `{app}`：取该锚点目录的**上一级**目录名。
    /// 例：`app/api/route/pc.php` + 锚点 `route` → `api`。
    pub app_anchor_dir: Option<String>,
    /// 推断不出时 `{app}` 的兜底值。
    pub app_fallback: String,
}

/// 一条「中间件类 → 能力」声明：某个中间件确凿地提供了什么能力。
///
/// 例（CRMEB）：`AuthTokenMiddleware` 提供 `Authentication`。
/// 匹配只对**短名**（去命名空间后的最后一段）做，且大小写不敏感 —— 同一类中间件
/// 在不同 app 目录下会有不同命名空间（`app\api\middleware\AuthTokenMiddleware`
/// 与 `app\kefuapi\middleware\KefuAuthTokenMiddleware`），但语义由名字表达。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiddlewareCapability {
    /// 中间件类短名里需要**包含**的片段，如 `AuthToken` / `Throttle`。
    pub matches: String,
    /// 产出的能力名（进 `Capability` 通道，如 `Authentication` / `RateLimiting`）。
    pub capability: String,
}

/// **路由守卫识别规则**（框架级声明，内核据此从调用图收守卫）。
///
/// 三种挂载模型覆盖主流写法：
/// * `chain`：`Route::get(path)->middleware(X)`（ThinkPHP / Laravel 反向）；
/// * `positional`：`app.get(path, mw1, mw2, handler)`（Express / Koa）；
/// * `decorator`：`@UseGuards(X)` / `@login_required` / `@PreAuthorize` 落在与被修饰
///   路由**同一方法**上，按 `owner_fqn` 关联（NestJS / Python / Spring）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteGuardSpec {
    /// 路由定义调用模式（可多条：如 ThinkPHP 的 `Route::`、Express 的 `app`/`router`、
    /// NestJS 的 `@Get` 装饰器）。
    pub route_calls: Vec<RouteCallSpec>,
    /// 守卫如何挂到路由上。**可以声明多个**：同一框架常有多种挂载写法
    /// （NestJS 既有 `@UseGuards` 也有 `consumer.apply(...).forRoutes(...)`），
    /// 内核会把各模型产出的守卫**并集**起来。
    #[serde(default)]
    pub guard_attach: GuardAttach,
    /// 可选：中间件别名表符号名（Laravel 的 `Kernel::$routeMiddleware` → `middleware_aliases`）。
    /// 守卫写的是别名（`auth` / `web`）时，按此表还原成真实类。
    #[serde(default)]
    pub alias_table: Option<String>,
    /// 是否把**图里查不到节点的守卫**也当一个 `Middleware` 节点建出来。默认 **false**。
    ///
    /// 为什么是框架知识：
    /// * PHP 的守卫恒为类，查不到 = 类在 `vendor` / 命名空间未还原 —— 建出来就是凭空造节点，
    ///   故保持 false（「宁可缺不可猜」，该行为由测试钉住）。
    /// * JS / Python 的守卫是**函数值**（`const loginLimiter = rateLimit({...})`），解析器
    ///   不会为它建语法节点 —— 但"这条路由挂了一个叫 `loginLimiter` 的中间件"是源码里的
    ///   确凿事实。此时建一个同名 `Middleware` 语义节点是**如实记录**，不是猜测。
    #[serde(default)]
    pub synthesize_unresolved: bool,
}

/// 一条「路由定义调用」识别模式。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteCallSpec {
    /// 匹配目标：
    /// * `by = receiver`（默认）：`call.receiver`（大小写不敏感）**含**此串即命中
    ///   （ThinkPHP `Route`、Express `app`）；`receiver_ends_with = true` 时改为「以此串结尾」
    ///   （Laravel 的 `\Route`）。
    /// * `by = callee`：按 `call.callee`（大小写不敏感）匹配——装饰器 / 注解写法
    ///   （NestJS `@Get` 的 callee 即 `Get`、Spring `@GetMapping` 的 callee 即 `GetMapping`、
    ///   Python `@app.route` 的 callee 即 `app.route`）。此时 `receiver` 字段填装饰器名。
    pub receiver: String,
    /// 匹配维度：`receiver`（默认）或 `callee`。
    #[serde(default)]
    pub by: RouteMatchBy,
    /// 方法名 → HTTP 动词（`GET`/`POST`/`PUT`/`DELETE`/`PATCH`/`ANY`）。键大小写不敏感；
    /// 值取大写。ThinkPHP 的 `rule` → `ANY`、Laravel 的 `any` → `ANY` 都在此归一。
    #[serde(default)]
    pub verb_methods: HashMap<String, String>,
    /// 路径实参下标（默认 0）。
    #[serde(default = "default_zero")]
    pub path_arg: usize,
    /// handler 实参下标（默认 1）。`None` 表示路由定义不带 handler 实参。
    #[serde(default)]
    pub handler_arg: Option<usize>,
    /// 组前缀方法名（如 ThinkPHP `group`）——用于把组前缀拼到组内每条路由路径前。
    #[serde(default)]
    pub group_method: Option<String>,
    /// `by = receiver` 时是否「以 `receiver` 结尾」而非「含」。默认 false。
    #[serde(default)]
    pub receiver_ends_with: bool,
    /// 是否把**标识符实参**（`app.post('/x', loginLimiter, handler)` 里的 `loginLimiter`）
    /// 也当作中间件。默认 **false**。
    ///
    /// 为什么是框架知识：PHP 的中间件恒为 `X::class` 字面量，而 JS / Python 的中间件是
    /// **函数引用**（`loginLimiter` / `isAuthenticated`），在调用图里落成变量名的
    /// `Unknown`。PHP 侧必须保持 false —— `->middleware($v)` 这种动态实参若被收进来，
    /// 会把一个变量名当成中间件挂上去，是编造（该行为由测试钉住）。
    #[serde(default)]
    pub accept_identifier: bool,
}

/// 路由匹配的维度。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatchBy {
    /// 按 `call.receiver` 匹配（链式 / 位置参数写法）。
    #[default]
    Receiver,
    /// 按 `call.callee` 匹配（装饰器 / 注解写法）。
    Callee,
}

/// 守卫挂载模型。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuardAttachSpec {
    /// 链式：`Route::get(path)->middleware(X[, arg])`。
    Chain(ChainGuardSpec),
    /// 位置参数：`app.get(path, mw1, mw2, handler)`——`path_arg` 之后到 `handler_arg`
    /// （不含）之间的实参都是中间件。
    #[default]
    Positional,
    /// 装饰器 / 注解：守卫与被修饰路由**同 owner**（方法 / 函数），按 `owner_fqn` 关联。
    Decorator(DecoratorGuardSpec),
    /// 消费者式挂载：NestJS 的 `consumer.apply(X).forRoutes(...)`。
    Consumer(ConsumerGuardSpec),
}

/// 一个 / 多个守卫挂载模型（`guard_attach` 的值）。
///
/// 允许单值或列表，是为了让同一框架声明**多种**挂载写法而不改内核、也不破坏既有 FKB。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GuardAttach {
    One(GuardAttachSpec),
    Many(Vec<GuardAttachSpec>),
}

impl GuardAttach {
    /// 归一成模型列表。
    pub fn specs(&self) -> Vec<&GuardAttachSpec> {
        match self {
            GuardAttach::One(s) => vec![s],
            GuardAttach::Many(v) => v.iter().collect(),
        }
    }
}

impl Default for GuardAttach {
    fn default() -> Self {
        GuardAttach::One(GuardAttachSpec::Positional)
    }
}

/// `consumer.apply(X).forRoutes(...)` 这类「模块里声明、作用于别处路由」的挂载。
///
/// NestJS 的中间件是在 `*.module.ts` 的 `configure()` 里挂的：
/// ```ts
/// consumer.apply(AuthMiddleware).forRoutes({ path: '*', method: RequestMethod.ALL });
/// ```
/// 它对哪些路由生效由 `forRoutes` 的**实参**决定，而模块 → 控制器 → 路由的映射
/// 内核无从得知，故"作用范围"必须由 FKB 声明（见 [`ConsumerScope`]）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsumerGuardSpec {
    /// 消费者变量名（默认 `consumer`）。
    pub receiver: String,
    /// 挂载中间件的方法名（默认 `apply`）。
    pub apply_method: String,
    /// 指定作用范围的方法名（默认 `forRoutes`）。
    pub for_routes_method: String,
    /// 视为"本模块全部路由"的通配实参（默认 `["*"]`）。
    pub wildcards: Vec<String>,
    /// 命中通配时按什么范围展开（默认 [`ConsumerScope::Directory`]）。
    pub scope: ConsumerScope,
}

/// `forRoutes` 命中通配时，中间件作用于哪些路由。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerScope {
    /// 只认**显式路径**（`forRoutes('users')`），通配一律不展开 —— 最保守。
    ExplicitOnly,
    /// 通配时作用于**与模块同目录**的控制器里的路由（NestJS 的惯例：
    /// `user.module.ts` 与 `user.controller.ts` 同在 `src/user/`）。默认。
    #[default]
    Directory,
    /// 通配时作用于**全部**路由（单模块工程的等价写法，多模块会过度声称）。
    All,
}

impl Default for ConsumerGuardSpec {
    fn default() -> Self {
        Self {
            receiver: "consumer".into(),
            apply_method: "apply".into(),
            for_routes_method: "forRoutes".into(),
            wildcards: vec!["*".into()],
            scope: ConsumerScope::Directory,
        }
    }
}

/// 链式守卫：`Route::get(path)->middleware(X)`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChainGuardSpec {
    /// 挂载守卫的成员方法名（如 `middleware`）。
    pub method: String,
    /// 守卫类实参下标（默认 0）。
    #[serde(default = "default_zero")]
    pub arg_index: usize,
    /// 第二实参下标（区分强制 / 可选，如 `AuthTokenMiddleware::class, false`）。`None` 表示无。
    #[serde(default)]
    pub arg2_index: Option<usize>,
}

/// 装饰器 / 注解守卫：按 `owner_fqn` 关联。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecoratorGuardSpec {
    /// 路由装饰器 / 注解的 callee 名单（如 `app.route` / `@Get` / `@GetMapping`）。
    /// 与解析器输出的 callee 逐字比对（TS 的装饰器带 `@` 前缀）；大小写不敏感。
    pub route_decorators: Vec<String>,
    /// 守卫装饰器 / 注解的 callee 名单（如 `login_required` / `@UseGuards` / `@PreAuthorize`）。
    pub guard_decorators: Vec<String>,
    /// 守卫名的**正则模式**（大小写不敏感）。命中其一即算守卫。
    ///
    /// 为什么必须有它：JS / Python 的守卫常常是**项目自己写的装饰器**
    /// （`@requires_admin` / `@jwt_or_403` / `@staff_only`…），穷举名字是打地鼠。
    /// 而"什么样的名字算守卫"是框架 / 项目约定，由 FKB 声明：
    ///   `["(login|auth|jwt|token)", "(permission|role|admin|staff|owner)", "(guard|secure|required|only)"]`
    /// Java / Spring 的注解是框架固定的（`@PreAuthorize` 等），用 `guard_decorators` 精确列举即可。
    #[serde(default)]
    pub guard_name_patterns: Vec<String>,
    /// 守卫名的**排除正则**（大小写不敏感），**优先级高于** `guard_decorators` 与
    /// `guard_name_patterns` —— 命中即"不是守卫"。
    ///
    /// 为什么必须有它：宽泛的包含模式会误伤——
    /// * Swagger / OpenAPI 的**文档**装饰器 `@ApiBearerAuth()` 名字里带 `auth`，
    ///   却完全不做鉴权（实测 NestJS realworld 有 17 条路由被它误标成"过了守卫"）；
    /// * NestJS 的**参数**装饰器 `@User('email')` / `@Body()` / `@Param()` 只是取值，
    ///   不是守卫；局部变量名（`_user`）也不该当中间件。
    /// 「哪些名字不算守卫」同样是框架知识，由 FKB 声明。
    #[serde(default)]
    pub guard_exclude_patterns: Vec<String>,
    /// 要求守卫 / 路由调用的 callee **以 `@` 开头**（即解析器标注的装饰器调用点）。
    /// 默认 false。
    ///
    /// TS 解析器给装饰器 callee 加 `@` 前缀（`@Get` / `@UseGuards`），据此可把装饰器与
    /// **普通方法调用**区分开 —— 否则 `this.userService.generateJWT(...)` 这种名字里带
    /// `jwt` 的业务方法会被当成守卫（实测误报）。
    #[serde(default)]
    pub require_at_prefix: bool,
    /// 按**路由调用的 handler 实参**去关联守卫（而不是按 owner）。默认 `None`。
    ///
    /// Django 这类框架把路由与视图**分开写**：
    ///   urls.py     `path('profile', views.profile)`        ← 路由在这里
    ///   views.py    `@login_required\ndef profile(request):` ← 守卫在这里
    /// 两者 owner 不同（一个是 urls 模块、一个是视图函数），按 owner 关联必然落空。
    /// 声明本字段（handler 所在实参下标）后，守卫按"owner_fqn **以 handler 名结尾**"匹配 ——
    /// handler 写 `views.profile`，视图函数 owner 是 `myapp.views.profile`，后缀即命中。
    #[serde(default)]
    pub link_via_handler_arg: Option<usize>,
    /// 要求守卫 / 路由调用**没有接收者**（裸名调用）。默认 false。
    ///
    /// Python 的 `@login_required`、Java 的 `@PreAuthorize` 都是裸名；而
    /// `self.generate_jwt()` / `this.checkAuth()` 这类成员调用有接收者，不是装饰器。
    #[serde(default)]
    pub require_no_receiver: bool,
    /// 守卫名取自**实参**还是**装饰器名本身**。默认 **true**（取自实参）。
    ///
    /// * `true`：NestJS 的 `@UseGuards(JwtAuthGuard)` —— 守卫是实参里的那个类；
    ///           无参的 `@login_required` 仍退回装饰器名。
    /// * `false`：Spring 的 `@PreAuthorize("hasRole('ADMIN')")` —— 实参是 SpEL 表达式，
    ///           真正的"守卫"是注解本身（`PreAuthorize` / `Secured` / `RolesAllowed`）。
    #[serde(default = "default_true")]
    pub name_from_args: bool,
    /// 是否也认**类级**守卫（守卫注解 / 装饰器打在类上，作用于该类的所有路由方法）。
    /// 默认 **true**：NestJS 常在 `@Controller` 类上打 `@UseGuards`，
    /// Spring 常在类上打 `@PreAuthorize`，Python 类视图也常用类级装饰器。
    #[serde(default = "default_true")]
    pub include_class_level: bool,
}

fn default_true() -> bool {
    true
}

fn default_zero() -> usize {
    0
}

/// 数据模型的读 / 写动词清单（方法名，大小写不敏感）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DbVerbsSpec {
    /// 写动词：`save` / `insert` / `update` / `delete` …
    #[serde(default)]
    pub write: Vec<String>,
    /// 读动词：`find` / `select` / `value` / `count` …
    #[serde(default)]
    pub read: Vec<String>,
}

/// 魔法方法（`@method` 注解）的转发目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MagicDelegationSpec {
    /// 转发目标属性名（如 CRMEB 的 `dao`）。类型由该属性的注入方式按既有规则推断。
    pub property: String,
    /// 转发解析的置信度（低于"方法精确命中"，因为它是注解声明而非源码）。
    pub confidence: f32,
}

impl Default for MagicDelegationSpec {
    fn default() -> Self {
        Self { property: String::new(), confidence: 0.7 }
    }
}

impl Default for HandlerSpec {
    fn default() -> Self {
        Self {
            method_separators: vec!["/".into()],
            hierarchy_separators: Vec::new(),
            class_templates: Vec::new(),
            app_segments: Vec::new(),
            app_anchor_dir: None,
            app_fallback: String::new(),
        }
    }
}

/// 框架识别信号。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Detector {
    /// manifest 文件里存在某个依赖。
    ManifestDependency {
        manifest: String,
        dependency: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
    /// 存在某个特征文件/目录。
    FileExists {
        path: String,
        #[serde(default = "default_conf")]
        confidence: f32,
    },
}

fn default_conf() -> f32 {
    0.9
}

/// 框架根解析规则。
///
/// 例：从 `composer.json` 的 `autoload.psr-4` 解析出 `AppRoot = "app"`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootRule {
    pub id: String,
    /// 产出的事实键，如 `app_root`。
    pub key: String,
    pub source: RootSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
    /// 兜底候选目录；解析失败时按顺序探测。
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootSource {
    /// 从 JSON manifest 的某个指针取值。
    ManifestJson {
        manifest: String,
        /// 点分路径，如 `autoload.psr-4`。
        pointer: String,
        /// 取值策略。
        pick: PickStrategy,
    },
    /// 直接探测目录是否存在。
    DirectoryExists { path: String },
    /// 从 PHP 配置文件（如 ThinkPHP 的 `config/database.php`）按点分指针取值。
    ///
    /// 用于自动探测工程级配置（如表前缀），避免把项目特定约定写死在 FKB。
    ManifestPhp {
        /// 相对工程根的路径，如 `config/database.php`。
        manifest: String,
        /// 点分路径，如 `connections.mysql.prefix`。
        pointer: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickStrategy {
    /// 取所有映射目录中最浅的一个。
    ShallowestDir,
    /// 取第一个映射目录。
    FirstDir,
    /// 取键名等于指定值的映射目录。
    ByNamespaceKey,
}

/// P3 符号表装载器。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoaderSpec {
    pub id: String,
    /// 输出符号表名：`schema` / `config_keys` / `i18n` / `facade_map` / `route_list` 等。
    pub table: String,
    pub from: LoaderSource,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoaderSource {
    /// 读取单个文件并按 key_path 取值。
    File {
        path: String,
        #[serde(default)]
        key_path: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// 按 glob 批量读取（如 `lang/*/*.php`），可用捕获组提取 locale。
    Glob {
        pattern: String,
        /// 从路径捕获组中提取 locale 的正则（第一个捕获组）。
        #[serde(default)]
        locale_regex: Option<String>,
        #[serde(default)]
        format: FileFormat,
    },
    /// FKB 内联声明的常量表（如 FacadeMap —— 由框架知识给出，不靠猜）。
    Inline { rows: Vec<Value> },
    /// 内置装载器（由流水线实现，如从 PHP 源码收集 `$table` 与 `Db::name`）。
    Builtin {
        name: String,
        #[serde(default)]
        params: Value,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileFormat {
    Php,
    Json,
    Yaml,
    Sql,
    Text,
    #[default]
    Auto,
}

/// 一条规则：在某阶段，对匹配到的目标执行一组动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub phase: Phase,
    pub selector: Selector,
    pub binding: Vec<Action>,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

/// 能力接口的匹配模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// 按「真实后端调用」匹配：对 `types × methods` 摊出 `Type::method|*Suffix::method`。
    /// 适用于 P7 能解析出内部调用的库（Predis / Redis / 自研客户端）。
    #[default]
    Backend,
    /// 按「封装类命名约定」匹配：对 `types` 摊出 `*Type::method`（类后缀 + 方法名）。
    /// 适用于框架门面（ThinkPHP / Laravel `Cache`）经魔术分发、P7 看不到内部真实调用，
    /// 只能认「名为 `*CacheService::get` 的封装方法」——这是框架自带约定，非随意猜测。
    Wrapper,
}

/// 能力接口声明：某库 / 框架的哪些类型、哪些方法暴露某能力（读 / 写）。
///
/// 例：`{ capability: cache, types: ["Predis\\Client"], read: [get], write: [set] }`
/// 由装载器查 `capability_templates` 摊平成匹配 `Predis\Client::get` / `*Client::get`
/// 的合成规则。属于「框架 / 库知识」（分语言），与跨语言的能力模板分离。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilityInterface {
    /// 能力名（查核心能力模板，如 `cache` / `queue` / `config`）。
    pub capability: String,
    /// 暴露该能力的类型 FQN（或尾部片段，如 `Predis\Client` / `Cache`）。
    pub types: Vec<String>,
    /// 读语义方法名。
    pub read: Vec<String>,
    /// 写语义方法名。
    pub write: Vec<String>,
    /// 覆盖默认置信度。
    pub confidence: Option<f32>,
    /// 匹配模式：`backend`（按真实调用）或 `wrapper`（按封装类命名约定）。
    pub match_mode: MatchMode,
}

/// 能力模板（跨语言、核心机制，**不属于 FKB**）：描述「某能力如何落成图节点与边」。
///
/// 例：`cache` → 建 `ExternalSystem`(Cache) 节点、身份取 arg0、`ReadsCache` / `WritesCache` 边。
/// 同一模板被所有语言、所有库的 `capability_interfaces` 复用 —— 识别逻辑只写一次，
/// 且它不绑定任何框架 / 语言，故放在核心配置而非 `fkb/<lang>/` 下。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CapabilityTemplate {
    pub node: NodeKind,
    pub subtype: Option<String>,
    pub identity: IdentitySpec,
    pub fields: Vec<FieldSpec>,
    pub read_link: EdgeKind,
    pub write_link: EdgeKind,
    #[serde(default = "default_conf")]
    pub confidence: f32,
}

/// 选择器：决定规则作用于什么。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Selector {
    /// 调用点，如 `Db::name('store_order')`。
    Call {
        /// callee 匹配模式，支持 `|` 分隔多模式与 `*` 通配：
        /// `think\facade\Db::name|*:where|Db::raw`
        #[serde(default)]
        callee: Option<String>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// 继承 / 实现 / trait。
    Inheritance {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        with_property: Option<String>,
    },
    /// 配置文件条目。
    ConfigEntry {
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        key_path: Option<String>,
        /// 附加谓词（只作用在配置条目上）。
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// 语法声明。
    Declaration {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        fqn_matches: Option<String>,
    },
    /// **图上的节点**（P6 专用：选择器是节点而非源码）。
    Node {
        #[serde(default)]
        node_kind: Option<NodeKind>,
        #[serde(default)]
        r#where: Vec<Predicate>,
    },
    /// P7 动态解析：容器 make / 事件触发 / 门面调用 / 获取器等。
    Dynamic {
        #[serde(default)]
        call: Option<String>,
        #[serde(default)]
        channel: Option<String>,
    },
}

/// 谓词（`where` 条件）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    /// 目标（类）拥有某属性。
    HasProperty(String),
    /// 权威符号表中该键存在。
    InSymbolTable { table: String, key_of: ValueSource },
    /// 权威符号表的列命中给定名字之一。
    ColumnsMatch { table: String, names: Vec<String> },
    /// 节点上已有某标注。
    HasAnnotation { kind: String },
    /// 作用域链上不存在给定能力。
    NoneOfCapability(Vec<String>),
    /// i18n 是否存在缺失 locale。
    HasMissing(bool),
    /// fan_in 不小于阈值。
    FanInGte(u64),
    /// 参数个数等于给定值。
    ArgCount(usize),
    /// 节点名（或 identity 值）包含给定子串（大小写不敏感）。
    NameMatches(String),
    /// 节点属性等于给定值。
    PropertyIs { name: String, value: String },
    /// 调用点第 `arg` 个实参（字符串）以 `prefix` 开头（大小写敏感）。
    /// 用于按调用实参前缀收窄匹配，例如只挑 `Route::get('crontab/...')` 这类路由。
    ArgStartsWith { arg: usize, prefix: String },
    /// 配置条目的值是**数组**且元素个数不少于 `n`（仅对 `kind: config_entry` 生效）。
    ///
    /// PHP 配置解析会把数组元素展开成独立条目（`listen.evt.0`）并与父条目
    /// （`listen.evt`）**同时存在**。而 `key_path` 是纯子串通配，`"*"` 与
    /// `"listen.*"` 都会把父子两条都匹配上，于是同一事件合成出 `evt` 与 `evt.0`
    /// 两个节点（后者是标量，建不出 `HandledBy` 边，纯噪声）。
    /// 本谓词只放行数组条目：既排除掉展开出的标量叶子，也顺带滤掉
    /// `app_init => []` 这类框架级空标签。
    EntryArityGte(usize),
    /// 节点 **FQN** 包含给定子串（大小写不敏感）。
    ///
    /// 为什么需要它：很多约定是按**命名空间位置**成立的，节点短名看不出来 ——
    /// 控制器方法 `detail` 的语义来自它的 FQN `app\api\controller\Goods::detail`，
    /// 自动路由规则只能靠 `\controller\` 这一段筛出来。
    FqnMatches(String),
    /// 节点名**不在**给定列表内（大小写不敏感）。
    ///
    /// 约定的适用面总要剔掉语言 / 框架钩子：`__construct` / `initialize` 同样落在
    /// controller 命名空间里，但绝不是 HTTP 入口。名单由 FKB 给出，内核不认识具体名字。
    NameNotIn(Vec<String>),
    /// 节点**尚未被认领**：既没有指定种类的入边，也没有该种类的待定链接指向它
    /// （详见 [`crate::GraphWorkspace::claimed_by`] 的形态比对）。
    ///
    /// 「显式声明优先于约定推断」：已经写在 `Route::get` / `Route::resource` 里的某个
    /// 方法，不该再被目录约定兜出第二个契约 —— 否则 CRMEB 这类全量注册路由的工程会
    /// 凭空多出上千个重复端点。与"宁可缺边，不可错边"是同一条记账原则。
    NotClaimedBy(String),
}

/// 绑定动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Action {
    /// 打标注。
    Annotate(AnnotateAction),
    /// 合成节点。
    Synthesize(SynthesizeAction),
    /// 仅建边。
    Link(LinkAction),
    /// 把**一类边投影到另一层**。
    Project(ProjectAction),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotateAction {
    /// 期望执行的阶段（Pre / Post）。当规则 phase 与此处不一致时跳过。
    pub phase: Option<Phase>,
    pub channel: AnnotationChannel,
    pub target: AnnotateTarget,
    pub annotations: Vec<AnnotationSpec>,
    pub merge: MergeStrategy,
    /// 作用域：`[RouteSelf, EnclosingGroup, Global]`。
    pub scope: Option<Vec<String>>,
    pub r#where: Vec<Predicate>,
    /// 从作用域链继承能力时的置信度衰减。
    pub confidence_scale: Option<f32>,
}

impl Default for AnnotateAction {
    fn default() -> Self {
        Self {
            phase: None,
            channel: AnnotationChannel(AnnotationChannel::FKB_MARK.to_string()),
            target: AnnotateTarget::Matched,
            annotations: Vec::new(),
            merge: MergeStrategy::MaxByKind,
            scope: None,
            r#where: Vec::new(),
            confidence_scale: None,
        }
    }
}

/// 标注目标。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotateTarget {
    /// 选择器直接匹配到的节点。
    Matched,
    /// 从匹配结果的某个字段派生（如 `array_values` → 解析成类）。
    FromField {
        source: ValueSource,
        resolve: Option<ResolveAs>,
    },
    /// 引用本规则此前合成出的节点。
    SynthesizedRef(String),
}

impl Default for AnnotateTarget {
    fn default() -> Self {
        AnnotateTarget::Matched
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotationSpec {
    pub kind: String,
    pub subkind: Option<SubkindSource>,
    pub severity: Option<String>,
    pub confidence: f32,
    pub evidence: Option<Value>,
    pub channel: Option<AnnotationChannel>,
}

impl Default for AnnotationSpec {
    fn default() -> Self {
        Self {
            kind: String::new(),
            subkind: None,
            severity: None,
            confidence: 1.0,
            evidence: None,
            channel: None,
        }
    }
}

/// subkind 的来源：字面量 / 权威符号表 / 计算值 / fan_in 分级。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubkindSource {
    Literal(String),
    FromSymbolTable {
        table: String,
        field: String,
        #[serde(default)]
        of: Option<ValueSource>,
    },
    FromFanIn {
        thresholds: FanInThresholds,
    },
    Computed(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanInThresholds {
    pub high: u64,
    pub medium: u64,
    #[serde(default)]
    pub low_label: Option<String>,
    #[serde(default)]
    pub medium_label: Option<String>,
    #[serde(default)]
    pub high_label: Option<String>,
}

/// 合成动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SynthesizeAction {
    /// 节点种类（开放字符串，如 `Table` / `HttpContract` / `Event` / `Queue` / `Cache` / `Topic`）。
    ///
    /// 若同时给了 `subtype`，则**子类型提升为 kind**（`kind = subtype`）；
    /// 此时 `category` 仍记为最终 kind（第一类语义节点等同于 kind）。
    pub node: NodeKind,
    /// 子类型（`Event` / `Queue` / `Cache`…），可选；给了就作为最终 kind。
    pub subtype: Option<String>,
    pub identity: IdentitySpec,
    pub fields: Vec<FieldSpec>,
    pub link: Option<LinkSpec>,
    pub confidence: f32,
    /// `MergeBy(key)` —— 多份数据源合并成一个节点的不同字段，而不是建多个节点。
    pub modifiers: Vec<String>,
    /// 别名注册（合成后自动写入 by_alias）。
    pub alias: Option<AliasSpec>,
    /// **一条调用展开成 N 个语义节点**（表驱动）。
    ///
    /// 典型场景：REST 资源路由 `Route::resource('cms', Ctrl::class)` 一条语句
    /// 其实是 7 条契约（index / create / save / read / edit / update / delete）。
    /// 展开表由 **FKB 给出**（内核零框架知识），内核只负责：按表逐个变体执行
    /// 同一份 `identity` / `fields` / `link`，并把变体的 `method` / `entry`
    /// 注入 `{ expand_method: true }` / `{ expand_entry: true }` 两个来源，
    /// 把 `path_suffix` 追加到算出的路径之后（在 `Route::group` 前缀之后）。
    pub expand: Option<ExpandSpec>,
}

/// 展开表：一条调用 → N 个语义节点。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExpandSpec {
    pub variants: Vec<ExpandVariant>,
    /// 白名单来源：**同一语句行**上名为该值的链式调用的实参数组
    /// （`->only(['index','delete'])`）。给出时只合成列表内的动作。
    pub only: Option<String>,
    /// 黑名单来源：`->except(['read'])`。给出时从动作表里剔除。
    pub except: Option<String>,
}

impl Default for ExpandSpec {
    fn default() -> Self {
        Self { variants: Vec::new(), only: None, except: None }
    }
}

/// 展开表的一行：一个动作（如 REST 的 `index`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ExpandVariant {
    /// 动作名（与 `only` / `except` 里写的名字对应）。
    pub name: String,
    /// HTTP method（供 `{ expand_method: true }` 取用）。
    pub method: Option<String>,
    /// 追加到路径之后的后缀（如 `/create`、`/:id`）。
    pub path_suffix: Option<String>,
    /// handler 的入口方法名（供 `{ expand_entry: true }` 取用）。
    pub entry: Option<String>,
}

impl Default for SynthesizeAction {
    fn default() -> Self {
        Self {
            node: NodeKind(NodeKind::UNKNOWN.to_string()),
            subtype: None,
            identity: IdentitySpec::default(),
            fields: Vec::new(),
            link: None,
            confidence: 0.9,
            modifiers: Vec::new(),
            alias: None,
            expand: None,
            }
            }
}

/// 合成节点的身份规格。
///
/// **`identity` 是整个 Synthesize 阶段的核心**：三条不同的规则只要算出
/// 相同的 identity，产出就会幂等合并成一个节点。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentitySpec {
    /// `Fqn` / `Named` / `ContractId`。
    pub kind: SynthesizedKind,
    /// 单值身份（`Fqn` / `Named`）。
    pub value: Option<ValueSource>,
    /// `ContractId` 的 HTTP 方法来源。
    pub method: Option<ValueSource>,
    /// `ContractId` 的路径来源。
    pub path: Option<ValueSource>,
    #[serde(default)]
    pub normalize: Vec<NormalizeStep>,
    /// 主身份取不到（或 `require_class` 判定非类）时的兜底来源。
    ///
    /// 例：队列 topic 优先取 `arg:0`（实参里的 Job 类），取不到时退回 `owner_class`
    /// （产生该调用的类自身）。两条来源算出相同 identity 时幂等合并为同一节点。
    #[serde(default)]
    pub value_fallback: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct FieldSpec {
    pub name: String,
    pub value: Option<ValueSource>,
    /// 累积合并：`{ key: locale, value: text }`。
    pub accumulate: Option<AccumulateSpec>,
    /// 从权威符号表补充字段。
    pub from_symbol_table: Option<SymbolFieldSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccumulateSpec {
    pub key: ValueSource,
    pub value: ValueSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolFieldSpec {
    pub table: String,
    pub field: String,
    #[serde(default)]
    pub of: Option<ValueSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkSpec {
    pub kind: EdgeKind,
    /// 边的另一端来源（如 handler 字符串）。
    pub to: Option<ValueSource>,
    /// 目标**方法名**（可选）。给出时优先连到 `类::方法`。
    ///
    /// 两个用途：① 数组式 handler 的方法部分（`[Ctrl::class, 'method']` 的第 1 项）；
    /// ② 让「消费入口方法」由 FKB 决定，而不是内核硬编码的
    /// `handle`/`fire`/`doJob`/`__invoke`/`run` 列表。
    #[serde(default)]
    pub to_method: Option<ValueSource>,
    /// `to` 取不到目标时的兜底来源（如队列消费方 `arg:0` 解析不出时退回 `receiver_class`）。
    #[serde(default)]
    pub to_fallback: Option<ValueSource>,
    /// 方向：incoming（来源指向新节点）/ outgoing（新节点指向来源）/ to_target。
    pub direction: Direction,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// 匹配到的调用方 ——> 新合成节点。
    #[default]
    Incoming,
    /// 新合成节点 ——> 匹配到的调用方。
    Outgoing,
    /// 新合成节点 ——> `to` 解析出的目标。
    ToTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AliasSpec {
    pub namespace: String,
    pub key: ValueSource,
    pub qualifier: Option<ValueSource>,
}

/// 值来源（结构化，便于 YAML 书写）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ValueSource {
    /// 第 n 个实参。
    pub arg: Option<usize>,
    /// 实参是**数组**时，按下标取第 n 项。
    ///
    /// 用于数组式 handler —— Laravel 主形式 `Route::get('/x', [Ctrl::class, 'method'])`
    /// 的类名在 `arg1[0]`、方法名在 `arg1[1]`，此前无法表达（只能取整个数组）。
    pub element: Option<usize>,
    /// 实参是对象字面量时取其字段（如 `uni.request({url:..})`）。
    pub field: Option<String>,
    /// 取类属性（如 Model 的 `$table`）。
    pub property: Option<String>,
    /// 取当前节点自身（类短名 / FQN）。
    #[serde(rename = "self")]
    pub self_value: Option<bool>,
    /// 跳过 key_path 的前 n 段后剩余的部分（点分连接）。
    pub path_segment: Option<usize>,
    /// 调用点的方法名（如 `Route::post` 的 `post`）。
    pub method_name: Option<bool>,
    /// 配置条目的值本身。
    pub entry_value: Option<bool>,
    /// 取数组的全部值（一对多）。
    pub array_values: Option<bool>,
    /// 取数组长度。
    pub array_length: Option<bool>,
    /// 取配置文件条目的 key_path。
    pub key_path: Option<bool>,
    /// 取文件名主干。
    pub file_stem: Option<bool>,
    /// 取当前 locale（i18n 装载时）。
    pub locale: Option<bool>,
    /// 取「发起调用的类」：`owner_fqn` 去掉末尾 `::method` 后的类 FQN。
    ///
    /// 通用语义：当身份是「调用方类」而非某个实参时（如 self-enqueue 模式里，
    /// 队列的 Job / topic 就是产生它的那个类自身，典型如 `QueueTrait::dispatch`
    /// 经 `->job(__CLASS__)` 把消费方设成调用类）。这是框架无关的提取能力。
    pub owner_class: Option<bool>,
    /// 取 `owner_fqn` 末尾的**成员名**（方法 / 字段），与 `owner_class` 互补。
    ///
    /// 典型用途：Java 方法级注解（`@GetMapping`）的调用点 `owner_fqn` 是方法 FQN
    /// `com.example.Ctrl.list`，`owner_member` 取出 `list`，供 link 的 `to_method`
    /// 把 `HandledBy` 精确连到**处理方法**节点（而非控制器类），让视角能沿方法
    /// 的调用链继续下钻。类级注解 `owner_fqn` 已是类 FQN，取到的是类短名，
    /// 查不到方法节点时 `find_target_node` 回退到类节点，语义安全。
    pub owner_member: Option<bool>,
    /// 取调用点的接收者类：把 `receiver` 经 import 别名还原成 FQN。
    ///
    /// 与 `owner_class`（调用所在类）不同，这是「被调用方的接收者类」，
    /// 例如 `QueueThink::push()` 里 `QueueThink` 经别名还原成 `think\facade\Queue`。
    pub receiver_class: Option<bool>,
    /// 取调用点关切的「主领域类型」（`CallSiteFact.entity`），如事件类型
    /// `OrderPlacedEvent`。用于把「同一事件类型」的发布方与订阅方归并到同一个
    /// `Event` 节点（而非各自以方法名命名）。取不到（parser 未识别）时整体返回
    /// `None`，交给 `value_fallback`（如 `owner_member`）兜底。
    pub entity: Option<bool>,
    /// 当 `resolve: class_const` 时，若解析结果在代码库中不存在为类节点，则整体返回
    /// `None`（而不是把变量名 / 字面量当类用）。用于「优先用实参里的 Job 类，否则
    /// 退回 `owner_class`」这类兜底，避免 `$action` 之类的字符串污染语义身份。
    pub require_class: Option<bool>,
    /// 只接受**字面量**（字符串 / 标量），拒绝变量与表达式文本。
    ///
    /// `arg` 对变量 / 拼接表达式会求值成 `FactValue::Unknown(Some(原文))`（见
    /// `gt-adapter-parser::php::value`），直接采信会把 `$name`、`self::X . $y` 这类
    /// 源码文本当成身份，凭空造出垃圾语义节点。与 `require_class` 对称：判否即整体
    /// 返回 `None`，交给 `value_fallback` 兜底。
    pub require_literal: Option<bool>,
    /// 字面量。
    pub literal: Option<String>,
    /// 嵌套来源：`{ source: { arg: 1 }, field: 'url' }`。
    pub source: Option<Box<ValueSource>>,
    /// 变换（如 `class_to_topic`、`snake_plural`）。
    pub transform: Option<TransformSpec>,
    /// 归一化链。
    pub normalize: Option<Vec<NormalizeStep>>,
    /// 解析方式。
    pub resolve: Option<ResolveAs>,
    /// 取不到时的默认值。
    pub default: Option<String>,
    /// 多段拼接：`{ path: [{file_stem:true},{key_path:true}], join: '.' }`。
    pub path: Option<Vec<ValueSource>>,
    pub join: Option<String>,
    /// 取**当前展开变体**的 HTTP method（`expand.variants[].method`）。
    ///
    /// 只有配合 `Synthesize.expand` 使用才有值：一条调用展开成 N 个语义节点时，
    /// 每个变体各有一套 method / 路径后缀 / 入口方法（如 REST 资源路由）。
    pub expand_method: Option<bool>,
    /// 取**当前展开变体**的入口方法名（`expand.variants[].entry`），供
    /// `link.to_method` 把边精确连到「处理该动作的方法」。
    pub expand_entry: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TransformSpec {
    pub snake_plural: Option<bool>,
    pub snake: Option<bool>,
    pub strip_namespace: Option<bool>,
    pub class_to_topic: Option<bool>,
    pub lower: Option<bool>,
    pub upper: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveAs {
    /// `Foo::class` → 完全限定类名 → 查 by_name。
    ClassConst,
    /// `'Login/appleLogin'` → 按 控制器/方法 模式拼出 FQN。
    HandlerPattern,
    /// 查 by_alias 索引。
    ByAlias,
    /// 直接当作名字使用。
    AsIs,
}

/// 归一化步骤（identity 幂等合并的关键）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NormalizeStep {
    StripPrefix(Vec<String>),
    Lower,
    Upper,
    /// 保证以 `/` 开头。
    LeadingSlash,
    /// 复数转单数。
    Singularize,
    /// 类名转 `snake_case` 复数（Model 约定表名）。
    SnakePlural,
    /// 去掉命名空间，只留最后一段。
    StripNamespace,
    /// 取**点分**路径的最后一段：`app.tasks.send_email` → `send_email`。
    ///
    /// 与 [`Self::StripNamespace`] 只差一个分隔符 `.`：Python / Java 的命名空间是
    /// 点分的，而 `StripNamespace` 刻意**不拆** `.`（否则会把 Java 自动路由的包名
    /// 一起拆掉，改变既有行为）。故另起一个**加性**的步骤，专供「同一语义实体的
    /// 长短名要归并」这类场景 —— 例如 Celery 任务的注册方只有短名、投递方却因
    /// `import` 还原成了完全限定名，不归一就会拆成两个节点。
    ShortName,
    /// 路径参数段归一化：每个 `:` 开头的段都折成 `:*`。
    ///
    /// 契约桥的关键一步：后端路由写 `invoice/detail/:id`，前端拼接式 URL
    /// `'invoice/detail/' + id` 规整出 `invoice/detail/:param` —— 参数名不同但
    /// **形状相同**，HTTP 匹配本就只看形状。不折一下这两条永远合不到一个节点，
    /// 路由视角里就"看不到前端"。
    ParamWildcard,
    /// 去掉 `?` 起的查询串（页面跳转 URL 常带 `?id=1`，但路由身份只看路径）：
    /// `uni.navigateTo({ url: '/pages/detail?id=1' })` 与 `pages.json` 里的
    /// `/pages/detail` 汇聚到同一个 `Page` 节点。
    StripQuery,
    Trim,
    Replace { from: String, to: String },
}

/// 仅建边的动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct LinkAction {
    pub kind: EdgeKind,
    pub from: Option<ValueSource>,
    pub to: Option<ValueSource>,
    pub resolve: Option<ResolveAs>,
    pub confidence: Option<f32>,
}

/// 边投影动作：把**一类边**从它所在的层投影到另一层。
///
/// 遍历匹配节点的每条 `along` 出边，起点沿 `from` 边种类链走、终点沿 `to` 链走，
/// 在两头的落点之间建一条 `kind` 边。**一对多**：一个实体有几条 `@ManyToOne`
/// 就产出几条外键边（这是 `Link` 做不到的 —— 它的两端只能各取一个名字）。
///
/// 为什么需要内核给这条能力：「类 → 它映射的表」本质是**沿 `MapsTo` 走一跳**，
/// 而 `ValueSource` 只认名字（`self_value` / `property`），取不到"边的那一头"。
/// 走哪条边仍完全由 FKB 声明 —— 内核依旧不认识 TypeORM。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ProjectAction {
    /// 产出的边种类。
    pub kind: EdgeKind,
    /// 遍历匹配节点的每条**此类**出边（没有该类边 → 无产出，天然跳过无关节点）。
    pub along: EdgeKind,
    /// 从**边的起点**沿此边种类链走到落点；为空则落点就是起点自身。
    #[serde(default)]
    pub from: Vec<String>,
    /// 从**边的终点**沿此边种类链走到落点；为空则落点就是终点自身。
    #[serde(default)]
    pub to: Vec<String>,
    pub confidence: Option<f32>,
}

/// P7 解析漏斗的层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveTier {
    /// L1 字面 FQN，如 `app()->make(StoreOrderServices::class)`。
    Exact = 1,
    /// L2 容器注册表（`provider.php` 中的绑定）。
    Registry = 2,
    /// L3 别名索引（Facade / 事件名 / 获取器）。
    Alias = 3,
    /// L4 约定（命名空间拼接、类名推导表名）。
    Convention = 4,
    /// L5 常量传播。
    ConstProp = 5,
    /// L6 与有限全集求交（schema 的 203 张表）。
    Intersection = 6,
    /// L7 完全未知。
    Unknown = 7,
}

impl ResolveTier {
    /// 该层级对应的基础置信度。
    pub fn base_confidence(self) -> f32 {
        match self {
            Self::Exact => 1.0,
            Self::Registry => 0.95,
            Self::Alias => 0.85,
            Self::Convention => 0.8,
            Self::ConstProp => 0.6,
            Self::Intersection => 0.7,
            Self::Unknown => 0.3,
        }
    }
}

/// 一次动态解析的产物。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub tier: ResolveTier,
    /// 候选节点；空表示未解析。
    pub candidates: Vec<super::ids::NodeId>,
    pub confidence: f32,
    pub evidence: String,
}

impl Resolution {
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            tier: ResolveTier::Unknown,
            candidates: Vec::new(),
            confidence: ResolveTier::Unknown.base_confidence(),
            evidence: reason.into(),
        }
    }
    pub fn resolved(tier: ResolveTier, candidate: super::ids::NodeId, evidence: impl Into<String>) -> Self {
        Self { tier, candidates: vec![candidate], confidence: tier.base_confidence(), evidence: evidence.into() }
    }
}

/// 调用点上下文（供选择器匹配使用）。
#[derive(Debug, Clone)]
pub struct CallContext {
    pub owner_fqn: String,
    pub owner_node: Option<super::ids::NodeId>,
    pub callee_text: String,
    pub receiver: Option<String>,
    pub method: Option<String>,
    pub args: Vec<super::syntax::FactValue>,
    pub span: Span,
    pub sub_project: Option<super::ids::SubProjectId>,
    pub file_path: String,
}

/// P7 动态解析声明。
///
/// 让「哪些调用需要动态解析」这件事也由 FKB 决定，而不是写死在内核里。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolverSpec {
    pub id: String,
    /// 匹配模式，如 `app()->make|app|make`。
    #[serde(default)]
    pub call: Option<String>,
    pub strategy: ResolveStrategy,
    /// 起始解析层级（容器默认 Regist发ry）。
    #[serde(default)]
    pub from_tier: Option<ResolveTier>,
}

/// 具体解析策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveStrategy {
    /// `app()->make(X)` / `app('x')`：L1 字面 → L2 注册表 → L4 约定 → L6 求交。
    Container,
    /// `event('x')`：查 L3 别名索引。
    Event,
    /// `Event::listen('x', Listener::class)` / `Event::subscribe(Listener::class)`：
    /// arg0 解析为事件节点（L3 别名），再把 `HandledBy` 边从事件节点指向 arg1 监听器类。
    EventListen,
    /// `think\facade\Cache::get()`：查 L3 FacadeMap。
    Facade,
    /// `$order->status_text`：复合键 accessor 别名。
    Accessor,
    /// `Route::post('p','Login/appleLogin')`：handler 模式解析。
    Handler,
    /// `$services->appAuth()`：按变量类型解析实例方法调用。
    ///
    /// 类型来源：方法参数类型提示（ThinkPHP 控制器 DI 约定）与构造器属性注入
    /// （`__construct(T $x){ $this->p = $x; }`），由 P2 记录、本策略消费。
    VariableType,
}
