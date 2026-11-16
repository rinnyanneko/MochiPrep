import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { checkTag, workspaceVersion } from './check-version.mjs';
import { buildNotes, readReports } from './release-notes.mjs';

const win = { target: 'win32-x64', metric: 'workingSet', bytes: 19327352, mib: 18.4, liveFeed: 'connected', idleSeconds: 12, measuredAt: '2026-11-16T00:00:00Z', commit: 'abc1234' };
const mac = { target: 'darwin-arm64', metric: 'footprint', bytes: 25165824, mib: 24, liveFeed: 'degraded', idleSeconds: 12, measuredAt: '2026-11-16T00:00:00Z', commit: 'abc1234' };

test('notes have a memory table row per target, sorted, with the method and caveats', () => {
  const notes = buildNotes([mac, win]);
  assert.match(notes, /^## Memory/m);
  const rows = notes.split('\n').filter((l) => l.startsWith('| ') && !l.includes('---') && !l.startsWith('| Target'));
  assert.equal(rows.length, 2);
  assert.equal(rows[0], '| darwin-arm64 | footprint | 24 | degraded |');
  assert.equal(rows[1], '| win32-x64 | workingSet | 18.4 | connected |');
  assert.match(notes, /interface closed/);
  assert.match(notes, /12 s/);
  assert.match(notes, /83 MiB/);
  assert.match(notes, /bd11fbe/);
  assert.match(notes, /20 MiB gate is checked on Windows x86_64 before a release; CI numbers are for reference/);
  assert.match(notes, /`win32-x64` is Windows x86_64/);
  assert.doesNotMatch(notes, /is gated/);
  assert.match(notes, /without a connected feed are not comparable/);
});

test('notes have Install and OKX sections', () => {
  const notes = buildNotes([win]);
  assert.match(notes, /^## Install/m);
  assert.match(notes, /README\.md#install/);
  assert.match(notes, /README\.md#unsigned-builds/);
  assert.match(notes, /not OS code-signed for now \(the updater artifacts are signed\)/);
  assert.doesNotMatch(notes, /are unsigned/);
  assert.match(notes, /^## OKX/m);
  assert.match(notes, /simulated exchange/);
  assert.match(notes, /not.*OKX Demo Trading/i);
});

test('no reports still yields a notes file that says so', () => {
  assert.match(buildNotes([]), /No memory reports/);
});

test('readReports finds memory-*.json recursively and skips others', () => {
  const dir = mkdtempSync(join(tmpdir(), 'notes-'));
  try {
    mkdirSync(join(dir, 'memory-a'));
    writeFileSync(join(dir, 'memory-a', 'memory-a.json'), JSON.stringify(win));
    writeFileSync(join(dir, 'other.json'), '{}');
    assert.deepEqual(readReports(dir), [win]);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test('checkTag accepts the workspace version and refuses others', () => {
  const toml = '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.1.0"\nedition = "2021"\n';
  assert.equal(workspaceVersion(toml), '0.1.0');
  assert.equal(checkTag('v0.1.0', toml), null);
  assert.match(checkTag('v0.2.0', toml), /0\.1\.0/);
  assert.match(checkTag('0.1.0', toml), /v0\.1\.0/);
  assert.match(checkTag('v0.1.0', '[package]\nversion = "0.1.0"\n'), /workspace\.package/);
});
