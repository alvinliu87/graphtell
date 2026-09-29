import { http } from '@/shared/api/http';
import type { ComposePromptRequest, ComposePromptResult, RecallQuery, RecallResult } from './model';

/** 提示词增强（代码召回 + 提示词合成）的数据访问。 */
export const recallApi = {
  recall: (projectId: number, q: RecallQuery) =>
    http.post<RecallResult>(`/api/projects/${projectId}/recall`, q),
  /**
   * 合成提示词：图谱召回上下文 + 用户任务 → 一段可直接投喂 LLM 的提示词。
   *
   * 页面主流程走这一个接口而不是先 recall 再本地拼：提示词模板（角色 + 【本次任务】
   * + 【要求】）在后端 `compose_prompt_text` 里，前端不该复制一份模板 ——
   * 否则 MCP、CLI、Web 三处提示词迟早各说各话。
   */
  compose: (projectId: number, body: ComposePromptRequest) =>
    http.post<ComposePromptResult>(`/api/projects/${projectId}/prompt`, body),
};
