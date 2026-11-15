import { mkdtempSync, rmSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { openDatabase } from '../src/db/database';
import { SqliteStore } from '../src/db/store';

describe('SqliteStore.scrub', () => {
  let dir: string;
  let path: string;
  let db: DatabaseSync;
  let store: SqliteStore;

  beforeEach(() => {
    dir = mkdtempSync(join(tmpdir(), 'mochi-scrub-'));
    path = join(dir, 'test.sqlite');
    db = openDatabase(path);
    db.exec('PRAGMA busy_timeout=0; CREATE TABLE scrub_probe (value TEXT)');
    store = new SqliteStore(db);
  });

  afterEach(() => {
    db.close();
    rmSync(dir, { recursive: true, force: true });
  });

  it('returns false and truncates an unblocked WAL', () => {
    db.exec("INSERT INTO scrub_probe VALUES ('synthetic')");
    expect(statSync(path + '-wal').size).toBeGreaterThan(0);
    expect(store.scrub()).toBe(false);
    expect(statSync(path + '-wal').size).toBe(0);
    expect(db.prepare('SELECT value FROM scrub_probe').get()).toMatchObject({ value: 'synthetic' });
  });

  it('reports a pinned reader and succeeds after that reader releases its snapshot', () => {
    db.exec("INSERT INTO scrub_probe VALUES ('before')");
    const reader = new DatabaseSync(path);
    try {
      reader.exec('BEGIN');
      reader.prepare('SELECT * FROM scrub_probe').all();
      db.exec("INSERT INTO scrub_probe VALUES ('after')");
      expect(store.scrub()).toBe(true);
      expect(statSync(path + '-wal').size).toBeGreaterThan(0);
      reader.exec('ROLLBACK');
      expect(store.scrub()).toBe(false);
      expect(statSync(path + '-wal').size).toBe(0);
      expect(db.prepare('SELECT COUNT(*) AS count FROM scrub_probe').get()).toMatchObject({ count: 2 });
    } finally {
      reader.close();
    }
  });

  it('defers inside nested transactions without disturbing commit or rollback', () => {
    store.transaction(() => {
      db.exec("INSERT INTO scrub_probe VALUES ('committed')");
      expect(store.scrub()).toBe(true);
      expect(() => store.transaction(() => {
        db.exec("INSERT INTO scrub_probe VALUES ('rolled back')");
        expect(store.scrub()).toBe(true);
        throw new Error('rollback probe');
      })).toThrow('rollback probe');
      expect(store.scrub()).toBe(true);
    });
    expect(store.scrub()).toBe(false);
    expect(db.prepare('SELECT value FROM scrub_probe').all()).toEqual([{ value: 'committed' }]);
  });
});
