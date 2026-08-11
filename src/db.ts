import { Database } from "bun:sqlite";
import { mkdirSync } from "node:fs";
import { dirname } from "node:path";

export interface Migration {
  from: number;
  to: number;
  up(db: Database): void;
}

const BOARD_MIGRATIONS: readonly Migration[] = [
  {
    from: 0,
    to: 1,
    up(db) {
      db.exec(`
        CREATE TABLE board_meta (
          key TEXT PRIMARY KEY NOT NULL,
          value TEXT NOT NULL
        ) STRICT;

        CREATE TABLE tasks (
          id TEXT PRIMARY KEY NOT NULL,
          type TEXT NOT NULL CHECK(type IN ('epic','story','task')),
          parent_id TEXT REFERENCES tasks(id),
          title TEXT NOT NULL,
          body TEXT,
          status TEXT NOT NULL CHECK(status IN ('backlog','todo','in_progress','blocked','review','done','cancelled')),
          priority INTEGER NOT NULL DEFAULT 3,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL,
          completed_at INTEGER,
          metadata TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata))
        ) STRICT;
        CREATE INDEX idx_tasks_status_priority ON tasks(status, priority, created_at);
        CREATE INDEX idx_tasks_parent ON tasks(parent_id);

        CREATE TABLE task_dependencies (
          task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          PRIMARY KEY (task_id, depends_on),
          CHECK(task_id <> depends_on)
        ) STRICT;

        CREATE TABLE task_notes (
          seq INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          author TEXT NOT NULL,
          kind TEXT NOT NULL,
          body TEXT NOT NULL,
          created_at INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id, seq);

        CREATE TABLE task_claims (
          task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          agent_id TEXT NOT NULL,
          session_id TEXT,
          lease_token TEXT NOT NULL UNIQUE,
          claimed_at INTEGER NOT NULL,
          heartbeat_at INTEGER NOT NULL,
          expires_at INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX idx_task_claims_expiry ON task_claims(expires_at);

        CREATE TABLE checkpoints (
          seq INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          author TEXT NOT NULL,
          session_id TEXT,
          model TEXT,
          state TEXT NOT NULL CHECK(state IN ('continue','blocked','done')),
          summary TEXT NOT NULL,
          intent TEXT NOT NULL,
          next_action TEXT NOT NULL,
          blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
          validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),
          repo_path TEXT,
          branch TEXT,
          head_sha TEXT,
          dirty_summary TEXT,
          created_at INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id, seq);

        CREATE TABLE events (
          seq INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id TEXT,
          kind TEXT NOT NULL,
          actor TEXT,
          payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
          created_at INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX idx_events_task_seq ON events(task_id, seq);
      `);
    },
  },
];

const REGISTRY_MIGRATIONS: readonly Migration[] = [
  {
    from: 0,
    to: 1,
    up(db) {
      db.exec(`
        CREATE TABLE workspaces (
          root_path TEXT PRIMARY KEY NOT NULL,
          name TEXT NOT NULL,
          board_path TEXT NOT NULL UNIQUE,
          created_at INTEGER NOT NULL,
          last_used_at INTEGER NOT NULL
        ) STRICT;
      `);
    },
  },
];

function migrate(db: Database, migrations: readonly Migration[]): void {
  const row = db.query("PRAGMA user_version").get() as { user_version: number } | null;
  let current = row?.user_version ?? 0;
  for (const migration of migrations) {
    if (migration.from < current) continue;
    if (migration.from !== current) {
      throw new Error(
        `migration gap: database is at ${current}, next migration starts at ${migration.from}`,
      );
    }
    db.transaction(() => {
      migration.up(db);
      db.exec(`PRAGMA user_version = ${migration.to}`);
    }).immediate();
    current = migration.to;
  }
}

function open(path: string, migrations: readonly Migration[]): Database {
  mkdirSync(dirname(path), { recursive: true });
  const db = new Database(path, { create: true, strict: true });
  db.exec("PRAGMA journal_mode = WAL");
  db.exec("PRAGMA synchronous = NORMAL");
  db.exec("PRAGMA busy_timeout = 5000");
  db.exec("PRAGMA foreign_keys = ON");
  migrate(db, migrations);
  return db;
}

export function openBoardDatabase(path: string): Database {
  return open(path, BOARD_MIGRATIONS);
}

export function openRegistryDatabase(path: string): Database {
  return open(path, REGISTRY_MIGRATIONS);
}

export function closeDatabase(db: Database): void {
  db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
  db.close();
}
