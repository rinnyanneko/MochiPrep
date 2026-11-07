import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['packages/*/test/**/*.test.ts', 'apps/*/test/**/*.test.ts'],
    setupFiles: ['./vitest.setup.ts'],
    testTimeout: 20000,
  },
});
