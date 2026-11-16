import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { buildManifest, updaterBundles, versionOfTag } from './updater-manifest.mjs';

const TAG = 'v0.1.0';
const base = `https://github.com/rinnyanneko/MochiPrep/releases/download/${TAG}/`;

/** A `.sig` as the Tauri CLI writes it: base64 of a minisign signature box. */
const sigFor = (name) =>
  Buffer.from(
    `untrusted comment: signature from tauri secret key\nRUQXim6brpHVEFAKE\ntrusted comment: timestamp:1763251200\tfile:${name}\nFAKEGLOBAL\n`,
  ).toString('base64');

/** Every asset a complete v0.1.0 draft holds, as the six legs upload them. */
const releaseAssets = [
  'MochiPrep_0.1.0_x64-setup.exe', 'MochiPrep_0.1.0_x64-setup.exe.sig',
  'MochiPrep_0.1.0_x64_en-US.msi', 'MochiPrep_0.1.0_x64_en-US.msi.sig',
  'MochiPrep_0.1.0_arm64-setup.exe', 'MochiPrep_0.1.0_arm64-setup.exe.sig',
  'MochiPrep_0.1.0_arm64_en-US.msi', 'MochiPrep_0.1.0_arm64_en-US.msi.sig',
  'MochiPrep_0.1.0_x64.dmg', 'MochiPrep_x64.app.tar.gz', 'MochiPrep_x64.app.tar.gz.sig',
  'MochiPrep_0.1.0_aarch64.dmg', 'MochiPrep_aarch64.app.tar.gz', 'MochiPrep_aarch64.app.tar.gz.sig',
  'MochiPrep_0.1.0_amd64.deb', 'MochiPrep_0.1.0_amd64.deb.sig',
  'MochiPrep_0.1.0_amd64.AppImage', 'MochiPrep_0.1.0_amd64.AppImage.sig',
  'MochiPrep_0.1.0_arm64.deb', 'MochiPrep_0.1.0_arm64.deb.sig',
  'MochiPrep_0.1.0_aarch64.AppImage', 'MochiPrep_0.1.0_aarch64.AppImage.sig',
];
const signaturesOf = (assets) =>
  Object.fromEntries(assets.filter((a) => a.endsWith('.sig')).map((a) => [a, `${sigFor(a.slice(0, -4))}\n`]));
const pubDate = new Date('2026-11-16T12:00:00Z');
const complete = () => buildManifest({ tag: TAG, assets: releaseAssets, signatures: signaturesOf(releaseAssets), pubDate });

test('a complete release gives every platform key the plugin looks up', () => {
  const manifest = complete();
  assert.equal(manifest.version, '0.1.0');
  assert.equal(manifest.pub_date, '2026-11-16T12:00:00.000Z');
  assert.match(manifest.notes, /releases\/tag\/v0\.1\.0/);
  const keys = [];
  for (const arch of ['x86_64', 'aarch64']) {
    keys.push(`windows-${arch}`, `windows-${arch}-nsis`, `windows-${arch}-msi`);
    keys.push(`darwin-${arch}`, `darwin-${arch}-app`);
    keys.push(`linux-${arch}`, `linux-${arch}-appimage`, `linux-${arch}-deb`);
  }
  assert.deepEqual(Object.keys(manifest.platforms).sort(), keys.sort());
});

test('each key points at the right bundle with its own signature', () => {
  const { platforms } = complete();
  const expect = {
    'windows-x86_64': 'MochiPrep_0.1.0_x64-setup.exe',
    'windows-x86_64-nsis': 'MochiPrep_0.1.0_x64-setup.exe',
    'windows-x86_64-msi': 'MochiPrep_0.1.0_x64_en-US.msi',
    'windows-aarch64': 'MochiPrep_0.1.0_arm64-setup.exe',
    'windows-aarch64-nsis': 'MochiPrep_0.1.0_arm64-setup.exe',
    'windows-aarch64-msi': 'MochiPrep_0.1.0_arm64_en-US.msi',
    'darwin-x86_64': 'MochiPrep_x64.app.tar.gz',
    'darwin-x86_64-app': 'MochiPrep_x64.app.tar.gz',
    'darwin-aarch64': 'MochiPrep_aarch64.app.tar.gz',
    'darwin-aarch64-app': 'MochiPrep_aarch64.app.tar.gz',
    'linux-x86_64': 'MochiPrep_0.1.0_amd64.AppImage',
    'linux-x86_64-appimage': 'MochiPrep_0.1.0_amd64.AppImage',
    'linux-aarch64': 'MochiPrep_0.1.0_aarch64.AppImage',
    'linux-aarch64-appimage': 'MochiPrep_0.1.0_aarch64.AppImage',
  };
  for (const [key, asset] of Object.entries(expect)) {
    assert.equal(platforms[key].url, base + asset, key);
    // Trimmed: the plugin base64-decodes the field, and a trailing newline is not base64.
    assert.equal(platforms[key].signature, sigFor(asset), key);
  }
});

test('a .deb install sees the version but never gets a .deb URL', () => {
  const { platforms } = complete();
  assert.deepEqual(platforms['linux-x86_64-deb'], platforms['linux-x86_64-appimage']);
  assert.deepEqual(platforms['linux-aarch64-deb'], platforms['linux-aarch64-appimage']);
  for (const entry of Object.values(platforms)) assert.doesNotMatch(entry.url, /\.deb$/);
});

test('every URL is a tag download URL on the owner repo, never a draft one', () => {
  for (const [key, entry] of Object.entries(complete().platforms)) {
    assert.ok(entry.url.startsWith(base), key);
    assert.doesNotMatch(entry.url, /untagged-/, key);
  }
});

test('a missing platform is an error naming it', () => {
  const assets = releaseAssets.filter((a) => !a.startsWith('MochiPrep_0.1.0_arm64-setup.exe'));
  assert.throws(
    () => buildManifest({ tag: TAG, assets, signatures: signaturesOf(assets), pubDate }),
    (error) =>
      /windows-aarch64-nsis: MochiPrep_0\.1\.0_arm64-setup\.exe, MochiPrep_0\.1\.0_arm64-setup\.exe\.sig/.test(error.message) &&
      !/windows-x86_64/.test(error.message),
  );
});

test('every missing platform is named at once', () => {
  const assets = releaseAssets.filter((a) => !a.includes('.app.tar.gz') && !a.endsWith('aarch64.AppImage.sig'));
  assert.throws(
    () => buildManifest({ tag: TAG, assets, signatures: signaturesOf(assets), pubDate }),
    (error) =>
      ['darwin-x86_64-app', 'darwin-aarch64-app', 'linux-aarch64-appimage: MochiPrep_0.1.0_aarch64.AppImage.sig'].every(
        (part) => error.message.includes(part),
      ),
  );
});

test('a bundle whose signature was listed but not read is an error', () => {
  const signatures = signaturesOf(releaseAssets);
  delete signatures['MochiPrep_x64.app.tar.gz.sig'];
  assert.throws(
    () => buildManifest({ tag: TAG, assets: releaseAssets, signatures, pubDate }),
    /darwin-x86_64-app: MochiPrep_x64\.app\.tar\.gz\.sig \(content\)/,
  );
});

test('a signature that is not a base64 minisign box is refused', () => {
  const raw = { ...signaturesOf(releaseAssets), 'MochiPrep_0.1.0_x64_en-US.msi.sig': 'untrusted comment: raw\nRUQ\n' };
  assert.throws(() => buildManifest({ tag: TAG, assets: releaseAssets, signatures: raw, pubDate }), /x64_en-US\.msi\.sig is not base64/);
  const notMinisign = { ...signaturesOf(releaseAssets), 'MochiPrep_0.1.0_x64_en-US.msi.sig': Buffer.from('hello').toString('base64') };
  assert.throws(
    () => buildManifest({ tag: TAG, assets: releaseAssets, signatures: notMinisign, pubDate }),
    /does not decode to a minisign signature/,
  );
});

test('assets of another version do not satisfy the manifest', () => {
  assert.throws(
    () => buildManifest({ tag: 'v0.2.0', assets: releaseAssets, signatures: signaturesOf(releaseAssets), pubDate }),
    /windows-x86_64-nsis: MochiPrep_0\.2\.0_x64-setup\.exe/,
  );
});

test('only vX.Y.Z tags are accepted', () => {
  assert.equal(versionOfTag('v1.22.333'), '1.22.333');
  for (const bad of ['0.1.0', 'v0.1', 'v0.1.0-rc.1', 'refs/tags/v0.1.0', '', undefined]) {
    assert.throws(() => versionOfTag(bad), /is not vX\.Y\.Z/);
  }
});

test('the bundle list covers the six build targets', () => {
  const targets = new Set(updaterBundles('0.1.0').map((b) => `${b.os}-${b.arch}`));
  assert.deepEqual([...targets].sort(), ['darwin-aarch64', 'darwin-x86_64', 'linux-aarch64', 'linux-x86_64', 'windows-aarch64', 'windows-x86_64']);
});

test('the CLI reads asset names and .sig files and writes latest.json', () => {
  const dir = mkdtempSync(join(tmpdir(), 'manifest-'));
  try {
    writeFileSync(join(dir, 'assets.txt'), `${releaseAssets.join('\r\n')}\n\n`);
    for (const [name, content] of Object.entries(signaturesOf(releaseAssets))) writeFileSync(join(dir, name), content);
    const script = fileURLToPath(new URL('./updater-manifest.mjs', import.meta.url));
    const out = join(dir, 'latest.json');
    const ok = spawnSync(process.execPath, [script, '--tag', TAG, '--assets', join(dir, 'assets.txt'), '--sigs', dir, '--out', out], { encoding: 'utf8' });
    assert.equal(ok.status, 0, ok.stderr);
    const manifest = JSON.parse(readFileSync(out, 'utf8'));
    assert.equal(manifest.version, '0.1.0');
    assert.equal(manifest.platforms['windows-x86_64'].url, `${base}MochiPrep_0.1.0_x64-setup.exe`);

    writeFileSync(join(dir, 'partial.txt'), releaseAssets.filter((a) => !a.includes('aarch64.app')).join('\n'));
    const bad = spawnSync(process.execPath, [script, '--tag', TAG, '--assets', join(dir, 'partial.txt'), '--sigs', dir], { encoding: 'utf8' });
    assert.equal(bad.status, 1);
    assert.equal(bad.stdout, '');
    assert.match(bad.stderr, /darwin-aarch64-app/);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
