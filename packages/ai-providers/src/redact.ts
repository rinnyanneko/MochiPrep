/** Strips known keys plus anything key-shaped, for text bound for logs, errors or the database. */
export function redactSecrets(text: string, secrets: Iterable<string> = []): string {
  // Replacing one secret first could destroy a match for another and expose its remainder.
  // Collect even self-overlapping matches from the original text before replacing any span.
  const matches: Array<[number, number]> = [];
  for (const secret of secrets) {
    if (!secret) continue;
    // Provider diagnostics can echo credentials inside JSON strings.
    const escaped = JSON.stringify(secret).slice(1, -1);
    for (const needle of escaped === secret ? [secret] : [secret, escaped]) {
      for (let start = text.indexOf(needle); start !== -1; start = text.indexOf(needle, start + 1)) {
        matches.push([start, start + needle.length]);
      }
    }
  }
  matches.sort(([a], [b]) => a - b);
  const spans: Array<[number, number]> = [];
  for (const [start, end] of matches) {
    const previous = spans[spans.length - 1];
    if (previous && start < previous[1]) previous[1] = Math.max(previous[1], end);
    else spans.push([start, end]);
  }
  const chunks: string[] = [];
  let cursor = 0;
  for (const [start, end] of spans) {
    chunks.push(text.slice(cursor, start), '[REDACTED]');
    cursor = end;
  }
  chunks.push(text.slice(cursor));
  const out = chunks.join('');
  return out
    .replace(/\b(sk|pk|rk)-[A-Za-z0-9_-]{8,}/g, '[REDACTED]')
    .replace(/\bAIza[A-Za-z0-9_-]{16,}/g, '[REDACTED]')
    .replace(/(authorization|x-api-key|x-goog-api-key|api[_-]?key)(["':=\s]+)(bearer\s+)?[^\s"',}]+/gi, '$1$2[REDACTED]');
}
