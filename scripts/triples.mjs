// The Rust targets the release ships. Checked before a CLI-supplied triple is put into a path,
// so an argument like "../../x" can never reach the file system.
export const KNOWN_TRIPLES = [
  'x86_64-pc-windows-msvc',
  'aarch64-pc-windows-msvc',
  'x86_64-apple-darwin',
  'aarch64-apple-darwin',
  'x86_64-unknown-linux-gnu',
  'aarch64-unknown-linux-gnu',
];

/**
 * The matching entry of KNOWN_TRIPLES, or undefined. Callers build paths from the returned
 * constant, never from the argument itself, so nothing typed on the command line reaches a path.
 */
export function knownTriple(triple) {
  return KNOWN_TRIPLES.find((known) => known === triple);
}

/** An error message when `triple` is not a shipped target, else null. */
export function tripleProblem(triple) {
  if (KNOWN_TRIPLES.includes(triple)) return null;
  return `unknown target triple ${JSON.stringify(triple)}; expected one of: ${KNOWN_TRIPLES.join(', ')}`;
}
