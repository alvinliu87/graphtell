import { http } from '@/shared/api/http';
import type { CreateProjectInput, Project, SubProject, UpdateProjectInput } from './model';

/** Data access for project entities (the only place that knows the `/api/projects` contract). */
export const projectApi = {
  list: () => http.get<Project[]>('/api/projects'),
  get: (id: number) => http.get<Project>(`/api/projects/${id}`),
  create: (input: CreateProjectInput) => http.post<Project>('/api/projects', input),
  update: (id: number, input: UpdateProjectInput) => http.put<Project>(`/api/projects/${id}`, input),
  remove: (id: number) => http.del<string>(`/api/projects/${id}`),
  subProjects: (id: number) => http.get<SubProject[]>(`/api/projects/${id}/sub-projects`),
};
