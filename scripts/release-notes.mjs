#!/usr/bin/env node
// Builds the release notes from the memory reports the build jobs upload, so the numbers in a
// release are the ones measured on that tag's builds.
//
//   node scripts/release-notes.mjs <dir-of-memory-json> [tag] > notes.md
import { readFileSync, readdirSync } from 'node:fs';
import path from 'node:path';
import { isMain } from './is-main.mjs';

/** Every `memory-*.json` under `dir`, recursively (download-artifact makes one folder each). */
export function readReports(dir) {
  const reports = [];
  for (const entry of readdirSync(dir, { withFileTypes: true, recursive: true })) {
    if (entry.isFile() && /^memory-.*\.json$/.test(entry.name)) {
      reports.push(JSON.parse(readFileSync(path.join(entry.parentPath, entry.name), 'utf8')));
    }
  }
  return reports;
}

export function buildNotes(reports, ref = 'HEAD') {
  const readme = `https://github.com/rinnyanneko/MochiPrep/blob/${ref}/README.md`;
  const rows = [...reports]
    .sort((a, b) => String(a.target).localeCompare(String(b.target)))
    .map((r) => `| ${r.target} | ${r.metric} | ${r.mib} | ${r.liveFeed} |`);
  const table = rows.length
    ? ['| Target | Metric | MiB | Live feed |', '| --- | --- | --- | --- |', ...rows].join('\n')
    : 'No memory reports were produced for this build.';
  return `## Memory

${table}

Method: the backend runs with the interface closed, idle for about 12 s, with the live OKX feed connected. Targets are \`<platform>-<arch>\` as Node names them (\`win32-x64\` is Windows x86_64). The metric is the working set on Windows, VmRSS on Linux and the footprint on macOS. The 32 MiB gate is checked on Windows x86_64 before a release; CI numbers are for reference. The Node backend measured 83 MiB at \`bd11fbe\`. Runs without a connected feed are not comparable.

## Install

See [Install](${readme}#install). The installers are not OS code-signed for now (the updater artifacts are signed); see [Unsigned builds](${readme}#unsigned-builds) for what your OS will say and how to open them.

## OKX

The OKX integration was tested only against a simulated exchange. It has not been tested against OKX Demo Trading or a real OKX account. Do not point it at real funds until you have tried it on Demo Trading yourself.
`;
}

if (isMain(import.meta.url)) {
  const dir = process.argv[2];
  if (!dir) {
    console.error('usage: node scripts/release-notes.mjs <dir-of-memory-json>');
    process.exit(1);
  }
  process.stdout.write(buildNotes(readReports(dir), process.argv[3] || 'HEAD'));
}
