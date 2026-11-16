// pnpm bundle --target <triple> [extra tauri build args]
// Builds the web UI, the release backend, stages it as the launcher's externalBin, then bundles.
// Args after --target <triple> go to `tauri build` (CI appends --config for no-signing-key runs).
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
// pnpm forwards a literal "--" when invoked as `pnpm bundle -- ...`.
const args = process.argv.slice(2).filter((a, i) => !(i === 0 && a === "--"));
const at = args.indexOf("--target");
const triple = at >= 0 ? args[at + 1] : undefined;
if (!triple) {
  console.error("usage: pnpm bundle --target <rust-target-triple> [tauri build args]");
  process.exit(1);
}

function run(cmd, cmdArgs) {
  console.log(`> ${cmd} ${cmdArgs.join(" ")}`);
  // Windows needs a shell for pnpm's .cmd shim; quote args ourselves so none is concatenated raw.
  const win = process.platform === "win32";
  const quote = (a) => (/^[\w@%+=:,./\\-]+$/.test(a) ? a : `"${a.replaceAll('"', '\\"')}"`);
  const r = win
    ? spawnSync([cmd, ...cmdArgs].map(quote).join(" "), { cwd: root, stdio: "inherit", shell: true })
    : spawnSync(cmd, cmdArgs, { cwd: root, stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status ?? 1);
}

run("pnpm", ["build"]);
run("cargo", ["build", "--release", "-p", "mochi-server", "--target", triple]);
run("node", ["scripts/stage-backend.mjs", triple]);
run("pnpm", [
  "tauri", "build",
  "--config", "crates/mochi-launcher/tauri.release.conf.json",
  ...args,
]);
