use crate::db::{checkpoint, integrity, open_registry};
use crate::model::{ProjectRecord, WorkspaceRecord};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_millis() as i64
}

pub fn data_root() -> Result<PathBuf> {
    if let Some(value) = env::var_os("KANBAN_DATA_DIR") {
        return Ok(PathBuf::from(value));
    }
    if let Some(value) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(value).join("kanban"));
    }
    let home = env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/share/kanban"))
}

fn row(record: &rusqlite::Row<'_>, canonical: bool) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        root_path: record.get("root_path")?,
        name: record.get("name")?,
        board_path: record.get("board_path")?,
        canonical,
        created_at: record.get("created_at")?,
        last_used_at: record.get("last_used_at")?,
    })
}

pub struct Registry {
    pub connection: Connection,
    root: PathBuf,
}

impl Registry {
    pub fn open() -> Result<Self> {
        let root = data_root()?;
        fs::create_dir_all(&root)?;
        let connection = open_registry(&root.join("registry.db"))?;
        Ok(Self { connection, root })
    }

    pub fn register(&mut self, workspace: &Path, name: &str) -> Result<WorkspaceRecord> {
        let root_path = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        let root_text = root_path.to_string_lossy().into_owned();
        let now = now_ms();
        if let Some(existing) = self.exact(&root_path)? {
            let table = if existing.canonical {
                "workspaces"
            } else {
                "workspace_aliases"
            };
            self.connection.execute(
                &format!("UPDATE {table} SET name=?,last_used_at=? WHERE root_path=?"),
                params![name, now, root_text],
            )?;
            return self
                .exact(&root_path)?
                .context("registered workspace disappeared");
        }
        let board_path = self
            .root
            .join("boards")
            .join(format!("{}.db", Uuid::new_v4()));
        self.connection.execute(
            "INSERT INTO workspaces(root_path,name,board_path,created_at,last_used_at) VALUES(?,?,?,?,?)",
            params![root_text, name, board_path.to_string_lossy(), now, now],
        )?;
        self.exact(&root_path)?
            .context("registered workspace not found")
    }

    pub fn exact(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let text = workspace.to_string_lossy();
        let canonical = self
            .connection
            .query_row(
                "SELECT * FROM workspaces WHERE root_path=?",
                [text.as_ref()],
                |r| row(r, true),
            )
            .optional()?;
        if canonical.is_some() {
            return Ok(canonical);
        }
        self.connection
            .query_row(
                "SELECT * FROM workspace_aliases WHERE root_path=?",
                [text.as_ref()],
                |r| row(r, false),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn resolve(&mut self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        loop {
            if let Some(found) = self.exact(&cursor)? {
                let table = if found.canonical {
                    "workspaces"
                } else {
                    "workspace_aliases"
                };
                self.connection.execute(
                    &format!("UPDATE {table} SET last_used_at=? WHERE root_path=?"),
                    params![now_ms(), found.root_path],
                )?;
                return self.exact(&cursor);
            }
            if !cursor.pop() {
                return Ok(None);
            }
        }
    }

    pub fn attach(
        &mut self,
        workspace: &Path,
        project_workspace: &Path,
    ) -> Result<WorkspaceRecord> {
        let root = workspace.canonicalize()?;
        let project = self.resolve(project_workspace)?.with_context(|| {
            format!("no Kanban project contains {}", project_workspace.display())
        })?;
        if let Some(existing) = self.exact(&root)? {
            if existing.board_path != project.board_path {
                bail!(
                    "{} is already attached to another Kanban project",
                    root.display()
                );
            }
            return Ok(existing);
        }
        let canonical_name: String = self.connection.query_row(
            "SELECT name FROM workspaces WHERE board_path=?",
            [&project.board_path],
            |row| row.get(0),
        )?;
        let now = now_ms();
        self.connection.execute(
            "INSERT INTO workspace_aliases(root_path,name,board_path,created_at,last_used_at) VALUES(?,?,?,?,?)",
            params![root.to_string_lossy(), canonical_name, project.board_path, now, now],
        )?;
        self.exact(&root)?.context("attached workspace not found")
    }

    pub fn list(&self) -> Result<Vec<WorkspaceRecord>> {
        let mut out = Vec::new();
        for (sql, canonical) in [
            ("SELECT * FROM workspaces ORDER BY last_used_at DESC", true),
            (
                "SELECT * FROM workspace_aliases ORDER BY last_used_at DESC",
                false,
            ),
        ] {
            let mut statement = self.connection.prepare(sql)?;
            out.extend(
                statement
                    .query_map([], |r| row(r, canonical))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        out.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(out)
    }

    pub fn projects(&self) -> Result<Vec<ProjectRecord>> {
        let mut statement = self.connection.prepare("SELECT * FROM workspaces")?;
        let canonical = statement
            .query_map([], |r| row(r, true))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let aliases = self
            .list()?
            .into_iter()
            .filter(|r| !r.canonical)
            .collect::<Vec<_>>();
        let mut projects = canonical
            .into_iter()
            .map(|record| {
                let project_aliases = aliases.iter().filter(|a| a.board_path == record.board_path);
                let mut roots = vec![record.root_path.clone()];
                let mut last = record.last_used_at;
                for alias in project_aliases {
                    roots.push(alias.root_path.clone());
                    last = last.max(alias.last_used_at);
                }
                ProjectRecord {
                    name: record.name,
                    board_path: record.board_path,
                    canonical_root: record.root_path,
                    workspace_roots: roots,
                    last_used_at: last,
                }
            })
            .collect::<Vec<_>>();
        projects.sort_by_key(|item| std::cmp::Reverse(item.last_used_at));
        Ok(projects)
    }

    /// Projects carrying `name`. Names are not unique in the registry, so
    /// callers must handle 0 and >1 rather than assume a single hit.
    pub fn by_name(&self, name: &str) -> Result<Vec<ProjectRecord>> {
        Ok(self
            .projects()?
            .into_iter()
            .filter(|project| project.name == name)
            .collect())
    }

    /// Mark a board used. Path-based resolution updates `last_used_at` as a
    /// side effect of walking the registry; name-based resolution has no walk,
    /// so without this the dashboard's recency ordering would freeze for any
    /// project addressed only by name.
    pub fn touch_board(&self, board_path: &str) -> Result<()> {
        let now = now_ms();
        for table in ["workspaces", "workspace_aliases"] {
            self.connection.execute(
                &format!("UPDATE {table} SET last_used_at=? WHERE board_path=?"),
                params![now, board_path],
            )?;
        }
        Ok(())
    }

    pub fn integrity(&self) -> Result<Vec<String>> {
        integrity(&self.connection)
    }

    pub fn backup(&self, destination: &Path) -> Result<()> {
        if destination.exists() {
            bail!(
                "backup destination already exists: {}",
                destination.display()
            );
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut target = Connection::open(destination)?;
        let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
        fs::set_permissions(destination, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = checkpoint(&self.connection);
    }
}
