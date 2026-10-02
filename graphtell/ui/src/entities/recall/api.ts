import { http } from '@/shared/api/http';
import type { ComposePromptRequest, ComposePromptResult, RecallQuery, RecallResult } from './model';

/** Data access for prompt augmentation (code recall + prompt composition). */
export const recallApi = {
  recall: (projectId: number, q: RecallQuery) =>
    http.post<RecallResult>(`/api/projects/${projectId}/recall`, q),
  /**
   * Compose a prompt: graph-recalled context + user task → a prompt ready to feed an LLM.
   *
   * The page's main flow calls this one endpoint instead of recalling first and assembling locally:
   * the prompt template (role + TASK + REQUIREMENTS) lives in the backend `compose_prompt_text`,
   * and the frontend must not keep a second copy of that template — otherwise the MCP, CLI and Web
   * prompts inevitably start saying different things.
   */
  compose: (projectId: number, body: ComposePromptRequest) =>
    http.post<ComposePromptResult>(`/api/projects/${projectId}/prompt`, body),
};
