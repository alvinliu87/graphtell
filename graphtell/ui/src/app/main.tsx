import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { initApiBase } from '@/shared/api/http';
import { fetchBackendEnv } from '@/shared/lib/backendEnv';
import './styles/global.css';

const container = document.getElementById('root');
if (!container) throw new Error('#root element not found');

// Resolve the backend base URL first (the desktop build gets its port injected by Tauri), then
// fetch the backend environment (including WSL detection), and only then render — so the first
// requests never go to an empty base and the WSL settings apply on the very first screen.
void initApiBase()
  .then(() => fetchBackendEnv().catch(() => {}))
  .then(() => {
    createRoot(container).render(
      <StrictMode>
        <App />
      </StrictMode>,
    );
  });
