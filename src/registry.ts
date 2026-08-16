import { randomUUID } from "node:crypto";
import { homedir } from "node:os";
import { basename, dirname, join, parse, resolve } from "node:path";
import { chmodSync, mkdirSync, realpathSync } from "node:fs";
import {
  closeDatabase,
  databaseIntegrity,
  openRegistryDatabase,
  writeDatabaseSnapshot,
} from "./db.js";

export interface WorkspaceRecord {
  rootPath: string;
  name: string;
  boardPath: string;
  canonical: boolean;
  createdAt: number;
  lastUsedAt: number;
}

export interface ProjectRecord {
  name: string;
  boardPath: string;
  canonicalRoot: string;
  workspaceRoots: string[];
  lastUsedAt: number;
}

interface WorkspaceRow {
  root_path: string;
  name: string;
  board_path: string;
  created_at: number;
  last_used_at: number;
}

interface WorkspaceAliasRow extends WorkspaceRow {}

export function dataRoot(env: NodeJS.ProcessEnv = process.env): string {
  return env.KANBAN_DATA_DIR
    ? resolve(env.KANBAN_DATA_DIR)
    : join(env.XDG_DATA_HOME ?? join(homedir(), ".local", "share"), "kanban");
}

function fromRow(row: WorkspaceRow, canonical: boolean): WorkspaceRecord {
  return {
    rootPath: row.root_path,
    name: row.name,
    boardPath: row.board_path,
    canonical,
    createdAt: row.created_at,
    lastUsedAt: row.last_used_at,
  };
}

export class Registry {
  private db;
  readonly root: string;

  constructor(root = dataRoot()) {
    this.root = resolve(root);
    mkdirSync(join(this.root, "boards"), { recursive: true, mode: 0o700 });
    chmodSync(this.root, 0o700);
    chmodSync(join(this.root, "boards"), 0o700);
    this.db = openRegistryDatabase(join(this.root, "registry.db"));
  }

  close(): void {
    closeDatabase(this.db);
  }

  integrityCheck(): string[] {
    return databaseIntegrity(this.db);
  }

  backup(path: string): void {
    writeDatabaseSnapshot(this.db, path);
  }

  register(workspace: string, name = basename(workspace), now = Date.now()): WorkspaceRecord {
    const rootPath = realpathSync(resolve(workspace));
    const existing = this.getExact(rootPath);
    if (existing && !existing.canonical) {
      this.db
        .query("UPDATE workspace_aliases SET name=?,last_used_at=? WHERE root_path=?")
        .run(name, now, rootPath);
      return this.getExact(rootPath)!;
    }
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
    if (row) return fromRow(row, true);
    const alias = this.db
      .query("SELECT * FROM workspace_aliases WHERE root_path = ?")
      .get(resolve(workspace)) as WorkspaceAliasRow | null;
    return alias ? fromRow(alias, false) : null;
  }

  resolve(workspace: string): WorkspaceRecord | null {
    let cursor = realpathSync(resolve(workspace));
    const filesystemRoot = parse(cursor).root;
    while (true) {
      const found = this.getExact(cursor);
      if (found) {
        const table = found.canonical ? "workspaces" : "workspace_aliases";
        this.db.query(`UPDATE ${table} SET last_used_at = ? WHERE root_path = ?`).run(Date.now(), found.rootPath);
        return found;
      }
      if (cursor === filesystemRoot) return null;
      const parent = dirname(cursor);
      if (parent === cursor) return null;
      cursor = parent;
    }
  }

  list(): WorkspaceRecord[] {
    const canonical = (
      this.db.query("SELECT * FROM workspaces").all() as WorkspaceRow[]
    ).map((row) => fromRow(row, true));
    const aliases = (
      this.db.query("SELECT * FROM workspace_aliases").all() as WorkspaceAliasRow[]
    ).map((row) => fromRow(row, false));
    return [...canonical, ...aliases].sort((a, b) => b.lastUsedAt - a.lastUsedAt);
  }

  attach(workspace: string, projectWorkspace: string, now = Date.now()): WorkspaceRecord {
    const rootPath = realpathSync(resolve(workspace));
    const project = this.resolve(projectWorkspace);
    if (!project) throw new Error(`no Kanban project contains ${resolve(projectWorkspace)}`);
    const existing = this.getExact(rootPath);
    if (existing?.boardPath !== project.boardPath) {
      if (existing) throw new Error(`${rootPath} is already attached to another Kanban project`);
    }
    if (existing) return existing;
    const canonical = this.db
      .query("SELECT * FROM workspaces WHERE board_path=?")
      .get(project.boardPath) as WorkspaceRow | null;
    if (!canonical) throw new Error(`project board ${project.boardPath} has no canonical workspace`);
    this.db
      .query(
        `INSERT INTO workspace_aliases(root_path,name,board_path,created_at,last_used_at)
         VALUES(?,?,?,?,?)`,
      )
      .run(rootPath, canonical.name, project.boardPath, now, now);
    return this.getExact(rootPath)!;
  }

  projects(): ProjectRecord[] {
    const canonical = this.db.query("SELECT * FROM workspaces").all() as WorkspaceRow[];
    const aliases = this.db.query("SELECT * FROM workspace_aliases").all() as WorkspaceAliasRow[];
    return canonical
      .map((project) => {
        const projectAliases = aliases.filter((alias) => alias.board_path === project.board_path);
        return {
          name: project.name,
          boardPath: project.board_path,
          canonicalRoot: project.root_path,
          workspaceRoots: [project.root_path, ...projectAliases.map((alias) => alias.root_path)],
          lastUsedAt: Math.max(project.last_used_at, ...projectAliases.map((alias) => alias.last_used_at)),
        };
      })
      .sort((a, b) => b.lastUsedAt - a.lastUsedAt);
  }
}
