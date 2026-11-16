// `pnpm dev`: runs the Rust server and the web UI's vite dev server together.
//
// Ports: MOCHIPREP_PORT picks the backend port (the server's default when unset) and
// MOCHIPREP_WEB_PORT the vite port (default 5173). vite's proxy reads MOCHIPREP_PORT too, so
// /api on the vite port reaches the backend.
import { spawn, spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const webDir = join(root, 'apps', 'web');
const DEFAULT_WEB_PORT = '5173';
const webPort = process.env.MOCHIPREP_WEB_PORT || DEFAULT_WEB_PORT;
const backendPort = process.env.MOCHIPREP_PORT || '8787';

const env = { ...process.env, MOCHIPREP_PORT: backendPort };
// The server accepts the default vite origins on its own; any other vite port must be added,
// or the browser's requests through the proxy are refused for their Origin.
if (webPort !== DEFAULT_WEB_PORT) {
  const extra = [`http://127.0.0.1:${webPort}`, `http://localhost:${webPort}`];
  env.MOCHIPREP_ALLOWED_ORIGINS = [process.env.MOCHIPREP_ALLOWED_ORIGINS, ...extra].filter(Boolean).join(',');
}

// vite's own bin run with this Node, so no .cmd shim (and so no shell) is needed on Windows.
const webRequire = createRequire(join(webDir, 'package.json'));
const viteManifest = webRequire.resolve('vite/package.json');
const viteBin = join(dirname(viteManifest), webRequire(viteManifest).bin.vite);

const children = [
  // cargo is the developer's own toolchain, found on PATH by design.
  spawn('cargo', ['run', '-p', 'mochi-server'], { cwd: root, env, stdio: 'inherit' }), // NOSONAR
  spawn(process.execPath, [viteBin, '--port', webPort], { cwd: webDir, env, stdio: 'inherit' }),
];

let stopping = false;

function stopAll() {
  for (const child of children) {
    if (child.exitCode !== null || child.signalCode !== null || child.pid === undefined) continue;
    if (process.platform === 'win32') {
      // `cargo run` on Windows starts mochi-server as its own child and does not pass a kill on,
      // so the whole tree is ended or the server would outlive this script.
      // By absolute path, so a directory earlier on PATH cannot stand in for taskkill.
      const taskkill = join(process.env.SystemRoot || String.raw`C:\Windows`, 'System32', 'taskkill.exe');
      spawnSync(taskkill, ['/pid', String(child.pid), '/T', '/F'], { stdio: 'ignore' });
    } else {
      // On Unix `cargo run` replaces itself with the server, so the signal reaches it directly.
      child.kill('SIGINT');
    }
  }
}

for (const child of children) {
  child.on('exit', (code, signal) => {
    if (stopping) return;
    stopping = true;
    stopAll();
    process.exitCode = code ?? (signal ? 1 : 0);
  });
  child.on('error', (error) => {
    console.error(error.message);
    if (stopping) return;
    stopping = true;
    stopAll();
    process.exitCode = 1;
  });
}

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    stopping = true;
    stopAll();
  });
}
