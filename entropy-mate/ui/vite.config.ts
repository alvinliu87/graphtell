import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// Tauri 环境下固定端口，方便 Rust 侧注入；Web 开发时用代理。
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 5174 } : undefined,
    proxy: {
      '/api': {
        target: process.env.ENTROPY_MATE_API ?? 'http://127.0.0.1:5177',
        changeOrigin: true,
      },
    },
    watch: { ignored: ['**/src-tauri/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'es2021',
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    outDir: 'dist',
  },
});
