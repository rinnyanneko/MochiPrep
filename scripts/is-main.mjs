// True when the module at `importMetaUrl` is the script Node was started with. Both sides go
// through realpath: argv[1] keeps a symlinked, 8.3-short or differently-cased path as typed, and a
// plain string compare then reads "imported", so the script would exit 0 without doing anything.
import { realpathSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

export function isMain(importMetaUrl) {
  if (!process.argv[1]) return false;
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(importMetaUrl));
  } catch {
    return false;
  }
}
