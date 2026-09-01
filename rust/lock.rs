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
use std::ffi::OsStr;
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
///
/// Asked of three spellings, because a symlink decides it the wrong way in
/// every direction and no one spelling catches them all.
///
/// Lexically alone, a board at `<root>/boards/x.db` reached through a symlink
/// somewhere else compared as *outside* the root and took no lock at all, so a
/// `restore` holding the root exclusively did not exclude the process mutating
/// a database file it was about to rename over; a symlinked root has the mirror
/// problem, where a board genuinely inside it compares as outside.
///
/// Resolved alone, a link that lives *inside* the root but points out of it
/// would stop being locked, and that link is itself a name inside the root that
/// a restore replaces.
///
/// Resolved after [`absolute`] alone still misses the traversal that runs the
/// other way. `absolute` collapses `..` first, so `/outside/link/../a.db` with
/// `link -> <root>/boards` becomes `/outside/a.db` and the component that put
/// the board inside the root is gone before anything is resolved — while the
/// kernel follows `link` first and opens `<root>/a.db`. So the third spelling
/// hands the path over exactly as written and lets `canonicalize` order the
/// `..` against the symlinks around it.
///
/// Any spelling landing inside is enough. That is what keeps this additive: the
/// first disjunct is the whole of the comparison this replaced, so resolving
/// can only ever add locks, never remove one.
fn contains(root: &Path, board: &Path) -> bool {
    let lexical_root = absolute(root);
    let lexical_board = absolute(board);
    // The board most invocations name is plainly inside the root, and this
    // answers those without touching the filesystem at all.
    lexical_board.starts_with(&lexical_root)
        || resolves_inside(&lexical_root, &lexical_board)
        || resolves_inside(&uncollapsed(root), &uncollapsed(board))
}

/// The same question asked of the paths the kernel would actually follow.
///
/// Unresolvable on either side answers "inside", matching
/// `touches_data_root(None)`: there is no half-resolved pair worth comparing,
/// and a shared lock nothing needed costs a syscall where a missing one is the
/// race this module exists to close. Each side is resolved only if the one
/// before it answered, so an unresolvable root costs one walk rather than two.
fn resolves_inside(root: &Path, board: &Path) -> bool {
    let Some(root) = real_path(root) else {
        return true;
    };
    let Some(board) = real_path(board) else {
        return true;
    };
    board.starts_with(root)
}

/// Absolute, with `.` and `..` resolved lexically.
///
/// Lexically, and before [`real_path`] rather than after: `..` past a symlink
/// names two different directories depending on which is applied first, and the
/// lexical reading is the one that keeps `<root>/link/../x` inside the root.
/// Following `link` out of the tree first would call that same path outside and
/// drop the lock.
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

/// Absolute with nothing collapsed, so `..` is left for the kernel to order
/// against the symlinks around it.
///
/// The counterpart to [`absolute`] rather than a replacement for it, because
/// the two disagree exactly where it matters and [`contains`] wants a lock if
/// either says inside: `<root>/link/../x` is inside the root under the lexical
/// reading and outside it under this one, and `/outside/link/../x` with `link`
/// pointing into the root is the reverse.
fn uncollapsed(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().unwrap_or_default().join(path)
    }
}

/// `path` with symlinks resolved as far as the filesystem allows.
///
/// `fs::canonicalize` on its own is not usable here: the board file often does
/// not exist yet, and whether a path is inside the data root must not depend on
/// whether it has been created. So this canonicalizes the deepest ancestor that
/// does exist and rejoins the rest lexically. A `--db` target's parent
/// directory exists in every case that matters, which is what makes the
/// symlinks that decide the answer resolvable while a board yet to be created
/// still compares correctly.
///
/// An ancestor that exists but will not resolve — an unreadable directory, a
/// symlink loop — simply ends the resolution there and keeps its lexical
/// spelling, which leaves a board inside the root inside it. `None` is the
/// narrower case of nothing resolving at all, the filesystem root included; a
/// relative path whose working directory has been deleted is the one that
/// happens.
fn real_path(path: &Path) -> Option<PathBuf> {
    let mut trailing: Vec<&OsStr> = Vec::new();
    let mut probe = path;
    loop {
        if let Ok(resolved) = probe.canonicalize() {
            let mut out = resolved;
            for part in trailing.iter().rev() {
                out.push(part);
            }
            return Some(out);
        }
        trailing.push(probe.file_name()?);
        probe = probe.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock moved backwards")
            .as_nanos()
    }

    /// Real directories on a real filesystem, because every case below turns on
    /// what the kernel does with a symlink. String fixtures would pass against
    /// the lexical comparison these tests exist to rule out.
    fn temp_root(label: &str) -> TempRoot {
        let root = env::temp_dir().join(format!(
            "kanban-lock-{label}-{}-{}",
            std::process::id(),
            unique()
        ));
        fs::create_dir_all(&root).expect("create temp lock dir");
        TempRoot(root)
    }

    /// A board file where the registry would put one.
    fn board_in(root: &Path) -> PathBuf {
        let boards = root.join("boards");
        fs::create_dir_all(&boards).expect("create boards dir");
        let board = boards.join("a.db");
        fs::write(&board, b"").expect("create board file");
        board
    }

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

    #[test]
    fn a_symlink_from_outside_the_root_to_a_board_inside_it_is_inside() {
        let temp = temp_root("symlinked-board");
        let root = temp.0.join("root");
        let board = board_in(&root);
        let link = temp.0.join("link.db");
        symlink(&board, &link).expect("symlink the board");
        assert!(
            contains(&root, &link),
            "a symlink is another name for the board it points at, and writing \
             through it mutates the very file restore renames"
        );
    }

    #[test]
    fn a_board_under_a_symlinked_data_root_is_inside_it() {
        let temp = temp_root("symlinked-root");
        let real = temp.0.join("real-root");
        let board = board_in(&real);
        let root = temp.0.join("root-link");
        symlink(&real, &root).expect("symlink the root");
        assert!(
            contains(&root, &board),
            "a symlinked root must not put its own boards outside itself"
        );
        assert!(
            contains(&root, &real.join("boards/not-created-yet.db")),
            "and a board that root has yet to create is inside it too"
        );
    }

    #[test]
    fn a_board_that_does_not_exist_yet_is_placed_by_the_ancestors_that_do() {
        let temp = temp_root("absent-board");
        let root = temp.0.join("root");
        fs::create_dir_all(root.join("boards")).expect("create boards dir");
        assert!(
            contains(&root, &root.join("boards/new.db")),
            "the board an init is about to create is data-root state already"
        );
        assert!(
            contains(&root, &root.join("does/not/exist/yet.db")),
            "a whole missing subtree still hangs under the root"
        );
        assert!(
            !contains(&root, &temp.0.join("outside.db")),
            "resolving what exists must not drag an unrelated path inside"
        );
    }

    #[test]
    fn a_symlink_inside_the_root_pointing_out_of_it_is_still_locked() {
        let temp = temp_root("escaping-symlink");
        let root = temp.0.join("root");
        fs::create_dir_all(&root).expect("create root");
        let outside = temp.0.join("outside.db");
        fs::write(&outside, b"").expect("create the outside board");
        let link = root.join("link.db");
        symlink(&outside, &link).expect("symlink out of the root");
        assert!(
            contains(&root, &link),
            "the link is a name inside the root and restore replaces names, so \
             resolving must only ever add locks"
        );
    }

    #[test]
    fn resolution_stops_at_the_deepest_ancestor_that_exists() {
        let temp = temp_root("deepest-ancestor");
        let real = temp.0.join("real");
        fs::create_dir_all(&real).expect("create real dir");
        let link = temp.0.join("link");
        symlink(&real, &link).expect("symlink the dir");
        let resolved = real
            .canonicalize()
            .expect("canonicalize real dir")
            .join("absent/board.db");
        assert_eq!(
            real_path(&link.join("absent/board.db")),
            Some(resolved),
            "the symlinked prefix resolves and the missing tail is rejoined"
        );
    }

    #[test]
    fn a_board_reached_through_a_symlink_into_the_root_is_inside_it() {
        let temp = temp_root("traversal-through-symlink");
        let root = temp.0.join("root");
        fs::create_dir_all(root.join("boards")).expect("create boards dir");
        let board = root.join("a.db");
        fs::write(&board, b"").expect("create board file");
        let outside = temp.0.join("outside");
        fs::create_dir_all(&outside).expect("create outside dir");
        let link = outside.join("link");
        symlink(root.join("boards"), &link).expect("symlink into the root");

        // The premise, from the kernel rather than from this module: following
        // `link` before `..` lands on a board inside the root.
        let spelled = link.join("../a.db");
        assert_eq!(
            spelled.canonicalize().expect("resolve the spelling"),
            board.canonicalize().expect("resolve the board"),
            "the kernel opens this path inside the root"
        );
        assert!(
            contains(&root, &spelled),
            "collapsing `..` first destroys the symlink that put this board \
             inside the root"
        );
        assert!(
            contains(&root, &link.join("../not-created-yet.db")),
            "and the same for a board that root has yet to create"
        );
    }

    #[test]
    fn an_uncollapsed_path_keeps_its_traversal_and_gains_a_root() {
        assert_eq!(
            uncollapsed(Path::new("/a/link/../b")),
            PathBuf::from("/a/link/../b"),
            "`..` is left for the kernel, not collapsed away from it"
        );
        let relative = uncollapsed(Path::new("boards/../a.db"));
        assert!(relative.is_absolute());
        assert!(
            relative.ends_with("boards/../a.db"),
            "the working directory is prepended and nothing else changes"
        );
    }

    #[test]
    fn a_path_that_will_not_resolve_at_all_fails_closed() {
        assert!(
            real_path(Path::new(".")).is_some(),
            "an ordinary relative path resolves through the working directory"
        );
        // What `absolute` hands over when `current_dir` fails, which is what a
        // deleted working directory does to a relative `--db`.
        let nowhere = PathBuf::from(format!("kanban-lock-nowhere-{}", unique()));
        assert!(real_path(&nowhere).is_none());
        assert!(
            resolves_inside(Path::new("/var/lib/kanban"), &nowhere),
            "nothing resolved, so take the lock: the same direction as \
             touches_data_root(None)"
        );
        assert!(
            resolves_inside(&nowhere, Path::new("/var/lib/kanban")),
            "and the same when it is the root that will not resolve"
        );
    }
}
