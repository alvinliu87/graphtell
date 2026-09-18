import { http } from '@/shared/api/http';
import type { HealthDto, RunAccepted, RunStatus } from './model';

export const pipelineApi = {
  run: (projectId: number) => http.post<RunAccepted>(`/api/projects/${projectId}/run`),
  status: (projectId: number) => http.get<RunStatus>(`/api/projects/${projectId}/run/status`),
  health: () => http.get<HealthDto>('/api/health'),
};
