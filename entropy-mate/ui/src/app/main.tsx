import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { initApiBase } from '@/shared/api/http';
import './styles/global.css';

const container = document.getElementById('root');
if (!container) throw new Error('#root 节点不存在');

// 先确定后端基地址（桌面端由 Tauri 注入端口），再渲染，避免首屏请求打空。
void initApiBase().then(() => {
  createRoot(container).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
});
