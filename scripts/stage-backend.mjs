// Copies the release mochi-server for one target to where Tauri's externalBin expects it:
// binaries/mochi-server-<triple>[.exe]. Tauri appends the triple itself, so the name must match.
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const triple = process.argv[2];
if (!triple) {
  console.error("usage: node scripts/stage-backend.mjs <rust-target-triple>");
  process.exit(1);
}

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const ext = triple.includes("windows") ? ".exe" : "";
const source = join(root, "target", triple, "release", `mochi-server${ext}`);
if (!existsSync(source)) {
  console.error(
    `missing ${source}\nbuild it first: cargo build --release -p mochi-server --target ${triple}`,
  );
  process.exit(1);
}

const destDir = join(root, "crates", "mochi-launcher", "binaries");
mkdirSync(destDir, { recursive: true });
const dest = join(destDir, `mochi-server-${triple}${ext}`);
copyFileSync(source, dest);
console.log(`staged ${dest}`);
