import { http } from '@/shared/api/http';

/** Live state of a (possibly running) one-click model download. */
export interface DownloadState {
  active: boolean;
  phase: string;
  /** 0..1 progress; -1 means indeterminate (stage-based only). */
  progress: number;
  log: string;
  error: string | null;
}

/** Backend + model status surfaced to the settings page. */
export interface ModelStatus {
  local_weights_present: boolean;
  backend_mode: string;
  backend_url: string | null;
  embedding_backend: string;
  embedding_dim: number;
  download: DownloadState;
}

/** Payload for switching the embedding backend at runtime. */
export interface SetBackendRequest {
  mode: 'auto' | 'local' | 'url' | 'hash';
  url?: string;
}

/** Result of a backend switch (same shape minus the download state). */
export type BackendStatus = Omit<ModelStatus, 'download'>;

export const modelApi = {
  status: () => http.get<ModelStatus>('/api/models/status'),
  download: () => http.post<ModelStatus>('/api/models/download'),
  setBackend: (req: SetBackendRequest) =>
    http.put<BackendStatus>('/api/server/backend', req),
};
