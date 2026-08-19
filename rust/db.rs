use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};
use std::fs::{self, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const BOARD_V1: &str = r#"
CREATE TABLE board_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
CREATE TABLE tasks (
 id TEXT PRIMARY KEY NOT NULL,type TEXT NOT NULL CHECK(type IN ('epic','story','task')),
 parent_id TEXT REFERENCES tasks(id),title TEXT NOT NULL,body TEXT,
 status TEXT NOT NULL CHECK(status IN ('backlog','todo','in_progress','blocked','review','done','cancelled')),
 priority INTEGER NOT NULL DEFAULT 3,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,
 completed_at INTEGER,metadata TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata))
) STRICT;
CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
CREATE INDEX idx_tasks_parent ON tasks(parent_id);
CREATE TABLE task_dependencies (
 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 PRIMARY KEY(task_id,depends_on),CHECK(task_id <> depends_on)
) STRICT;
CREATE TABLE task_notes (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL,created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id,seq);
CREATE TABLE task_claims (
 task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 agent_id TEXT NOT NULL,session_id TEXT,lease_token TEXT NOT NULL UNIQUE,
 claimed_at INTEGER NOT NULL,heartbeat_at INTEGER NOT NULL,expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_task_claims_expiry ON task_claims(expires_at);
CREATE TABLE checkpoints (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,session_id TEXT,model TEXT,state TEXT NOT NULL CHECK(state IN ('continue','blocked','done')),
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id,seq);
CREATE TABLE events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT,kind TEXT NOT NULL,actor TEXT,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_events_task_seq ON events(task_id,seq);
"#;

const BOARD_V2: &str = r#"
CREATE TABLE handoffs (
 id TEXT PRIMARY KEY NOT NULL,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 checkpoint_seq INTEGER NOT NULL REFERENCES checkpoints(seq),
 reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
 from_agent TEXT NOT NULL,from_session TEXT,from_model TEXT,to_agent TEXT,
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL,accepted_at INTEGER,
 accepted_by TEXT,accepted_session TEXT
) STRICT;
CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
"#;

const BOARD_V3: &str = r#"
ALTER TABLE tasks ADD COLUMN assignee TEXT;
ALTER TABLE tasks ADD COLUMN lane TEXT;
ALTER TABLE tasks ADD COLUMN deliverable TEXT;
ALTER TABLE tasks ADD COLUMN stale_minutes INTEGER CHECK(stale_minutes IS NULL OR stale_minutes >= 0);
ALTER TABLE tasks ADD COLUMN driver_only INTEGER NOT NULL DEFAULT 0 CHECK(driver_only IN (0,1));
CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
"#;

const REGISTRY_V1: &str = r#"
CREATE TABLE workspaces (
 root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
 created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
) STRICT;
"#;

const REGISTRY_V2: &str = r#"
CREATE TABLE workspace_aliases (
 root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
 created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
"#;

/// Create `dir` and any missing ancestors, each mode 0700.
///
/// Directories that already exist are left exactly as the operator set them.
/// Kanban must never re-permission a directory it does not own: the previous
/// unconditional `chmod 0700` on the parent meant `--db /tmp/board.db` locked
/// `/tmp` to the calling user, and as root that breaks every other process on
/// the host. Use [`own_private_dir`] for the one tree Kanban does own.
pub fn create_private_dir_all(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_private_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        // A concurrent kanban process won the race; its mode is ours.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create directory {}", dir.display())),
    }
}

/// Create `dir` if missing and assert mode 0700 on it. Only for the private
/// data root, which Kanban owns outright; never for an operator-supplied path.
pub fn own_private_dir(dir: &Path) -> Result<()> {
    create_private_dir_all(dir)?;
    fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("secure directory {}", dir.display()))
}

/// Create `path` with mode 0600 before anything can open it.
///
/// `Connection::open` creates with 0644 and a follow-up `chmod` leaves a window
/// in which any local user can open the file and keep the descriptor across the
/// narrowing. Boards hold private work state, so they are never briefly public.
fn create_private_file(path: &Path) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

fn open(path: &Path) -> Result<Connection> {
    let parent = path.parent().context("database path has no parent")?;
    create_private_dir_all(parent)?;
    create_private_file(path)?;
    let connection = Connection::open(path)
        .with_context(|| format!("open SQLite database {}", path.display()))?;
    // Re-assert for databases created before this rule, or by another tool.
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    // synchronous=FULL, not NORMAL: a checkpoint that survives the agent but
    // not the host is not durable, and this ledger exists to be resumed from.
    // Write volume is a handful of rows per model turn, so the extra fsync is
    // not a meaningful cost.
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;",
    )?;
    Ok(connection)
}

/// Open a fresh database file for a backup copy. Fails if the destination
/// exists: creation is the existence check, so no window separates the two.
pub fn create_backup_target(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| match error.kind() {
            ErrorKind::AlreadyExists => {
                anyhow::anyhow!("backup destination already exists: {}", path.display())
            }
            _ => anyhow::Error::new(error).context(format!("create {}", path.display())),
        })?;
    Connection::open(path).with_context(|| format!("open backup target {}", path.display()))
}

fn migrate(connection: &mut Connection, migrations: &[&str]) -> Result<()> {
    let mut current: usize = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > migrations.len() {
        bail!(
            "database version {current} is newer than supported version {}",
            migrations.len()
        );
    }
    while current < migrations.len() {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(migrations[current])?;
        transaction.pragma_update(None, "user_version", (current + 1) as i64)?;
        transaction.commit()?;
        current += 1;
    }
    Ok(())
}

pub fn open_board(path: &Path) -> Result<Connection> {
    let mut connection = open(path)?;
    migrate(&mut connection, &[BOARD_V1, BOARD_V2, BOARD_V3])?;
    Ok(connection)
}

pub fn open_registry(path: &Path) -> Result<Connection> {
    let mut connection = open(path)?;
    migrate(&mut connection, &[REGISTRY_V1, REGISTRY_V2])?;
    Ok(connection)
}

pub fn integrity(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn checkpoint(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

/// Integrity-check a database file without registering or migrating it.
/// Used to verify a snapshot before it is allowed to overwrite live state.
pub fn verify(path: &Path) -> Result<Vec<String>> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open snapshot {}", path.display()))?;
    integrity(&connection)
}

/// Put `source` in place of `destination`, atomically as far as readers are
/// concerned.
///
/// Copies to a temporary sibling first and renames over the target, so an
/// interrupted restore cannot leave a half-written board. Any `-wal`/`-shm`
/// belonging to the replaced database is removed: they describe the file that
/// just went away, and leaving them would let SQLite replay the old log over
/// the new contents.
pub fn replace_database(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        create_private_dir_all(parent)?;
    }
    let staging = destination.with_extension("db.restoring");
    let _ = fs::remove_file(&staging);
    fs::copy(source, &staging)
        .with_context(|| format!("copy {} to {}", source.display(), staging.display()))?;
    fs::set_permissions(&staging, Permissions::from_mode(0o600))?;
    fs::rename(&staging, destination)
        .with_context(|| format!("replace {}", destination.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = destination.as_os_str().to_owned();
        sidecar.push(suffix);
        let _ = fs::remove_file(Path::new(&sidecar));
    }
    Ok(())
}
