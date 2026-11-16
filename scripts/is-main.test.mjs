import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, symlinkSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

// A junction needs no privileges on Windows and is a plain directory symlink elsewhere.
test('a script started through a linked path still runs its main block', () => {
  const dir = mkdtempSync(join(tmpdir(), 'is-main-'));
  try {
    const link = join(dir, 'scripts');
    symlinkSync(fileURLToPath(new URL('.', import.meta.url)), link, 'junction');
    // A tag that never matches: exit 1 proves the check ran; exit 0 would mean it was skipped.
    const run = spawnSync(process.execPath, [join(link, 'check-version.mjs'), 'v0.0.0-never'], { encoding: 'utf8' });
    assert.equal(run.status, 1, run.stdout + run.stderr);
    assert.match(run.stderr, /does not match the workspace version/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
