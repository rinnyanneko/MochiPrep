// Bundles the server into one ESM file for `start`. Running from source through tsx costs about
// 30 MiB for its loader; `dev` keeps using tsx for watch mode. Takes well under a second.
import { build } from 'esbuild';
import { cpSync, readFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const out = join(root, 'dist');
const { dependencies } = JSON.parse(readFileSync(join(root, 'package.json'), 'utf8'));

rmSync(out, { recursive: true, force: true });
await build({
  entryPoints: [join(root, 'src/main.ts')],
  outfile: join(out, 'main.mjs'),
  bundle: true,
  platform: 'node',
  format: 'esm',
  target: 'node22',
  // Workspace packages ship TypeScript source, so they are bundled. Third-party packages the
  // server depends on directly resolve from its node_modules at runtime; anything only a
  // workspace package depends on (decimal.js) is not reachable from here under pnpm, so it is bundled.
  external: Object.keys(dependencies).filter((name) => !name.startsWith('@mochiprep/')),
  sourcemap: true,
  logLevel: 'warning',
});
// database.ts reads migrations next to itself, which is dist/ once bundled.
cpSync(join(root, 'src/db/migrations'), join(out, 'migrations'), { recursive: true });
