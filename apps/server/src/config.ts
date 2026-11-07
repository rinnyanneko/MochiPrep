import { join, resolve } from 'node:path';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

export const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');

export interface ServerConfig {
  host: '127.0.0.1';
  port: number;
  dataDir: string;
  allowedOrigins: string[];
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): ServerConfig {
  const port = Number(env.MOCHIPREP_PORT ?? 8787);
  const extra = (env.MOCHIPREP_ALLOWED_ORIGINS ?? '').split(',').map((s) => s.trim()).filter(Boolean);
  return {
    host: '127.0.0.1',
    port,
    dataDir: env.MOCHIPREP_DATA_DIR ? resolve(env.MOCHIPREP_DATA_DIR) : join(REPO_ROOT, '.mochiprep'),
    allowedOrigins: [
      'http://localhost:5173',
      'http://127.0.0.1:5173',
      `http://localhost:${port}`,
      `http://127.0.0.1:${port}`,
      ...extra,
    ],
  };
}
