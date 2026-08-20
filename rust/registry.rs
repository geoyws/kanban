use crate::db::{checkpoint, create_backup_target, integrity, open_registry, own_private_dir};
use crate::model::{ProjectRecord, WorkspaceRecord};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use std::env;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Milliseconds since the Unix epoch.
///
/// Infallible by construction rather than by `expect`: a panic here would
/// abort mid-command with a Rust backtrace instead of an error an agent can
/// read, and every caller writes the result into a durable record. The clamps
/// are unreachable in practice because [`require_sane_clock`] refuses the run
/// before any command gets this far — they exist so the function has no panic
/// path at all, not as a fallback anybody should lean on.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, millis_of)
}

/// `Duration` to milliseconds, saturating rather than wrapping.
///
/// `as i64` on the `u128` this comes from truncates the high bits, so a clock
/// far enough in the future produced a small or negative "now" — an ordering
/// error rather than an obvious one.
fn millis_of(elapsed: std::time::Duration) -> i64 {
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

/// Refuse to run on a clock that cannot produce a timestamp.
///
/// Every task, note, checkpoint, event and lease is stamped, and leases expire
/// by comparing stamps, so a clock before the epoch does not degrade the
/// ledger — it makes every ordering in it meaningless. Saying so once, up
/// front, beats writing records that are wrong in a way nothing later can
/// detect.
pub fn require_sane_clock() -> Result<()> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            anyhow::anyhow!(
                "the system clock reads before 1970-01-01; kanban stamps every record it writes \
                 and orders leases by those stamps, so it will not write a time it cannot compute"
            )
        })?;
    Ok(())
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
        own_private_dir(&root)?;
        let connection = open_registry(&root.join("registry.db"))?;
        Ok(Self { connection, root })
    }

    pub fn register(
        &mut self,
        workspace: &Path,
        name: &str,
        force: bool,
    ) -> Result<WorkspaceRecord> {
        let root_path = workspace
            .canonicalize()
            .with_context(|| format!("resolve workspace {}", workspace.display()))?;
        let root_text = root_path.to_string_lossy().into_owned();
        let now = now_ms();
        // An init below a registered root used to create a second board that
        // shadowed the first: tasks added from the subdirectory resolved to the
        // nearer board and were invisible from the project root. Attaching is
        // almost always what was meant; nesting has to be asked for.
        if !force
            && self.exact(&root_path)?.is_none()
            && let Some(enclosing) = self.enclosing(&root_path)?
        {
            bail!(
                "{} is already inside Kanban project {} ({}).\n\
                 To share that project's board:   kanban workspace attach --to {}\n\
                 To create a separate board here: kanban init --name {name} --force",
                root_path.display(),
                enclosing.name,
                enclosing.root_path,
                enclosing.root_path
            );
        }
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

    /// The nearest registered workspace strictly above `workspace`, if any.
    /// Read-only: unlike `resolve`, it does not touch `last_used_at`.
    fn enclosing(&self, workspace: &Path) -> Result<Option<WorkspaceRecord>> {
        let mut cursor = workspace.to_path_buf();
        while cursor.pop() {
            if let Some(found) = self.exact(&cursor)? {
                return Ok(Some(found));
            }
        }
        Ok(None)
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
        let mut target = create_backup_target(destination)?;
        let backup = rusqlite::backup::Backup::new(&self.connection, &mut target)?;
        backup.run_to_completion(64, std::time::Duration::from_millis(1), None)?;
        Ok(())
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        let _ = checkpoint(&self.connection);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn millis_saturate_instead_of_wrapping() {
        assert_eq!(millis_of(Duration::from_millis(1_234)), 1_234);
        assert_eq!(millis_of(Duration::ZERO), 0);
        // The old `as i64` truncated the high bits of the u128 here, so an
        // absurd clock read as a small — or negative — instant.
        assert_eq!(millis_of(Duration::MAX), i64::MAX);
        assert!(millis_of(Duration::MAX) > 0);
    }

    #[test]
    fn a_working_clock_is_accepted_and_produces_a_positive_now() {
        // The refusal branch needs a system clock set before 1970, which this
        // test will not do to the host it runs on. What is asserted here is
        // that a sane clock is not refused, and that `now_ms` has no panic
        // path left: `millis_saturate_instead_of_wrapping` covers the
        // conversion that used to be the unsound part.
        assert!(require_sane_clock().is_ok());
        assert!(now_ms() > 1_700_000_000_000, "now_ms went backwards");
    }
}
