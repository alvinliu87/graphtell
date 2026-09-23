# FKB 编写指南（Framework Knowledge Base Authoring）

FKB 是 GraphTell 的「框架知识」层：**一份 YAML 描述一个框架怎么识别、怎么从源码里抽出语义节点和边**。
内核**不认识任何具体框架**——新增对某个框架/语言的支持，只需要加一份 YAML，不需要改 Rust 代码。

本指南面向「想让 GraphTell 支持某个新框架」的你（无论是手写还是用 AI 生成）。

---

## 1. 它是怎么被加载的（先懂机制，少踩坑）

- **目录自动发现**：启动时递归扫描 `fkb/<任意子目录>/*.yaml`，丢一份新文件就自动加载，**无需注册清单**。
- **语义种类自动登记**：FKB 里声明的 `semantic_kinds` 在装载时自动登记进内核（见 §5.2），新增节点种类不必改内核。
- **可插件式、不改核心仓库**：用 `--fkb-dir <目录>` 或环境变量 `GRAPHTELL_FKB_DIR` 指向你自己的 FKB 目录，Engine 会整体改用它。别人可以自带一套规则跑你的 Engine，完全不用来提 PR。
- **损坏文件优雅跳过**：某份 YAML 写坏不会让程序崩溃，只会 warn 并跳过。

> 校验：写好一份 FKB 后，先跑 `graphtell validate`（见 §8）再做建图，能省掉大量「写了没反应」的沉默失败。

---

## 2. 一份 FKB 长什么样（顶层字段）

```yaml
id: my-framework          # 必须，全局唯一；同 id 的外部目录会覆盖内置
display_name: My Framework
language: php             # 开放字符串：php / java / javascript / … 决定哪些子工程应用它
version_hint: ">=2.0"     # 仅展示用

detectors: [...]          # 如何「识别」本框架（命中后才生效，除非 apply_without_detection）
root_rules: [...]         # 如何解析框架根（如 app 目录）
loaders: [...]            # P3 装载权威符号表（schema / config_keys / i18n / facade_map …）
resolvers: [...]          # P7 动态解析声明（容器 make / 事件 / 门面 / handler …）

rules: [...]              # ★ 核心：本框架的抽取规则
semantic_kinds: []        # 本 FKB 引入的「新语义节点种类」（不在内核清单里时必须声明，见 §5.2）

scope: framework          # framework（通用框架）| project（项目专有，仅该项目识别时加载）
apply_without_detection: false  # true=对所有同语言子工程生效（用于「语言通用层」，不含框架假设）
exclude_globs: ["node_modules/**", "dist/**"]

# 框架特定约定（都不含框架假设的放语言通用层，含强假设的留在这里）：
handler: {...}            # 路由 handler 字符串如何还原成「类 + 方法」
db_verbs: {read: [...], write: [...]}   # 模型 CRUD 动词 → 读/写分类
magic_delegation: {...}   # @method 注解 + __call 转发到某属性
external_calls: [...]     # 会发起网络请求的 callee（供「循环内外部调用」判定）
tx_calls: [...]           # 事务边界标记
entry_methods: [...]      # 消费入口方法名候选（handle/fire/doJob/__invoke/run …）
```

**核心原则**：`rules` 才是重点；`detectors` 决定「这份知识对谁生效」；`semantic_kinds` 决定「你造的新节点会不会被看见」。

---

## 3. 规则 = 阶段 + 选择器 + 动作

```yaml
- id: my-rule
  phase: Synthesize        # 流水线阶段（见 §3.1）
  selector:                # 作用于什么
    kind: call
    callee: "Cache::get|*Cache::*"   # 见 §3.2
  binding:                 # 命中后做什么（可多个动作）
    - Synthesize: {...}     # 造一个语义节点（最常用）
    # - Link: {...}        # 只建一条边
    # - Annotate: {...}     # 打标注
  confidence: 0.9
```

### 3.1 阶段（phase）

| 值 | 含义 |
|---|---|
| `Synthesize` | **P5 合成非代码语义节点**（Cache / Event / Table / HttpContract …）。绝大多数抽取规则用这个。 |
| `AnnotatePre` | P4 按源码选择器打标注 |
| `AnnotatePost` | P6 在汇聚结果上打标 / 注册别名 |

（其余 `Ingest`/`CfAst`/`Prepare`/`Alias` 由内核与装载流程使用，一般不在手写规则里出现。）

### 3.2 选择器（selector）

最常用的是 `call`：

```yaml
selector:
  kind: call
  callee: "Cache::get|Cache::has|*Cache::get"   # `|` = 或；`*` = 通配（按语言分隔符匹配，如 `think\facade\Cache`）
  where: []                                      # 可选谓词收窄（见源码 Predicate 枚举）
```

其它选择器：`inheritance`（继承/实现）、`config_entry`（配置项）、`declaration`（语法声明）、`node`（图上已有节点）、`dynamic`（P7 动态解析调用）。

### 3.3 动作（binding）

- `Synthesize`：物化一个语义节点（见 §4）。
- `Link`：只建一条边（`kind` + `from`/`to` 两个 `ValueSource` + `resolve`）。
- `Annotate`：打标注（合规/资产标记用）。

---

## 4. Synthesize 动作：如何造一个语义节点

```yaml
- Synthesize:
    node: Cache            # 节点种类（开放字符串）。★ 现代写法直接写具体种类，不要再写 `node: ExternalSystem, subtype: Cache`（旧写法，仍兼容）
    # subtype: Cache       # 旧机制：若填则「提升为 kind」（node 被忽略）。新规则请用上面的直接写法
    identity:
      kind: Named          # Fqn | Named | ContractId（见 §4.1）
      value: { arg: 0, require_literal: true }   # 取第 0 个实参且必须是字面量
      value_fallback: { literal: "Cache" }        # 取不到时的兜底
    fields:
      - name: key
        value: { arg: 0, require_literal: true }
      - name: side          # ★★★ 见 §5.1：进程外中介/资产类节点必须带 side
        value: { literal: "backend" }
    link:
      kind: ReadsCache      # 边种类（见 §5.4）
      direction: incoming   # incoming（调用方→本节点）| outgoing | to_target
    confidence: 0.85
```

### 4.1 identity（决定「哪些调用合并成同一个节点」）

- `Named`：具名（`{ value: { arg: 0 } }` 取实参作为名字，如缓存 key、事件名）。
- `Fqn`：完全限定名（如 `Table:store_order`）。
- `ContractId`：HTTP 契约 `METHOD /path`（用于 `HttpContract`，配合 `method`/`path` 两个来源）。

> **幂等合并**：三条不同规则只要算出相同 `identity`，就合并成一个节点。所以「后端 `Cache::get('token')`」和「前端 `uni.setStorageSync('token')`」靠 `side`（见下）区分成两个节点。

### 4.2 ValueSource（取值，FKB 表达力的核心）

常见来源（详见 `gt-domain/src/model/fkb.rs` 的 `ValueSource`）：

`arg`(第 n 实参) · `element`(实参是数组取下标) · `field`(对象字面量取字段) · `property`(类属性) ·
`method_name`(调用的方法名) · `owner_class`(产生调用的类) · `owner_member`(产生调用的方法/字段) ·
`receiver_class`(被调接收者类，经 import 别名还原) · `entity`(调用点关切的**主领域类型**，由 parser 按调用种类填入，如 `@EventListener` 的首个形参类型、`publishEvent(new X())` 的 `X`；取不到时返回 None，交 `value_fallback` 兜底) ·
`literal`(字面量) · `require_literal`(只接受字面量，拒绝变量) ·
`require_class`(解析结果须是真实类，否则整体 None) · `transform`(snake_plural/lower/…) · `normalize`(归一化链)。

> `entity` 的典型用途是**类型级归并**：Spring 的 `@EventListener` 与 `publishEvent` 都用事件类型（而非收发方法名）作身份，使同一事件类型的发布方与订阅方归并到同一个 `Event` 节点（见 `fkb/java/spring-boot.yaml` 的 `spring-event-*` 规则 + 端到端测试 `tests/java_spring_features.rs`）。

---

## 5. 约定（最容易踩坑的地方，请务必读）

### 5.1 `side`：前后端拆分（最重要）

进程外中介（Cache / ConfigKey / Event / Queue / Topic）和资产类节点**必须**在 `fields` 里写明它属于哪一端：

```yaml
fields:
  - name: side
    value: { literal: "backend" }   # 或 "frontend"
```

**为什么必须**：Engine 会把 `side` 注入节点 `identity` 的 scope（见 `engine.rs` 的 `with_scope`）。
没有它，前端 `uni.setStorageSync('token')` 和后端 `Cache::get('token')` 同名会被合并成**同一个** Cache 节点，图就乱了。
这也是「缓存视角 / 事件视角」按 `side` 分流的依据（`views/perspectives.yaml` 里 `side: backend` / `side: frontend`）。

### 5.2 `semantic_kinds`：引入新节点种类

**内置语义节点种类**（折叠视图默认只显示这些）：
`Table` · `HttpContract` · `ConfigKey` · `I18nKey` · `Event` · `Queue` · `Cache` · `Topic` · `Schedule` · `Page` · `EventBus` · `EventHandler`。

若你的 FKB 用了一个**不在上面清单里的新 kind**（例如 `Store`），**必须**在顶层声明：

```yaml
semantic_kinds: [Store]
```

否则该节点不会被当作语义节点，折叠视图会把它藏起来（看起来「写了没反应」）。
参考：`fkb/js/uni-app.yaml` 声明了 `semantic_kinds: [Store, Page, EventBus]`（`Store` 不在内置清单，必须声明）。

### 5.3 让节点进某个「视角」

节点要显示在某个视角里，其 `kind`（及 `side`）必须在 `views/perspectives.yaml` 注册：

- 内置 kind 已有对应视角（cache / local_storage / event / event_bus / queue / topic / route / table …）。
- 你若用了**新 kind**，需要在 `perspectives.yaml` 加一条 perspective（声明 `node_kind` + 可选 `side`）并在 `node_views` 加映射，否则点该节点只会开 Inspector、不会切到视角。

### 5.4 边种类：新边种类也能「只写 FKB、零代码」（与节点同构）

**内置语义边**：`Triggers` · `PublishesTo` · `ReadsDb` · `WritesDb` · `MapsTo` · `ReadsConfig` · `ResolvesTo` · `WritesCache` · `Mutates` · `NavigatesTo` · `Emits` · `ListensTo` · `ReadsCache`。
**内置桥边**：`HandledBy` · `CallsHttp`。

绝大多数情况请**复用已有的边种类**，别发明新词。但如果你确实需要一个全新的边种类，现在也**不用改 `kinds.rs`**——直接在 FKB 顶层声明即可，装载时自动登记进注册表：

```yaml
semantic_edge_kinds: [SendsWebhook]   # 新语义边：会被当语义边计数 / 绘制
bridge_edge_kinds:   [MyBridge]       # 新桥边：连接语义节点 ↔ 语法节点
```

声明后，`is_semantic` / `is_bridge` / `is_chain_edge` 会自动把它当一等公民（与 `semantic_kinds` 对节点的处理同构），渲染与「入边 N」计数都会正确。
> 注：引擎对**全新**边种类的「专属渲染」（如 `HandledBy`/`CallsHttp` 的桥边布局）仍是内置特例；新边种类会走默认渲染。这与「节点无需改码即可显示」一致——分类零代码，专属样式才需小改。

`graphtell validate` 也认这套声明：某条边种类只要在内核清单**或**任意 FKB 的 `semantic_edge_kinds`/`bridge_edge_kinds` 里，就不会报「未注册」警告。

---

## 6. 完整示例

### 6.1 后端缓存（节选自 `fkb/php/common.yaml`）—— 展示 `side: backend`

```yaml
- id: php-common-cache-read
  phase: Synthesize
  selector:
    kind: call
    callee: "Cache::get|Cache::has|*Cache::get|*Cache::remember"
  binding:
    - Synthesize:
        node: Cache
        identity:
          kind: Named
          value: { arg: 0, require_literal: true }
          value_fallback: { literal: "Cache" }
        fields:
          - name: key
            value: { arg: 0, require_literal: true }
          - name: side
            value: { literal: "backend" }
        link: { kind: ReadsCache, direction: incoming }
        confidence: 0.85
```

### 6.2 前端事件总线（`fkb/js/uni-app.yaml`）—— 展示 `semantic_kinds` + `side: frontend`

```yaml
id: frontend-js
language: javascript
semantic_kinds: [Store, Page, EventBus]   # Store 不在内置清单，必须声明
rules:
  - id: frontend-emit
    phase: Synthesize
    selector:
      kind: call
      callee: "uni.$emit|uni.$on|\\$eventHub.$emit"
    binding:
      - Synthesize:
          node: EventBus
          identity:
            kind: Named
            value: { arg: 0, require_literal: true }
          fields:
            - name: side
              value: { literal: "frontend" }
          link:
            kind: Emits
            direction: outgoing
```

### 6.3 Java HTTP 契约（`fkb/java/spring-boot.yaml`）—— 展示 `ContractId` identity

```yaml
- id: spring-mapping-http-contract
  phase: Synthesize
  selector:
    kind: call
    callee: "RequestMapping|GetMapping|PostMapping|PutMapping|DeleteMapping"
  binding:
    - Synthesize:
        node: HttpContract
        identity:
          kind: ContractId
          method: { method_name: true }                 # GET/POST…
          path:   { arg: 0, require_literal: true }     # "/api/login"
        link: { kind: HandledBy, direction: incoming }
```

---

## 7. 用 AI 批量生成 FKB（推荐）

FKB 是声明式 YAML + 固定匹配语义，非常适合交给 LLM 从框架文档/源码里提炼：

1. 把**本指南** + **2~3 份参考样例**（`fkb/php/common.yaml`、`fkb/js/uni-app.yaml`、`fkb/java/spring-boot.yaml`）+ 目标框架的文档/源码 交给 LLM。
2. 让它产出一份 FKB YAML（重点：用已有节点/边种类、记得 `side`、新种类记得 `semantic_kinds`）。
3. 跑 `graphtell validate --fkb-dir <你的目录>` 做语法 + 约定校验。
4. 用 `graphtell create --path <样本仓库>` 跑一份真实代码，**肉眼看图画得对不对**。

> ⚠ AI 会写出「看起来对、实际误匹配/漏匹配」的 FKB——必须第 4 步验证，不能只信生成。

---

## 8. `validate` 子命令

```
graphtell validate            # 校验内置 FKB 目录
graphtell validate --fkb-dir ./my-fkbs   # 校验你自己的目录
```

逐文件报告：

- `✓` 解析成功：打印 `id` / 语言 / 规则数 / 其中合成节点数 / `semantic_kinds`。
- `✗` 解析失败：打印具体错误（字段级）。
- `⚠` 约定警告（最有价值）：
  - 某条规则造出的节点种类既不在内核清单、也不在 `semantic_kinds` —— 折叠视图会隐藏它；
  - 某条边种类不在 `kinds.rs` 的 `SEMANTIC`/`BRIDGE` 清单 —— 不会被当语义/桥边渲染。

最后汇总「N 通过 / M 失败」，有失败会非零退出。

---

## 9. 何时需要改引擎（而不是只写 FKB）

以下情况**只写 FKB 不够**，需要动 Rust：

1. 引入**全新的边种类**（§5.4）—— 给 `kinds.rs` 的 `SEMANTIC`/`BRIDGE` 加一行。
2. 需要的**匹配/取值能力引擎还不支持**（例如某种新的参数提取、新的解析策略）—— 扩展 `ValueSource` / `ResolveStrategy` / 选择器。
3. 某节点种类需要**渲染特殊处理**（如 `view_service.rs` 里硬编码的 `Event|Queue|Topic` 分支）。

绝大多数真实框架（Spring / Laravel / Django / Rails / Express …）都能映射到**已有的** Cache/Event/Queue/HttpContract/Table/ConfigKey 种类，
所以约 90% 的 FKB 可以**零引擎改动**完成。
