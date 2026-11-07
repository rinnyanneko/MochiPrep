import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { defineConfig } from '@playwright/test';

const WEB_PORT = 5199;
const API_PORT = 8877;
const env = {
  MOCHIPREP_PORT: String(API_PORT),
  MOCHIPREP_DATA_DIR: mkdtempSync(join(tmpdir(), 'mochiprep-e2e-')),
  MOCHIPREP_ALLOWED_ORIGINS: `http://127.0.0.1:${WEB_PORT}`,
};

// Runs against a throwaway database, on ports apart from `pnpm dev`. Uses the installed Chrome.
export default defineConfig({
  testDir: 'e2e',
  timeout: 90_000,
  use: { baseURL: `http://127.0.0.1:${WEB_PORT}`, channel: 'chrome', viewport: { width: 1440, height: 900 } },
  webServer: [
    { command: 'pnpm --filter @mochiprep/server start', port: API_PORT, env, reuseExistingServer: false },
    { command: `pnpm --filter @mochiprep/web exec vite --port ${WEB_PORT}`, port: WEB_PORT, env, reuseExistingServer: false },
  ],
});
