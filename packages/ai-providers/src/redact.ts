/** Strips known keys plus anything key-shaped, for text bound for logs, errors or the database. */
export function redactSecrets(text: string, secrets: Iterable<string> = []): string {
  let out = text;
  for (const secret of secrets) {
    if (secret) out = out.split(secret).join('[REDACTED]');
  }
  return out
    .replace(/\b(sk|pk|rk)-[A-Za-z0-9_-]{8,}/g, '[REDACTED]')
    .replace(/\bAIza[A-Za-z0-9_-]{16,}/g, '[REDACTED]')
    .replace(/(authorization|x-api-key|x-goog-api-key|api[_-]?key)(["':=\s]+)(bearer\s+)?[^\s"',}]+/gi, '$1$2[REDACTED]');
}
