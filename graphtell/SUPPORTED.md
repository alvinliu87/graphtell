# 支持矩阵（SUPPORTED）

GraphTell 当前已落地的语言 / 框架 / 语义特征，以及**已知的诚实边界**。
完整的架构与流水线说明见 [`README.md`](./README.md)，FKB 编写方法见 [`docs/fkb-authoring.md`](./docs/fkb-authoring.md)。

---

## 1. 语言与框架

| 语言 | 框架 / 形态 | 语义提取 | 合规规则 |
| --- | --- | --- | --- |
| **PHP** | ThinkPHP 6 / CRMEB / Laravel / Uni-app 后端契约 | 完整（表 / 路由 / 配置 / 国际化 / 缓存 / 事件 / 队列 / 验签 …） | 完整（含 N+1、验签、循环内外部调用、多写无事务等） |
| **Java** | Spring Boot（Spring Cache / ApplicationEvent / Spring AMQP / Spring Kafka / Spring Scheduling / JPA / MyBatis-Plus） | 完整（见 §2） | 复用 `rules/global/`（按图拓扑判定的规则）；框架专属规则（`orphan-event` / `orphan-queue` 等）尚未为 Java 写 |
| **JavaScript / TypeScript** | Uni-app 前端（事件总线 / 本地存储 / 页面 / Store） | 完整 | `rules/js/`（前端事件总线死代码） |

> 新语言 = 实现 `gt_domain::port::LanguageParser` 把语法树翻译成 `SyntaxFacts`（参考 `gt-adapter-parser/src/java`），在 `DefaultParserRegistry` 注册即可，内核零改动。

---

## 2. Java（Spring Boot）语义特征

所有语义节点都来自 `fkb/java/spring-boot.yaml`（声明式 FKB，**没改内核**）。
注解在内核里被建模成调用点（`callee` = 注解名），方法调用实参按位置捕获字面量。

| 语义 | 触发 | 节点 | 边 | 备注 |
| --- | --- | --- | --- | --- |
| **Cache** | `@Cacheable` / `@CachePut` / `@CacheEvict` | `Cache` | `ReadsCache` / `WritesCache` | 缓存名取自注解实参 `arg0` |
| **Event** | `@EventListener(handler)` / `publisher.publishEvent(new X())` | `Event` | `ListensTo` / `Emits` | **类型级归并**：同一事件类型的发布方与订阅方归并到同一节点（见 §4） |
| **Queue** | `@RabbitListener(queues="q")` / `rabbitTemplate.convertAndSend("q", …)` | `Queue` | `ListensTo` / `PublishesTo` | 消费端来自注解，生产端来自方法调用实参 |
| **Topic** | `@KafkaListener(topics="t")` / `kafkaTemplate.send("t", …)` | `Topic` | `ListensTo` / `PublishesTo` | 生产端按 receiver 变量名约定匹配（见 §4） |
| **Schedule** | `@Scheduled` | `Schedule` | `Triggers` | 调度器触发该方法（出边） |
| **HttpContract** | `@RequestMapping` / `@GetMapping` / `@PostMapping` … | `HttpContract` | `HandledBy` | 契约 `METHOD /path` 归一，可与前端 `uni.request` 契约桥接 |
| **Table** | `@TableName("x")` / `@Table(name="x")` / MyBatis `mapper/*.xml` | `Table` | `MapsTo` / `ReadsDb` / `WritesDb` | MyBatis-Plus 与 JPA 表名；XML 语句按读写落边 |
| **ConfigKey** | `@Value("${app.name}")` | `ConfigKey` | `ReadsConfig` | 去 `${` `}` 得到配置键 |

端到端自检见 `crates/gt-pipeline/tests/java_spring_features.rs`（合成样本，无需外部工程）。

---

## 3. 合规检查（rules）

规则按适用环境分目录装载，内核不认识任何具体规则：

| 目录 | 适用 | 内容 |
| --- | --- | --- |
| `rules/global/` | 跨语言通用（只依赖图拓扑） | 契约桥（`http-contract-without-handler` / `frontend-calls-missing-backend` / `backend-endpoint-never-called`）、热点表（`hot-table`）、配置读取热点（`config-read-hotspot`）、死表（`dead-table`）、只写不读 / 只读不写表、高扇出方法（`hotspot-method`） |
| `rules/php/` | 仅 PHP 工程 | 原始 SQL 执行点、PII 表、从未触发的事件 / 队列（**Java 侧的 Event/Queue/Topic 已能产出，可后续补同类规则**）、循环内逐条读写库（N+1）、循环内外部调用、多写无事务、验签质量 |
| `rules/js/` | 含前端子工程的工程 | 前端事件总线死代码 |

内置约 30 条规则。规则用 `applies_to.languages` / `applies_to.frameworks` 先验声明适用范围；环境不匹配直接跳过（`rules_not_applicable`），判据依赖的事实图里没有则停用（`rules_unavailable`），避免"0 命中"这种比误报更危险的静默失效。详见 README「规则怎么知道该在哪跑」。

---

## 4. 已知边界（诚实声明）

这些是**当前图的真实缺口**，不是 bug，写规则与解读图时都要考虑：

- **Java 的 N+1（循环内逐条读写库）未做**：Java parser 未识别循环体、FKB 无 Java 的 `db_verbs`，所以 `n1-query-in-loop` 等 PHP 规则不会在 Java 工程上跑。
- **Kafka 生产端靠 receiver 变量名约定匹配**：`kafkaTemplate` / `kafkaProducer` / `producer` 三个常见字段名 + `KafkaTemplate.sendDefault`（专属方法名兜底）。项目若用别的字段名（如 `kt`），需把变量名加进 FKB 的 `callee` 备选。裸方法名 `send` 不能直用（会和 `sendError` / `email.send` 等误伤）。
- **事件类型级归并的兜底**：`publishEvent(var)`（实参是变量而非 `new X()`），或解析不出 handler 形参类型时，身份退回 `owner_member`（收发方法名）——此时不归并，但 `ListensTo` / `Emits` 边不丢。`entity` 取的是裸类型名（泛型已剥离）。
- **消息生产端 destination 依赖「方法调用实参字面量」的位置级捕获**：`convertAndSend` 同样来自 `RedisTemplate` / `JmsTemplate`，统一落 `Queue`；若需区分可按 receiver 伞名细化。
- **语法层无类型**：`rabbitTemplate` / `kafkaTemplate` 是变量名，不能还原成类，所以 producer 匹配用变量名约定而非类型（要更稳需符号表）。
- **不做向量、不调 LLM**：纯中文且不含标识符的召回做不到（见 README「代码召回」）。

---

## 5. 如何扩展 / 验证

- 加框架 = 在 `fkb/` 加一份 YAML（detectors / root_rules / loaders / rules / resolvers），跑 `graphtell validate` 校验。
- 加语言 = 实现 `LanguageParser`（见 `gt-adapter-parser/src/java` 这个"第二语言"范本）。
- 加合规规则 = 在 `rules/<env>/` 加 YAML，先在样本库（5 个 ThinkPHP + 3 个 Spring Boot 已建图工程）量一遍命中数：既不能是 0（静默失效），也不能刷屏（噪声），再决定是否发货。
- 改了 FKB 不触发重新建图；改了解析器 / 引擎才需要重建图。
