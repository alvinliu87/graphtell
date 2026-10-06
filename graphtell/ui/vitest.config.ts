import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'jsdom',
    setupFiles: ['./vitest.setup.ts'],
    // Component tests may still opt into node via `// @vitest-environment node`;
    // pure-logic tests run fine under jsdom too.
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
  },
});
