#!/usr/bin/env node
// Refuses a release tag that does not match the Cargo workspace version, so a tag can never
// publish binaries whose own version (and updater manifest) says something else.
//
//   node scripts/check-version.mjs v0.1.0
import { readFileSync } from 'node:fs';
import { isMain } from './is-main.mjs';

/** The `version` under `[workspace.package]`, or null. */
export function workspaceVersion(toml) {
  const section = /^\[workspace\.package\]\s*$([\s\S]*?)(?=^\[|(?![\s\S]))/m.exec(toml);
  if (!section) return null;
  const match = /^\s*version\s*=\s*"([^"]+)"/m.exec(section[1]);
  return match ? match[1] : null;
}

/** An error message, or null when the tag is `v` + the workspace version. */
export function checkTag(tag, toml) {
  const version = workspaceVersion(toml);
  if (!version) return 'Cargo.toml has no [workspace.package] version.';
  if (tag !== `v${version}`) return `Tag ${tag} does not match the workspace version: expected v${version}.`;
  return null;
}

if (isMain(import.meta.url)) {
  const tag = process.argv[2];
  if (!tag) {
    console.error('usage: node scripts/check-version.mjs <tag>');
    process.exit(1);
  }
  const toml = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8');
  const problem = checkTag(tag, toml);
  if (problem) {
    console.error(problem);
    process.exit(1);
  }
  console.log(`Tag ${tag} matches the workspace version.`);
}
