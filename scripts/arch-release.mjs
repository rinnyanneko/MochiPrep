#!/usr/bin/env node
// Run from the release checkout: node scripts/arch-release.mjs <tag> <owner/repo> <output-dir>
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { checkTag } from './check-version.mjs';
import { versionOfTag } from './updater-manifest.mjs';

try {
  const [tag, repository, out] = process.argv.slice(2);
  if (!tag || !repository || !out) {
    throw new Error('usage: node scripts/arch-release.mjs <tag> <owner/repo> <output-dir>');
  }
  const version = versionOfTag(tag);
  if (!/^[\w-]+\/[\w.-]+$/.test(repository)) throw new Error('invalid GitHub repository');
  const problem = checkTag(tag, execFileSync('git', ['show', 'HEAD:Cargo.toml'], { encoding: 'utf8' }));
  if (problem) throw new Error(problem);

  mkdirSync(out, { recursive: true });
  const archive = join(out, `mochiprep-${version}.tar.gz`);
  // Publish these exact bytes: GitHub's on-demand tag archives can change compression.
  execFileSync('git', ['archive', '--format=tar.gz', `--prefix=MochiPrep-${version}/`, `--output=${archive}`, 'HEAD']);
  const checksum = createHash('sha256').update(readFileSync(archive)).digest('hex');
  const recipe = readFileSync(new URL('../packaging/arch/PKGBUILD', import.meta.url), 'utf8')
    .replaceAll('@PKGVER@', version)
    .replaceAll('@GITHUB_REPOSITORY@', repository)
    .replaceAll('@SHA256@', checksum)
    .replaceAll('\r\n', '\n');
  writeFileSync(join(out, 'PKGBUILD'), recipe);
} catch (error) {
  console.error(`arch-release: ${error.message}`);
  process.exitCode = 1;
}
