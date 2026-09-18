# GraphTell

把任意代码库**图化**的分析平台：以 `tree-sitter` 解析出语法级节点，再按**框架知识库（FKB）**合成语义节点，最终得到一张可以查询、可以标注、可以做影响面与死代码分析的图。

- 后端：Rust（**六边形架构** + SOLID），SQLite 持久化
- 前端：React + TypeScript + Ant Design（**Feature Sliced Design**）
- 桌面常驻：Tauri（后端在**进程内**启动 HTTP 服务，桌面端与 Web 端共用同一套 `/api` 契约）
- 目标：用 tree-sitter 兼容所有主流技术栈 —— 当前已落地 **PHP**（ThinkPHP 6 / CRMEB / Uni-app 前端契约）

---

## 快速开始

```bash
# 1) 构建后端
cargo build

# 2) 创建工程并自动建图（P0 → P7）
./target/debug/graphtell create --name CRMEB --path /path/to/CRMEB-master

# 3) 启动 HTTP 服务（Web / Tauri 前端共用）
./target/debug/graphtell serve --port 5177

# 4) 前端
cd ui && npm install && npm run dev      # http://localhost:5173

# 5) 桌面端（需要系统 WebView：Windows WebView2 / macOS WKWebView / Linux webkit2gtk）
cd ui && npm run tauri dev
```

其它命令：

```bash
./target/debug/graphtell list
./target/debug/graphtell run    --project 1
./target/debug/graphtell stats  --project 1
./target/debug/graphtell delete --project 1
```

---

## 架构

### 后端：六边形架构（端口与适配器）

```
                ┌─────────────────────────────────────────────┐
   inbound      │  gt-adapter-http (axum)   src-tauri (Tauri) │
   adapters     └───────────────┬─────────────────────────────┘
                                │ 用例调用
                ┌───────────────▼─────────────────────────────┐
   application  │  gt-application：ProjectService /            │
                │  PipelineService / GraphQueryService         │
                └───────────────┬─────────────────────────────┘
                                │ 领域服务
                ┌───────────────▼─────────────────────────────┐
   pipeline     │  gt-pipeline：P0 Ingest → P2 CfAst → P3      │
                │  Prepare → P4 AnnotatePre → P5 Synthesize    │
                │  → P6 AnnotatePost → P7 Resolve              │
                └───────────────┬─────────────────────────────┘
                                │ 端口（trait）
                ┌───────────────▼─────────────────────────────┐
   domain       │  gt-domain：实体 / 值对象 / 端口 trait        │
                └─────────────────────────────────────────────┘

   outbound     gt-adapter-parser(tree-sitter)  gt-adapter-fkb(YAML)
   adapters     gt-adapter-sqlite(SQLite)       gt-adapter-fs(扫描)
```

**依赖方向永远指向内核**：`gt-domain` 不依赖任何具体技术；所有 IO 都通过 `port` 中定义的 trait 反向注入（`Persistence` / `FileSystem` / `FileScanner` / `ParserRegistry` / `KnowledgeProvider` / `PipelineObserver` / `Clock`）。

SOLID 落点：

| 原则 | 体现 |
| --- | --- |
| S 单一职责 | 每个阶段一个模块；每个端口只暴露一种能力（`ProjectReader` / `GraphSink` / `DiagnosticSink` …） |
| O 开闭 | `NodeKind` / `EdgeKind` / `Phase` 是**开放字符串** + 常量速记；新语言、新框架、新节点种类都不需要改内核 |
| L 里氏 | 任何 `Persistence` 实现（SQLite / 内存）可互换 |
| I 接口隔离 | 持久化被拆成 6 个细粒度 trait，再用 blanket impl 组合成 `Persistence` |
| D 依赖倒置 | 内核定义端口，适配器实现；`gt-app` 是唯一知道全部具体技术的组装根 |

### 前端：Feature Sliced Design

```
ui/src
├── app/        应用入口：主题、路由、全局样式
├── pages/      ProjectsPage / GraphPage / ExplorerPage / DiagnosticsPage
├── widgets/    AppShell / ProjectTable / PipelineProgress / GraphCanvas
├── features/   create-project / delete-project / run-pipeline / node-detail
├── entities/   project / graph / pipeline（模型 + api + hooks）
└── shared/     api(HTTP) / lib(format, useAsync) / ui(PageHeader, StatCard)
```

依赖只能**从上往下**：`pages → widgets → features → entities → shared`。

---

## 流水线

| 阶段 | 做什么 | 产物 |
| --- | --- | --- |
| **P0 Ingest** | 识别子工程（`composer.json` / `package.json` / `pom.xml` …），排除 `vendor`、`node_modules`、`target`、静态资源、编译产物 | 子工程 + 待分析文件 |
| **P2 CfAst** | 语言无关地把 `SyntaxFacts` 落成节点 | `Class` / `Interface` / `Trait` / `Enum` / `Method` / `Function` / `Property` / `Const` / `Namespace` / **`CallSite`**；`imports` 表（短名→FQN）；`by_name` 索引；继承/实现/trait 边 |
| **P3 Prepare** | 按 FKB 识别框架、解析 `AppRoot`、装载权威源 | `app_root`、容器绑定、事件表、`schema`、`config_keys`、`i18n`、`facade_map`、`route_list`、`nginx` |
| **P4 AnnotatePre** | 选择器作用在**源码**上 | Taint（`source` / `sanitizer` / `sink`）、`listener` 等标签 |
| **P5 Synthesize** | 按 `identity` **幂等合成**语义节点 | `Table` / `HttpContract` / `ConfigKey` / `I18nKey` / `Event` / `Queue` / `Cache` / `Topic` + `HandledBy` 等边 |
| **P6 AnnotatePost** | 选择器作用在**图节点**上 | `pii.phone`、`data.criticality`、`config.storage`、`auth.public`、`entrypoint.login`、`i18n.missing_locale`；注册 `by_alias` |
| **P7 Resolve** | 漏斗式解析（L1 字面 → L2 注册表 → L3 别名 → L4 约定 → L6 与全集求交）+ **不动点迭代** | 动态边：`ResolvesTo` / `Triggers` / `HandledBy` |

阶段的**顺序不可颠倒**：P5 要查 P3 的权威符号表；P6 要查 P5 的汇聚结果（fan_in 只有汇聚完才算得准）；P7 要查 P6 注册的别名。

---

## 关键设计

### 1. `identity` 幂等合并

三条不同规则（`Db::name('store_order')`、Model 的 `$table`、类名约定）只要算出相同的 `identity`，就合并成**同一个** `Table` 节点：

```yaml
identity:
  kind: Fqn
  value: { arg: 0 }
  normalize: [ { strip_prefix: [] }, singularize, { strip_prefix: [] } ]
```

`strip_prefix: []`（空列表）表示「使用当前工程探测到的表前缀」，而非写死某个具体前缀。
前缀在 P3 由 FKB 的 `db_prefix` root_rule 从框架配置（如 ThinkPHP 的 `config/database.php`
的 `connections.mysql.prefix`，支持 `env('KEY', 'default')` 默认值）自动读出，或来自工程配置
`ProjectConfig.table_prefixes`；通用层 `ProjectConfig::default()` 不再内置任何项目特定前缀。

归一化让 `store_order` / `eb_store_order` / `store_orders` 收敛到一个节点；否则 fan_in 会从 200 变成 67+66+67，影响面分析与死表检测全部失真。

### 2. 契约桥：`HttpContract`

后端 `Route::post('apple_login', 'Login/appleLogin')` 与前端 `uni.request({url:'/api/apple_login', method:'POST'})`
归一化出**同一个 identity** `POST /apple_login` → 合并为一个节点。
于是「死端点（只有后端）」与「幽灵调用（只有前端）」自动可检。

### 3. 一切靠 FKB，不硬编码

`fkb/` 下的每份 YAML 声明：如何识别框架、如何解析 `AppRoot`、装载哪些权威表、各阶段跑哪些规则。
新增框架 = 新增一份 YAML。内核不认识 ThinkPHP、CRMEB 或 Uni-app。

例如 `AppRoot` 的解析（`composer.json` 的 `autoload.psr-4`）：

```json
{"value": "app", "confidence": 1.0,
 "source": ".../crmeb/composer.json autoload.psr-4 (map_dir=app/)",
 "fallback_used": false}
```

### 4. 诊断是一等产物

`UnresolvedLink`（路由指向不存在的 handler → 运行时 500）、`AnnotateTargetMissing`、
`AliasTargetMissing`（目标在 vendor，属预期）、`IdentityUnresolved` 都会进诊断表并在 UI 展示。

---

## 交互范式：从「理解」到「行动」

### 1. 两级筛选器 + 单对象链路

顶部固定两级：**一级选视角，二级选对象**。选中后只渲染**当前这一个对象**的链路子图，
其它对象的链路边**直接不画**（不是变暗）。被刻意省略的部分靠三件事守住诚实性：

* **计数提示** —— `已画 74 条边，另有 96 条属于其它对象的链路边被刻意省略`
* **未解析记账** —— 面板列出 `UnresolvedLink` / `AnnotateTargetMissing` 等
* **可切换** —— 二级列表随时切到别的对象

视角由 `views/perspectives.yaml` 声明，分两类，语义完全不同：

| | 对象类视角 | 聚合类视角 |
| --- | --- | --- |
| 例子 | 路由 / 表 / Schedule / Event / Queue / Cache / Topic | Domain / DeployUnit / Platform |
| 语义 | **单链路**：中心 + 同心环 | **聚合概览**：聚类框 / 矩阵，不是单链路 |
| 二级筛选器 | 有 | 无 |

### 2. 布局由算法决定，任何节点都不允许自由漂移

| 视图模式 | 布局 | 边样式 |
| --- | --- | --- |
| 对象入口子图（默认） | **径向 / 同心环**（环 = 跳数） | 直线 |
| 分层调用链 | **分层**（自上而下） | 90° 正交折线 |
| 路径模式（取证） | **线性 Spine**（最长链排成主轴） | 直线 |
| 聚合全貌 | **聚类 Compound**（大框 + 计数） | 直线 |
| 多端对比 | **矩阵**（行列两维度，单元格色块） | 无 |
| 表关系 / ER | **ER 正交** | 90° |

同一份输入必然得到同一份输出 —— 可复现、可截图对比、可写单测。

### 3. 单击切视角（只对有视角的节点）

| 节点 | 单击行为 |
| --- | --- |
| `HttpContract` / `Table` / `Schedule` / `Event` / `Queue` / `Cache` / `Topic` | 切到对应**对象视角**，二级同步为该节点 |
| `Domain` / `DeployUnit` / `Platform` | 切到**聚合视角**（框 + 计数 / 矩阵） |
| `ConfigKey` / `KeyPattern` / `Component` / `SecretLocation` | **不切**顶部筛选器，只开右侧 Inspector |

配套约束：悬停只高亮、单击才切、右键与详情图标不切、面包屑可回退、旧中心保留为邻居并标记 `from`、二级列表联动高亮。

### 4. URL 即现场

`?p=route&n=58548&d=2&m=radial&i=123` 就是完整现场：刷新、前进/后退、分享链接都能还原。
URL 过期（节点 id 失效、视角不存在）时由 `reconcileViewState` 修正并退回默认，绝不静默展示一张无关的图。

### 5. 跳转：图可信度的自证手段

* **Node → 定义位置**：合成节点（`Table:user`）必然来自多处共现（SQL 建表 + Model 的 `$table` + 各调用点），
  因此给**多位置列表**而不是编造单一位置。`user` 表在本样本里就是 14 处。
* **Edge → 证据链**：实线单点跳；虚线展开途经的每个 CallSite 位置，让用户亲自验证 —— 这才是"虚线是待验证假设"的落地。
* **未解析** → 跳到断链点本身并标注原因。

交互上和"单击切视角"严格分离：**单击 = 导航，跳转走右键 / 悬停图标 / Inspector 的 Open in IDE**。

技术上用 `vscode://file:line`、`jetbrains://`、`cursor://`，并**必须配「复制 path:line」作为 fallback**
（覆盖 CI / Web / 无 IDE / 远程容器）。定位用 **path + symbol + line 三元组**以防行号漂移。
`SecretLocation` 只跳键名位置、绝不显示值 —— 分析工具不能成为泄露源。

## 在 CRMEB 样本上的实测

`samples/CRMEB-master`（3 个子工程、2178 个源文件）全量建图约 **6 秒**：

| 阶段 | 节点 | 边 | 标注 | 耗时 |
| --- | --- | --- | --- | --- |
| Ingest | 0 | 0 | 0 | 91ms |
| CfAst | 57 554 | 57 852 | 0 | 2.8s |
| Prepare | 0 | 0 | 0 | 0.25s |
| AnnotatePre | 45 | 19 | 27 | 0.19s |
| Synthesize | 1 610 | 957 | 0 | 0.36s |
| AnnotatePost | 0 | 0 | 1 866 | 0.06s |
| Resolve | 0 | 234 | 0 | 0.13s |

产出：`Class` 1011、`Method` 6620、`CallSite` 47354、**`HttpContract` 1227**、**`Table` 156`、`Event` 45、`ConfigKey` 227；
标注含 `pii.phone`（19 张表，含通过 `user_phone` 变体列名识别出的 `store_order`）、`data.criticality`、`config.storage:Database`、`entrypoint.login`（10 个端点，含 `POST /apple_login`）。

---

## 目录

```
crates/
├── gt-domain            领域内核（实体 + 端口）
├── gt-application       用例编排
├── gt-pipeline          P0/P2/P3/P4/P5/P6/P7
├── gt-adapter-fs        文件扫描（排除规则）
├── gt-adapter-parser    tree-sitter（当前：PHP）
├── gt-adapter-fkb       FKB YAML 装载
├── gt-adapter-sqlite    SQLite 持久化
├── gt-adapter-http      axum REST API
└── gt-app               组装根 + CLI
src-tauri/               Tauri 桌面端（独立 workspace）
ui/                      React + TS + antd（FSD）
fkb/                     预置框架知识
views/                   视角声明（两级筛选器的一级选项）
```

## 扩展新语言 / 新框架

* **新语言**：实现 `gt_domain::port::LanguageParser`（把语法树翻译成 `SyntaxFacts`），在 `DefaultParserRegistry` 注册；在 `scanner::language_of_extension` 补扩展名。
* **新框架**：在 `fkb/` 加一份 YAML（detectors / root_rules / loaders / rules / resolvers）。
* **新节点种类**：直接在 YAML 里写新的 `node:` 名称，无需改 Rust。
