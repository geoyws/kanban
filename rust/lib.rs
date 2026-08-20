mod context;
mod db;
mod import;
mod lock;
mod model;
mod registry;
mod store;

use crate::context::{render_context, render_todo};
use crate::import::{ImportOptions, import_json, import_sqlite};
use crate::model::*;
use crate::registry::{Registry, data_root, now_ms, require_sane_clock};
use crate::store::{ClaimOptions, Store, UpdateTask};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const HELP: &str = r#"kanban — durable work ledger for agents (Rust)

Usage:
  kanban version
  kanban init [--name NAME] [--workspace PATH] [--force]
  kanban workspace list [--json]
  kanban workspace attach --to REGISTERED_PATH [--workspace PATH]
  kanban dashboard [--json]
  kanban doctor [--json]
  kanban backup [--output DIRECTORY] [--keep N] [--json]
  kanban restore --from DIRECTORY --force [--json]
  kanban task add TITLE [--id ID] [--type epic|story|task] [--parent ID]
             [--body TEXT] [--status STATUS] [--priority 0-9] [--depends-on ID ...]
             [--assignee AGENT] [--lane LANE] [--deliverable TEXT]
             [--stale-minutes N] [--driver-only]
  kanban task list [--status STATUS] [--with-relations] [--json]
  kanban task show ID [--json]
  kanban task move ID STATUS --as ACTOR [--metadata-patch-json JSON_OBJECT] [--force]
  kanban task remove ID --as ACTOR [--force]
  kanban task update ID --as ACTOR [task fields]
  kanban task metadata ID --as ACTOR --patch-json JSON_OBJECT
  kanban story advance ID --as ACTOR [--to STATE] [--reviewer AGENT] [--committer AGENT]
  kanban story signoff|unsignoff ID --as ACTOR [--note TEXT]
  kanban claim [ID | --next] --as AGENT [claim options] [--json]
  kanban heartbeat ID --lease TOKEN [--lease-minutes N]
  kanban release ID --lease TOKEN [--keep-status]
  kanban note ID TEXT --as AGENT [--kind KIND]
  kanban checkpoint ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT --next-action TEXT
  kanban handoff create ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT --next-action TEXT
  kanban handoff list [--task ID] [--status STATUS] [--json]
  kanban handoff accept ID --as AGENT [--session ID] [--lease-minutes N] [--json]
  kanban import atmux-json|atmux-sqlite PATH --as ACTOR [--reconcile] [--force]
             [--dry-run] [--verify] [--json]
  kanban events [--task ID] [--kind KIND] [--limit N] [--json]
  kanban stale [--json]
  kanban context ID [--max-chars N] [--json]
  kanban todo [--output PATH]
  kanban schema [--json]

Global options (accepted by every board command):
  --project NAME     address a registered project by name, from any directory
  --workspace PATH   use the project containing PATH instead of the cwd
  --db PATH          operate on a board file directly

Environment:
  KANBAN_PROJECT     default for --project
  KANBAN_DB          default for --db
  KANBAN_DATA_DIR    private data root (else $XDG_DATA_HOME/kanban, else
                     ~/.local/share/kanban)

Aliases (the binary installs as both `kanban` and `kb`):
  t=task  s=story  h=handoff  w/ws=workspace  cp=checkpoint  hb=heartbeat
  ctx=context  ev=events  dash=dashboard  rel=release  n=note  v=version
  task:      ls=list  mv=move  rm=remove  new=add  up=update  meta=metadata  cat=show
  story:     adv=advance
  handoff:   ls=list  new=create  acc=accept
  workspace: ls=list  att=attach
Aliases resolve by exact match; abbreviations such as --proj are not accepted.

--force is required to override a live lease (task move/remove) or to nest a
second board inside a registered project tree (init). Unknown flags are errors.

SQLite is authoritative. Generated TODO files are read-only projections."#;

const BOOLEAN: [&str; 19] = [
    "help",
    "json",
    "version",
    "force",
    "next",
    "keep-status",
    "driver-only",
    "no-driver-only",
    "unassign",
    "clear-lane",
    "clear-deliverable",
    "no-cross-lane",
    "allow-reassign",
    "with-relations",
    "clear-parent",
    "clear-dependencies",
    "reconcile",
    "dry-run",
    "verify",
];

/// Flags that may be given more than once, because their value is a list.
///
/// Everything else is single-valued, and repeating one used to keep the last
/// occurrence silently. `--project alpha --project beta` wrote to beta: the
/// wrong-board write ADR-007 exists to prevent, reached through a repeated
/// flag instead of a mistyped one, and trivially produced by a wrapper script
/// that appends a default the caller had already set.
const REPEATABLE: [&str; 3] = ["depends-on", "blocker", "validation"];

/// Accepted on every board command; see `store_path`.
const GLOBAL_FLAGS: [&str; 5] = ["help", "json", "db", "project", "workspace"];

/// The flags that each select a board. At most one may be given explicitly.
const BOARD_SELECTORS: [&str; 3] = ["db", "project", "workspace"];

/// Every command, and every flag it accepts.
///
/// An unrecognized flag is an error, never a silent no-op. A mistyped
/// `--projct alpha` used to fall through to directory resolution and write to
/// whichever board contained the working directory — the exact "wrong board"
/// damage ADR-007 exists to prevent, reported as success.
///
/// This is the single description of the command surface: `allowed_flags`
/// reads it, and the drift guards in the test module iterate it, so a command
/// added without its flag list fails the gate instead of reaching an operator.
/// One row of the command surface.
///
/// Name, subcommand, the flags it accepts, the most positionals it may hold
/// counting its own name words, and whether it writes anything anywhere.
type CommandRow = (
    &'static str,
    Option<&'static str>,
    &'static [&'static str],
    usize,
    bool,
);

const COMMANDS: &[CommandRow] = &[
    ("init", None, &["name", "force"], 1, false),
    ("workspace", Some("list"), &[], 2, true),
    ("workspace", Some("attach"), &["to"], 2, false),
    ("dashboard", None, &[], 1, true),
    ("doctor", None, &[], 1, true),
    ("backup", None, &["output", "keep"], 1, false),
    ("restore", None, &["from", "force"], 1, false),
    (
        "task",
        Some("add"),
        &[
            "id",
            "type",
            "parent",
            "body",
            "status",
            "priority",
            "depends-on",
            "assignee",
            "lane",
            "deliverable",
            "stale-minutes",
            "driver-only",
        ],
        3,
        false,
    ),
    ("task", Some("list"), &["status", "with-relations"], 2, true),
    ("task", Some("show"), &[], 3, true),
    (
        "task",
        Some("move"),
        &["as", "metadata-patch-json", "force"],
        4,
        false,
    ),
    ("task", Some("remove"), &["as", "force"], 3, false),
    ("task", Some("metadata"), &["as", "patch-json"], 3, false),
    (
        "task",
        Some("update"),
        &[
            "as",
            "parent",
            "clear-parent",
            "title",
            "body",
            "assignee",
            "unassign",
            "lane",
            "clear-lane",
            "deliverable",
            "clear-deliverable",
            "stale-minutes",
            "driver-only",
            "no-driver-only",
            "priority",
            "depends-on",
            "clear-dependencies",
        ],
        3,
        false,
    ),
    (
        "story",
        Some("advance"),
        &["as", "to", "reviewer", "committer"],
        3,
        false,
    ),
    ("story", Some("signoff"), &["as", "note"], 3, false),
    ("story", Some("unsignoff"), &["as", "note"], 3, false),
    (
        "claim",
        None,
        &[
            "as",
            "session",
            "lease-minutes",
            "lane",
            "role",
            "caller-scope",
            "no-cross-lane",
            "allow-reassign",
            "next",
        ],
        2,
        false,
    ),
    ("heartbeat", None, &["lease", "lease-minutes"], 2, false),
    ("release", None, &["lease", "keep-status"], 2, false),
    ("note", None, &["as", "kind"], 3, false),
    (
        "checkpoint",
        None,
        &[
            "lease",
            "as",
            "session",
            "model",
            "state",
            "summary",
            "intent",
            "next-action",
            "blocker",
            "validation",
            "repo",
            "branch",
            "head",
            "dirty",
        ],
        2,
        false,
    ),
    (
        "handoff",
        Some("create"),
        &[
            "lease",
            "as",
            "session",
            "model",
            "to",
            "reason",
            "summary",
            "intent",
            "next-action",
            "blocker",
            "validation",
            "repo",
            "branch",
            "head",
            "dirty",
        ],
        3,
        false,
    ),
    ("handoff", Some("list"), &["task", "status"], 2, true),
    (
        "handoff",
        Some("accept"),
        &["as", "session", "lease-minutes", "caller-scope"],
        3,
        false,
    ),
    (
        "import",
        Some("atmux-json"),
        &["as", "reconcile", "force", "dry-run", "verify"],
        3,
        false,
    ),
    (
        "import",
        Some("atmux-sqlite"),
        &["as", "reconcile", "force", "dry-run", "verify"],
        3,
        false,
    ),
    ("schema", None, &[], 1, true),
    ("events", None, &["task", "kind", "limit"], 1, true),
    ("stale", None, &[], 1, true),
    ("context", None, &["max-chars"], 2, true),
    ("todo", None, &["output"], 1, false),
];

/// The flags a command accepts, and the most positionals it can hold —
/// counting the command word and any subcommand word.
fn command_spec(command: &str, sub: Option<&str>) -> Option<(&'static [&'static str], usize)> {
    COMMANDS
        .iter()
        .find(|(name, expected, ..)| *name == command && *expected == sub)
        .map(|(_, _, flags, positionals, _)| (*flags, *positionals))
}

/// Commands whose second positional is a subcommand rather than an id.
const SUBCOMMAND_GROUPS: [&str; 5] = ["task", "story", "handoff", "import", "workspace"];

/// Short names for commands, resolved by exact match only.
///
/// Never prefix inference: every alias is written down, so adding a command
/// later cannot silently retarget one that already exists (ADR-008). An alias
/// that is not listed stays an unknown command.
fn canonical_command(value: &str) -> &str {
    match value {
        "t" => "task",
        "s" => "story",
        "h" => "handoff",
        "w" | "ws" => "workspace",
        "cp" => "checkpoint",
        "hb" => "heartbeat",
        "ctx" => "context",
        "ev" => "events",
        "dash" => "dashboard",
        "rel" => "release",
        "n" => "note",
        "v" => "version",
        other => other,
    }
}

/// Short names for subcommands, scoped to their group so `ls` can mean the
/// obvious thing under each without ever being ambiguous.
///
/// Only applied to [`SUBCOMMAND_GROUPS`]: for `claim`, `note` or `checkpoint`
/// the second positional is a task id, and a task genuinely called `rm` must
/// not be rewritten.
fn canonical_sub<'a>(command: &str, value: &'a str) -> &'a str {
    match (command, value) {
        ("task", "ls") => "list",
        ("task", "mv") => "move",
        ("task", "rm") => "remove",
        ("task", "new") => "add",
        ("task", "up") => "update",
        ("task", "meta") => "metadata",
        ("task", "cat") => "show",
        ("story", "adv") => "advance",
        ("handoff", "ls") => "list",
        ("handoff", "new") => "create",
        ("handoff", "acc") => "accept",
        ("workspace", "ls") => "list",
        ("workspace", "att") => "attach",
        (_, other) => other,
    }
}

struct Args {
    positionals: Vec<String>,
    flags: HashMap<String, Vec<String>>,
}

impl Args {
    fn parse(values: Vec<String>) -> Result<Self> {
        let mut positionals = Vec::new();
        let mut flags: HashMap<String, Vec<String>> = HashMap::new();
        let mut index = 0;
        while index < values.len() {
            let value = &values[index];
            if !value.starts_with("--") {
                positionals.push(value.clone());
                index += 1;
                continue;
            }
            let raw = &value[2..];
            let (name, inline) = raw
                .split_once('=')
                .map_or((raw, None), |(a, b)| (a, Some(b)));
            let item = if let Some(value) = inline {
                value.to_owned()
            } else if BOOLEAN.contains(&name) {
                "true".to_owned()
            } else {
                index += 1;
                values
                    .get(index)
                    .with_context(|| format!("--{name} requires a value"))?
                    .clone()
            };
            flags.entry(name.to_owned()).or_default().push(item);
            index += 1;
        }
        Ok(Self { positionals, flags })
    }
    fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    fn one(&self, name: &str) -> Option<&str> {
        self.flags
            .get(name)
            .and_then(|v| v.last())
            .map(String::as_str)
    }
    fn many(&self, name: &str) -> Vec<String> {
        self.flags.get(name).cloned().unwrap_or_default()
    }
    fn require(&self, name: &str) -> Result<&str> {
        self.one(name)
            .with_context(|| format!("--{name} is required"))
    }
    /// A flag's value as an integer, naming both the flag and what it got.
    ///
    /// `str::parse` alone reports "invalid digit found in string", which does
    /// not say which of several numeric flags was wrong — nothing an agent
    /// that reads stderr and moves on can act on. Every numeric flag goes
    /// through here so no call site can quietly parse without it.
    fn optional_integer(&self, name: &str) -> Result<Option<i64>> {
        let Some(raw) = self.one(name) else {
            return Ok(None);
        };
        raw.parse::<i64>()
            .map(Some)
            .map_err(|_| anyhow::anyhow!("--{name} must be an integer, got {raw:?}"))
    }

    fn integer(&self, name: &str, fallback: i64) -> Result<i64> {
        Ok(self.optional_integer(name)?.unwrap_or(fallback))
    }

    /// Fail on an argument this command was never going to read.
    ///
    /// Extra positionals were silently dropped, so `kanban task add Fix the
    /// parser` recorded the title `Fix` and reported success, and
    /// `kanban note t-1 the build is red --as ci` recorded the body `the`.
    /// Forgetting to quote is the likeliest slip at a shell and it produced a
    /// durable record that was wrong with nothing to notice it by, so the
    /// error leads with that possibility.
    fn reject_extra_positionals(&self, allowed: usize) -> Result<()> {
        if self.positionals.len() <= allowed {
            return Ok(());
        }
        let (accepted, extra) = self.positionals.split_at(allowed);
        bail!(
            "unexpected argument{} {} after `{}`; quote anything that contains spaces",
            if extra.len() == 1 { "" } else { "s" },
            extra
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            accepted.join(" ")
        );
    }

    /// Fail on a single-valued flag given more than once.
    ///
    /// Taking the last occurrence is a common convention and the wrong one
    /// here: the values disagree, only one of them is what the caller meant,
    /// and nothing in the receipt says which was used.
    fn reject_repeated(&self) -> Result<()> {
        let mut repeated = self
            .flags
            .iter()
            .filter(|(name, values)| values.len() > 1 && !REPEATABLE.contains(&name.as_str()))
            .map(|(name, values)| format!("--{name} ({})", values.join(", ")))
            .collect::<Vec<_>>();
        if repeated.is_empty() {
            return Ok(());
        }
        repeated.sort_unstable();
        bail!(
            "{} given more than once; it takes a single value, and guessing which one \
             was meant is how the wrong board gets written",
            repeated.join(", ")
        );
    }

    /// Fail when a command line names the board more than one way.
    ///
    /// `--db`, `--project` and `--workspace` each select a board, and the
    /// resolver reads them in that order. Precedence is right for a flag
    /// overriding its environment default, and for a flag overriding the
    /// working directory — neither of those is a second request. It is wrong
    /// for two flags a caller typed: the values disagree, only one is what they
    /// meant, and nothing in the receipt says which was used.
    ///
    /// `--project alpha --db /tmp/scratch.db` answered from the scratch file,
    /// conjuring it empty on the way, and `--project alpha --workspace ../beta`
    /// wrote to alpha. Both are the wrong-board write ADR-007 exists to
    /// prevent, reached through two valid flags instead of a mistyped one.
    ///
    /// Only flags the caller supplied are counted. `KANBAN_DB` and
    /// `KANBAN_PROJECT` stay defaults that a flag overrides, because that is
    /// what a default is.
    fn reject_conflicting_board_selectors(&self) -> Result<()> {
        let given = BOARD_SELECTORS
            .iter()
            .filter(|flag| self.flags.contains_key(**flag))
            .map(|flag| format!("--{flag} {}", self.one(flag).unwrap_or_default()))
            .collect::<Vec<_>>();
        if given.len() < 2 {
            return Ok(());
        }
        bail!(
            "{} each name a board, and they disagree; give exactly one, because \
             picking one by precedence is how the wrong board gets written",
            given.join(" and ")
        );
    }

    /// Fail on any flag this command does not define, naming the nearest match.
    fn reject_unknown(&self, allowed: &[&str]) -> Result<()> {
        let mut unknown = self
            .flags
            .keys()
            .filter(|name| {
                !allowed.contains(&name.as_str()) && !GLOBAL_FLAGS.contains(&name.as_str())
            })
            .map(String::as_str)
            .collect::<Vec<_>>();
        if unknown.is_empty() {
            return Ok(());
        }
        unknown.sort_unstable();
        let mut known = allowed
            .iter()
            .chain(GLOBAL_FLAGS.iter())
            .copied()
            .collect::<Vec<_>>();
        known.sort_unstable();
        let suggestion = nearest(unknown[0], &known)
            .map(|hit| format!("; did you mean --{hit}?"))
            .unwrap_or_default();
        bail!(
            "unknown flag{} {}{suggestion}\naccepted here: {}",
            if unknown.len() == 1 { "" } else { "s" },
            unknown
                .iter()
                .map(|name| format!("--{name}"))
                .collect::<Vec<_>>()
                .join(", "),
            known
                .iter()
                .map(|name| format!("--{name}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// The accepted flag the operator most likely meant, or nothing.
///
/// Abbreviating is at least as common as mistyping, and edit distance scores a
/// truncation badly — `proj` is three edits from `project`, so a typo-sized
/// budget misses it entirely. Try prefixes first, then typos.
///
/// This only ever suggests. `--proj` stays an error rather than an alias for
/// `--project`: accepting unambiguous prefixes means adding a `--projection`
/// later silently retargets every existing `--proj` caller, which is the
/// silent-change-of-meaning this guard exists to remove.
fn nearest<'a>(value: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if value.is_empty() {
        return None;
    }
    let mut prefixed = candidates
        .iter()
        .filter(|candidate| candidate.starts_with(value));
    match (prefixed.next(), prefixed.next()) {
        // Exactly one flag extends this stem.
        (Some(only), None) => return Some(only),
        // Several do. Guessing between them would be a coin flip, and the
        // error already prints every flag accepted here.
        (Some(_), Some(_)) => return None,
        (None, _) => {}
    }
    let budget = (value.chars().count() / 3).max(1);
    candidates
        .iter()
        .map(|candidate| (edit_distance(value, candidate), *candidate))
        .filter(|(distance, _)| *distance <= budget)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, a) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, b) in right.iter().enumerate() {
            current[j + 1] = (previous[j] + usize::from(a != *b))
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Minutes to milliseconds, refusing values that would overflow or expire
/// instantly. `--lease-minutes 999999999999999` used to panic on the multiply.
fn lease_ms(args: &Args) -> Result<i64> {
    const MAX_MINUTES: i64 = 43_200; // 30 days
    let minutes = args.integer("lease-minutes", 15)?;
    if !(1..=MAX_MINUTES).contains(&minutes) {
        bail!("lease minutes must be between 1 and {MAX_MINUTES}, got {minutes}");
    }
    Ok(minutes * 60_000)
}

fn print<T: Serialize>(value: &T, _pretty: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn cwd() -> Result<PathBuf> {
    env::current_dir().context("read current directory")
}

/// Comma-listed project names, for error messages. Empty when the registry is
/// empty, so a first-run error is not padded with a pointless "known projects:".
fn known_projects(registry: &Registry) -> Result<String> {
    let names = registry
        .projects()?
        .into_iter()
        .map(|project| project.name)
        .collect::<Vec<_>>();
    Ok(if names.is_empty() {
        String::new()
    } else {
        format!("\nknown projects: {}", names.join(", "))
    })
}

/// Resolve a project by registry name. Names are NOT unique — two projects may
/// share one — so an ambiguous name is an error naming the candidates, never a
/// silent pick: writing to the wrong board is unrecoverable work-state damage.
fn board_by_name(registry: &Registry, name: &str) -> Result<PathBuf> {
    let matches = registry.by_name(name)?;
    match matches.as_slice() {
        [project] => {
            registry.touch_board(&project.board_path)?;
            Ok(PathBuf::from(&project.board_path))
        }
        [] => bail!(
            "no Kanban project named {name}{}",
            known_projects(registry)?
        ),
        many => bail!(
            "{} Kanban projects are named {name}; disambiguate with --workspace PATH: {}",
            many.len(),
            many.iter()
                .map(|project| project.canonical_root.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Board selection, most explicit first:
///   1. `--db` / `KANBAN_DB`        — a board file directly
///   2. `--project` / `KANBAN_PROJECT` — a registered project by name, from anywhere
///   3. `--workspace PATH`          — the project containing PATH
///   4. the current directory       — the project containing it
///
/// The order resolves a flag against its environment default, and a flag
/// against the working directory. It never resolves two flags against each
/// other: `reject_conflicting_board_selectors` refuses that command line before
/// this runs, so at most one of (1)-(3) is ever present as a flag here.
///
/// (2) and (3) are what make the CLI usable outside a registered tree: an agent
/// in an unrelated cage, a cron line, or any shell in $HOME can address a board
/// without cd-ing into it.
/// The operation surface, as data a harness can generate an adapter from.
///
/// [ADR-001](../docs/adr/ADR-001-durable-agent-work-ledger.md) §6 says
/// consumers receive narrow operations rather than arbitrary write SQL, and
/// that MCP and plugin adapters expose *the same operations* the CLI does. An
/// adapter that hard-codes its own list of those operations is a second
/// description of the surface, and it drifts the first time a command or a
/// flag changes here — silently, because nothing compares them.
///
/// So the manifest is projected from `COMMANDS`, the same table the parser
/// validates against. There is one description of the surface, and an adapter
/// reads it rather than restating it.
///
/// `readOnly` is what lets a harness withhold mutation without maintaining its
/// own allow-list. It means the operation writes nothing anywhere — not the
/// board, not the registry, not a file — so `backup` and `todo` are not
/// read-only even though neither changes work state.
fn schema() -> Value {
    let operations = COMMANDS
        .iter()
        .map(|(command, sub, flags, positionals, read_only)| {
            let name = match sub {
                Some(sub) => format!("{command} {sub}"),
                None => (*command).to_owned(),
            };
            let flags = flags
                .iter()
                .map(|flag| {
                    let kind = if REPEATABLE.contains(flag) {
                        "list"
                    } else if BOOLEAN.contains(flag) {
                        "boolean"
                    } else {
                        "value"
                    };
                    json!({ "name": flag, "kind": kind })
                })
                .collect::<Vec<_>>();
            json!({
                "name": name,
                "command": command,
                "subcommand": sub,
                "flags": flags,
                // The words that name the operation are counted in, so a
                // caller can tell how many arguments of its own it may pass.
                "positionals": positionals,
                "readOnly": read_only,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "globalFlags": GLOBAL_FLAGS,
        "boardSelectors": BOARD_SELECTORS,
        "operations": operations,
    })
}

/// A board named straight by path, bypassing the registry entirely.
///
/// Read by both the board resolver and the data-root lock, so the two can
/// never disagree about whether an invocation is registry-addressed.
fn direct_db(args: &Args) -> Option<PathBuf> {
    args.one("db")
        .map(PathBuf::from)
        .or_else(|| env::var_os("KANBAN_DB").map(PathBuf::from))
}

fn store_path(args: &Args) -> Result<PathBuf> {
    if let Some(path) = direct_db(args) {
        return Ok(path);
    }
    let mut registry = Registry::open()?;
    let named = args.one("project").map(str::to_owned).or_else(|| {
        env::var("KANBAN_PROJECT")
            .ok()
            .filter(|value| !value.is_empty())
    });
    if let Some(name) = named {
        let path = board_by_name(&registry, &name)?;
        if !board_is_present(&path.to_string_lossy()) {
            return Err(missing_board_error(&path.to_string_lossy()));
        }
        return Ok(path);
    }
    let workspace = args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?);
    if let Some(record) = registry.resolve(&workspace)? {
        if !board_is_present(&record.board_path) {
            return Err(missing_board_error(&record.board_path));
        }
        return Ok(PathBuf::from(record.board_path));
    }
    bail!(
        "no Kanban project contains {}; address one from anywhere with --project NAME or KANBAN_PROJECT, or run 'kanban init' there{}",
        workspace.display(),
        known_projects(&registry)?
    )
}

/// Whether a registered board's file is still on disk.
///
/// Opening a board creates it, which is right for `--db` — that is how a board
/// is made — and wrong for one the registry already knows about. A registered
/// board file that has gone missing was destroyed, and standing an empty one
/// up in its place turns recoverable data loss into a board that reports
/// itself fine. `doctor` did exactly that: it recreated the file it was asked
/// to inspect, then certified the result healthy.
///
/// Commands that do work on one board refuse. Commands that survey every board
/// — `doctor`, `dashboard`, `backup` — report the gap and carry on, because
/// dying on the first missing board is no use to whoever has to fix it, and
/// `restore` would otherwise be unable to repair the very thing that stops it
/// from running.
fn board_is_present(board_path: &str) -> bool {
    Path::new(board_path).is_file()
}

fn missing_board_error(board_path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "board file {board_path} is registered but missing.\n\
         Recover it:      kanban restore --from SNAPSHOT --force\n\
         Or start over:   kanban init   (in the project, recreates it empty)"
    )
}

fn open_store(args: &Args) -> Result<Store> {
    Store::open(&store_path(args)?)
}

fn option_string(args: &Args, name: &str) -> Option<String> {
    args.one(name).map(str::to_owned)
}

/// Serialize a struct that is always a JSON object, so callers can extend it.
fn object_of<T: Serialize>(value: &T) -> Result<Map<String, Value>> {
    match serde_json::to_value(value)? {
        Value::Object(map) => Ok(map),
        other => bail!("expected a JSON object, got {other}"),
    }
}

fn list_json(store: &Store, status: Option<&str>, relations: bool) -> Result<Value> {
    let tasks = store.list_tasks(status)?;
    if !relations {
        return Ok(serde_json::to_value(tasks)?);
    }
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut value = object_of(&task)?;
        value.insert(
            "dependencies".into(),
            json!(
                store
                    .dependencies(&task.id)?
                    .into_iter()
                    .map(|dependency| dependency.id)
                    .collect::<Vec<_>>()
            ),
        );
        out.push(Value::Object(value));
    }
    Ok(Value::Array(out))
}

/// Delete all but the newest `keep` snapshots under the managed backups root.
///
/// Only ever prunes the directory Kanban itself writes snapshots into, and only
/// entries whose names are the millisecond stamps it generates. An operator who
/// passed `--output` gets their directory left alone: deleting from a path
/// someone else chose is the same overreach as re-permissioning one.
fn prune_backups(keep: i64, just_written: &Path) -> Result<Vec<String>> {
    if keep < 1 {
        bail!("--keep must be at least 1");
    }
    let root = data_root()?.join("backups");
    if just_written.parent() != Some(root.as_path()) {
        bail!(
            "--keep only prunes the managed backups directory ({}); \
             remove snapshots under --output yourself",
            root.display()
        );
    }
    let mut snapshots = fs::read_dir(&root)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.parse::<i64>().ok().map(|stamp| (stamp, entry.path()))
        })
        .collect::<Vec<_>>();
    snapshots.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
    let mut pruned = Vec::new();
    for (_, path) in snapshots.into_iter().skip(keep as usize) {
        fs::remove_dir_all(&path).with_context(|| format!("prune snapshot {}", path.display()))?;
        pruned.push(path.to_string_lossy().into_owned());
    }
    Ok(pruned)
}

/// Replace the live registry and boards with a snapshot.
///
/// A backup nobody can restore is not a recovery path, but this overwrites
/// live work state, so it verifies the source first, refuses without --force,
/// and snapshots what it is about to replace.
fn restore(args: &Args) -> Result<()> {
    let source = PathBuf::from(args.require("from")?);
    let registry_source = source.join("registry.db");
    if !registry_source.is_file() {
        bail!(
            "{} is not a Kanban snapshot: no registry.db",
            source.display()
        );
    }
    // Verify before destroying: restoring a corrupt snapshot over good state
    // would turn a recovery into the incident.
    let mut boards = Vec::new();
    for entry in fs::read_dir(source.join("boards"))
        .with_context(|| format!("read {}/boards", source.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) == Some("db") {
            boards.push(path);
        }
    }
    for path in std::iter::once(&registry_source).chain(boards.iter()) {
        let check = db::verify(path)?;
        if check != vec!["ok"] {
            bail!(
                "snapshot file {} failed integrity check: {:?}",
                path.display(),
                check
            );
        }
    }
    if !args.has("force") {
        bail!(
            "restore replaces the live registry and {} board(s) from {}; \
             rerun with --force once every kanban process is stopped",
            boards.len(),
            source.display()
        );
    }
    // Snapshot what is about to be overwritten, so a mistaken restore is itself
    // recoverable.
    let root = data_root()?;
    let rescue = root
        .join("backups")
        .join(format!("pre-restore-{}", now_ms()));
    let registry = Registry::open()?;
    registry.backup(&rescue.join("registry.db"))?;
    for project in registry.projects()? {
        // A board that is already gone is what a restore is often for; it
        // cannot be a precondition of running one.
        if !board_is_present(&project.board_path) {
            continue;
        }
        let file_name = Path::new(&project.board_path)
            .file_name()
            .with_context(|| format!("board path has no file name: {}", project.board_path))?;
        Store::open(Path::new(&project.board_path))?
            .backup(&rescue.join("boards").join(file_name))?;
    }
    drop(registry);

    let mut restored = Vec::new();
    for (from, to) in std::iter::once((registry_source.clone(), root.join("registry.db"))).chain(
        boards.iter().map(|path| {
            (
                path.clone(),
                root.join("boards")
                    .join(path.file_name().unwrap_or_default()),
            )
        }),
    ) {
        db::replace_database(&from, &to)?;
        restored.push(to.to_string_lossy().into_owned());
    }
    print(
        &json!({"restored":restored,"from":source,"rescueSnapshot":rescue}),
        args.has("json"),
    )
}

/// Shared entry point for both installed binaries, `kanban` and `kb`.
pub fn entrypoint() -> ! {
    match run() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::exit(1)
        }
    }
}

fn run() -> Result<()> {
    let args = Args::parse(env::args().skip(1).collect())?;
    if args.has("version") {
        println!("kanban {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.positionals.is_empty() || args.has("help") {
        println!("{HELP}");
        return Ok(());
    }
    let command = canonical_command(args.positionals[0].as_str());
    let sub = args
        .positionals
        .get(1)
        .map(String::as_str)
        .map(|value| canonical_sub(command, value));
    let rest = args.positionals.get(2..).unwrap_or(&[]);

    if command == "version" {
        println!("kanban {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Describes the binary, not a board, so it resolves none and takes no lock.
    if command == "schema" {
        return print(&schema(), args.has("json"));
    }

    let spec_sub = sub.filter(|_| SUBCOMMAND_GROUPS.contains(&command));
    match command_spec(command, spec_sub) {
        Some((allowed, positionals)) => {
            args.reject_unknown(allowed)?;
            args.reject_repeated()?;
            args.reject_extra_positionals(positionals)?;
            args.reject_conflicting_board_selectors()?;
        }
        None => bail!("unknown command; run kanban --help"),
    }

    require_sane_clock()?;

    // Held until `run` returns. `restore` replaces database files behind
    // SQLite's back, so it needs the data root to itself; everything else
    // only needs the assurance that no restore is doing so underneath it.
    // Acquired here rather than inside `Registry::open`, which `restore`
    // itself calls to write its rescue snapshot — an flock conflicts with a
    // second descriptor in the same process, so a lower-level acquire would
    // deadlock restore against itself.
    let _data_root = if lock::touches_data_root(direct_db(&args).as_deref()) {
        Some(if command == "restore" {
            lock::exclusive()?
        } else {
            lock::shared()?
        })
    } else {
        None
    };

    if command == "init" {
        let workspace = args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?);
        let fallback = workspace
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("project");
        let mut registry = Registry::open()?;
        let record = registry.register(
            &workspace,
            args.one("name").unwrap_or(fallback),
            args.has("force"),
        )?;
        let store = Store::open(Path::new(&record.board_path))?;
        store.initialize(&record.name)?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("list") {
        return print(&Registry::open()?.list()?, args.has("json"));
    }
    if command == "workspace" && sub == Some("attach") {
        let workspace = args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?);
        let mut registry = Registry::open()?;
        let record = registry.attach(&workspace, Path::new(args.require("to")?))?;
        return print(&record, args.has("json"));
    }
    if command == "dashboard" {
        let registry = Registry::open()?;
        let mut output = Vec::new();
        for project in registry.projects()? {
            let mut value = object_of(&project)?;
            if !board_is_present(&project.board_path) {
                value.insert("boardMissing".into(), json!(true));
                output.push(Value::Object(value));
                continue;
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let tasks = store.list_tasks(None)?;
            let mut counts = Map::new();
            for status in TASK_STATUSES {
                counts.insert(
                    status.into(),
                    json!(tasks.iter().filter(|task| task.status == status).count()),
                );
            }
            value.insert("taskCounts".into(), Value::Object(counts));
            value.insert(
                "pendingHandoffs".into(),
                json!(store.handoffs(None, Some("pending"), 100)?.len()),
            );
            value.insert("totalTasks".into(), json!(tasks.len()));
            value.insert("staleTasks".into(), json!(store.stale_tasks()?.len()));
            output.push(Value::Object(value));
        }
        return print(&output, args.has("json"));
    }
    if command == "doctor" {
        let registry = Registry::open()?;
        let registry_check = registry.integrity()?;
        let mut projects = Vec::new();
        let mut healthy = registry_check == vec!["ok"];
        for project in registry.projects()? {
            // Checked before opening, because opening would create it.
            if !board_is_present(&project.board_path) {
                healthy = false;
                projects.push(json!({
                    "name": project.name,
                    "boardPath": project.board_path,
                    "present": false,
                }));
                continue;
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let check = store.integrity()?;
            // `integrity_check` validates the b-tree and nothing about what
            // the rows mean, so a structurally perfect board can still hold a
            // note on a task that is gone, or work stamped in the future whose
            // lease no sweep will ever retire.
            let orphans = store.foreign_key_violations()?;
            let future = store.future_dated_tasks()?;
            healthy &= check == vec!["ok"] && orphans.is_empty() && future.is_empty();
            projects.push(json!({
                "name": project.name,
                "boardPath": project.board_path,
                "present": true,
                "integrity": check,
                "orphanedRows": orphans,
                "futureDatedTasks": future,
            }));
        }
        let result = json!({"healthy":healthy,"registry":registry_check,"projects":projects});
        print(&result, args.has("json"))?;
        if !healthy {
            bail!("Kanban integrity check failed");
        }
        return Ok(());
    }
    if command == "backup" {
        let registry = Registry::open()?;
        let directory = args
            .one("output")
            .map(PathBuf::from)
            .unwrap_or(data_root()?.join("backups").join(now_ms().to_string()));
        let registry_path = directory.join("registry.db");
        registry.backup(&registry_path)?;
        let mut boards = Vec::new();
        let mut missing = Vec::new();
        for project in registry.projects()? {
            // A snapshot of what is still here beats refusing to snapshot
            // anything, but it has to say what it could not include.
            if !board_is_present(&project.board_path) {
                missing.push(project.board_path.clone());
                continue;
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let file_name = Path::new(&project.board_path)
                .file_name()
                .with_context(|| format!("board path has no file name: {}", project.board_path))?;
            let destination = directory.join("boards").join(file_name);
            store.backup(&destination)?;
            boards.push(destination.to_string_lossy().into_owned());
        }
        let pruned = match args.one("keep") {
            Some(_) => prune_backups(args.integer("keep", 0)?, &directory)?,
            None => Vec::new(),
        };
        return print(
            &json!({"directory":directory,"registry":registry_path,"boards":boards,"missingBoards":missing,"pruned":pruned}),
            args.has("json"),
        );
    }
    if command == "restore" {
        return restore(&args);
    }

    let mut store = open_store(&args)?;
    if command == "task" && sub == Some("add") {
        let title = rest.first().context("task title is required")?.clone();
        let task = store.add_task(crate::model::AddTask {
            id: option_string(&args, "id"),
            task_type: args.one("type").unwrap_or("task").into(),
            parent_id: option_string(&args, "parent"),
            title,
            body: option_string(&args, "body"),
            assignee: option_string(&args, "assignee"),
            lane: option_string(&args, "lane"),
            deliverable: option_string(&args, "deliverable"),
            stale_minutes: args.optional_integer("stale-minutes")?,
            driver_only: args.has("driver-only"),
            status: args.one("status").unwrap_or("todo").into(),
            priority: args.integer("priority", 3)?,
            dependencies: args.many("depends-on"),
            metadata: json!({}),
        })?;
        return print(&task, args.has("json"));
    }
    if command == "task" && sub == Some("list") {
        return print(
            &list_json(&store, args.one("status"), args.has("with-relations"))?,
            args.has("json"),
        );
    }
    if command == "task" && sub == Some("show") {
        let id = rest.first().context("task id is required")?;
        let task = store.require_task(id)?;
        let claim = store.get_claim(id)?.as_ref().map(ClaimSummary::from);
        let mut value = object_of(&task)?;
        value.insert(
            "dependencies".into(),
            serde_json::to_value(store.dependencies(id)?)?,
        );
        value.insert("claim".into(), serde_json::to_value(claim)?);
        value.insert("notes".into(), serde_json::to_value(store.notes(id, 100)?)?);
        value.insert(
            "checkpoints".into(),
            serde_json::to_value(store.checkpoints(id, 20)?)?,
        );
        value.insert(
            "handoffs".into(),
            serde_json::to_value(store.handoffs(Some(id), None, 100)?)?,
        );
        return print(&Value::Object(value), args.has("json"));
    }
    if command == "task" && sub == Some("move") {
        let id = rest.first().context("task id is required")?;
        let status = rest.get(1).context("target status is required")?;
        let patch = args
            .one("metadata-patch-json")
            .map(serde_json::from_str)
            .transpose()?
            .unwrap_or_else(|| json!({}));
        let task = store.move_task(id, status, args.require("as")?, patch, args.has("force"))?;
        return print(&task, args.has("json"));
    }
    if command == "task" && sub == Some("remove") {
        let id = rest.first().context("task id is required")?;
        store.remove_task(id, args.require("as")?, args.has("force"))?;
        return print(&json!({"removed":id}), args.has("json"));
    }
    if command == "task" && sub == Some("metadata") {
        let id = rest.first().context("task id is required")?;
        let patch: Value = serde_json::from_str(args.require("patch-json")?)?;
        let task = store.patch_metadata(id, patch, args.require("as")?)?;
        return print(&task, args.has("json"));
    }
    if command == "task" && sub == Some("update") {
        let id = rest.first().context("task id is required")?;
        for (a, b) in [
            ("driver-only", "no-driver-only"),
            ("assignee", "unassign"),
            ("lane", "clear-lane"),
            ("deliverable", "clear-deliverable"),
            ("parent", "clear-parent"),
            ("depends-on", "clear-dependencies"),
        ] {
            if args.has(a) && args.has(b) {
                bail!("--{a} and --{b} are mutually exclusive");
            }
        }
        let input = UpdateTask {
            parent_id: if let Some(v) = args.one("parent") {
                Some(Some(v.into()))
            } else if args.has("clear-parent") {
                Some(None)
            } else {
                None
            },
            title: option_string(&args, "title"),
            body: args.one("body").map(|v| Some(v.into())),
            assignee: if let Some(v) = args.one("assignee") {
                Some(Some(v.into()))
            } else if args.has("unassign") {
                Some(None)
            } else {
                None
            },
            lane: if let Some(v) = args.one("lane") {
                Some(Some(v.into()))
            } else if args.has("clear-lane") {
                Some(None)
            } else {
                None
            },
            deliverable: if let Some(v) = args.one("deliverable") {
                Some(Some(v.into()))
            } else if args.has("clear-deliverable") {
                Some(None)
            } else {
                None
            },
            stale_minutes: args.optional_integer("stale-minutes")?.map(Some),
            driver_only: if args.has("driver-only") {
                Some(true)
            } else if args.has("no-driver-only") {
                Some(false)
            } else {
                None
            },
            priority: args.optional_integer("priority")?,
            dependencies: if args.has("clear-dependencies") {
                Some(vec![])
            } else if args.has("depends-on") {
                Some(args.many("depends-on"))
            } else {
                None
            },
        };
        return print(
            &store.update_task(id, input, args.require("as")?)?,
            args.has("json"),
        );
    }
    if command == "story" && sub == Some("advance") {
        let id = rest.first().context("story id is required")?;
        let value = store.advance_story(
            id,
            args.require("as")?,
            args.one("to"),
            args.one("reviewer"),
            args.one("committer"),
        )?;
        return print(&value, args.has("json"));
    }
    if command == "story" && (sub == Some("signoff") || sub == Some("unsignoff")) {
        let id = rest.first().context("story id is required")?;
        let value = store.signoff_story(
            id,
            args.require("as")?,
            sub == Some("signoff"),
            args.one("note"),
        )?;
        return print(&value, args.has("json"));
    }
    if command == "claim" {
        // `kanban claim t-5 --next` used to ignore the id and hand back
        // whatever was at the head of the queue, so an agent that asked for a
        // named task got a lease on a different one and no hint of the swap.
        if sub.is_some() && args.has("next") {
            bail!(
                "claim takes a task id or --next, not both: `claim {} --next` asked for one \
                 named task and for whichever comes first",
                sub.unwrap_or_default()
            );
        }
        let id = if args.has("next") { None } else { sub };
        if id.is_none() && !args.has("next") {
            bail!("task id or --next is required");
        }
        let value = store.claim(
            id,
            ClaimOptions {
                agent_id: args.require("as")?.into(),
                session_id: option_string(&args, "session"),
                lease_ms: lease_ms(&args)?,
                caller_lane: option_string(&args, "lane"),
                role_filter: option_string(&args, "role"),
                caller_scope: option_string(&args, "caller-scope"),
                cross_lane: !args.has("no-cross-lane"),
                allow_reassign: args.has("allow-reassign"),
            },
        )?;
        return print(&value, args.has("json"));
    }
    if command == "heartbeat" {
        let id = sub.context("task id is required")?;
        return print(
            &store.heartbeat(id, args.require("lease")?, lease_ms(&args)?)?,
            args.has("json"),
        );
    }
    if command == "release" {
        let id = sub.context("task id is required")?;
        store.release(id, args.require("lease")?, args.has("keep-status"))?;
        return print(&json!({"released":id}), args.has("json"));
    }
    if command == "note" {
        let id = sub.context("task id is required")?;
        let body = rest.first().context("note body is required")?;
        return print(
            &store.add_note(
                id,
                args.require("as")?,
                args.one("kind").unwrap_or("progress"),
                body,
            )?,
            args.has("json"),
        );
    }
    if command == "checkpoint" {
        let id = sub.context("task id is required")?;
        let value = store.checkpoint(CheckpointInput {
            task_id: id.into(),
            lease_token: args.require("lease")?.into(),
            author: args.require("as")?.into(),
            session_id: option_string(&args, "session"),
            model: option_string(&args, "model"),
            state: args.one("state").unwrap_or("continue").into(),
            summary: args.require("summary")?.into(),
            intent: args.require("intent")?.into(),
            next_action: args.require("next-action")?.into(),
            blockers: args.many("blocker"),
            validations: args.many("validation"),
            repo_path: option_string(&args, "repo"),
            branch: option_string(&args, "branch"),
            head_sha: option_string(&args, "head"),
            dirty_summary: option_string(&args, "dirty"),
        })?;
        return print(&value, args.has("json"));
    }
    if command == "handoff" && sub == Some("create") {
        let id = rest.first().context("task id is required")?;
        let value = store.create_handoff(HandoffInput {
            task_id: id.into(),
            lease_token: args.require("lease")?.into(),
            from_agent: args.require("as")?.into(),
            from_session: option_string(&args, "session"),
            from_model: option_string(&args, "model"),
            to_agent: option_string(&args, "to"),
            reason: args.one("reason").unwrap_or("token_pressure").into(),
            summary: args.require("summary")?.into(),
            intent: args.require("intent")?.into(),
            next_action: args.require("next-action")?.into(),
            blockers: args.many("blocker"),
            validations: args.many("validation"),
            repo_path: option_string(&args, "repo"),
            branch: option_string(&args, "branch"),
            head_sha: option_string(&args, "head"),
            dirty_summary: option_string(&args, "dirty"),
        })?;
        return print(&value, args.has("json"));
    }
    if command == "handoff" && sub == Some("list") {
        return print(
            &store.handoffs(args.one("task"), args.one("status"), 100)?,
            args.has("json"),
        );
    }
    if command == "handoff" && sub == Some("accept") {
        let id = rest.first().context("handoff id is required")?;
        let (handoff, claim) = store.accept_handoff(
            id,
            args.require("as")?,
            option_string(&args, "session"),
            lease_ms(&args)?,
            args.one("caller-scope"),
        )?;
        return print(&json!({"handoff":handoff,"claim":claim}), args.has("json"));
    }
    if command == "import" && (sub == Some("atmux-json") || sub == Some("atmux-sqlite")) {
        let path = rest.first().context("import path is required")?;
        let actor = args.require("as")?;
        let options = ImportOptions {
            reconcile: args.has("reconcile"),
            force: args.has("force"),
            dry_run: args.has("dry-run"),
            verify: args.has("verify"),
        };
        // --verify reads and reports; the others describe a write. Asking for
        // both in one command line is two requests, and silently letting one
        // win is how an operator comes away believing a cutover was checked
        // when it was performed, or performed when it was checked.
        if options.verify {
            let writes = ["reconcile", "force", "dry-run"]
                .into_iter()
                .filter(|flag| args.has(flag))
                .map(|flag| format!("--{flag}"))
                .collect::<Vec<_>>();
            if !writes.is_empty() {
                bail!(
                    "--verify compares the source against the board and writes nothing, so it \
                     cannot be combined with {}; run the verification and the import separately",
                    writes.join(" or ")
                );
            }
        }
        let receipt = if sub == Some("atmux-json") {
            import_json(&mut store, Path::new(path), actor, options)?
        } else {
            import_sqlite(&mut store, Path::new(path), actor, options)?
        };
        return print(&receipt, args.has("json"));
    }
    if command == "stale" {
        return print(&store.stale_tasks()?, args.has("json"));
    }
    if command == "events" {
        return print(
            &store.events(
                args.one("task"),
                args.one("kind"),
                args.integer("limit", 50)?,
            )?,
            args.has("json"),
        );
    }
    if command == "context" {
        let id = sub.context("task id is required")?;
        let packet = store.context_packet(id)?;
        if args.has("json") {
            // --max-chars bounds the rendered text and has never had any
            // effect here, so accepting it silently handed back an unbounded
            // packet to a caller who had asked for a bounded one.
            if args.has("max-chars") {
                bail!(
                    "--max-chars bounds the rendered context; --json returns the whole packet. \
                     Drop one of them — the packet's `truncated` field already says whether \
                     history was left out"
                );
            }
            return print(&packet, true);
        }
        let max_chars = args.integer("max-chars", 20_000)?;
        if max_chars < 0 {
            bail!("max chars must be positive");
        }
        println!("{}", render_context(&packet, max_chars as usize)?);
        return Ok(());
    }
    if command == "todo" {
        let rendered = render_todo(&store)?;
        if let Some(path) = args.one("output") {
            fs::write(path, &rendered)?;
            return print(&json!({"output":path}), args.has("json"));
        }
        print!("{rendered}");
        return Ok(());
    }
    bail!("unknown command; run kanban --help")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Args {
        Args::parse(values.iter().map(|value| (*value).to_owned()).collect()).unwrap()
    }

    #[test]
    fn edit_distance_counts_single_edits() {
        assert_eq!(edit_distance("status", "status"), 0);
        assert_eq!(edit_distance("statis", "status"), 1); // substitution
        assert_eq!(edit_distance("staus", "status"), 1); // deletion
        assert_eq!(edit_distance("statuss", "status"), 1); // insertion
        assert_eq!(edit_distance("proj", "project"), 3); // truncation scores badly
        assert_eq!(edit_distance("", "status"), 6);
    }

    #[test]
    fn nearest_prefers_prefixes_then_typos() {
        let flags = ["db", "help", "json", "project", "status", "workspace"];
        // A truncation is what edit distance alone misses.
        assert_eq!(nearest("proj", &flags), Some("project"));
        assert_eq!(nearest("pro", &flags), Some("project"));
        assert_eq!(nearest("p", &flags), Some("project"));
        // A typo still resolves.
        assert_eq!(nearest("statis", &flags), Some("status"));
        assert_eq!(nearest("wrokspace", &flags), Some("workspace")); // transposition
        // Nothing close is better than a confident wrong answer.
        assert_eq!(nearest("frobnicate", &flags), None);
        assert_eq!(nearest("", &flags), None);
    }

    #[test]
    fn nearest_refuses_to_guess_between_equally_valid_stems() {
        let flags = ["parent", "priority", "project"];
        // `--p` could be any of three: the caller gets the accepted list instead.
        assert_eq!(nearest("p", &flags), None);
        assert_eq!(nearest("pr", &flags), None);
        // Once the stem is decisive, it resolves again.
        assert_eq!(nearest("pare", &flags), Some("parent"));
        assert_eq!(nearest("prio", &flags), Some("priority"));
    }

    #[test]
    fn lease_minutes_is_bounded_and_cannot_overflow() {
        assert_eq!(
            lease_ms(&args(&["--lease-minutes", "15"])).unwrap(),
            900_000
        );
        assert_eq!(lease_ms(&args(&[])).unwrap(), 900_000); // default
        assert_eq!(
            lease_ms(&args(&["--lease-minutes", "43200"])).unwrap(),
            2_592_000_000
        );
        for bad in ["0", "-1", "43201", "999999999999999", &i64::MAX.to_string()] {
            assert!(
                lease_ms(&args(&["--lease-minutes", bad])).is_err(),
                "--lease-minutes {bad} must be refused, not wrapped"
            );
        }
    }

    #[test]
    fn a_bad_number_names_the_flag_and_the_value() {
        assert_eq!(
            args(&["--limit", "5"]).optional_integer("limit").unwrap(),
            Some(5)
        );
        assert_eq!(args(&[]).optional_integer("limit").unwrap(), None);
        assert_eq!(args(&["--limit", "5"]).integer("limit", 9).unwrap(), 5);
        assert_eq!(args(&[]).integer("limit", 9).unwrap(), 9);

        let error = args(&["--priority", "abc"])
            .optional_integer("priority")
            .unwrap_err()
            .to_string();
        // "invalid digit found in string" does not say which of several
        // numeric flags was wrong.
        assert!(error.contains("--priority"), "{error}");
        assert!(error.contains("\"abc\""), "{error}");

        // The helper is the only place that knows how to name what went
        // wrong, so no call site may go back to parsing a value itself. Both
        // of these were real: `--stale-minutes abc` and `--priority abc` on
        // `task update` reported only "invalid digit found in string".
        //
        // The needles are assembled rather than written out, because this
        // test reads its own file back and a literal would match itself.
        const SOURCE: &str = include_str!("lib.rs");
        for pattern in [
            format!(".map({}::parse)", "str"),
            format!(".parse().map({})", "Some"),
        ] {
            assert!(
                !SOURCE.contains(&pattern),
                "a flag value is parsed by `{pattern}`, which reports no flag name"
            );
        }
    }

    #[test]
    fn unknown_flags_are_rejected_and_globals_are_not() {
        let allowed = ["status", "with-relations"];
        assert!(args(&["--status", "todo"]).reject_unknown(&allowed).is_ok());
        for global in GLOBAL_FLAGS {
            assert!(
                args(&[&format!("--{global}"), "x"])
                    .reject_unknown(&allowed)
                    .is_ok(),
                "--{global} must be accepted everywhere"
            );
        }
        let error = args(&["--projct", "x"])
            .reject_unknown(&allowed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown flag --projct"), "{error}");
        assert!(error.contains("did you mean --project?"), "{error}");
    }

    #[test]
    fn every_command_declares_its_flags_without_duplicates() {
        for (command, sub, flags, positionals, _) in COMMANDS {
            let mut seen = flags.to_vec();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(
                before,
                seen.len(),
                "{command} {sub:?} declares a flag twice"
            );
            for flag in *flags {
                assert!(
                    !GLOBAL_FLAGS.contains(flag),
                    "{command} {sub:?} redeclares the global --{flag}"
                );
            }
            // The lookup must actually reach this row.
            assert_eq!(command_spec(command, *sub), Some((*flags, *positionals)));
            // A command has to leave room for the words that name it, and
            // for whatever positional the dispatcher then reads.
            let named = 1 + usize::from(sub.is_some());
            assert!(
                *positionals >= named,
                "{command} {sub:?} accepts fewer positionals than its own name"
            );
        }
        // No command is described twice, which would make the second row dead.
        let mut keys = COMMANDS
            .iter()
            .map(|(command, sub, ..)| (*command, *sub))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        let unique = keys.len();
        keys.dedup();
        assert_eq!(
            unique,
            keys.len(),
            "a command is declared twice in COMMANDS"
        );
        assert!(command_spec("frobnicate", None).is_none());
        assert!(command_spec("task", Some("frobnicate")).is_none());
    }

    #[test]
    fn every_list_valued_flag_is_declared_repeatable() {
        // Read back at compile time so the two cannot drift: a flag whose
        // value is collected with `many` but is missing from REPEATABLE would
        // be refused the moment someone legitimately passed it twice, and the
        // refusal would look like a rule rather than an oversight.
        const SOURCE: &str = include_str!("lib.rs");
        for (command, sub, flags, ..) in COMMANDS {
            for flag in *flags {
                let collected = SOURCE.contains(&format!("many(\"{flag}\")"));
                assert_eq!(
                    collected,
                    REPEATABLE.contains(flag),
                    "{command} {sub:?}: --{flag} is collected with many() but not listed \
                     REPEATABLE, or listed and never collected"
                );
            }
        }
        for flag in REPEATABLE {
            assert!(
                COMMANDS
                    .iter()
                    .any(|(_, _, flags, ..)| flags.contains(&flag)),
                "--{flag} is repeatable but no command accepts it"
            );
        }
    }

    #[test]
    fn repeating_a_single_valued_flag_is_refused() {
        assert!(args(&["--project", "alpha"]).reject_repeated().is_ok());
        assert!(args(&[]).reject_repeated().is_ok());
        // A list-valued flag is exactly what repeating is for.
        assert!(
            args(&["--blocker", "a", "--blocker", "b"])
                .reject_repeated()
                .is_ok()
        );
        let error = args(&["--project", "alpha", "--project", "beta"])
            .reject_repeated()
            .unwrap_err()
            .to_string();
        assert!(error.contains("--project (alpha, beta)"), "{error}");
    }

    #[test]
    fn every_boolean_flag_is_declared_by_some_command() {
        for flag in BOOLEAN {
            let declared = GLOBAL_FLAGS.contains(&flag)
                || flag == "version"
                || COMMANDS
                    .iter()
                    .any(|(_, _, flags, ..)| flags.contains(&flag));
            assert!(
                declared,
                "--{flag} parses as a boolean but no command accepts it"
            );
        }
    }

    #[test]
    fn aliases_resolve_to_real_commands_and_shadow_nothing() {
        // Every alias must land on a command that exists...
        for alias in [
            "t", "s", "h", "w", "ws", "cp", "hb", "ctx", "ev", "dash", "rel", "n", "v",
        ] {
            let resolved = canonical_command(alias);
            assert_ne!(resolved, alias, "{alias} is not wired up");
            assert!(
                resolved == "version" || COMMANDS.iter().any(|(command, ..)| *command == resolved),
                "alias {alias} resolves to unknown command {resolved}"
            );
            // ...and must not be the name of a different real command.
            assert!(
                !COMMANDS.iter().any(|(command, ..)| *command == alias),
                "alias {alias} shadows a real command"
            );
        }
        // A canonical name passes through untouched.
        for (command, ..) in COMMANDS {
            assert_eq!(canonical_command(command), *command);
        }
    }

    #[test]
    fn subcommand_aliases_are_scoped_to_their_group() {
        assert_eq!(canonical_sub("task", "ls"), "list");
        assert_eq!(canonical_sub("task", "mv"), "move");
        assert_eq!(canonical_sub("task", "rm"), "remove");
        assert_eq!(canonical_sub("workspace", "ls"), "list");
        assert_eq!(canonical_sub("handoff", "ls"), "list");
        // `ls` means nothing under story, so it stays a bad subcommand.
        assert_eq!(canonical_sub("story", "ls"), "ls");
        // Groups that take an id rather than a subcommand never rewrite it: a
        // task genuinely called `rm` must stay addressable.
        assert_eq!(canonical_sub("note", "rm"), "rm");
        assert!(!SUBCOMMAND_GROUPS.contains(&"note"));
        assert!(!SUBCOMMAND_GROUPS.contains(&"claim"));
        // Unlisted stems are not inferred.
        assert_eq!(canonical_sub("task", "li"), "li");
    }
}
