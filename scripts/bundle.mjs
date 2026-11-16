// pnpm bundle --target <triple> [extra tauri build args]
// Builds the web UI, the release backend, stages it as the launcher's externalBin, then bundles.
// Args after --target <triple> go to `tauri build`.
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { tripleProblem } from "./triples.mjs";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
// pnpm forwards a literal "--" when invoked as `pnpm bundle -- ...`.
const args = process.argv.slice(2).filter((a, i) => !(i === 0 && a === "--"));
const at = args.indexOf("--target");
const triple = at >= 0 ? args[at + 1] : undefined;
if (!triple) {
  console.error("usage: pnpm bundle --target <rust-target-triple> [tauri build args]");
  process.exit(1);
}
const problem = tripleProblem(triple);
if (problem) {
  console.error(problem);
  process.exit(1);
}

function run(cmd, cmdArgs) {
  console.log(`> ${cmd} ${cmdArgs.join(" ")}`);
  // Windows needs a shell for pnpm's .cmd shim; quote args ourselves so none is concatenated raw.
  const win = process.platform === "win32";
  const quote = (a) => (/^[\w@%+=:,./\\-]+$/.test(a) ? a : `"${a.replaceAll('"', String.raw`\"`)}"`);
  const r = win
    ? spawnSync([cmd, ...cmdArgs].map(quote).join(" "), { cwd: root, stdio: "inherit", shell: true })
    : spawnSync(cmd, cmdArgs, { cwd: root, stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status ?? 1);
}

run("pnpm", ["build"]);
run("cargo", ["build", "--locked", "--release", "-p", "mochi-server", "--target", triple]);
run("node", ["scripts/stage-backend.mjs", triple]);
// The release config turns on updater artifacts, and Tauri fails after bundling when it cannot
// sign them. Without a signing key a local bundle gets the installers only; a second --config is
// merged over the first. An explicit --config in args comes later and still wins.
const overrides = [];
if (!process.env.TAURI_SIGNING_PRIVATE_KEY) {
  const overrideDir = mkdtempSync(join(tmpdir(), "mochi-bundle-"));
  // On exit rather than in a finally: run() ends the process on a failed step.
  process.on("exit", () => rmSync(overrideDir, { recursive: true, force: true }));
  const override = join(overrideDir, "no-updater.json");
  writeFileSync(override, JSON.stringify({ bundle: { createUpdaterArtifacts: false } }));
  overrides.push("--config", override);
  console.log("TAURI_SIGNING_PRIVATE_KEY is not set: updater artifacts are skipped, installers only.");
}
run("pnpm", [
  "tauri", "build",
  "--config", "crates/mochi-launcher/tauri.release.conf.json",
  ...overrides,
  ...args,
]);
