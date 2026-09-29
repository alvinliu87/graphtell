# GraphTell

把代码库**图化**的分析平台：以 `tree-sitter` 解析出语法级节点，再按**框架知识库（FKB）**合成语义节点，得到一张可查询、可标注、可做影响面与死代码分析的图。图建好之后还能回答两个问题：

- **合规检查** —— `rules/*.yaml` 声明的规则 → 违规清单（`path:line` 可跳转）
- **提示词增强** —— 一段（中文）提示词 → 该看哪些代码 + 可直接粘给 LLM 的上下文包

## 链接

| | |
| --- | --- |
| 🚀 **在线 Demo** | <https://alvinliu87.github.io/graphtell/> —— 多项目切换 · 图 / 规则检验 / 提示词增强 |
| 📖 **完整文档** | [`graphtell/README.md`](graphtell/README.md)（架构 · 流水线 · 快速开始 · 部署 · 实测数据） |
| ✅ **支持矩阵与已知边界** | [`graphtell/SUPPORTED.md`](graphtell/SUPPORTED.md) |
| ⚖️ **样本许可证** | [`graphtell/docs/samples-licenses.md`](graphtell/docs/samples-licenses.md) |

## 仓库结构

> **代码在工作区子目录 `graphtell/` 下。** GitHub 只在**仓库根**渲染 README，所以本文件只做索引，完整内容见 [`graphtell/README.md`](graphtell/README.md)。

| 路径 | 内容 |
| --- | --- |
| `graphtell/` | 源代码（Rust 后端 + React 前端 + Tauri 桌面端）、`fkb/`、`rules/`、`views/` 与完整文档 |
| `graphtell/docs/demo/` | 示例 demo 静态站点（`tools/gen_demo.sh` 生成，GitHub Pages 托管） |
| `graphtell/tools/` | 分析与演示脚本（含 `gen_demo.sh`） |
| `graphtell/scripts/` | 发布脚本（`package-release.sh`） |
| `samples/` | 样本代码库 —— **不随仓库分发**（仅本地测试用；自造夹具 `frontend-backend-link` 除外） |
| `.github/workflows/` | CI 与 GitHub Pages 发布 |
| `LICENSE` | MIT |

## 快速开始

```bash
cd graphtell
cargo build

# 建图（P0 → P7）
./target/debug/graphtell create --name 我的项目 --path /path/to/repo

# 启动 HTTP 服务（Web / 桌面端共用）
./target/debug/graphtell serve --port 5177
```

完整的构建 / 前端 / 桌面端 / Docker / 发布包步骤见 [`graphtell/README.md`](graphtell/README.md#快速开始)。

## 技术栈

Rust（**六边形架构** + SOLID，SQLite 持久化）· React + TypeScript + Ant Design（**Feature Sliced Design**）· Tauri（桌面常驻，与 Web 共用同一套 `/api`）· tree-sitter（多语言解析）· 框架知识库（FKB，YAML 驱动）。

当前已落地的语言 / 框架：**PHP**（ThinkPHP 6 / CRMEB / Laravel）、**Java**（Spring Boot）、**JavaScript / TypeScript**（Uni-app 前端、NestJS / Express 后端、TypeORM）、**Python**（FastAPI / Flask / Celery / SQLAlchemy）。

## 许可证

[MIT](LICENSE) © GraphTell contributors
