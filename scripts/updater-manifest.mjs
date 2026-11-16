#!/usr/bin/env node
// Writes the updater manifest (`latest.json`) for one draft release, once, after every build leg
// has uploaded. The legs used to merge it themselves and could lose each other's platforms.
//
//   node scripts/updater-manifest.mjs --tag vX.Y.Z --assets <names.txt> --sigs <dir> [--out latest.json]
//
// <names.txt> holds the release's asset names, one per line; <dir> holds each `.sig` asset under
// its own name. The launcher reads the manifest from
// https://github.com/rinnyanneko/MochiPrep/releases/latest/download/latest.json.
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { isMain } from './is-main.mjs';

export const RELEASES_URL = 'https://github.com/rinnyanneko/MochiPrep/releases';
const PRODUCT = 'MochiPrep';

/**
 * The updater bundles a release must hold, by updater-plugin platform.
 *
 * Names are the ones the Tauri CLI 2.12.4 bundler writes with `createUpdaterArtifacts: true`
 * (NSIS `<product>_<version>_<x64|arm64>-setup.exe`, MSI `<product>_<version>_<x64|arm64>_en-US.msi`,
 * AppImage `<product>_<version>_<amd64|aarch64>.AppImage`), except the macOS archive: the bundler
 * writes `<product>.app.tar.gz` for both architectures, and tauri-action v0.6.2 uploads it as
 * `<product>_<x64|aarch64>.app.tar.gz` so the two legs do not overwrite each other.
 */
export function updaterBundles(version) {
  return [
    { os: 'windows', arch: 'x86_64', kind: 'nsis', asset: `${PRODUCT}_${version}_x64-setup.exe` },
    { os: 'windows', arch: 'x86_64', kind: 'msi', asset: `${PRODUCT}_${version}_x64_en-US.msi` },
    { os: 'windows', arch: 'aarch64', kind: 'nsis', asset: `${PRODUCT}_${version}_arm64-setup.exe` },
    { os: 'windows', arch: 'aarch64', kind: 'msi', asset: `${PRODUCT}_${version}_arm64_en-US.msi` },
    { os: 'darwin', arch: 'x86_64', kind: 'app', asset: `${PRODUCT}_x64.app.tar.gz` },
    { os: 'darwin', arch: 'aarch64', kind: 'app', asset: `${PRODUCT}_aarch64.app.tar.gz` },
    { os: 'linux', arch: 'x86_64', kind: 'appimage', asset: `${PRODUCT}_${version}_amd64.AppImage` },
    { os: 'linux', arch: 'aarch64', kind: 'appimage', asset: `${PRODUCT}_${version}_aarch64.AppImage` },
  ];
}

/**
 * The version a tag releases. Only plain X.Y.Z: the MSI bundler refuses pre-release versions,
 * so a pre-release tag could never carry the Windows MSI updater bundle anyway.
 */
export function versionOfTag(tag) {
  const match = /^v(\d+\.\d+\.\d+)$/.exec(tag ?? '');
  if (!match) throw new Error(`tag ${JSON.stringify(tag)} is not vX.Y.Z`);
  return match[1];
}

/** The `.sig` file's content as the plugin wants it: base64 of a minisign signature box. */
function checkedSignature(name, content) {
  const signature = String(content).trim();
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(signature)) throw new Error(`${name} is not base64`);
  if (!Buffer.from(signature, 'base64').toString('utf8').startsWith('untrusted comment:')) {
    throw new Error(`${name} does not decode to a minisign signature`);
  }
  return signature;
}

/**
 * The manifest for `tag`, from the release's asset names and the contents of its `.sig` assets.
 * Throws one error naming every platform whose bundle or signature is missing, so a partial
 * release never gets a manifest that silently leaves a platform without updates.
 */
export function buildManifest({ tag, assets, signatures, pubDate = new Date(), notes }) {
  const version = versionOfTag(tag);
  const present = new Set(assets);
  const missing = [];
  const entries = new Map();
  for (const bundle of updaterBundles(version)) {
    const platform = `${bundle.os}-${bundle.arch}-${bundle.kind}`;
    const sigName = `${bundle.asset}.sig`;
    const gaps = [bundle.asset, sigName].filter((name) => !present.has(name));
    if (!gaps.length && signatures[sigName] === undefined) gaps.push(`${sigName} (content)`);
    if (gaps.length) {
      missing.push(`${platform}: ${gaps.join(', ')}`);
      continue;
    }
    // A tag URL, never the draft's `untagged-…` one, which stops working once the owner publishes.
    const url = `${RELEASES_URL}/download/${encodeURIComponent(tag)}/${encodeURIComponent(bundle.asset)}`;
    entries.set(platform, { signature: checkedSignature(sigName, signatures[sigName]), url });
  }
  if (missing.length) throw new Error(`release ${tag} is missing updater bundles:\n  ${missing.join('\n  ')}`);

  const platforms = {};
  for (const arch of ['x86_64', 'aarch64']) {
    const nsis = entries.get(`windows-${arch}-nsis`);
    const app = entries.get(`darwin-${arch}-app`);
    const appimage = entries.get(`linux-${arch}-appimage`);
    // The plugin tries `<os>-<arch>-<installer>` first, then `<os>-<arch>`. The bare Windows key
    // is the NSIS bundle, as tauri-action's `updaterJsonPreferNsis` made it before.
    platforms[`windows-${arch}`] = nsis;
    platforms[`windows-${arch}-nsis`] = nsis;
    platforms[`windows-${arch}-msi`] = entries.get(`windows-${arch}-msi`);
    platforms[`darwin-${arch}`] = app;
    platforms[`darwin-${arch}-app`] = app;
    platforms[`linux-${arch}`] = appimage;
    platforms[`linux-${arch}-appimage`] = appimage;
    // A `.deb` install is package-managed: the launcher only reads the version from the manifest
    // and opens the release page, never installs. The key points at the AppImage so the check
    // finds this version, without publishing a `.deb` URL the plugin would `dpkg -i` as root.
    platforms[`linux-${arch}-deb`] = appimage;
  }

  for (const [key, entry] of Object.entries(platforms)) {
    if (entry.url.includes('untagged-')) throw new Error(`${key} has a draft (untagged) URL: ${entry.url}`);
  }
  return {
    version,
    notes: notes ?? `MochiPrep ${tag}: see ${RELEASES_URL}/tag/${tag}`,
    pub_date: pubDate.toISOString(),
    platforms,
  };
}

function parseArgs(argv) {
  const options = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!['--tag', '--assets', '--sigs', '--out'].includes(arg) || argv[i + 1] === undefined) {
      throw new Error(`unexpected argument ${arg}`);
    }
    i += 1;
    options[arg.slice(2)] = argv[i];
  }
  for (const required of ['tag', 'assets', 'sigs']) {
    if (!options[required]) throw new Error(`--${required} is required`);
  }
  return options;
}

if (isMain(import.meta.url)) {
  try {
    const options = parseArgs(process.argv.slice(2));
    const assets = readFileSync(options.assets, 'utf8').split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
    const signatures = {};
    for (const name of assets.filter((asset) => asset.endsWith('.sig'))) {
      signatures[name] = readFileSync(path.join(options.sigs, name), 'utf8');
    }
    const json = `${JSON.stringify(buildManifest({ tag: options.tag, assets, signatures }), null, 2)}\n`;
    if (options.out) writeFileSync(options.out, json);
    else process.stdout.write(json);
  } catch (error) {
    console.error(`updater-manifest: ${error.message}`);
    process.exit(1);
  }
}
