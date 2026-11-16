import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    include: ['apps/web/test/**/*.test.ts'],
    setupFiles: ['./vitest.setup.ts'],
    testTimeout: 20000,
  },
});
