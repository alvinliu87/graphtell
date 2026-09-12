/** 工程实体：与后端 `ProjectDto` 对齐。 */

export type ProjectStatus = 'created' | 'indexing' | 'ready' | 'failed';

export interface ProjectConfig {
  exclude_globs: string[];
  required_locales: string[];
  table_prefixes: string[];
  full_pipeline: boolean;
}

export interface Project {
  id: number;
  name: string;
  root_path: string;
  description: string | null;
  status: ProjectStatus;
  config: ProjectConfig;
  created_at: number;
  updated_at: number;
}

export interface CreateProjectInput {
  name: string;
  root_path: string;
  description?: string;
  config?: Partial<ProjectConfig>;
}

export interface UpdateProjectInput {
  name?: string;
  root_path?: string;
  description?: string;
  config?: Partial<ProjectConfig>;
}

export interface SubProject {
  id: number;
  name: string;
  root_path: string;
  language: string;
  role: string;
  detected_by: string;
  frameworks: string[];
  facts: Record<string, unknown> | null;
}

export const STATUS_META: Record<ProjectStatus, { label: string; color: string }> = {
  created: { label: '待建图', color: 'default' },
  indexing: { label: '建图中', color: 'processing' },
  ready: { label: '就绪', color: 'success' },
  failed: { label: '失败', color: 'error' },
};
