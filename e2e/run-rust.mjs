// Runs Playwright against the prebuilt Rust server. A script rather than an inline env prefix so it
// works the same in cmd, PowerShell and sh.
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

const args = process.argv.slice(2).filter((arg) => arg !== '--');
// Use this Node executable and the installed CLI directly so arguments never pass through a shell.
const result = spawnSync(process.execPath, [require.resolve('@playwright/test/cli'), 'test', ...args], {
  stdio: 'inherit',
  env: { ...process.env, MOCHIPREP_E2E_BACKEND: 'rust' },
});
process.exit(result.status ?? 1);
