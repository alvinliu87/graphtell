import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { initApiBase } from '@/shared/api/http';
import { fetchBackendEnv } from '@/shared/lib/backendEnv';
import './styles/global.css';

const container = document.getElementById('root');
if (!container) throw new Error('#root 节点不存在');

// 先确定后端基地址（桌面端由 Tauri 注入端口），再拉取后端环境（含 WSL 探测），
// 最后渲染，避免首屏请求打空、且 WSL 设置能在首屏即生效。
void initApiBase()
  .then(() => fetchBackendEnv().catch(() => {}))
  .then(() => {
    createRoot(container).render(
      <StrictMode>
        <App />
      </StrictMode>,
    );
  });
