# 贡献指南（CONTRIBUTING）

欢迎为 GraphTell 做贡献。本文聚焦「怎么改才不会被退回」：架构约束、FKB 工作流、规则纪律、测试与验证。

架构与流水线背景见 [`README.md`](./README.md)；FKB 编写细则见 [`docs/fkb-authoring.md`](./docs/fkb-authoring.md)；当前支持清单见 [`SUPPORTED.md`](./SUPPORTED.md)。

---

## 1. 开发环境

```bash
cargo build                                    # 构建后端
cargo test                                     # 跑全部 Rust 测试
cargo run -p gt-app -- validate                 # 校验内置 FKB
cargo run -p gt-app -- create --name X --path /repo   # 建图
```

后端在进程内起 HTTP 服务，桌面端（Tauri）与 Web 端共用同一套 `/api` 契约；前端在 `ui/`（React + TS + antd，Feature Sliced Design）。

---

## 2. 架构约束（改代码前必读）

仓库是**六边形架构 + SOLID**，依赖方向永远指向内核 `gt-domain`：

- **`gt-domain` 不能依赖任何具体技术**（不得 `use` 任何 adapter / 框架）。所有 IO 通过 `port` 里的 trait 反向注入。
- **新语言 / 新框架 / 新节点种类优先用声明式方式完成，不要上来就改引擎**。约 90% 的 FKB 可以零引擎改动。
- 确实要改引擎的少数情况（见 `docs/fkb-authoring.md` §9）：引入全新的边种类、需要的取值/匹配能力 `ValueSource` 还不支持、某节点需要专属渲染。
- **向后兼容的 schema 改动**：给 `CallSiteFact` / `ValueSource` 等结构体加字段时，务必加 `#[serde(default)]` 并在所有构造点补 `None` / 默认值（搜索 `CallSiteFact {` / `CallRecord {` 确认无遗漏，否则编译不过）。

---

## 3. 想支持一个新框架 / 语言

### 新框架（最常见，纯 YAML）
1. 读 [`docs/fkb-authoring.md`](./docs/fkb-authoring.md) 的 §1–§6。
2. 在 `fkb/<子目录>/` 加一份 YAML：`detectors`（识别条件）+ `rules`（抽取规则）+ 必要的 `semantic_kinds` / `semantic_edge_kinds`。
3. **关键纪律**：
   - 复用已有节点 / 边种类，别发明新词（新种类也要在 YAML 声明，否则折叠视图会藏起来）。
   - 进程外中介（Cache / ConfigKey / Event / Queue / Topic）必须写 `side`（`backend` / `frontend`），否则前后端同名节点会被错误合并。
   - 用 `arg` + `require_literal` 防止把变量名当成身份造出垃圾节点；取不到时用 `value_fallback` 兜底。
4. `graphtell validate` 跑通（注意 `⚠` 约定警告，比报错更有价值）。
5. 用真实样本建图，**肉眼确认图画得对**。

### 新语言
1. 实现 `gt_domain::port::LanguageParser`，把 tree-sitter 语法树翻译成语言无关的 `SyntaxFacts`（范本：`src/java` 是"第二语言"，`src/python` 是"第三语言"——后者额外示范了装饰器建模、模块级函数的 `owner_class` 回填等动态语言问题）。
2. 在 `DefaultParserRegistry` 注册，并在 `scanner::language_of_extension` 补扩展名。
3. 在 `fkb/` 为该语言写框架 YAML（语义提取完全走 FKB，内核不认识任何框架）。

> **先 dump 语法树，别靠记忆写解析器**。加一个临时测试打印 `root_node().to_sexp()`，确认节点类型与**字段名**再动手。
> 两次真实 bug 都是这么抓到的：`typed_parameter` 在 tree-sitter-python 里**没有** `name` 字段（形参类型全丢）、
> `from x import y` 把 `module_name` 节点本身也当成了导入项（凭空造出 `fastapi.fastapi`）。
> 两者都不报错，只会静默产出错误事实 —— 靠读代码是想不出来的。

> 参考范本：`fkb/java/spring-boot.yaml` + 端到端测试 `crates/gt-pipeline/tests/java_spring_features.rs`（合成样本，无需外部工程即可验证 cache/event/queue/topic/schedule 全部落成）；Python 侧见 `fkb/python/fastapi.yaml` + `tests/python_fastapi_features.rs`。

---

## 4. 合规规则：先量后写

写规则的成本很低，**验证它不产噪声的成本很高**。每条候选规则都先在样本库（5 个 ThinkPHP + 3 个 Spring Boot 已建图工程）量一遍再决定是否发货，标准只有两条：

- 命中数**不能是 0**（静默失效——比误报更危险，因为不表现为报错）；
- 命中数**不能刷屏**（噪声）。

已被实测否决、留档避免重复讨论的候选（私有方法从未被调用、配置键无读取方、缓存写了从不读、表被写但不被读、上帝方法、队列投递无消费者、GET 契约名含 create/edit、外部回调未验签 …）见 README「新规则怎么才算能发货」。

规则用 `applies_to.languages` / `applies_to.frameworks` **先验声明**适用范围，避免「PHP 专属语义在 Java 工程上把每个节点都报成违规」。引擎还会从谓词自动推导依赖（边 / 标注 / 能力），图里没有这些事实就停用该规则（`rules_unavailable`），无需手写 `requires`。

---

## 5. 测试与验证

- **引擎 / 规则单测**：`cargo test -p gt-pipeline -p gt-domain -p gt-adapter-parser`。
- **端到端建图测试**：`crates/gt-pipeline/tests/crmeb_pipeline.rs`（PHP 样本）、`crates/gt-pipeline/tests/java_spring_features.rs`（Java 合成样本）、`tests/python_fastapi_features.rs` / `tests/python_flask_features.rs`（Python 合成样本）、`tests/node_real_samples.rs`（NestJS / Express 合成 + 真实样本）、`tests/unsupported_language.rs`（无解析器语言的可见性）。
- **新增 Java 语义特征时**：优先在 `java_spring_features.rs` 的合成样本里加对应注解 / 调用，并断言节点与边（这是验证「FKB 真的把框架语义落成图」最便宜的方式）。
- **FKB 改动**：`graphtell validate` 必须全绿；改 FKB **不触发重新建图**，但要让某条测试覆盖到你改的规则。
- **解析器 / 引擎改动**：需要重建图，跑对应的 e2e 测试。

---

## 6. 提交与 PR

- 描述清楚「改了什么、为什么、怎么验证的」。
- 若动了 `gt-domain` 的数据结构，确认所有构造点都补了默认值（见 §2）。
- 若加了框架支持，请在 `SUPPORTED.md` 同步更新矩阵；若改了 FKB 编写能力（如新增 `ValueSource`），请在 `docs/fkb-authoring.md` 同步。
- 文档用中文（与现有 `README.md` / `docs/` 保持一致）。

不要求你熟悉 Rust 也能贡献 FKB / 规则 / 视角声明（都是 YAML）；涉及引擎的改动欢迎先开 issue 讨论方案。
