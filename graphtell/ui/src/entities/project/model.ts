/** Project entity: aligned with the backend `ProjectDto`. */

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

/** Status display metadata; labels are English source strings translated via `t()`. */
export const STATUS_META: Record<ProjectStatus, { label: string; color: string }> = {
  created: { label: 'Pending', color: 'default' },
  indexing: { label: 'Indexing', color: 'processing' },
  ready: { label: 'Ready', color: 'success' },
  failed: { label: 'Failed', color: 'error' },
};
