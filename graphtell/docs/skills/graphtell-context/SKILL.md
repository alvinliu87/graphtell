---
name: graphtell-context
description: >-
  仅当任务涉及"在本仓库里写/改代码、或做合规检查/审计、或评估 FKB（知识库）覆盖度"时使用——根据
  代码图谱召回相关上下文、组装给代码生成 LLM 的 prompt、运行合规检查、或查看覆盖率缺口。闲聊、
  与代码改动无关的纯问答、纯解释性请求不要加载本技能，也不要调用任何 graphtell 工具。
---

# GraphTell MCP 工具使用指引

当本地 `graphtell serve` 在跑时，IDE 会注入以下 MCP 工具。它们**不是每次对话都该用**——只在下列
"调用条件"成立时才用；其余情况直接用你自己的知识回答，不要发起任何工具调用（工具走本地服务 TCP
往返、回 Markdown 占 token，乱调既慢又贵，还可能把召回噪音当证据）。

## 工具与门控

- `recall_code(query, limit?, hops?, include_body?)`
  - 调用条件：你即将改/写本仓库代码，需要定位相关实现、调用关系、文件行号。
  - 不调用：任务不涉及本仓库代码；或能直接从已有上下文回答。
  - 注意：返回顶部有**质量分层**。`low` 时不要直接采信，用末尾 feature terms 再搜或读源码；
    `medium` 可能不全，多搜一次再下结论。冷路径（warming）召回更弱。

- `compose_prompt(query, intent?, limit?, hops?, with_snippets?)`
  - 调用条件：要把"召回上下文 + 用户意图 + 质量约束"打包成一条可直接喂给代码生成 LLM 的 prompt。
  - 不调用：用户只要局部答案，或你已在手动组装上下文。

- `check_compliance(rule_ids?)`
  - 调用条件：用户明确要求合规检查，或即将提交/做大改动想先看风险。
  - 不调用：用户没提合规，或只是小改动问答。

- `list_violations(limit?)`
  - 调用条件：想看最近一次合规检查的结果（不重跑规则）。
  - 不调用：还没跑过检查，或用户问的是别的。

- `coverage()`
  - 调用条件：评估当前项目"已加载的 FKB 到底提取出了多少代码"——写/改 FKB 规则前后对比缺口、
    判断某个子项目是不是 `language_unknown` / `no_framework`（无歧义知识缺口）、或决定要不要补规则。
  - 不调用：用户没问覆盖度/知识库完整性；或只是普通编码问答。
  - 注意：`sub_projects_with_gaps` 只统计 `language_unknown` / `no_framework` 这类**无歧义缺口**，
    `low_coverage` 只是提示（实用程序调用如 `Math.min`、动态 URL 被拒都是正常的），不要把它当真缺口去追。

- `warmup_status()`
  - 调用条件：召回质量差、怀疑在冷路径时，先查向量预热进度，决定是否稍后重试。
  - 不调用：召回结果已经够好。

## 负约束（务必遵守）

- 不要把工具结果当"真理"直接采信——尤其是 `recall_code` 的 `low` 档与 `coverage` 的 `low_coverage` 提示。
- 不要在用户没提合规时主动跑 `check_compliance`。
- 不要为了"显得在工作"而无目的地调用工具；与代码改动无关的 prompt 直接回答即可。
- 调用任何工具前先确认它服务于当前任务，而不是习惯性地每轮都调。
