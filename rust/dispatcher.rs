use crate::lock::{self, DataRootLock};
use crate::registry::Registry;
use crate::store::Store;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);
static SIGNALS_INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardSelector {
    Db(PathBuf),
    Project(String),
    Workspace(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatcherArgs {
    pub(crate) selector: Option<BoardSelector>,
    pub(crate) consumer: Option<String>,
    pub(crate) once: bool,
    pub(crate) json: bool,
    pub(crate) help: bool,
    pub(crate) version: bool,
}

pub(crate) struct DispatcherContext {
    pub(crate) board_path: PathBuf,
    pub(crate) board_name: Option<String>,
    pub(crate) consumer: Option<String>,
    pub(crate) once: bool,
    pub(crate) json: bool,
    _data_root_lock: Option<DataRootLock>,
}

fn os_is_empty(value: &OsStr) -> bool {
    value.as_bytes().is_empty()
}

fn flag_name(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("dispatcher flag name must be valid UTF-8")
}

fn text_value(value: OsString, flag: &str) -> Result<String> {
    if os_is_empty(&value) {
        bail!("--{flag} requires a nonempty value");
    }
    value
        .into_string()
        .map_err(|_| anyhow::anyhow!("--{flag} must be valid UTF-8"))
}

fn path_value(value: OsString, flag: &str) -> Result<PathBuf> {
    if os_is_empty(&value) {
        bail!("--{flag} requires a nonempty value");
    }
    Ok(PathBuf::from(value))
}

fn nonempty_env(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !os_is_empty(value))
}

fn require_value(
    values: &[OsString],
    index: &mut usize,
    name: &str,
    inline: Option<OsString>,
) -> Result<OsString> {
    if let Some(value) = inline {
        if os_is_empty(&value) {
            bail!("--{name} requires a nonempty value");
        }
        return Ok(value);
    }
    *index += 1;
    let value = values
        .get(*index)
        .with_context(|| format!("--{name} requires a value"))?
        .clone();
    if value.as_bytes().starts_with(b"--") {
        bail!("--{name} requires a value before the next flag");
    }
    if os_is_empty(&value) {
        bail!("--{name} requires a nonempty value");
    }
    Ok(value)
}

pub(crate) fn parse_args(values: Vec<OsString>) -> Result<DispatcherArgs> {
    parse_args_with_env(
        values,
        env::var_os("KANBAN_DB"),
        env::var_os("KANBAN_PROJECT"),
    )
}

fn parse_args_with_env(
    values: Vec<OsString>,
    kanban_db: Option<OsString>,
    kanban_project: Option<OsString>,
) -> Result<DispatcherArgs> {
    let mut selector = None;
    let mut consumer = None;
    let mut once = false;
    let mut json = false;
    let mut help = false;
    let mut version = false;
    let mut seen = HashSet::new();
    let mut index = 0_usize;

    while index < values.len() {
        let token = &values[index];
        let bytes = token.as_bytes();
        if !bytes.starts_with(b"--") || bytes.len() == 2 {
            bail!("unexpected dispatcher positional argument {:?}", token);
        }
        let raw = &bytes[2..];
        let (name_bytes, inline) = match raw.iter().position(|byte| *byte == b'=') {
            Some(position) => (
                &raw[..position],
                Some(OsString::from_vec(raw[position + 1..].to_vec())),
            ),
            None => (raw, None),
        };
        let name = flag_name(name_bytes)?;
        if !matches!(
            name,
            "db" | "project" | "workspace" | "consumer" | "once" | "json" | "help" | "version"
        ) {
            bail!("unknown dispatcher flag --{name}");
        }
        if !seen.insert(name.to_owned()) {
            bail!("--{name} given more than once");
        }

        match name {
            "once" | "json" | "help" | "version" => {
                if inline.is_some() {
                    bail!("--{name} is a boolean flag and takes no value");
                }
                match name {
                    "once" => once = true,
                    "json" => json = true,
                    "help" => help = true,
                    "version" => version = true,
                    _ => unreachable!(),
                }
            }
            "db" | "project" | "workspace" => {
                if selector.is_some() {
                    bail!(
                        "dispatcher board selectors conflict; give exactly one of --db, --project, or --workspace"
                    );
                }
                let value = require_value(&values, &mut index, name, inline)?;
                selector = Some(match name {
                    "db" => BoardSelector::Db(path_value(value, name)?),
                    "project" => BoardSelector::Project(text_value(value, name)?),
                    "workspace" => BoardSelector::Workspace(path_value(value, name)?),
                    _ => unreachable!(),
                });
            }
            "consumer" => {
                let value = require_value(&values, &mut index, name, inline)?;
                consumer = Some(text_value(value, name)?);
            }
            _ => unreachable!(),
        }
        index += 1;
    }

    if help && version {
        bail!("--help and --version cannot be combined");
    }
    if !help && !version && selector.is_none() {
        bail!("dispatcher requires exactly one explicit selector: --db, --project, or --workspace");
    }

    let kanban_db = nonempty_env(kanban_db);
    let kanban_project = nonempty_env(kanban_project);
    if !help && !version {
        if kanban_db.is_some() && kanban_project.is_some() {
            bail!("KANBAN_DB and KANBAN_PROJECT both select a board; unset one");
        }
        match selector
            .as_ref()
            .expect("normal execution requires a selector")
        {
            BoardSelector::Db(path) => {
                if kanban_project.is_some() {
                    bail!("--db conflicts with KANBAN_PROJECT");
                }
                if let Some(env_path) = kanban_db
                    && env_path != path.as_os_str()
                {
                    bail!("--db conflicts with KANBAN_DB");
                }
            }
            BoardSelector::Project(name) => {
                if kanban_db.is_some() {
                    bail!("--project conflicts with KANBAN_DB");
                }
                if let Some(env_name) = kanban_project
                    && env_name != OsStr::new(name)
                {
                    bail!("--project conflicts with KANBAN_PROJECT");
                }
            }
            BoardSelector::Workspace(_) => {
                if kanban_db.is_some() || kanban_project.is_some() {
                    bail!("--workspace conflicts with KANBAN_DB or KANBAN_PROJECT");
                }
            }
        }
    }

    Ok(DispatcherArgs {
        selector,
        consumer,
        once,
        json,
        help,
        version,
    })
}

fn existing_board(path: &Path) -> Result<()> {
    let metadata = path
        .metadata()
        .with_context(|| format!("dispatcher board {} is not present", path.display()))?;
    if !metadata.is_file() {
        bail!("dispatcher board {} is not a regular file", path.display());
    }
    Ok(())
}

pub(crate) fn resolve_context(args: DispatcherArgs) -> Result<DispatcherContext> {
    if args.help || args.version {
        bail!("help and version do not resolve a dispatcher board");
    }
    let selector = args
        .selector
        .as_ref()
        .context("dispatcher selector is required")?;
    let direct_canonical = match selector {
        BoardSelector::Db(path) => Some(
            path.canonicalize()
                .with_context(|| format!("dispatcher board {} is not present", path.display()))?,
        ),
        BoardSelector::Project(_) | BoardSelector::Workspace(_) => None,
    };
    let needs_data_root_lock = match selector {
        BoardSelector::Db(path) => {
            lock::touches_data_root(Some(path))
                || lock::touches_data_root(direct_canonical.as_deref())
        }
        BoardSelector::Project(_) | BoardSelector::Workspace(_) => true,
    };
    let data_root_lock = needs_data_root_lock.then(lock::shared).transpose()?;

    let (board_path, board_name) = match selector {
        BoardSelector::Db(path) => {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("dispatcher board {} is not present", path.display()))?;
            if direct_canonical.as_ref() != Some(&canonical) {
                bail!(
                    "dispatcher board {} changed while resolving",
                    path.display()
                );
            }
            existing_board(&canonical)?;
            let store = Store::open_readonly(&canonical)?;
            (canonical, store.board_name()?)
        }
        BoardSelector::Project(name) => {
            let registry = Registry::open_readonly()?;
            let projects = registry.by_name(name)?;
            let project = match projects.as_slice() {
                [project] => project,
                [] => bail!("no Kanban project named {name}"),
                many => bail!(
                    "{} Kanban projects are named {name}; select one with --workspace",
                    many.len()
                ),
            };
            let path = PathBuf::from(&project.board_path);
            existing_board(&path)?;
            (path, Some(project.name.clone()))
        }
        BoardSelector::Workspace(workspace) => {
            let registry = Registry::open_readonly()?;
            let project = registry
                .resolve_readonly(workspace)?
                .with_context(|| format!("no Kanban project contains {}", workspace.display()))?;
            let path = PathBuf::from(&project.board_path);
            existing_board(&path)?;
            (path, Some(project.name))
        }
    };

    Ok(DispatcherContext {
        board_path,
        board_name,
        consumer: args.consumer,
        once: args.once,
        json: args.json,
        _data_root_lock: data_root_lock,
    })
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

pub(crate) fn install_signal_handlers() -> Result<()> {
    if SIGNALS_INSTALLED.load(Ordering::SeqCst) {
        return Ok(());
    }
    // SAFETY: sigaction is fully initialized before the calls, the handler
    // performs only a lock-free atomic store, and installing the same handler
    // more than once is deterministic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(std::io::Error::last_os_error()).context("initialize dispatcher signals");
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("install dispatcher signal {signal}"));
            }
        }
    }
    SIGNALS_INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) fn cancellation_flag() -> &'static AtomicBool {
    &CANCELLED
}

#[cfg(test)]
fn reset_cancellation() {
    CANCELLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use uuid::Uuid;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse(values: &[&str]) -> Result<DispatcherArgs> {
        parse_args_with_env(strings(values), None, None)
    }

    #[test]
    fn parser_accepts_the_exact_surface_and_inline_values() {
        assert_eq!(
            parse(&[
                "--db=/tmp/board.db",
                "--consumer",
                "worker",
                "--once",
                "--json"
            ])
            .unwrap(),
            DispatcherArgs {
                selector: Some(BoardSelector::Db("/tmp/board.db".into())),
                consumer: Some("worker".into()),
                once: true,
                json: true,
                help: false,
                version: false,
            }
        );
        assert!(matches!(
            parse(&["--project", "demo"]).unwrap().selector,
            Some(BoardSelector::Project(name)) if name == "demo"
        ));
        assert!(matches!(
            parse(&["--workspace", "/tmp"]).unwrap().selector,
            Some(BoardSelector::Workspace(path)) if path == Path::new("/tmp")
        ));
    }

    #[test]
    fn parser_rejects_unknown_positionals_missing_empty_repeated_and_conflicting_values() {
        for values in [
            vec!["--db", "/tmp/a", "extra"],
            vec!["--unknown"],
            vec!["--db"],
            vec!["--db="],
            vec!["--db", "/tmp/a", "--db", "/tmp/a"],
            vec!["--db", "/tmp/a", "--project", "demo"],
            vec!["--once", "--once", "--db", "/tmp/a"],
            vec!["--once=true", "--db", "/tmp/a"],
            vec!["--consumer", "", "--db", "/tmp/a"],
            vec!["--consumer", "--once", "--db", "/tmp/a"],
            vec![],
            vec!["--help", "--version"],
        ] {
            assert!(parse(&values).is_err(), "accepted {values:?}");
        }
    }

    #[test]
    fn help_and_version_do_not_require_a_selector() {
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["--version"]).unwrap().version);
    }

    #[test]
    fn matching_selector_environment_is_accepted() {
        let db =
            parse_args_with_env(strings(&["--db", "/tmp/a"]), Some("/tmp/a".into()), None).unwrap();
        assert!(matches!(db.selector, Some(BoardSelector::Db(_))));
        let project =
            parse_args_with_env(strings(&["--project", "demo"]), None, Some("demo".into()))
                .unwrap();
        assert!(matches!(project.selector, Some(BoardSelector::Project(_))));
    }

    #[test]
    fn every_selector_environment_conflict_fails_closed() {
        let cases = [
            (vec!["--db", "/tmp/a"], Some("/tmp/b"), None),
            (vec!["--db", "/tmp/a"], None, Some("demo")),
            (vec!["--project", "demo"], Some("/tmp/a"), None),
            (vec!["--project", "demo"], None, Some("other")),
            (vec!["--workspace", "/tmp"], Some("/tmp/a"), None),
            (vec!["--workspace", "/tmp"], None, Some("demo")),
            (vec!["--db", "/tmp/a"], Some("/tmp/a"), Some("demo")),
        ];
        for (values, db, project) in cases {
            assert!(
                parse_args_with_env(
                    strings(&values),
                    db.map(OsString::from),
                    project.map(OsString::from),
                )
                .is_err(),
                "accepted {values:?} with db={db:?} project={project:?}"
            );
        }
    }

    #[test]
    fn a_missing_direct_database_is_rejected_without_creation() {
        let path = env::temp_dir().join(format!(
            "kanban-dispatcher-missing-{}-{}.db",
            std::process::id(),
            Uuid::new_v4()
        ));
        let args =
            parse_args_with_env(vec!["--db".into(), path.as_os_str().to_owned()], None, None)
                .unwrap();
        let error = match resolve_context(args) {
            Ok(_) => panic!("missing database resolved"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not present"), "{error:#}");
        assert!(!path.exists());
    }

    #[test]
    fn signal_handler_only_marks_the_cancellation_flag() {
        reset_cancellation();
        assert!(!cancellation_flag().load(Ordering::SeqCst));
        handle_signal(libc::SIGTERM);
        assert!(cancellation_flag().load(Ordering::SeqCst));
        reset_cancellation();
    }

    #[test]
    fn direct_external_database_resolution_holds_no_data_root_lock() {
        let dir = env::temp_dir().join(format!(
            "kanban-dispatcher-board-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("board.db");
        let mut store = Store::open(&path).unwrap();
        store
            .initialize("dispatcher-test", "test@dispatcher")
            .unwrap();
        drop(store);
        let args =
            parse_args_with_env(vec!["--db".into(), path.as_os_str().to_owned()], None, None)
                .unwrap();
        let context = resolve_context(args).unwrap();
        assert_eq!(context.board_path, path.canonicalize().unwrap());
        assert_eq!(context.board_name.as_deref(), Some("dispatcher-test"));
        assert!(context._data_root_lock.is_none());
        drop(context);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_symlink_resolution_uses_the_stable_canonical_board() {
        let dir = env::temp_dir().join(format!(
            "kanban-dispatcher-symlink-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.db");
        let link = dir.join("link.db");
        let mut store = Store::open(&target).unwrap();
        store
            .initialize("dispatcher-link", "test@dispatcher")
            .unwrap();
        drop(store);
        symlink(&target, &link).unwrap();
        let args =
            parse_args_with_env(vec!["--db".into(), link.as_os_str().to_owned()], None, None)
                .unwrap();
        let context = resolve_context(args).unwrap();
        assert_eq!(context.board_path, target.canonicalize().unwrap());
        assert_eq!(context.board_name.as_deref(), Some("dispatcher-link"));
        drop(context);
        let _ = fs::remove_dir_all(dir);
    }
}
