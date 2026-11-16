import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { defineConfig } from '@playwright/test';

const WEB_PORT = 5199;
// MOCHIPREP_E2E_API_PORT moves the backend off 8877 when that port is taken.
const API_PORT = Number(process.env.MOCHIPREP_E2E_API_PORT || 8877);
// The prebuilt release binary: `pnpm test:e2e` builds it first.
const rustBinary = join('target', 'release', process.platform === 'win32' ? 'mochi-server.exe' : 'mochi-server');

const env = {
  MOCHIPREP_PORT: String(API_PORT),
  MOCHIPREP_DATA_DIR: mkdtempSync(join(tmpdir(), 'mochiprep-e2e-')),
  MOCHIPREP_ALLOWED_ORIGINS: `http://127.0.0.1:${WEB_PORT}`,
  // The server skips OKX and the news feeds; the specs need only the replay tape.
  MOCHIPREP_OFFLINE: '1',
};

// Runs against a throwaway database, on ports apart from `pnpm dev`. Uses the installed Chrome.
export default defineConfig({
  testDir: 'e2e',
  timeout: 90_000,
  // One backend and database are shared, so files run one after another in name order (demo,
  // display, paper). The order matters: demo.spec asserts a 40,000.00 total equity, which holds
  // only while paper.spec has not yet created its account. A new spec that creates an account
  // must sort after demo.spec.
  workers: 1,
  use: { baseURL: `http://127.0.0.1:${WEB_PORT}`, channel: 'chrome', viewport: { width: 1440, height: 900 } },
  webServer: [
    { command: rustBinary, port: API_PORT, env, reuseExistingServer: false },
    { command: `pnpm --filter @mochiprep/web exec vite --port ${WEB_PORT}`, port: WEB_PORT, env, reuseExistingServer: false },
  ],
});
