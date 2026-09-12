import { http } from '@/shared/api/http';
import type { CreateProjectInput, Project, SubProject, UpdateProjectInput } from './model';

/** 工程实体的数据访问（唯一知道 `/api/projects` 这一契约的地方）。 */
export const projectApi = {
  list: () => http.get<Project[]>('/api/projects'),
  get: (id: number) => http.get<Project>(`/api/projects/${id}`),
  create: (input: CreateProjectInput) => http.post<Project>('/api/projects', input),
  update: (id: number, input: UpdateProjectInput) => http.put<Project>(`/api/projects/${id}`, input),
  remove: (id: number) => http.del<string>(`/api/projects/${id}`),
  subProjects: (id: number) => http.get<SubProject[]>(`/api/projects/${id}/sub-projects`),
};
