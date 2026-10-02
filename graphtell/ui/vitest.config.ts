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
    // Component tests use `.tsx`, and switch to jsdom per-file with `// @vitest-environment jsdom`
    // (the default stays node; pure logic tests needn't pay jsdom's startup cost).
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
