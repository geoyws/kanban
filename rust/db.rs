use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior};
use std::cell::Cell;
use std::fs::{self, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// A handoff no longer has to be about a task.
///
/// `task_id` and `checkpoint_seq` were both NOT NULL, so the only handoff that
/// could exist was one taken over a claimed task — the record of a lease
/// changing hands. There was nowhere to put the other kind: what a session
/// itself learned, spanning several tasks or none, which is the thing a
/// successor most needs and the thing no task owns.
///
/// SQLite cannot relax NOT NULL in place, so the table is rebuilt. At creation
/// the two columns move together — a handoff is about a task and carries the
/// checkpoint that closed it, or it is about neither — and `create_handoff`
/// enforces that, because it is a rule about how a handoff is made.
///
/// It is deliberately not a CHECK, because it must not hold forever. Both
/// columns are now `ON DELETE SET NULL` rather than `CASCADE`: removing a task
/// used to delete every handoff ever taken over it, so the record of who
/// handed what to whom vanished with the row it described. A handoff is a
/// historical account of a handover that happened, and deleting the subject
/// does not un-happen it. The links are dropped and the account survives.
///
/// The rebuild starts by creating the *old* shape if it is missing. A v3 board
/// written by the retired TypeScript implementation can lack tables entirely,
/// so there may be nothing to copy from; standing the old shape up first means
/// one path handles both, and such a board ends the migration with the table it
/// should have had. The columns are listed in the original order because the
/// copy is positional.
const BOARD_V4: &str = r#"
CREATE TABLE IF NOT EXISTS handoffs (
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
CREATE TABLE handoffs_next (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 checkpoint_seq INTEGER REFERENCES checkpoints(seq) ON DELETE SET NULL,
 reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
 from_agent TEXT NOT NULL,from_session TEXT,from_model TEXT,to_agent TEXT,
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL,accepted_at INTEGER,
 accepted_by TEXT,accepted_session TEXT
) STRICT;
INSERT INTO handoffs_next SELECT * FROM handoffs;
DROP TABLE handoffs;
ALTER TABLE handoffs_next RENAME TO handoffs;
CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
"#;

/// Things that need the operator, kept where the work is.
///
/// Agents surface blockers, decisions and approvals in whatever they happen to
/// be writing — a report, a commit message, a chat reply — and every one of
/// those is a channel that scrolls away. An item raised at 03:00 and never
/// acted on leaves no trace that it was ever raised, so the same question gets
/// asked again three sessions later, or worse, quietly answered by an agent
/// that should not have decided it.
///
/// This is the durable place for them. Rows are resolved, never deleted, so
/// what was asked, by whom, when, and how it was settled stays on the board —
/// which is the point: the trail is the feature, not a side effect of storing
/// them.
///
/// `task_id` is optional and `ON DELETE SET NULL` for the same reason handoffs
/// are: an item may be about the session rather than one row, and removing the
/// row it referred to does not un-ask the question.
const BOARD_V5: &str = r#"
CREATE TABLE attention (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 kind TEXT NOT NULL CHECK(kind IN ('blocking','decision','approval','review','risk')),
 body TEXT NOT NULL,
 raised_by TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('open','resolved')),
 resolved_at INTEGER,resolved_by TEXT,resolution TEXT,
 CHECK((status = 'open') = (resolved_at IS NULL))
) STRICT;
CREATE INDEX idx_attention_status_created ON attention(status,created_at);
CREATE INDEX idx_attention_task ON attention(task_id);
"#;

/// A task that is not finished being written.
///
/// `backlog` already meant "real work, not scheduled yet". There was nothing
/// for the state before that: a row someone is still drafting, whose title,
/// body or scope may still be wrong. Agents treat every row on the board as a
/// specification, so an unfinished one gets decomposed, depended on, and worked
/// as though it were settled — and the ledger said nothing to stop it.
///
/// `draft` is that state. It is not claimable, which needs no new rule: `claim`
/// already accepts only `todo` and `in_progress`, and `--next` selects `todo`
/// alone. Widening the CHECK is the whole schema change.
///
/// SQLite cannot alter a CHECK in place, so the table is rebuilt. Foreign keys
/// are already off across the ladder (see `open_board`), which is what lets the
/// referencing tables survive the drop.
///
/// The rebuilt table reproduces the original exactly apart from the widened
/// CHECK — same `DEFAULT 3` on priority, same four indexes, and `parent_id`
/// still `REFERENCES tasks(id)` with no `ON DELETE` clause. That last one is
/// load-bearing: removal is meant to fail and name the children, and giving it
/// `SET NULL` here would silently orphan them instead. A rebuild is the easiest
/// place in a schema to change something nobody asked to change.
const BOARD_V6: &str = r#"
CREATE TABLE tasks_next (
 id TEXT PRIMARY KEY NOT NULL,type TEXT NOT NULL CHECK(type IN ('epic','story','task')),
 parent_id TEXT REFERENCES tasks(id),title TEXT NOT NULL,body TEXT,
 status TEXT NOT NULL CHECK(status IN ('draft','backlog','todo','in_progress','blocked','review','done','cancelled')),
 priority INTEGER NOT NULL DEFAULT 3,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,
 completed_at INTEGER,metadata TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata)),
 assignee TEXT,lane TEXT,deliverable TEXT,
 stale_minutes INTEGER CHECK(stale_minutes IS NULL OR stale_minutes >= 0),
 driver_only INTEGER NOT NULL DEFAULT 0 CHECK(driver_only IN (0,1))
) STRICT;
INSERT INTO tasks_next(id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata,assignee,lane,deliverable,stale_minutes,driver_only)
 SELECT id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata,assignee,lane,deliverable,stale_minutes,driver_only FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_next RENAME TO tasks;
CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
CREATE INDEX idx_tasks_parent ON tasks(parent_id);
CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
"#;

/// Where work happened, recorded on the rows that describe work happening.
///
/// A claim said who held a task and until when, and nothing about where they
/// were holding it. On a box running several driver lanes of the same
/// repository that is the first question anyone asks — which worktree is this
/// lane in — and the ledger could not answer it.
///
/// Checkpoints and handoffs already had `repo_path`, `branch` and `head_sha`.
/// They gain `root_head`: a submodule's own commit says nothing about which
/// revision of the whole tree it belonged to, and for a nested checkout the
/// answer to "what was checked out" is the outermost superproject's commit.
///
/// Every column is nullable. Provenance is captured when the caller is standing
/// in a repository and recorded as absent when they are not, which is the
/// truthful outcome rather than a failure.
///
/// The two `CREATE TABLE IF NOT EXISTS` are for the same reason V4 carries one:
/// a v3 board written by the retired TypeScript implementation can lack tables
/// outright, and `ALTER TABLE` has no `IF EXISTS`. Standing the original shape
/// up first means one path handles both, and such a board finishes the ladder
/// with the tables it should have had.
const BOARD_V7: &str = r#"
CREATE TABLE IF NOT EXISTS task_claims (
 task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 agent_id TEXT NOT NULL,session_id TEXT,lease_token TEXT NOT NULL UNIQUE,
 claimed_at INTEGER NOT NULL,heartbeat_at INTEGER NOT NULL,expires_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS checkpoints (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,session_id TEXT,model TEXT,state TEXT NOT NULL CHECK(state IN ('continue','blocked','done')),
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL
) STRICT;
ALTER TABLE task_claims ADD COLUMN worktree TEXT;
ALTER TABLE task_claims ADD COLUMN worktree_kind TEXT;
ALTER TABLE task_claims ADD COLUMN branch TEXT;
ALTER TABLE task_claims ADD COLUMN head_sha TEXT;
ALTER TABLE task_claims ADD COLUMN root_head TEXT;
ALTER TABLE checkpoints ADD COLUMN root_head TEXT;
ALTER TABLE handoffs ADD COLUMN root_head TEXT;
"#;

/// Tags, and the master file that says which ones exist.
///
/// A board needs an axis that says *what a row is about* — `infra`, `queuer`,
/// `askie` — and had none. `lane` is the nearest field and is the wrong one: it
/// carries the *kind of work* (`fe`, `be`, `ops`, `test`, `review`) and
/// `claim --next` routes on it, so overloading it with subsystems would both
/// lose a distinction and change who gets handed what. `metadata` is free JSON
/// and would make every tag a spelling.
///
/// So tags are registered before they are used. That is the difference between
/// a vocabulary and a habit: without a master file, `infra`, `Infra` and
/// `infrastructure` all exist, nothing says which is meant, and every filter
/// silently misses rows. Applying an unregistered tag is refused, with the
/// nearest registered name suggested and the command that would add it — the
/// same shape as a mistyped flag.
///
/// `ON DELETE RESTRICT` on the tag reference is what makes the master file
/// authoritative: a tag in use cannot quietly disappear from under the rows
/// carrying it. Removing one names how many rows would be orphaned and requires
/// `--force`, like every other destructive path here.
const BOARD_V8: &str = r#"
CREATE TABLE tags (
 name TEXT PRIMARY KEY NOT NULL,
 description TEXT,
 created_by TEXT,
 created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE task_tags (
 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 tag TEXT NOT NULL REFERENCES tags(name) ON DELETE RESTRICT,
 PRIMARY KEY(task_id,tag)
) STRICT;
CREATE INDEX idx_task_tags_tag ON task_tags(tag);
"#;

/// Status updates: the low-ceremony sibling of a handoff.
///
/// Keyed to a **lane**, not a task, which is the whole point. A note needs a
/// task; a checkpoint needs a task *and* a lease. An agent working across
/// several tasks, or between them, or exploring before it claims anything, had
/// nowhere to write down where things stand — so it went in a reply that
/// scrolls away, or it waited for a handoff nobody had time to write.
///
/// `archived` is set by the writer, not by a sweep: posting an update archives
/// what it superseded, so the current view is bounded without anything having
/// to run on a timer.
const BOARD_V9: &str = r#"
CREATE TABLE status_updates (
 id TEXT PRIMARY KEY NOT NULL,
 lane TEXT NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 author TEXT NOT NULL,
 body TEXT NOT NULL,
 worktree TEXT,
 branch TEXT,
 head_sha TEXT,
 root_head TEXT,
 dirty_summary TEXT,
 archived INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_status_lane_created ON status_updates(lane,archived,created_at DESC);
"#;

/// Rename the one-day-old status-update surface to the unambiguous `sitrep`.
///
/// V9 had reached live boards, so this is a data-preserving rename rather than
/// an edit to the old migration. Its `u-` ids and event trail are rewritten as
/// well because every existing row was still a probe. A mature table with real
/// history would keep its old ids and accept that seam instead.
const BOARD_V10: &str = r#"
ALTER TABLE status_updates RENAME TO sitreps;
DROP INDEX IF EXISTS idx_status_lane_created;
CREATE INDEX idx_sitreps_lane_created ON sitreps(lane,archived,created_at DESC);
UPDATE sitreps SET id = 'sr-' || substr(id, 3) WHERE id LIKE 'u-%';
CREATE TABLE IF NOT EXISTS events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT,kind TEXT NOT NULL,actor TEXT,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_events_task_seq ON events(task_id,seq);
UPDATE events
SET kind = 'sitrep_posted',
    payload = replace(replace(payload, '"statusID"', '"sitrepID"'), '"u-', '"sr-')
WHERE kind = 'status_posted';
"#;

/// Per-project operator rules: short constraints agents receive from the board.
///
/// These rows are a working copy, not a secret store or cross-machine source
/// of truth. They retire rather than delete so their audit trail stays useful.
const BOARD_V11: &str = r#"
CREATE TABLE rules (
 id TEXT PRIMARY KEY NOT NULL,
 body TEXT NOT NULL,
 author TEXT NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_rules_active ON rules(archived,created_at);
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

/// How long a writer keeps retrying a locked board before giving up.
///
/// Measured on a 16-agent fan-out against one board: the median write took
/// 208ms and 3% of writes failed, every one of them at the old 5s ceiling. A
/// 15s budget covers a write queue roughly seventy deep; past that the board
/// is genuinely oversubscribed and failing is the honest answer, because a
/// command that never returns is worse for an agent than one that says no.
const BUSY_BUDGET: Duration = Duration::from_secs(15);

/// Longest single pause between retries. Small enough that a lock released
/// early is picked up promptly, large enough that waiting is not a spin.
const BUSY_MAX_PAUSE_MS: u64 = 100;

thread_local! {
    /// When the current contention episode began. SQLite passes a retry count
    /// but no clock, and the handler is a bare `fn` pointer with nowhere to
    /// keep state, so the deadline is anchored here instead of extrapolated
    /// from the count — an extrapolated one would let the error report a wait
    /// that never happened.
    static BUSY_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Sub-millisecond entropy, decorrelated per process.
///
/// Enough to break up a herd, which is all it is for; nothing here needs an
/// RNG dependency or cryptographic quality.
fn jitter(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| u64::from(value.subsec_nanos()));
    (nanos ^ u64::from(std::process::id()).wrapping_mul(2_654_435_761)) % bound
}

/// Retry a locked database with randomized exponential backoff.
///
/// SQLite's built-in handler sleeps on a fixed schedule with no
/// randomization, so writers that collide wake together and collide again;
/// the same unlucky process can lose every round while the board as a whole
/// makes progress. Jitter decorrelates them, which is what turns a starving
/// writer into a merely slow one.
fn busy_backoff(count: i32) -> bool {
    let started = if count == 0 {
        let now = Instant::now();
        BUSY_SINCE.with(|since| since.set(Some(now)));
        now
    } else {
        BUSY_SINCE
            .with(|since| since.get())
            .unwrap_or_else(Instant::now)
    };
    if started.elapsed() >= BUSY_BUDGET {
        BUSY_SINCE.with(|since| since.set(None));
        return false;
    }
    let ceiling = (1u64 << count.clamp(0, 7)).min(BUSY_MAX_PAUSE_MS);
    sleep(Duration::from_millis(1 + jitter(ceiling)));
    true
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
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
    )?;
    // Replaces `busy_timeout`, which installs SQLite's own unjittered handler.
    connection.busy_handler(Some(busy_backoff))?;
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
    // Foreign keys are off across the upgrade, and on for everything after.
    //
    // This is SQLite's own procedure for rebuilding a table, and it has to
    // happen out here: `PRAGMA foreign_keys` is a no-op inside a transaction,
    // which is where every migration runs. Rebuilding `handoffs` in V4 drops
    // and recreates a table carrying references, and with enforcement on that
    // reads the tables it points at — including `checkpoints`, which a v3
    // board written by the retired TypeScript implementation may not have.
    //
    // Nothing is weakened by this: the rows are copied verbatim from a board
    // that already satisfied its own constraints, `foreign_key_check` is what
    // `doctor` runs to prove it afterwards, and enforcement is restored before
    // the connection does any work.
    connection.pragma_update(None, "foreign_keys", false)?;
    let outcome = migrate(
        &mut connection,
        &[
            BOARD_V1, BOARD_V2, BOARD_V3, BOARD_V4, BOARD_V5, BOARD_V6, BOARD_V7, BOARD_V8,
            BOARD_V9, BOARD_V10, BOARD_V11,
        ],
    );
    connection.pragma_update(None, "foreign_keys", true)?;
    outcome?;
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

/// Rows whose foreign key points at something that is not there.
///
/// `integrity_check` validates the b-tree and says nothing about referential
/// consistency, so a board could be structurally perfect and still hold a note
/// on a task that no longer exists — the shape a v3 board written by the
/// retired TypeScript implementation, or a partial import, can leave behind.
pub fn foreign_key_violations(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let rows = statement.query_map([], |row| {
        Ok(format!(
            "{} row {} references missing {}",
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?
                .map_or_else(|| "?".to_owned(), |id| id.to_string()),
            row.get::<_, String>(2)?
        ))
    })?;
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

#[cfg(test)]
mod tests {
    /// Every migration that exists is in the ladder that runs it.
    ///
    /// A migration can be written, reviewed and committed without ever being
    /// added to `open_board`'s list, and nothing says so: the constant is used
    /// nowhere, the build is clean, the tests pass, and the first sign is a
    /// column missing at runtime on a board that thought it was current. That
    /// happened once here, which is why this reads the file back.
    #[test]
    fn every_board_migration_is_in_the_ladder() {
        const SOURCE: &str = include_str!("db.rs");
        // Assembled, because this test reads its own source and a literal
        // would match itself.
        let declaration = format!("{} BOARD_V", "const");
        let declared = SOURCE.matches(declaration.as_str()).count();
        // Checked by name rather than by counting a text span: the list is
        // formatter-wrapped, so any span-based guard is one `cargo fmt` away
        // from measuring the wrong thing.
        let applied = SOURCE
            .split_once("pub fn open_board")
            .map(|(_, rest)| rest)
            .expect("open_board must exist");
        for n in 1..=declared {
            let name = format!("BOARD_V{n}");
            assert!(
                applied.contains(&name),
                "{name} is declared but never applied; a migration that does not \
                 run is invisible until a column is missing at runtime"
            );
        }
        let ladder = declared;
        assert_eq!(
            declared, ladder,
            "{declared} board migrations are declared but {ladder} are applied; \
             a migration that is never run is invisible until a column is missing"
        );
    }

    use super::*;

    #[test]
    fn jitter_stays_inside_its_bound() {
        assert_eq!(jitter(0), 0);
        for bound in [1u64, 2, 4, 64, BUSY_MAX_PAUSE_MS] {
            assert!(jitter(bound) < bound, "jitter escaped bound {bound}");
        }
    }

    #[test]
    fn a_fresh_contention_episode_does_not_inherit_the_last_one_s_deadline() {
        // A long-lived process that once waited out a full budget must not
        // refuse every write it attempts afterwards. `count == 0` is SQLite
        // saying "this is a new lock attempt", so it re-anchors the clock.
        BUSY_SINCE.with(|since| since.set(Some(Instant::now() - BUSY_BUDGET * 2)));
        assert!(
            busy_backoff(0),
            "a new episode reused an expired deadline and gave up immediately"
        );
    }

    #[test]
    fn a_writer_gives_up_once_its_budget_is_spent() {
        BUSY_SINCE
            .with(|since| since.set(Some(Instant::now() - BUSY_BUDGET - Duration::from_secs(1))));
        assert!(
            !busy_backoff(1),
            "a writer past its budget kept retrying instead of surfacing the lock"
        );
        // Cleared, so the next episode starts from its own clock.
        assert_eq!(BUSY_SINCE.with(|since| since.get()), None);
    }

    #[test]
    fn backoff_pauses_are_bounded_at_every_retry_count() {
        for count in [0i32, 1, 7, 31, i32::MAX] {
            let ceiling = (1u64 << count.clamp(0, 7)).min(BUSY_MAX_PAUSE_MS);
            assert!(
                (1..=BUSY_MAX_PAUSE_MS).contains(&ceiling),
                "retry {count} would pause for {ceiling}ms"
            );
        }
    }
}
