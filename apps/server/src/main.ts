import { join } from 'node:path';
import { buildApp } from './app';
import { loadConfig } from './config';
import { openDatabase } from './db/database';
import { Runtime } from './runtime';

const config = loadConfig();
const db = openDatabase(join(config.dataDir, 'mochiprep.db'));
const runtime = new Runtime({ db });
const { app } = buildApp(runtime, config);

runtime.start();
await app.listen({ host: config.host, port: config.port });
console.log(`MochiPrep backend on http://${config.host}:${config.port} (data: ${config.dataDir})`);

const shutdown = async () => {
  await runtime.stop();
  await app.close();
  db.close();
  process.exit(0);
};
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
