import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { KNOWN_TRIPLES, tripleProblem } from './triples.mjs';

test('the six shipped triples are accepted', () => {
  assert.equal(KNOWN_TRIPLES.length, 6);
  for (const triple of KNOWN_TRIPLES) assert.equal(tripleProblem(triple), null);
});

test('anything else is refused', () => {
  for (const bad of ['../../etc', 'x86_64-pc-windows-msvc/..', '', undefined, 'riscv64-unknown-linux-gnu']) {
    assert.match(tripleProblem(bad), /unknown target triple/);
  }
});

test('stage-backend and bundle exit 1 on a path-like triple before touching anything', () => {
  const stage = fileURLToPath(new URL('./stage-backend.mjs', import.meta.url));
  const bundle = fileURLToPath(new URL('./bundle.mjs', import.meta.url));
  for (const argv of [[stage, '../../evil'], [bundle, '--target', '../../evil']]) {
    const run = spawnSync(process.execPath, argv, { encoding: 'utf8' });
    assert.equal(run.status, 1, run.stdout + run.stderr);
    assert.match(run.stderr, /unknown target triple/);
  }
});
