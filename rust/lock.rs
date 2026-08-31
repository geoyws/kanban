//! An advisory lock over the private data root.
//!
//! Every other mutation in Kanban is serialized by SQLite: concurrent writers
//! queue on `BEGIN IMMEDIATE` and readers see a consistent snapshot. `restore`
//! is the one operation that goes around SQLite entirely — it renames whole
//! database files into place while other processes may have them open, which
//! no transaction can protect against.
//!
//! So the data root carries one flock: board commands take it shared, and
//! `restore` takes it exclusively. Shared holders do not block each other, so
//! an agent swarm is unaffected; the only thing the lock excludes is a restore
//! racing live work, in either direction.
//!
//! ADR-008: `restore --force` used to *document* "stop every kanban process
//! first" and enforce nothing. A flag that implies more safety than it
//! delivers is worse than no flag, because the operator stops checking.

use crate::db::own_private_dir;
use crate::registry::data_root;
use anyhow::{Context, Result, bail};
use std::env;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a board command waits for an in-progress restore before refusing.
/// Deliberately the same 5s as the SQLite `busy_timeout` those commands
/// already run under, so contention feels identical whichever layer it hits.
const WAIT: Duration = Duration::from_millis(5_000);
const INIT_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(25);

/// Held for the lifetime of a command. The kernel drops the lock when the
/// descriptor closes, so this releases on unwind and on `process::exit` alike.
pub struct DataRootLock {
    _file: File,
}

/// Open (never create-and-delete) the named lock file.
///
/// The file is not removed afterwards on purpose: unlinking it would let the
/// next process create a *different* inode and take a lock that excludes
/// nobody. It is an empty 0600 marker whose only content is its identity.
fn open_lock_file(name: &str) -> Result<(PathBuf, File)> {
    let root = data_root()?;
    own_private_dir(&root)?;
    let path = root.join(name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Never truncate: the file's contents are irrelevant but its inode is
        // the lock, and rewriting it would be a pointless write on every call.
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open lock file {}", path.display()))?;
    Ok((path, file))
}

/// Take the data root exclusively, for a caller about to replace files in it.
///
/// Refuses immediately instead of waiting. A restore is a deliberate,
/// operator-driven recovery step; if something else holds the root open, the
/// useful answer is to say so and stop, not to block the operator's terminal
/// for as long as some agent keeps working.
pub fn exclusive() -> Result<DataRootLock> {
    let (path, file) = open_lock_file(".lock")?;
    match file.try_lock() {
        Ok(()) => Ok(DataRootLock { _file: file }),
        Err(TryLockError::WouldBlock) => bail!(
            "another kanban process is using {}; stop every kanban process and retry.\n\
             restore replaces database files while SQLite has them open, so it cannot \
             run alongside one",
            path.parent().unwrap_or(&path).display()
        ),
        Err(TryLockError::Error(error)) => {
            Err(anyhow::Error::new(error).context(format!("lock {}", path.display())))
        }
    }
}

/// Take the data root shared, for a caller that reads and writes through
/// SQLite. Excludes nothing but a restore.
pub fn shared() -> Result<DataRootLock> {
    let (path, file) = open_lock_file(".lock")?;
    let deadline = Instant::now() + WAIT;
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(DataRootLock { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => sleep(POLL),
            Err(TryLockError::WouldBlock) => bail!(
                "a kanban restore is replacing {} (waited {}s); retry once it finishes",
                path.parent().unwrap_or(&path).display(),
                WAIT.as_secs()
            ),
            Err(TryLockError::Error(error)) => {
                return Err(anyhow::Error::new(error).context(format!("lock {}", path.display())));
            }
        }
    }
}

/// Take the data root exclusively while an `init` is registering a board.
///
/// This uses its own stable `.init.lock`, so it never contends with the
/// ordinary `.lock` used by restore and board commands.
pub fn initialization() -> Result<DataRootLock> {
    let (path, file) = open_lock_file(".init.lock")?;
    let deadline = Instant::now() + INIT_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(DataRootLock { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => sleep(POLL),
            Err(TryLockError::WouldBlock) => bail!(
                "another init is registering a board in {}; waited {}s; retry once it finishes",
                path.parent().unwrap_or(&path).display(),
                INIT_WAIT.as_secs()
            ),
            Err(TryLockError::Error(error)) => {
                return Err(anyhow::Error::new(error).context(format!("lock {}", path.display())));
            }
        }
    }
}

/// Whether this invocation touches the private data root at all.
///
/// A board addressed straight by path (`--db` / `KANBAN_DB`) that lives
/// elsewhere is not data-root state. Locking it anyway would create a data
/// root as a side effect of a command that never wanted one — the same
/// overreach as re-permissioning a directory we do not own — and would fail
/// outright wherever `HOME` is unset. Everything else resolves through the
/// registry, or reads and writes the root directly.
pub fn touches_data_root(direct_db: Option<&Path>) -> bool {
    let Some(board) = direct_db else {
        return true;
    };
    // No resolvable data root means there is nothing to protect.
    let Ok(root) = data_root() else {
        return false;
    };
    contains(&root, board)
}

/// Whether `board` resolves to somewhere under `root`.
fn contains(root: &Path, board: &Path) -> bool {
    absolute(board).starts_with(absolute(root))
}

/// Absolute, with `.` and `..` resolved lexically.
///
/// `fs::canonicalize` is not usable here: the board file often does not exist
/// yet, and whether a path is inside the data root must not depend on whether
/// it has been created.
fn absolute(path: &Path) -> PathBuf {
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        env::current_dir().unwrap_or_default()
    };
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_resolves_traversal_without_touching_the_filesystem() {
        assert_eq!(
            absolute(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(absolute(Path::new("/a/../..")), PathBuf::from("/"));
        assert!(absolute(Path::new("relative/board.db")).is_absolute());
    }

    #[test]
    fn a_board_reached_through_traversal_is_still_inside_the_data_root() {
        let root = Path::new("/var/lib/kanban");
        assert!(contains(root, Path::new("/var/lib/kanban/boards/a.db")));
        assert!(
            contains(root, Path::new("/var/lib/kanban/boards/../boards/a.db")),
            "traversal must not smuggle a board out of the locked tree"
        );
        assert!(!contains(root, Path::new("/tmp/elsewhere.db")));
        assert!(
            !contains(root, Path::new("/var/lib/kanban-other/boards/a.db")),
            "a sibling directory sharing a name prefix is not inside the root"
        );
    }

    #[test]
    fn a_registry_resolved_command_always_takes_the_lock() {
        assert!(touches_data_root(None));
    }
}
