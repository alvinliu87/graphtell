# 支持矩阵（SUPPORTED）

GraphTell 当前已落地的语言 / 框架 / 语义特征，以及**已知的诚实边界**。
完整的架构与流水线说明见 [`README.md`](./README.md)，FKB 编写方法见 [`docs/fkb-authoring.md`](./docs/fkb-authoring.md)。

---

## 1. 语言与框架

| 语言 | 框架 / 形态 | 语义提取 | 合规规则 |
| --- | --- | --- | --- |
| **PHP** | ThinkPHP 6 / CRMEB / Laravel / Uni-app 后端契约 / **Symfony（PHP 8 属性路由，见 §3.3）** | 完整（表 / **ORM 关联** / **表外键** / 路由 / 配置 / 国际化 / 缓存 / 事件 / 队列 / 验签 …，见 §3.2） | 完整（含 N+1、验签、循环内外部调用、多写无事务等） |
| **Java** | Spring Boot（Spring Cache / ApplicationEvent / Spring AMQP / Spring Kafka / Spring Scheduling / JPA / MyBatis-Plus） | 完整（见 §2） | 复用 `rules/global/`（按图拓扑判定的规则）；框架专属规则（`orphan-event` / `orphan-queue` 等）尚未为 Java 写 |
| **JavaScript / TypeScript** | Uni-app 前端（事件总线 / 本地存储 / 页面 / Store） | 完整 | `rules/js/`（前端事件总线死代码） |
| **JavaScript / TypeScript** | **Node 后端**：NestJS（装饰器路由 / 控制器）/ Express（成员式路由）/ **Koa** / **Fastify** | 路由契约（NestJS 另含 `HandledBy` 连到方法节点，见 §3.1） | 复用 `rules/global/`（按图拓扑）；框架专属规则尚未为 Node 写 |
| **Python** | FastAPI / Flask / Celery / SQLAlchemy / **Django（ORM：表 / 列 / 关系 / 表外键，见 §3.4）** | 路由 / 依赖注入 / 配置 / 缓存 / 表映射 / 任务队列 / 定时 / Django 模型表（见 §3） | 复用 `rules/global/`（按图拓扑）；框架专属规则尚未为 Python 写 |

> 新语言 = 实现 `gt_domain::port::LanguageParser` 把语法树翻译成 `SyntaxFacts`（参考 `gt-adapter-parser/src/java`），在 `DefaultParserRegistry` 注册即可，内核零改动。

---

## 2. Java（Spring Boot）语义特征

所有语义节点都来自 `fkb/java/spring-boot.yaml`（声明式 FKB，**没改内核**）。
注解在内核里被建模成调用点（`callee` = 注解名），方法调用实参按位置捕获字面量。

| 语义 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **Cache** | `@Cacheable` / `@CachePut` / `@CacheEvict` | `Cache` | `ReadsCache` / `WritesCache` | 缓存名取自注解实参 `arg0` |
| **Event** | `@EventListener(handler)` / `publisher.publishEvent(new X())` | `Event` | `ListensTo` / `Emits` | **类型级归并**：同一事件类型的发布方与订阅方归并到同一节点（见 §5） |
| **Queue** | `@RabbitListener(queues="q")` / `rabbitTemplate.convertAndSend("q", …)` | `Queue` | `ListensTo` / `PublishesTo` | 消费端来自注解，生产端来自方法调用实参 |
| **Topic** | `@KafkaListener(topics="t")` / `kafkaTemplate.send("t", …)` | `Topic` | `ListensTo` / `PublishesTo` | 生产端按 receiver 变量名约定匹配（见 §5） |
| **Schedule** | `@Scheduled` | `Schedule` | `Triggers` | 调度器触发该方法（出边） |
| **HttpContract** | `@RequestMapping` / `@GetMapping` / `@PostMapping` … | `HttpContract` | `HandledBy` | 契约 `METHOD /path` 归一，可与前端 `uni.request` 契约桥接 |
| **Table** | `@TableName("x")` / `@Table(name="x")` / MyBatis `mapper/*.xml` | `Table` | `MapsTo` / `ReadsDb` / `WritesDb` | MyBatis-Plus 与 JPA 表名；XML 语句按读写落边 |
| **ConfigKey** | `@Value("${app.name}")` | `ConfigKey` | `ReadsConfig` | 去 `${` `}` 得到配置键 |

端到端自检见 `crates/gt-pipeline/tests/java_spring_features.rs`（合成样本，无需外部工程）。

---

## 3. Python（FastAPI / Flask / Celery / SQLAlchemy）语义特征

Python 是**第三个落地的语言**：只实现了一个 `LanguageParser` 并注册进来，语义全部由
`fkb/python/*.yaml` 声明，**内核零改动**。关键建模是**装饰器 ≡ 调用点**（与 Java 注解同机制），
所以 `@app.get("/x")` 能被 `kind: call` 选择器直接命中 —— 这也是 Flask 这类「同语言的第 N 个
框架」几乎零成本的原因（只多一份 YAML）。

| 语义 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **HttpContract**（FastAPI / Flask 2.x） | `@app.get/post/put/delete/patch("/x")` | `HttpContract` | `HandledBy` | 方法由装饰器名推导 |
| **HttpContract**（Flask） | `@app.route("/x")` / `@bp.route` | `HttpContract` | `HandledBy` | 写了 `methods=["POST"]` 时解码首项；未声明时按 Flask 默认语义落 `GET` |
| **依赖注入** | `def h(db=Depends(get_db))` | 不造节点（两端都是已有函数节点） | `DependsOn` | 依赖写在**形参默认值**里；`DependsOn` 由本 FKB 用 `bridge_edge_kinds` 声明 |
| **ConfigKey** | `os.environ.get("K")` / `os.getenv("K")` | `ConfigKey` | `ReadsConfig` | |
| **Cache** | `cache/redis::{get,set}` | `Cache` | `ReadsCache` / `WritesCache` | |
| **Table**（SQLAlchemy） | 类属性 `__tablename__ = "users"` | `Table` | `MapsTo` | 走 P6 的**图节点选择器** + `HasProperty`（不靠调用点） |
| **Queue**（Celery） | `@celery_app.task` / `@shared_task` 与 `.delay()` / `.apply_async()` | `Queue` | `ListensTo` / `PublishesTo` | 生产端与消费端按任务名**归并到同一节点** |
| **Schedule**（Celery beat） | `add_periodic_task(30.0, …)` | `Schedule` | `Triggers` | |

解析器侧同时补齐了几项**通用**能力（不专为 Python）：关键字实参按名可取、列表 / 元组字面量
以下标为键可取、形参默认值里的调用会被记成调用点、类体内字面量赋值登记为 `Property`。

端到端自检（均为合成样本，无需外部工程）：
`crates/gt-pipeline/tests/python_fastapi_features.rs`、
`crates/gt-pipeline/tests/python_flask_features.rs`。

### 3.4 Django（Python）语义特征

Django 是 Python 生态体量最大的框架，本文件补上它的 **ORM 建模**（表 / 列 / 关系 / 表级外键），
与 Node（TypeORM）、PHP（Laravel / ThinkPHP）**同一套边**（`References` / `ForeignKey` / `HasColumn` /
`MapsTo`），`ForeignKey` 直接复用通用的 `Project` 动作。几乎只写 YAML —— 仅 Python 解析器加了一处
中性的「类体内 `name = Call(...)` 字段声明」捕获（与 JS 字段装饰器、Java 字段注解同机制）。

| 语义 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **Table** | `class Post(models.Model)` | `Table` | `MapsTo` | 表名取类短名（`snake_plural` + `short_name` + `singularize`：`Post` → `post`）；Django 真实表名是 `app_label_modelname`，这里取不到 app_label，先用类短名复数兜底 |
| **列** | `name = models.CharField(max_length=200)` | `Column` | `HasColumn`（模型类 → 列） | 身份取 `模型类.字段`（`app.models.Post.title`），模型类已 `MapsTo` → `表`，故「表 → 列」是 表 ←MapsTo— 模型 —HasColumn→ 列 两跳；只收列字段类型，`ForeignKey` / `OneToOneField` / `ManyToManyField` 算关系 |
| **模型关联** | `author = models.ForeignKey(User, on_delete=...)` | —（两端都是已有模型类节点） | `References`（声明方 → 目标模型） | 目标取首个位置实参的类名（解析器按 import 或模块前缀解析成 FQN，存入 `entity`），只建外键持有方 |
| **表外键** | 由上面的 `References` 投影而来 | —（两端都是已有表节点） | `ForeignKey`（表 → 表） | P6 用 `Project`：遍历每条 `References`，两端各沿 `MapsTo` 走一跳 |
| **HTTP 路由** | `path("users/", user_list)`（`urls.py`） | `HttpContract` | `HandledBy`（契约 → 视图） | 见下方「路由」说明 |

**路由（`path()` / `re_path()` / `url()`）**：Django 把 URL 与视图写在 `urls.py`、且**不编码 HTTP method**
（一个视图处理所有方法），故契约方法用通配 `ANY`（`is_wildcard_http_method` 认定 `ANY` 匹配任意前端调用
方法），既避免把 Django 端点误标成具体动词，也避免读写启发式对「未知方法 → 读」的误判。路径取 route 字面量
（规整成前导 `/`）；视图（`arg1`）由解析器解析成全 FQN 存入 `entity`，再经 `HandledBy` 连到视图节点
（函数不在短名索引里，必须给全 FQN 才能 `find_by_name` 命中 —— 与 FastAPI 用 `owner_class` 全 FQN 命中同一机制）。

**前置（解析器侧，语言中性的扩展）**：`gt-adapter-parser/src/python/mod.rs`
1. 把类体内的 `name = SomeCallable(...)` 翻成调用点，owner 精确到「类.字段」（`owner_class` = 模型类、
   `owner_member` = 字段名），故 FKB 取 `owner_class.owner_member` 即列身份；关系字段首个位置实参
   （类名标识符）解析成 FQN 存入 `entity`，供 `References` 直接连到目标模型类。该扩展**不认识 Django**，
   只是「类体内裸标识符 = 调用」这一中性语法形态，SQLAlchemy 之外的其它「字段式 ORM」也能直接受益。
2. `path()` / `re_path()` / `url()` 调用的**视图实参**解析成全 FQN 存入 `entity`（同上理由：函数节点
   不在短名索引，必须全 FQN 才能连上）。
3. **修复相对导入**：`from .views import user_list` 此前解析成错误的 `views.user_list`，现按当前模块父包
   还原成 `myapp.views.user_list`（前导点层数对齐父包层级）；绝对导入不受影响。

**诚实边界**：
- **Django 惯用法是 `from django.db import models` 后写 `models.CharField`**：若项目改成
  `from django.db.models import CharField` 直接写 `CharField(...)`，当前 callee 匹配不到
  （裸字段名太通用，不愿放宽以免误伤普通变量）。
- **字符串引用的关系不解析**：`tags = models.ManyToManyField("Tag")` 的实参是字符串、
  且 `Tag` 未必在同一编译单元，解析器不解析字符串里的类名，按 `require_class` 跳过、不建悬空边
  （与 PHP 的 `morphTo()` 同处理）。要支持字符串外键需额外解析。
- **未读 `class Meta: db_table`**：显式指定的真实表名当前未取，表名用类短名复数兜底；
  若项目大量用 `db_table`，需补一条读 `Meta` 属性的能力。
- **路由视图解析的三种写法都已支持**：① 裸名 `from .views import user_list` 后 `path("x/", user_list)`；
  ② 模块属性 `from . import views` 后 `path("x/", views.user_list)`（`resolve_symbol` 现递归还原
  模块前缀 → `myapp.views.user_list`）；③ CBV `path("x/", ArticleListView.as_view())`（只认 `as_view()`
  这一 Django 惯用法，取被调对象类名）。三种都能经 `HandledBy` 连到视图节点。
- **`path("api/", include("api.urls"))` 这类「前缀 + include」**：会被当成一条端点契约（无 handler 边），
  属于已知噪声（include 挂载的子路由真正端点在其子 `urls.py` 里）。

端到端自检（合成工程，无需外部样本）：`crates/gt-pipeline/tests/django_features.rs`（含表 / 列 / 关系 /
外键 / 路由四条）。

---

## 3.1 Node 后端（NestJS / Express）语义特征

Node 后端**不需要新解析器**：`JsFrontendParser` 早已把 TS 的**类 / 方法**装饰器建模成调用点
（`crates/gt-adapter-parser/src/js/mod.rs` 的 `nestjs_decorators_become_call_sites` 单测即证明，
`callee` 带 `@` 前缀以区别于普通的 `obj.get()` 成员调用），与 FastAPI 的 `@app.get` 同机制。
Express 的 `app.get('/x')` 则是普通成员调用（receiver `app`、method `get`），形态同 FastAPI 的动词快捷方式。
所以路由 / 依赖注入 / 实体映射三类语义都只靠 `fkb/js/*.yaml` 声明。

**表级外键**另需一处**通用**能力：新增 `Project` 动作 —— 把一类边投影到另一层
（遍历匹配节点的每条 `along` 出边，两端各沿自己的边种类链走到落点，在两落点间建边）。
「类 → 它映射的表」本质是**沿 `MapsTo` 走一跳**，而 `ValueSource` 只认名字、取不到"边的那一头"；
且一个实体可能有多个 `@ManyToOne`，必须一对多地投影（`Link` 两端各只能取一个名字，会静默丢边）。
走哪些边仍由 FKB 声明，内核依旧不认识 TypeORM。

**字段级语义**（`@Column`）另需两处**通用**能力（不为 Node 专属，其它语言同享）：
① 解析器把**字段装饰器**挂到字段 FQN（`Class.field`）—— 此前只挂类 / 方法装饰器，
`@Column` 会被整条丢掉；② P2 在调用点 `owner_fqn` 精确查不到节点时**退回所属类**
（此前整份退到文件节点，导致 `@Column` 的 `HasCallSite` / 语义边从 `File` 发出）。

| 框架 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **NestJS** 路由 | `@Get('user')` / `@Post('users')` … | `HttpContract` | `HandledBy`（由契约指出，连到 `Class.method` 节点） | 方法名取装饰器名（去掉 `@`）；路径取实参 |
| **NestJS** 控制器 | `@Controller()` | `Controller` | — | 纯结构展示节点 |
| **NestJS** 依赖注入 | `constructor(private svc: UserService)` | —（provider 即类节点） | `DependsOn`（`UserController --DependsOn--> UserService`） | 构造器**带访问修饰符**的形参类型 = provider；解析器记成 `@Inject` 调用点，FKB 建边 |
| **TypeORM** 实体 | `@Entity('user')` | `Table` | `MapsTo`（实体类 → 表） | 表名取实参；无实参的 `@Entity()` / 对象式 `@Entity({name})` 跳过 |
| **TypeORM** 列 | `@Column()` / `@PrimaryGeneratedColumn()` / `@CreateDateColumn()` … | `Column` | `HasColumn`（实体类 → 列） | 列身份取 `实体类.字段`（`UserEntity.username`），故同名列（`id`）在不同实体各算各的；`Column` / `HasColumn` 由本 FKB 用 `semantic_kinds` / `semantic_edge_kinds` 声明 |
| **TypeORM** 关联 | `@ManyToOne` / `@ManyToMany` / `@OneToOne` | —（两端都是已有实体类节点） | `References`（引用方 → 被引用实体） | 目标实体取**字段类型注解**（箭头函数实参 `type => X` 取不到字面量）；只建**外键持有方**，`@OneToMany` 是反向声明、不重复建边 |
| **TypeORM** 表外键 | 由上面的 `References` 投影而来 | —（两端都是已有表节点） | `ForeignKey`（表 → 表） | P6 用 `Project` 动作：遍历每条 `References`，两端各沿 `MapsTo` 走一跳落到表。**一对多**，一个实体有几条 `@ManyToOne` 就产出几条 |
| **Express** 路由 | `app.get('/login')` / `router.post(...)` | `HttpContract` | — | 方法取成员名（get/post…）；路径取 arg0 |
| **Koa** 路由 | `router.get('/users')` / `admin.post(...)` | `HttpContract` | — | 同上；接收者只认 Router 实例（**不含 `app`** —— Koa 的 app 只用来 `use()`）。`router.all(...)` 归一化成 `ANY` |
| **Fastify** 路由 | `fastify.get('/users')` / `server.put(...)` | `HttpContract` | — | 同上；接收者 `fastify` / `app` / `server`。`fastify.route({method,url})` 的 schema 写法暂不支持 |

Koa / Fastify **零解析器改动**（与 Express 同一形态：`receiver.method(path, handler)` 就是普通
成员调用），只各加一份 FKB。三者接收者不同，故分开写：Express 是 `app`/`router`/`api`，
Koa 是 `router`（不含 `app`），Fastify 是 `fastify`/`app`/`server`。
端到端自检：`crates/gt-pipeline/tests/node_koa_fastify_features.rs`。

> 两类路由的 **HandledBy 方向**不同，容易踩坑：NestJS 用 `link.direction: to_target`
> （边由 `HttpContract` **指出**到方法节点），所以查的是「契约的**出边**」，而非「方法的出边」。

**验证样本**：除合成自检外，另下载了真实开源工程做端到端验证（git 忽略、不入库）：
`lujakob/nestjs-realworld-example-app`（NestJS + TypeORM）、`sahat/hackathon-starter`（Express）。
`crates/gt-pipeline/tests/node_real_samples.rs` 在样本存在时跑真实建图断言、缺失则跳过。

**诚实边界**：
- **NestJS 控制器前缀不向前缀拼接**：`@Controller('user')` 的前缀未与 `@Get('user')` 的方法路径合并，
  仅取方法装饰器的实参路径（`n1-query-in-loop` 这类跨装饰器合并需要额外的 FKB 能力）。
- **Express 不连 `HandledBy`**：Express 的路由与处理器**解耦**（`handler` 在另一文件），模块级路由的
  owner 是文件路径、没有对应节点；为避免悬空边，Express 仅落成契约节点。
- **依赖注入只认"构造器带修饰符的形参"**：`constructor(private svc: UserService)` 算，
  `constructor(plain: Foo)` 不算；provider 类型解析不到对应类节点（泛型 / 接口 / 未导入）时静默跳过。
- **TypeORM 只认字符串实参的 `@Entity('user')`**：对象式 `@Entity({ name: 'user' })` 与无参 `@Entity()`
  暂不提取表名（取不到字面量就跳过，宁可缺不可猜）—— 这类实体的 `Column` 仍会挂在实体类上，但**没有**对应的 `Table`
  （真实样本里 `Comment` 即此形态：2 个列节点、无表节点）。
- **列只收"列"装饰器**：`@Column` / `@PrimaryGeneratedColumn` / `@CreateDateColumn` / `@UpdateDateColumn` /
  `@DeleteDateColumn` 算列；关系装饰器（`@ManyToOne` …）不是列，走 `References` 边单独建模。
  列名取字段名（TypeORM 默认），`@Column({ name: 'x' })` 的显式重命名尚未提取。
- **关联只建外键持有方**：`@ManyToOne` / `@ManyToMany` / `@OneToOne` 建 `References`；
  `@OneToMany` 是反向声明，与配对的那条 `@ManyToOne` 描述同一关系，再建就是冗余反向边，故跳过 ——
  **单向 `@OneToMany`（没有配对的 ManyToOne）因此不建边**。`@JoinColumn` / `@JoinTable` 只标 JOIN 形态，不单独建边。
- **表外键只覆盖"有表"的实体**：外键由 `References` 投影而来，两端都要能沿 `MapsTo` 走到 `Table`。
  无参 `@Entity()` 的实体没有表节点，它的外键不入图（真实样本里 `Comment → article` 正因此缺失；
  `article → user` 与 `user → article` 正常产出）。
- **路由 receiver 靠变量名约定收窄**（同 Python）：`app` / `router` / `api` 等；裸 `get/post` 太通用。

---

## 3.2 PHP（ThinkPHP 6 / Laravel）ORM 语义特征

PHP 侧的**表映射**早已有之（比 Node 侧更成熟：还有 `db_verbs` 支撑 `ReadsDb` / `WritesDb` 读写分类）。
本轮补上的是**模型关联**与**表级外键** —— 与 Node 侧同一套边（`References` / `ForeignKey`），
`ForeignKey` 直接复用 §3.1 引入的 `Project` 动作。

| 语义 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **Table**（既有） | `Db::name('x')`（TP6）/ `DB::table('x')`（Laravel）/ 模型约定 `extends Model` | `Table` | `MapsTo`（+ P7 的 `ReadsDb` / `WritesDb`） | 每个框架各写一份（表名来源不同） |
| **模型关联** | `$this->hasMany(Post::class)` / `belongsTo` / `belongsToMany` / `morphXxx` … | —（两端都是已有模型类节点） | `References`（声明方 → 目标模型） | 目标取 arg0 的类常量；**两侧都建**，见下 |
| **表外键** | 由上面的 `References` 投影而来 | —（两端都是已有表节点） | `ForeignKey`（表 → 表） | P6 用 `Project`：遍历每条 `References`，两端各沿 `MapsTo` 走一跳 |
| **列** | ① SQL 安装脚本的 `CREATE TABLE`；② Laravel migration `database/migrations/*.php` | `Column` | `HasColumn`（表 → 列） | 两个装载器都写权威 `schema` 符号表（**并集**，装载顺序无关），内核在 P6 末尾沉淀成图节点 |

**为什么这里两侧都建、而 Node 侧只建持有方**：TypeORM 的 `@ManyToOne` / `@OneToMany` 通常**成对**写，
只建持有方即可避免反向冗余；Laravel / ThinkPHP **常只声明一侧**（最常见就是 `User hasMany Post`，
不写反向 `belongsTo`），只建一侧会大面积漏边 —— 宁可冗余也不漏。

规则按 `db_verbs` 的先例**各框架各写一份**（`fkb/php/laravel.yaml` 与 `fkb/php/thinkphp6.yaml`），
不抽到 `php-common.yaml` —— 关系方法属于 **ORM 强假设**，而 `php-common` 声明了
`apply_without_detection: true`、**对所有 PHP 工程无条件生效**。

### 列从哪来：沉淀权威 schema，而不是啃 migration

PHP ORM 的模型**通常不声明字段**（字段在 migration / 表结构里），没有 TypeORM `@Column` 那样的
字段级声明源。更关键的是：**Laravel migration 的 `$table->string('email')` 写在闭包里**，
调用点的 owner 是闭包而非模型类，纯 FKB 拿不到它属于哪张表 —— 这条路走不通。

故改为**沉淀权威 schema**：`schema` 符号表（P3 从 SQL 安装脚本解析 `CREATE TABLE`、并从源码
表名调用点补齐）里的列，在 P6 末尾由内核物化成 `Table --HasColumn--> Column`。
这是**语言 / 框架无关**的能力 —— 只认「`Table` 节点 + schema 里有它的列」，
任何有 SQL 安装脚本的工程都自动获得字段级图节点。列身份带表名作用域（`user.email`），
故同名列（`id` / `created`）在不同表各算各的。

**Laravel 的列来自 migration 装载器**（`php_migration_schema`，扫描 `database/migrations/*.php`
的 `Schema::create` / `Schema::table`）。两个装载器写同一张 `schema` 表，内部做**并集** ——
否则后跑的会整份覆盖先跑的（装载顺序不保证）。

解析只认**列声明方法白名单**（`$table->string('email')`），不能"取第一个字符串实参"：
`->comment('说明')` / `->after('col')` / `->dropColumn('x')` 这类修饰符与非列声明也带字符串实参，
会被误当列名。无实参的声明按 Laravel 约定补列名：`id()` → `id`、
`timestamps()` → `created_at` / `updated_at`、`softDeletes()` → `deleted_at`。

### 列**默认不进折叠视图**（刻意）

列是「表数 × 列数」的量级（几十张表 × 十几列 = 几百个节点）。若把 `Column` 算作语义节点，
折叠视图会被撑爆，还会吃掉可达语义节点统计的 `MAX_NODES = 400` 预算，把真正的表 / 契约挤掉。
故：

* `Column` **不在** `NodeKind::SYNTHESIZED` 里 —— 折叠视图（`is_semantic()` 过滤）不画列；
  节点照建、`HasColumn` 边照连，影响面照样能下到字段级。
* **点开表仍能看到列**：`NodeView.columns` 把列作为**节点属性**带出来（裸列名），
  故"展开一张表看看有哪些字段"不受影响 —— 列只是在折叠视图里不占位。
  视图侧两条取列路径：PHP 是 `Table --HasColumn--> Column`；TypeORM 是
  `Table <--MapsTo-- 实体类 --HasColumn--> Column`（`@Column` 挂在实体类上，要绕一跳）。
* `HasColumn` 归入**桥边**而非语义边：因此**不计入**「语义入边 / 出边 N」
  （它是组成关系，不是资源依赖；算语义边会让一张 15 列的表出边数直接 +15、口径失真），
  但仍留在 `is_chain_edge` 里**可遍历**。
* `ForeignKey`（表 → 表）两端都是语义节点、是真正的资源依赖 → 语义边，会计入 fan、会画。

一个必须知道的机制细节：**FKB 的 `semantic_kinds` / `semantic_edge_kinds` 是全局注册的**
（`register_semantic_kinds` 在 `load_dir` 里对所有 FKB 无条件登记，语言过滤只发生在选规则时）。
所以想"只让某个框架的列可见"做不到 —— 任何一份 FKB 声明 `Column` 为语义，
会让**所有工程**（含 PHP）的列都变语义。要按语言区分必须先改装载逻辑按语言登记。

**诚实边界**：
- **migration 只认 Laravel 的 `database/migrations/*.php`**：ThinkPHP 的列来源仍是 SQL 安装脚本
  （它没有等价的 migration 目录约定）。`$table->xxx()` 之外的写法（如 `$table->morphs('taggable')`
  产生的 `taggable_id` / `taggable_type`）按白名单补充，未覆盖的方言会被跳过（宁可缺不可猜）。
- **表名单复数**：表节点名经 `singularize`（`user`），而 DDL / migration 常写复数（`users`），
  沉淀时会再试一次复数形式兜底；`ColumnsMatch` 谓词**没有**这层兜底，故 PII 打标在复数表名上仍可能漏。
- **`morphTo()` 不建边**：它没有类常量实参（多态目标运行时才定），取不到目标就跳过，不建悬空边。

端到端自检（合成工程，无需外部样本）：`crates/gt-pipeline/tests/php_orm_features.rs`。

### 3.3 Symfony（PHP）路由语义特征

Symfony 是 PHP 生态体量最大的框架之一，本轮补上它的 **PHP 8 属性路由**（`#[Route]` /
`#[Get]` / `#[Post]` …）→ `HttpContract` + `HandledBy`，与 Spring（注解）、
FastAPI（装饰器）、Django（`path()`）**同一套语义**。

解析器侧（`gt-adapter-parser/src/php/mod.rs` 的 `collect_method`）把每个路由属性
**合成成调用点**（此前 PHP 解析器不认 `attribute_list` 这类 PHP 8 属性节点）：

| 写法 | 契约方法 | 说明 |
| --- | --- | --- |
| `#[Route('/x', methods: ['GET'])]` | `GET` | 读具名实参 `methods` 数组 |
| `#[Route('/x', methods: ['GET','POST'])]` | `GET` + `POST` | 拆成**两条**契约（每个 `(方法, 路径)` 一条） |
| `#[Route('/x')]`（无 methods） | `ANY` | Symfony 不限制方法 → 通配 |
| `#[Get('/x')]` / `#[Post('/x')]` … | `GET` / `POST` | 快捷属性隐含方法 |

合成调用点的 `callee_text` 带 **`attr.` 前缀**（如 `attr.Route`）。这是必须的：FKB 的
`callee` 模式不是正则 —— 单冒号被解释成 `receiver:method`、裸名会**按方法名**匹配，
故 `attr:Route` 匹配不上（receiver=`attr`、method=`Route`），而裸 `Get` 会误命中
`$cache->Get()` 这类调用。`entity` 指向控制器方法自身，FKB 用
`to: { entity: true, resolve: class_const }` 连成 `HandledBy`。

端到端自检（合成工程）：`crates/gt-pipeline/tests/symfony_route_features.rs`。

**诚实边界**：
- **只认方法级属性**：类级 `#[Route]` 前缀（`#[Route('/api')] class X`）当前不处理
  （不做前缀拼接），这类路由不会成契约。
- **只认这 8 个属性名**（`Route` / `Get` / `Post` / `Put` / `Delete` / `Patch` /
  `Options` / `Head`），其余属性（如 `#[ORM\Entity]`、`#[IsGranted]`）忽略。
- **`use ... Route as MyRoute` 别名**不还原，`#[MyRoute('/x')]` 识别不到。
- 路由路径里的占位符保留原文（`/api/users/{id}`），不与该路径的模板变量做别名归并。

---

## 4. 合规检查（rules）

规则按适用环境分目录装载，内核不认识任何具体规则：

| 目录 | 适用 | 内容 |
| --- | --- | --- |
| `rules/global/` | 跨语言通用（只依赖图拓扑） | 契约桥（`http-contract-without-handler` / `frontend-calls-missing-backend` / `backend-endpoint-never-called`）、热点表（`hot-table`）、配置读取热点（`config-read-hotspot`）、死表（`dead-table`）、只写不读 / 只读不写表、高扇出方法（`hotspot-method`） |
| `rules/php/` | 仅 PHP 工程 | 原始 SQL 执行点、PII 表、从未触发的事件 / 队列、循环内逐条读写库（N+1）、循环内外部调用、多写无事务、验签质量 |
| `rules/java/` | 仅 Java 工程 | 循环内逐条读写库（N+1）—— 判据与 PHP 版同构，见 §5 |
| `rules/js/` | 含前端子工程的工程 | 前端事件总线死代码 |

内置约 30 条规则。规则用 `applies_to.languages` / `applies_to.frameworks` 先验声明适用范围；环境不匹配直接跳过（`rules_not_applicable`），判据依赖的事实图里没有则停用（`rules_unavailable`），避免"0 命中"这种比误报更危险的静默失效。详见 README「规则怎么知道该在哪跑」。

---

## 5. 已知边界（诚实声明）

这些是**当前图的真实缺口**，不是 bug，写规则与解读图时都要考虑：

- **Java 的 N+1（循环内逐条读写库）：已做通**（`rules/java/n1-query.yaml`）。补齐了三环：
  P7 的读 / 写分类要求**接收者类型有 `MapsTo` 边**，而 Spring FKB 只给 `@Table` /
  `@TableName` 标注的**实体类**建 `MapsTo → Table`；JPA Repository / MyBatis Mapper 接口
  （`UserRepository extends JpaRepository<User, Long>`）本身没有该边，故
  `userRepository.save()` 落不出 `WritesDb`。三步分别是：

  1. **Java parser 捕获 `extends` 的泛型实参** —— **已完成**。`interface UserRepository
     extends JpaRepository<User, Long>` 现能取出 `User`，记成 `generic.JpaRepository`
     合成调用点（`entity` = 实体名）。顺带修掉一个既有缺陷：接口的 `extends` 在
     tree-sitter-java 里是 **`extends_interfaces` 节点且不带字段名**，`child_by_field_name`
     永远取不到，此前「接口继承接口」整条继承关系是丢失的。
  2. **加规则把 Repository / Mapper → 实体 → 表串起来** —— **已打通**。`spring-boot.yaml`
     里写好 `Link`（DAO → 实体的 `References`）+ `Project`（沿实体 `MapsTo` 投影成
     DAO → 表），`crates/gt-pipeline/tests/java_db_verbs.rs` 验证通过。两个坑：
     * `Action::Link` 是 `ev.string(src)` 后直接 `find_by_name`，**顶层 `resolve` 不参与**，
       必须写在每个 `ValueSource` 上；
     * 泛型实参是**裸名**（实体与 DAO 同包、没有 import），而短名索引只收 import，
       故 parser 必须按 DAO 所在包把它补成 FQN，否则 `find_by_name` 落空、不产边。

  3. **字段声明类型按同包补成 FQN** —— 也是关键一环。`private UserRepository repo;`
     在源码里是**短名**，而 `mapped_tables` 用 FQN 查 `MapsTo`，短名查不到 ⇒
     `repo.save()` 仍落不出动作边。parser 现按所在类的包补齐（`com.demo.UserRepository`）。

  端到端自检（合成工程）：`crates/gt-pipeline/tests/java_db_verbs.rs`，覆盖
  「实体 → 表」「DAO → 表（MapsTo）」「`findById` → ReadsDb / `save` → WritesDb」。

  **诚实边界**：
  * **只认同包的裸类型名**：字段类型若来自**跨包 import**（`import com.other.User;`），
    当前按本包补齐会得到错误的 FQN，故这类调用落不出动作边（宁可缺不可猜）。
  * **JPA 派生查询不在动词表**：`findByEmail` / `countByStatus` 这类名字不固定，
    无法枚举，不会落 `ReadsDb`。
  * **DAO 泛型实参只取第一个**：`JpaRepository<User, Long>` 取 `User`（实体），符合惯例。

     注：不能用 `Synthesize` 绕过 —— `Synthesize` 是「造节点再连边」，而这里需要连的
     是**已存在的** Table 节点；从实体名 `User` 推不出表名 `eb_user`（`@TableName`
     给的值），另造一个会重复建表。MyBatis **XML** mapper 那条路（`mybatis::select`
     等伪调用点）已经能直出 `ReadsDb` / `WritesDb`，不受此限制。
- **Kafka 生产端靠 receiver 变量名约定匹配**：`kafkaTemplate` / `kafkaProducer` / `producer` 三个常见字段名 + `KafkaTemplate.sendDefault`（专属方法名兜底）。项目若用别的字段名（如 `kt`），需把变量名加进 FKB 的 `callee` 备选。裸方法名 `send` 不能直用（会和 `sendError` / `email.send` 等误伤）。
- **事件类型级归并的兜底**：`publishEvent(var)`（实参是变量而非 `new X()`），或解析不出 handler 形参类型时，身份退回 `owner_member`（收发方法名）——此时不归并，但 `ListensTo` / `Emits` 边不丢。`entity` 取的是裸类型名（泛型已剥离）。
- **消息生产端 destination 依赖「方法调用实参字面量」的位置级捕获**：`convertAndSend` 同样来自 `RedisTemplate` / `JmsTemplate`，统一落 `Queue`；若需区分可按 receiver 伞名细化。
- **语法层无类型**：`rabbitTemplate` / `kafkaTemplate` 是变量名，不能还原成类，所以 producer 匹配用变量名约定而非类型（要更稳需符号表）。
- **Python 的路由 / 缓存靠 receiver 变量名约定收窄**：`app` / `router` / `api_router` / `cache` / `redis` …。Python 的 `get` / `set` 太通用，裸方法名会把 `requests.get("/api")` 之类误伤成路由。项目若用别的变量名（如 `v1_router`），需把它加进 FKB 的 `callee` 备选（与 Java 侧 `kafkaTemplate::send` 同一类取舍）。
- **Flask 的 `methods=` 只在 arg1 解码**：写在更靠后的位置会退回 `GET`；多方法路由（`["GET","POST"]`）只取第一个。
- **Celery 任务归并用 `short_name` 归一化**：不同模块的**同名任务会合并成一个节点**（归并优先于区分）。
- **Celery beat 的 `Triggers` 指向注册点函数**，而非被触发的任务 —— 后者在实参里且非字面量，当前无法作为链接目标（宁可不连，不编造边）。
- **`Depends` 的嵌套依赖不展开**：`get_db` 自己再 `Depends(...)` 不再递归。
- **无解析器的语言会告警，但图仍然为空**：Go / Rust / Kotlin / C# / Ruby 会被识别成子工程、文件也扫得进来，只是没有解析器 —— P2 产出 `NoParserForLanguage` 诊断并跳过。**它消除了静默失败，但没有消除能力缺口**：真正支持仍需各自补一个 `LanguageParser`。
- **不做向量、不调 LLM**：纯中文且不含标识符的召回做不到（见 README「代码召回」）。

---

## 6. 如何扩展 / 验证

- 加框架 = 在 `fkb/` 加一份 YAML（detectors / root_rules / loaders / rules / resolvers），跑 `graphtell validate` 校验。
- 加语言 = 实现 `LanguageParser`（范本：`gt-adapter-parser/src/java` 为"第二语言"、`src/python` 为"第三语言"；后者额外示范了装饰器建模、模块级函数的 `owner_class` 回填等动态语言问题）。
- 加语言时若**暂时不写解析器**，请知悉：该文件仍会被扫描、子工程仍会被识别，P2 会报 `NoParserForLanguage`。不要把 `MARKERS` 里的标记当作"已支持"。
- 加合规规则 = 在 `rules/<env>/` 加 YAML，先在样本库（5 个 ThinkPHP + 3 个 Spring Boot 已建图工程）量一遍命中数：既不能是 0（静默失效），也不能刷屏（噪声），再决定是否发货。
- 改了 FKB 不触发重新建图；改了解析器 / 引擎才需要重建图。
