import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { defineConfig } from '@playwright/test';

const WEB_PORT = 5199;
const API_PORT = 8877;
// MOCHIPREP_E2E_BACKEND=rust runs the prebuilt release binary (`pnpm test:e2e:rust` builds it first).
const rust = process.env.MOCHIPREP_E2E_BACKEND === 'rust';
const rustBinary = join('target', 'release', process.platform === 'win32' ? 'mochi-server.exe' : 'mochi-server');
const apiCommand = rust ? rustBinary : 'pnpm --filter @mochiprep/server start';

const env = {
  MOCHIPREP_PORT: String(API_PORT),
  MOCHIPREP_DATA_DIR: mkdtempSync(join(tmpdir(), 'mochiprep-e2e-')),
  MOCHIPREP_ALLOWED_ORIGINS: `http://127.0.0.1:${WEB_PORT}`,
  // The Rust server skips OKX and the news feeds; the specs need only the replay tape.
  ...(rust ? { MOCHIPREP_OFFLINE: '1' } : {}),
};

// Runs against a throwaway database, on ports apart from `pnpm dev`. Uses the installed Chrome.
export default defineConfig({
  testDir: 'e2e',
  timeout: 90_000,
  // One backend and database are shared, so files run one after another (demo, display, paper).
  workers: 1,
  use: { baseURL: `http://127.0.0.1:${WEB_PORT}`, channel: 'chrome', viewport: { width: 1440, height: 900 } },
  webServer: [
    { command: apiCommand, port: API_PORT, env, reuseExistingServer: false },
    { command: `pnpm --filter @mochiprep/web exec vite --port ${WEB_PORT}`, port: WEB_PORT, env, reuseExistingServer: false },
  ],
});
