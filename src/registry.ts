import { randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { basename, dirname, join, parse, resolve } from "node:path";
import { realpathSync } from "node:fs";
import { closeDatabase, openRegistryDatabase } from "./db.js";

export interface WorkspaceRecord {
  rootPath: string;
  name: string;
  boardPath: string;
  createdAt: number;
  lastUsedAt: number;
}

interface WorkspaceRow {
  root_path: string;
  name: string;
  board_path: string;
  created_at: number;
  last_used_at: number;
}

export function dataRoot(env: NodeJS.ProcessEnv = process.env): string {
  return env.KANBAN_DATA_DIR
    ? resolve(env.KANBAN_DATA_DIR)
    : join(env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"), "kanban");
}

function fromRow(row: WorkspaceRow): WorkspaceRecord {
  return {
    rootPath: row.root_path,
    name: row.name,
    boardPath: row.board_path,
    createdAt: row.created_at,
    lastUsedAt: row.last_used_at,
  };
}

export class Registry {
  private db;
  readonly root: string;

  constructor(root = dataRoot()) {
    this.root = resolve(root);
    this.db = openRegistryDatabase(join(this.root, "registry.db"));
  }

  close(): void {
    closeDatabase(this.db);
  }

  register(workspace: string, name = basename(workspace), now = Date.now()): WorkspaceRecord {
    const rootPath = realpathSync(resolve(workspace));
    const existing = this.getExact(rootPath);
    const boardPath = existing?.boardPath ?? join(this.root, "boards", `${randomUUID()}.db`);
    this.db
      .query(
        `INSERT INTO workspaces(root_path,name,board_path,created_at,last_used_at)
         VALUES(?,?,?,?,?)
         ON CONFLICT(root_path) DO UPDATE SET name=excluded.name,last_used_at=excluded.last_used_at`,
      )
      .run(rootPath, name, boardPath, existing?.createdAt ?? now, now);
    return this.getExact(rootPath)!;
  }

  getExact(workspace: string): WorkspaceRecord | null {
    const row = this.db
      .query("SELECT * FROM workspaces WHERE root_path = ?")
      .get(resolve(workspace)) as WorkspaceRow | null;
    return row ? fromRow(row) : null;
  }

  resolve(workspace: string): WorkspaceRecord | null {
    let cursor = realpathSync(resolve(workspace));
    const filesystemRoot = parse(cursor).root;
    while (true) {
      const found = this.getExact(cursor);
      if (found) {
        this.db
          .query("UPDATE workspaces SET last_used_at = ? WHERE root_path = ?")
          .run(Date.now(), found.rootPath);
        return found;
      }
      if (cursor === filesystemRoot) return null;
      const parent = dirname(cursor);
      if (parent === cursor) return null;
      cursor = parent;
    }
  }

  list(): WorkspaceRecord[] {
    return (
      this.db.query("SELECT * FROM workspaces ORDER BY last_used_at DESC").all() as WorkspaceRow[]
    ).map(fromRow);
  }
}
