import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'node',
    // 组件测试用 `.tsx`，并在文件头用 `// @vitest-environment jsdom` 单独切到 jsdom
    // （默认仍是 node，纯逻辑测试不必付 jsdom 的启动开销）。
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
