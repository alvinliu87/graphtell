# 样本代码库的来源与许可证

`samples/` 下是**供 GraphTell 做分析与演示的样本代码库**，不是本产品的源码。
其中大部分是第三方开源项目（按各自许可证授权），仅用作测试材料与 demo 素材。

> 若下文任何信息与上游仓库的声明不一致，**以上游仓库为准**。

## 清单

| 目录 | 性质 | 上游 | 许可证 | 许可证文本 |
| --- | --- | --- | --- | --- |
| `frontend-backend-link` | **本项目自造的合成夹具**（包名 `synthetic/frontend`、`synthetic/backend`） | — | 本项目自有 | 无需第三方授权 |
| `hackathon-starter` | 第三方 OSS | https://github.com/sahat/hackathon-starter | MIT © Sahat Yalkabov | ✅ 上游随附（`samples/hackathon-starter/LICENSE`） |
| `nestjs-realworld-example-app` | 第三方 OSS | https://github.com/lujakob/nestjs-realworld-example-app | ISC | ⚠️ 上游未随附，按 package.json 声明补齐标准文本 |
| `typescript-starter` | 第三方 OSS | https://github.com/nestjs/typescript-starter | MIT | ⚠️ 上游未随附，按 package.json 声明补齐标准文本 |
| `php-projects/laravel-starter` | 第三方 OSS（Laravel 官方骨架） | https://github.com/laravel/laravel | MIT © Taylor Otwell | ⚠️ 按 composer.json 声明补齐标准文本 |
| CRMEB / Bagisto | 第三方大型工程 | — | 非标准宽松许可 | **不含在本仓库**：需设 `GRAPHTELL_SAMPLE_DIR` 自备 |

## 说明

- **为什么补齐许可证文本**：MIT / ISC 均允许再分发，条件是随附版权声明与许可证文本。
  上表标 ⚠️ 的三个样本上游未随附 `LICENSE` 文件，此处按上游 `package.json` /
  `composer.json` 的声明补齐对应许可证的标准文本，版权归上游作者所有。
- **`frontend-backend-link` 是自造夹具**：它由本项目编写，用于覆盖「前端 ↔ 后端」的
  跨端链路、`Cache` / `Store` / `ConfigKey` 等语义节点等场景，不含任何第三方代码，
  因此可自由用于公开 demo。
- **CRMEB / Bagisto 不入库**：它们是大型电商系统、许可证并非标准宽松许可（CRMEB 偏
  商业 / 开放核心），故本仓库不分发其源码；相关测试与文档在样本缺席时会自动跳过。
- **是否修改过样本**：样本基本按上游原样取用；部分样本下可能带有本项目自己的
  `.graphtell/aliases.json`（工程级意图别名配置，属本项目的配置文件，非上游内容）。
- **公开 demo（`docs/demo/`）**：展示的是 GraphTell 对这些样本的**分析结果**（图结构、
  规则违规、召回上下文），并在页面上标注每个样本的名称、上游链接与许可证；
  `recall` 上下文包会引用样本中的少量源码片段，属节选引用，已随署名给出。
