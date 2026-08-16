import { Database } from "bun:sqlite";
import { chmodSync, existsSync, mkdirSync, renameSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
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
  {
    from: 1,
    to: 2,
    up(db) {
      db.exec(`
        CREATE TABLE handoffs (
          id TEXT PRIMARY KEY NOT NULL,
          task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
          checkpoint_seq INTEGER NOT NULL REFERENCES checkpoints(seq),
          reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
          status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
          from_agent TEXT NOT NULL,
          from_session TEXT,
          from_model TEXT,
          to_agent TEXT,
          summary TEXT NOT NULL,
          intent TEXT NOT NULL,
          next_action TEXT NOT NULL,
          blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
          validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),
          repo_path TEXT,
          branch TEXT,
          head_sha TEXT,
          dirty_summary TEXT,
          created_at INTEGER NOT NULL,
          accepted_at INTEGER,
          accepted_by TEXT,
          accepted_session TEXT
        ) STRICT;
        CREATE INDEX idx_handoffs_task_created ON handoffs(task_id, created_at);
        CREATE INDEX idx_handoffs_status_created ON handoffs(status, created_at);
      `);
    },
  },
  {
    from: 2,
    to: 3,
    up(db) {
      db.exec(`
        ALTER TABLE tasks ADD COLUMN assignee TEXT;
        ALTER TABLE tasks ADD COLUMN lane TEXT;
        ALTER TABLE tasks ADD COLUMN deliverable TEXT;
        ALTER TABLE tasks ADD COLUMN stale_minutes INTEGER CHECK(stale_minutes IS NULL OR stale_minutes >= 0);
        ALTER TABLE tasks ADD COLUMN driver_only INTEGER NOT NULL DEFAULT 0 CHECK(driver_only IN (0,1));
        CREATE INDEX idx_tasks_assignee_status ON tasks(assignee, status);
        CREATE INDEX idx_tasks_lane_status ON tasks(lane, status);
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
  {
    from: 1,
    to: 2,
    up(db) {
      db.exec(`
        CREATE TABLE workspace_aliases (
          root_path TEXT PRIMARY KEY NOT NULL,
          name TEXT NOT NULL,
          board_path TEXT NOT NULL,
          created_at INTEGER NOT NULL,
          last_used_at INTEGER NOT NULL
        ) STRICT;
        CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
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
  chmodSync(path, 0o600);
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

export function databaseIntegrity(db: Database): string[] {
  const rows = db.query("PRAGMA integrity_check").all() as Array<{ integrity_check: string }>;
  return rows.map((row) => row.integrity_check);
}

export function writeDatabaseSnapshot(db: Database, path: string): void {
  if (existsSync(path)) throw new Error(`backup destination already exists: ${path}`);
  mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${randomUUID()}.part`;
  writeFileSync(temporary, db.serialize(), { flag: "wx", mode: 0o600 });
  renameSync(temporary, path);
  chmodSync(path, 0o600);
}
