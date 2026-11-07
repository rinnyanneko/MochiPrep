import { chmodSync, existsSync, mkdirSync, readdirSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { DatabaseSync } from 'node:sqlite';

const MIGRATIONS_DIR = join(dirname(fileURLToPath(import.meta.url)), 'migrations');

/** Opens the database, restricts its permissions and applies pending migrations in order. */
export function openDatabase(path: string): DatabaseSync {
  const memory = path === ':memory:';
  if (!memory) {
    const dir = dirname(path);
    if (!existsSync(dir)) mkdirSync(dir, { recursive: true, mode: 0o700 });
  }
  const db = new DatabaseSync(path);
  db.exec('PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;');
  if (!memory) {
    for (const file of [path, `${path}-wal`, `${path}-shm`]) {
      if (existsSync(file)) chmodSync(file, 0o600);
    }
  }
  migrate(db);
  return db;
}

function migrate(db: DatabaseSync) {
  db.exec('CREATE TABLE IF NOT EXISTS schema_migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL)');
  const applied = new Set(
    (db.prepare('SELECT name FROM schema_migrations').all() as { name: string }[]).map((r) => r.name),
  );
  for (const file of readdirSync(MIGRATIONS_DIR).filter((f) => f.endsWith('.sql')).sort()) {
    if (applied.has(file)) continue;
    db.exec('BEGIN');
    try {
      db.exec(readFileSync(join(MIGRATIONS_DIR, file), 'utf8'));
      db.prepare('INSERT INTO schema_migrations (name, applied_at) VALUES (?, ?)').run(file, Date.now());
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
  }
}
