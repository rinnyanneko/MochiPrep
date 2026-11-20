import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./arch-release.mjs', import.meta.url));

function releaseFixture(t) {
  const root = mkdtempSync(join(tmpdir(), 'mochi-arch-release-'));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const repo = join(root, 'checkout');
  mkdirSync(repo);
  const git = (...args) => execFileSync('git', args, { cwd: repo });
  git('init', '--quiet');
  writeFileSync(join(repo, 'Cargo.toml'), '[workspace.package]\nversion = "2.3.4"\n');
  writeFileSync(join(repo, 'README.md'), 'Tracked release content\n');
  git('-c', 'core.autocrlf=false', 'add', 'Cargo.toml', 'README.md');
  git('-c', 'user.name=Test', '-c', 'user.email=test@example.test', 'commit', '--quiet', '-m', 'Release fixture');
  writeFileSync(join(repo, 'README.md'), 'Uncommitted content\n');
  const out = join(root, 'assets');
  return {
    out,
    run: (tag) => spawnSync(process.execPath, [script, tag, 'example/MochiPrep', out], {
      cwd: repo,
      encoding: 'utf8',
    }),
  };
}

test('the release PKGBUILD pins the tagged version and verifies its release source asset', (t) => {
  const { out, run } = releaseFixture(t);
  const result = run('v2.3.4');
  assert.equal(result.status, 0, result.stderr);

  const archive = join(out, 'mochiprep-2.3.4.tar.gz');
  const recipe = readFileSync(join(out, 'PKGBUILD'), 'utf8');
  const checksum = createHash('sha256').update(readFileSync(archive)).digest('hex');
  assert.match(recipe, /^pkgver=2\.3\.4$/m);
  assert.match(recipe, /^pkgrel=1$/m);
  assert.ok(recipe.split('\n').includes(
    'source=("https://github.com/example/MochiPrep/releases/download/v$pkgver/$pkgname-$pkgver.tar.gz")',
  ));
  assert.ok(recipe.split(/\r?\n/).includes(`sha256sums=('${checksum}')`));
  assert.equal(execFileSync('tar', ['-xOzf', archive, 'MochiPrep-2.3.4/README.md'], { encoding: 'utf8' })
    .replace(/\r\n/g, '\n'), 'Tracked release content\n');

  const retry = run('v2.3.4');
  assert.equal(retry.status, 0, retry.stderr);
  assert.equal(readFileSync(join(out, 'PKGBUILD'), 'utf8'), recipe);
  assert.equal(createHash('sha256').update(readFileSync(archive)).digest('hex'), checksum);
});

test('a tag that disagrees with the archived Cargo version produces no release files', (t) => {
  const { out, run } = releaseFixture(t);
  const result = run('v2.3.5');
  assert.equal(result.status, 1);
  assert.match(result.stderr, /expected v2\.3\.4/);
  assert.equal(existsSync(out), false);
});
