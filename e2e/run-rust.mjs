// Runs Playwright against the prebuilt Rust server. A script rather than an inline env prefix so it
// works the same in cmd, PowerShell and sh.
import { spawnSync } from 'node:child_process';

const args = process.argv.slice(2).filter((arg) => arg !== '--');
const result = spawnSync('pnpm', ['exec', 'playwright', 'test', ...args], {
  stdio: 'inherit',
  shell: true,
  env: { ...process.env, MOCHIPREP_E2E_BACKEND: 'rust' },
});
process.exit(result.status ?? 1);
