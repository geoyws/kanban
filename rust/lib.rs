mod adapter_process;
mod adapter_protocol;
mod audit;
#[allow(dead_code)]
mod claude_print_adapter;
#[allow(dead_code)]
mod codex_app_server_adapter;
#[allow(dead_code)]
mod codex_app_server_messages;
#[allow(dead_code)]
mod codex_app_server_state;
mod codex_queue_adapter;
mod context;
mod db;
mod dispatch;
mod dispatcher;
mod gitctx;
mod import;
mod lock;
mod mcp;
mod model;
mod registry;
mod search;
mod serve;
mod store;
mod watch;

use crate::context::{render_context, render_todo};
use crate::import::{ImportOptions, import_json, import_sqlite};
use crate::model::*;
use crate::registry::{
    BoardPathState, PreparedAdoption, Registry, WORKSPACE_ADOPT_HELPER_COMMAND, data_root, now_ms,
    preflight_live_root_for_adoption, prepare_live_root_for_adoption, require_sane_clock,
    retired_board_message, run_workspace_adopt_helper, spawn_workspace_adopt_helper,
};
use crate::store::{ClaimOptions, Store, UpdateTask};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const HELP: &str = r#"kanban — durable work ledger for agents (Rust)

Usage:
  kanban version
  kanban init --name NAME [--workspace PATH] [--rootless] [--force] [--as ACTOR]
  kanban workspace list [--all] [--json]
  kanban workspace attach --to NAME|REGISTERED_PATH [--workspace PATH] [--as ACTOR]
  kanban workspace adopt --from-board PATH --name NAME (--workspace PATH | --rootless)
             --as ACTOR [--json]
  kanban workspace detach --root REGISTERED_PATH --as ACTOR
  kanban workspace retire NAME --as ACTOR --note TEXT
  kanban workspace unretire NAME --as ACTOR
  kanban workspace repoint [--root PATH] [--as ACTOR] [--json]
  kanban dashboard [--all] [--json]
  kanban doctor [--all] [--json]
  kanban audit verify [--against MANIFEST] [--json]
  kanban search QUERY [--source KIND] [--status STATUS] [--tag NAME] [--lane LANE]
             [--after MS] [--before MS] [--all] [--all-boards]
             [--limit N] [--max-chars N] [--json]
  kanban search-rebuild --as ACTOR [--all-boards] [--json]
  kanban serve [--port N] [--actor-header NAME]
  kanban events [--task ID | --rule ID | --registry] [--kind KIND]
             [--after MS] [--before MS] [--limit N] [--all] [--json]
  kanban watch [--task ID | --rule ID | --registry] [--kind KIND ...]
             [--relation KIND:ID ...] [--prior-status STATUS ...]
             [--current-status STATUS ...] [--tag NAME ...]
             [--cursor TOKEN|0] [--follow] [--all] [--limit N] [--json]
  kanban subscription add --consumer NAME --action NAME --timeout-ms N
             --max-retries N --rate-per-minute N --max-concurrency N --as ACTOR
             [--id ID] [--subject task:ID] [--relation KIND:ID ...]
             [--kind KIND ...] [--prior-status STATUS ...]
             [--current-status STATUS ...] [--tag NAME ...] [--secret-ref NAME] [--json]
  kanban subscription list [--status active|paused] [--consumer NAME] [--all] [--json]
  kanban subscription show ID [--json]
  kanban subscription pause|resume ID --as ACTOR [--json]
  kanban backup [--output DIRECTORY] [--keep N] [--json]
  kanban archive --older-than-days N --as ACTOR [--dry-run] [--json]
  kanban deploy start --repo REPO --commit FULL_SHA --tier TIER --environment NAME
             --host HOST --url URL --as ACTOR [--task ID] [--branch NAME]
             [--lane LANE] [--mechanism NAME] [--operation-id ID] [--retry-of ID]
  kanban deploy finish ID --token TOKEN --result succeeded|failed|cancelled --as ACTOR
             --phase build|publish|start|verification --receipt TEXT
             [--served-commit FULL_SHA] [--artifact-uri URI]
  kanban deploy abandon ID --as ACTOR --note TEXT [--token TOKEN | --force]
  kanban deploy show ID | list [--status STATUS] [--tier TIER] [--limit N] [--all]
  kanban deploy current
  kanban restore --from DIRECTORY --force [--as ACTOR] [--json]
  kanban task add TITLE [--as ACTOR] [--id ID] [--type epic|story|task] [--parent ID]
             [--body TEXT | --body-file PATH] [--status draft|backlog|todo|…]
             [--priority P0|P1|P2|0-9] [--depends-on ID ...]
             [--assignee AGENT] [--lane LANE] [--deliverable TEXT]
             [--stale-minutes N] [--driver-only]
  kanban task list [--status STATUS] [--tag NAME] [--lane LANE] [--with-relations] [--all]
             [--fields id,title,status,... | --no-body] [--json]
  kanban task show ID [--json]
  kanban task move ID STATUS --as ACTOR [--metadata-patch-json JSON_OBJECT] [--force]
  kanban task remove ID --as ACTOR [--force]
  kanban task update ID --as ACTOR [task fields, incl. --body-file PATH]
  kanban task metadata ID --as ACTOR --patch-json JSON_OBJECT
  kanban story advance ID --as ACTOR [--to STATE] [--reviewer AGENT] [--committer AGENT]
  kanban story signoff|unsignoff ID --as ACTOR [--note TEXT]
  kanban claim [ID | --next] --as AGENT [claim options] [--json]
  kanban claim --candidates --as AGENT [--tag NAME] [--lane LANE] [--role ROLE]
             [--caller-scope driver] [--no-cross-lane] [--allow-reassign]
             [--limit N] [--json]
  kanban heartbeat ID --lease TOKEN [--lease-minutes N]
  kanban release ID --lease TOKEN [--keep-status]
  kanban note ID TEXT --as AGENT [--kind KIND]
  kanban checkpoint ID --lease TOKEN --as AGENT --summary TEXT --intent TEXT
             --next-action TEXT [--session ID] [--model NAME] [--state STATE]
             [--blocker TEXT ...] [--validation TEXT ...]
             [--repo PATH] [--branch NAME] [--head SHA] [--dirty TEXT] [--json]
  kanban handoff create [ID --lease TOKEN] --as AGENT --summary TEXT --intent TEXT
             --next-action TEXT [--priority P0|P1|P2|0-9] [--to AGENT]
             [--reason TEXT] [--session ID] [--model NAME]
             [--blocker TEXT ...] [--validation TEXT ...]
             [--repo PATH] [--branch NAME] [--head SHA] [--dirty TEXT] [--json]
             (without ID: a session handoff, about no one task)
             (--repo, --branch, --head and --dirty are captured from the cwd's
             git checkout when omitted; an explicit flag overrides the capture)
  kanban handoff list [--task ID] [--status STATUS] [--to AGENT] [--json]
  kanban handoff accept ID --as AGENT [--session ID] [--lease-minutes N] [--json]
  kanban import atmux-json|atmux-sqlite PATH --as ACTOR [--reconcile] [--force]
             [--dry-run] [--verify] [--json]
  kanban tag add NAME [--description TEXT] [--as ACTOR] [--json]
  kanban tag list [--json]
  kanban tag remove NAME [--force] [--as ACTOR] [--json]
  kanban rule add [BODY | --body TEXT | --body-file PATH] --as ACTOR
             [--board NAME ... | --except-board NAME ...] [--tag NAME ...] [--json]
  kanban rule list [--all] [--full] [--json]
  kanban rule show ID [--json]
  kanban rule update ID [--body TEXT | --body-file PATH]
             [--board NAME ... | --except-board NAME ...] [--tag NAME ... | --clear-tags]
             --as ACTOR [--json]
  kanban rule retire ID --as ACTOR [--json]
  kanban rule export --board NAME ... --as ACTOR [--output PATH] [--json]
  kanban rule import PATH --as ACTOR [--json]
  kanban rule consolidate --as ACTOR [--json]
  kanban attention raise TEXT --as AGENT [--kind blocking|decision|approval|review|risk]
             [--priority P0|P1|P2|0-9]
             [--task ID] [--tag NAME ...] [--json]
  kanban attention list [--status open|resolved] [--kind KIND] [--task ID] [--tag NAME]
             [--lane LANE] [--limit N] [--fields id,kind,status,... | --no-body] [--json]
  kanban attention update ID --as ACTOR [--body TEXT | --body-file PATH]
             [--tag NAME ... | --clear-tags] [--json]
  kanban attention resolve ID --as ACTOR --note TEXT [--json]
  kanban attention reopen ID --as ACTOR --note TEXT [--json]
  kanban sitrep post TEXT --as AGENT --lane LANE [--task ID] [--json]
  kanban sitrep list [--lane LANE] [--task ID] [--all] [--limit N] [--json]
  kanban stale [--json]
  kanban context ID [--max-chars N] [--json]
  kanban todo [--output PATH]
  kanban schema [--json]
  kanban mcp

Global options (accepted by every board command; a command that addresses the
registry instead — doctor, dashboard, backup, restore, audit verify, serve,
schema, mcp, workspace, rule — refuses the ones it would discard):
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
  ctx=context  ev=events  dash=dashboard  rel=release  n=note  r=rule  sr=sitrep  v=version
  att/attn=attention
  task:      ls=list  mv=move  rm=remove  new=add  up=update  meta=metadata  cat=show
  story:     adv=advance
  handoff:   ls=list  new=create  acc=accept
  workspace: ls=list  att=attach  det=detach
  rule:      ls=list  new=add  up=update  cat=show
             repeat --board NAME; --except-board NAME means ALL except that board
Aliases resolve by exact match; abbreviations such as --proj are not accepted.

--force is required to override a live lease (task move/remove) or to nest a
second board inside a registered project tree (init). claim has no --force, and
--allow-reassign only filters claim --candidates: to take a task off an agent
that died holding the lease, task move ID todo --as ACTOR --force, then claim it
again for a fresh token. Unknown flags are errors.

SQLite is authoritative. Generated TODO files are read-only projections."#;

pub(crate) const BOOLEAN: [&str; 28] = [
    "help",
    "json",
    "version",
    "force",
    "rootless",
    "next",
    "candidates",
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
    "clear-tags",
    "reconcile",
    "dry-run",
    "verify",
    "all",
    "full",
    "all-boards",
    "registry",
    "follow",
    "no-body",
];

/// Removed boolean flags that remain recognizable only to return an actionable
/// migration diagnostic instead of consuming the next positional as a value.
const DEPRECATED_BOOLEAN: [&str; 1] = ["global"];

/// Flags that may be given more than once, because their value is a list.
///
/// Everything else is single-valued, and repeating one used to keep the last
/// occurrence silently. `--project alpha --project beta` wrote to beta: the
/// wrong-board write ADR-007 exists to prevent, reached through a repeated
/// flag instead of a mistyped one, and trivially produced by a wrapper script
/// that appends a default the caller had already set.
pub(crate) const REPEATABLE: [&str; 6] = [
    "depends-on",
    "blocker",
    "validation",
    "tag",
    "board",
    "except-board",
];

/// Watch-only list-valued filters. Other commands retain their historical
/// single-valued `--kind` behavior.
pub(crate) const WATCH_REPEATABLE: [&str; 4] =
    ["kind", "relation", "prior-status", "current-status"];

pub(crate) const SUBSCRIPTION_REPEATABLE: [&str; 4] =
    ["kind", "relation", "prior-status", "current-status"];

/// Commands that are processes rather than operations.
///
/// `mcp` and `serve` block until killed. That makes them meaningless as tool
/// calls — the MCP layer spawns the binary and reads its result, so a tool that
/// never returns hangs the caller — and impossible to exercise the way the
/// read-only guard exercises everything else, which runs each operation and
/// compares the board before and after.
///
/// This was a bare `!= "mcp"` inside the tool builder until `serve` arrived and
/// the filter named only the first of two. It is a set with a guard now,
/// because the next one will be the same mistake.
pub(crate) const LONG_RUNNING: [&str; 3] = ["mcp", "serve", "watch"];

/// Accepted on every board command; see `store_path`.
pub(crate) const GLOBAL_FLAGS: [&str; 5] = ["help", "json", "db", "project", "workspace"];

/// Upper bound for a watch replay batch.
///
/// The reader can ask for nothing or a small bounded batch, but a caller
/// cannot request an unbounded `Vec` through `--limit`.
pub(crate) const WATCH_BATCH_LIMIT: i64 = 1_000;

/// The flags that each select a board. At most one may be given explicitly.
const BOARD_SELECTORS: [&str; 3] = ["db", "project", "workspace"];

/// One command, and the board selectors it never consults.
///
/// Name, subcommand, the selectors it discards, and what it addresses instead.
pub(crate) type IgnoredSelectorRow = (
    &'static str,
    Option<&'static str>,
    &'static [&'static str],
    &'static str,
);

/// The commands that survey the registry instead of resolving one board.
///
/// `--db`, `--project` and `--workspace` are in [`GLOBAL_FLAGS`], so every
/// command parses them and `reject_unknown` exempts them. A command that never
/// asks the resolver for a board therefore accepted a selector and threw it
/// away. `doctor --db /nowhere/absent.db --json` answered
/// `{"healthy": true, ...}` — computed over every registered board, about a
/// file that was not there — and a health check that returns green about a
/// different subject is worse than one that refuses. `backup --db` was the
/// same shape, and both skipped the data-root lock on the way, because
/// `lock::touches_data_root` reads the very `--db` path the command then
/// ignored: `restore --db /tmp/elsewhere.db` replaced the whole data root
/// without the exclusive lock that exists to keep a concurrent reader off it.
///
/// Refusing is the whole fix. None of these grows a per-board mode here; they
/// say the flag does not apply and name what they do address, the way `rule`
/// and `watch --registry` already do.
///
/// Listed per command, and only the selectors actually discarded: `init` and
/// `workspace attach` both honour `--workspace` and ignore the other two, so a
/// blanket rule would break the two uses that work.
///
/// Every row here is unconditional — the command discards the selector on every
/// invocation, so the manifest and the MCP tool builder can read it and be
/// right every time. Three refusals elsewhere are deliberately *not* rows and
/// must stay inline, because they depend on another flag rather than on the
/// command: `events --registry` / `events --rule` and `watch --registry` /
/// `watch --rule` read the registry trail only when that flag is present
/// (`events --db` addresses a board and is honoured), and
/// `search --all-boards` / `search-rebuild --all-boards` refuse a selector only
/// in their all-boards mode. Tabling any of those would refuse a selector the
/// command honours the rest of the time, which is the mirror image of the
/// defect this table exists to fix.
pub(crate) const IGNORED_SELECTORS: &[IgnoredSelectorRow] = &[
    (
        "init",
        None,
        &["db", "project"],
        "creates its own board under the data root, with --workspace naming the tree it belongs to",
    ),
    (
        "workspace",
        Some("list"),
        &["db", "project", "workspace"],
        "lists every registered project",
    ),
    (
        "workspace",
        Some("attach"),
        &["db", "project"],
        "attaches the tree named by --workspace to the project named by --to",
    ),
    (
        "workspace",
        Some("adopt"),
        &["db", "project"],
        "copies a source board into registry-owned storage under --name",
    ),
    (
        "workspace",
        Some("detach"),
        &["db", "project", "workspace"],
        "detaches the registered root named by --root",
    ),
    (
        "workspace",
        Some("retire"),
        &["db", "project", "workspace"],
        "retires one named board, not a board selector",
    ),
    (
        "workspace",
        Some("unretire"),
        &["db", "project", "workspace"],
        "restores one named board, not a board selector",
    ),
    (
        "workspace",
        Some("repoint"),
        &["db", "project", "workspace"],
        "repairs registered roots, named by --root or taken from the registry",
    ),
    (
        "dashboard",
        None,
        &["db", "project", "workspace"],
        "summarizes every registered board",
    ),
    (
        "doctor",
        None,
        &["db", "project", "workspace"],
        "checks the registry and every board in it",
    ),
    (
        "audit",
        Some("verify"),
        &["db", "project", "workspace"],
        "verifies the registry and every board in it",
    ),
    (
        "backup",
        None,
        &["db", "project", "workspace"],
        "snapshots the registry and every board in it",
    ),
    (
        "restore",
        None,
        &["db", "project", "workspace"],
        "replaces the whole data root from the snapshot named by --from",
    ),
    (
        "serve",
        None,
        &["db", "project", "workspace"],
        "serves every registered board",
    ),
    (
        "schema",
        None,
        &["db", "project", "workspace"],
        "describes this binary, not a board",
    ),
    (
        "mcp",
        None,
        &["db", "project", "workspace"],
        "resolves a board per tool call, not once for the server",
    ),
    // Rules refused these before this table existed, from two inline loops in
    // the dispatcher. Refusing there and declaring nothing here left the rest
    // of the surface reading the wrong answer: `schema --json` published
    // `ignoredSelectors: []` for all six rows, and the MCP tool builder — which
    // withholds exactly what this table names — went on advertising `project`
    // on `rule_list`, so an agent could read the schema, send the argument it
    // was offered, and get back "--project does not select a rule collection".
    // The refusal is unconditional, so it belongs here and the loops are gone.
    (
        "rule",
        Some("add"),
        &["db", "project", "workspace"],
        "writes to the one registry-owned, tag-scoped rule collection",
    ),
    (
        "rule",
        Some("list"),
        &["db", "project", "workspace"],
        "reads the one registry-owned, tag-scoped rule collection",
    ),
    (
        "rule",
        Some("show"),
        &["db", "project", "workspace"],
        "reads the one registry-owned, tag-scoped rule collection",
    ),
    (
        "rule",
        Some("update"),
        &["db", "project", "workspace"],
        "writes to the one registry-owned, tag-scoped rule collection",
    ),
    (
        "rule",
        Some("retire"),
        &["db", "project", "workspace"],
        "writes to the one registry-owned, tag-scoped rule collection",
    ),
    (
        "rule",
        Some("consolidate"),
        &["db", "project", "workspace"],
        "migrates every registered board, so naming one would imply a partial migration",
    ),
];

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
pub(crate) type CommandRow = (
    &'static str,
    Option<&'static str>,
    &'static [&'static str],
    &'static [&'static str],
    bool,
);

pub(crate) const COMMANDS: &[CommandRow] = &[
    (
        "init",
        None,
        &["name", "force", "rootless", "as"],
        &[],
        false,
    ),
    ("workspace", Some("list"), &["all"], &[], true),
    ("workspace", Some("attach"), &["to", "as"], &[], false),
    (
        "workspace",
        Some("adopt"),
        &["from-board", "name", "rootless", "as"],
        &[],
        false,
    ),
    ("workspace", Some("detach"), &["root", "as"], &[], false),
    (
        "workspace",
        Some("retire"),
        &["as", "note"],
        &["name"],
        false,
    ),
    ("workspace", Some("unretire"), &["as"], &["name"], false),
    ("workspace", Some("repoint"), &["root", "as"], &[], false),
    ("dashboard", None, &["all"], &[], true),
    ("doctor", None, &["all"], &[], true),
    ("audit", Some("verify"), &["against"], &[], true),
    (
        "search",
        None,
        &[
            "source",
            "status",
            "tag",
            "lane",
            "after",
            "before",
            "all",
            "all-boards",
            "limit",
            "max-chars",
        ],
        &["query"],
        true,
    ),
    ("search-rebuild", None, &["as", "all-boards"], &[], false),
    ("serve", None, &["port", "actor-header"], &[], true),
    ("backup", None, &["output", "keep"], &[], false),
    (
        "archive",
        None,
        &["older-than-days", "as", "dry-run"],
        &[],
        false,
    ),
    (
        "deploy",
        Some("start"),
        &[
            "task",
            "repo",
            "commit",
            "branch",
            "tier",
            "environment",
            "host",
            "url",
            "mechanism",
            "operation-id",
            "retry-of",
            "as",
            "lane",
        ],
        &[],
        false,
    ),
    (
        "deploy",
        Some("finish"),
        &[
            "token",
            "result",
            "phase",
            "served-commit",
            "receipt",
            "artifact-uri",
            "as",
        ],
        &["id"],
        false,
    ),
    (
        "deploy",
        Some("abandon"),
        &["token", "force", "note", "as"],
        &["id"],
        false,
    ),
    ("deploy", Some("show"), &[], &["id"], true),
    (
        "deploy",
        Some("list"),
        &["status", "tier", "limit", "all"],
        &[],
        true,
    ),
    ("deploy", Some("current"), &[], &[], true),
    (
        "subscription",
        Some("add"),
        &[
            "id",
            "subject",
            "relation",
            "kind",
            "prior-status",
            "current-status",
            "tag",
            "consumer",
            "action",
            "timeout-ms",
            "max-retries",
            "rate-per-minute",
            "max-concurrency",
            "secret-ref",
            "as",
        ],
        &[],
        false,
    ),
    (
        "subscription",
        Some("list"),
        &["status", "consumer", "all"],
        &[],
        true,
    ),
    ("subscription", Some("show"), &[], &["id"], true),
    ("subscription", Some("pause"), &["as"], &["id"], false),
    ("subscription", Some("resume"), &["as"], &["id"], false),
    ("restore", None, &["from", "force", "as"], &[], false),
    (
        "task",
        Some("add"),
        &[
            "tag",
            "as",
            "body-file",
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
        &["title"],
        false,
    ),
    (
        "task",
        Some("list"),
        &[
            "status",
            "with-relations",
            "tag",
            "lane",
            "fields",
            "no-body",
            "all",
        ],
        &[],
        true,
    ),
    ("task", Some("show"), &[], &["id"], true),
    (
        "task",
        Some("move"),
        &["as", "metadata-patch-json", "force"],
        &["id", "status"],
        false,
    ),
    ("task", Some("remove"), &["as", "force"], &["id"], false),
    (
        "task",
        Some("metadata"),
        &["as", "patch-json"],
        &["id"],
        false,
    ),
    (
        "task",
        Some("update"),
        &[
            "clear-tags",
            "tag",
            "body-file",
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
        &["id"],
        false,
    ),
    (
        "story",
        Some("advance"),
        &["as", "to", "reviewer", "committer"],
        &["id"],
        false,
    ),
    ("story", Some("signoff"), &["as", "note"], &["id"], false),
    ("story", Some("unsignoff"), &["as", "note"], &["id"], false),
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
            "candidates",
            "tag",
            "limit",
        ],
        &["?id"],
        false,
    ),
    (
        "heartbeat",
        None,
        &["lease", "lease-minutes"],
        &["id"],
        false,
    ),
    ("release", None, &["lease", "keep-status"], &["id"], false),
    ("note", None, &["as", "kind"], &["id", "text"], false),
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
        &["id"],
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
            "priority",
        ],
        &["?id"],
        false,
    ),
    (
        "handoff",
        Some("list"),
        &["task", "status", "to", "all"],
        &[],
        true,
    ),
    (
        "handoff",
        Some("accept"),
        &["as", "session", "lease-minutes", "caller-scope"],
        &["id"],
        false,
    ),
    (
        "import",
        Some("atmux-json"),
        &["as", "reconcile", "force", "dry-run", "verify"],
        &["path"],
        false,
    ),
    (
        "import",
        Some("atmux-sqlite"),
        &["as", "reconcile", "force", "dry-run", "verify"],
        &["path"],
        false,
    ),
    ("schema", None, &[], &[], true),
    ("mcp", None, &[], &[], false),
    ("tag", Some("add"), &["as", "description"], &["name"], false),
    ("tag", Some("list"), &[], &[], true),
    ("tag", Some("remove"), &["as", "force"], &["name"], false),
    (
        "rule",
        Some("add"),
        &["as", "body", "body-file", "board", "except-board", "tag"],
        &["?body"],
        false,
    ),
    ("rule", Some("list"), &["all", "full"], &[], true),
    ("rule", Some("show"), &[], &["id"], true),
    (
        "rule",
        Some("update"),
        &[
            "as",
            "body",
            "body-file",
            "board",
            "except-board",
            "tag",
            "clear-tags",
        ],
        &["id"],
        false,
    ),
    ("rule", Some("retire"), &["as"], &["id"], false),
    (
        "rule",
        Some("export"),
        &["as", "board", "output"],
        &[],
        false,
    ),
    ("rule", Some("import"), &["as"], &["path"], false),
    ("rule", Some("consolidate"), &["as"], &[], false),
    (
        "attention",
        Some("raise"),
        &["as", "kind", "task", "priority", "tag"],
        &["text"],
        false,
    ),
    (
        "attention",
        Some("list"),
        &[
            "status", "kind", "task", "tag", "lane", "limit", "fields", "no-body", "all",
        ],
        &[],
        true,
    ),
    (
        "attention",
        Some("update"),
        &["as", "body", "body-file", "tag", "clear-tags"],
        &["id"],
        false,
    ),
    (
        "attention",
        Some("resolve"),
        &["as", "note"],
        &["id"],
        false,
    ),
    ("attention", Some("reopen"), &["as", "note"], &["id"], false),
    (
        "sitrep",
        Some("post"),
        &["as", "lane", "task"],
        &["text"],
        false,
    ),
    (
        "sitrep",
        Some("list"),
        &["lane", "task", "limit", "all"],
        &[],
        true,
    ),
    (
        "events",
        None,
        &[
            "task", "rule", "registry", "kind", "after", "before", "limit", "all",
        ],
        &[],
        true,
    ),
    ("stale", None, &[], &[], true),
    (
        "watch",
        None,
        &[
            "task",
            "rule",
            "registry",
            "kind",
            "relation",
            "prior-status",
            "current-status",
            "tag",
            "cursor",
            "follow",
            "all",
            "limit",
        ],
        &[],
        true,
    ),
    ("context", None, &["max-chars"], &["id"], true),
    ("todo", None, &["output"], &[], false),
];

/// The flags a command accepts, and the positionals it takes after its own
/// name, in order. A leading `?` marks one the command can do without.
///
/// The arity the parser enforces is derived from those names rather than
/// stored beside them, so a command cannot declare that it takes two arguments
/// and then name three.
fn command_spec(
    command: &str,
    sub: Option<&str>,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    COMMANDS
        .iter()
        .find(|(name, expected, ..)| *name == command && *expected == sub)
        .map(|(_, _, flags, positionals, _)| (*flags, *positionals))
}

/// The most positionals an invocation may hold, counting the words that name it.
fn arity(sub: Option<&str>, positionals: &[&str]) -> usize {
    1 + usize::from(sub.is_some()) + positionals.len()
}

/// Commands whose second positional is a subcommand rather than an id.
const SUBCOMMAND_GROUPS: [&str; 12] = [
    "task",
    "story",
    "handoff",
    "import",
    "workspace",
    "attention",
    "tag",
    "rule",
    "sitrep",
    "audit",
    "deploy",
    "subscription",
];

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
        "att" | "attn" => "attention",
        "sr" => "sitrep",
        "ev" => "events",
        "dash" => "dashboard",
        "rel" => "release",
        "n" => "note",
        "r" => "rule",
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
        ("workspace", "adopt") => "adopt",
        ("workspace", "det") => "detach",
        ("tag", "ls") => "list",
        ("tag", "new") => "add",
        ("tag", "rm") => "remove",
        ("rule", "ls") => "list",
        ("rule", "new") => "add",
        ("rule", "up") => "update",
        ("rule", "cat") => "show",
        ("sitrep", "ls") => "list",
        ("sitrep", "new") => "post",
        ("deploy", "ls") => "list",
        ("deploy", "cat") => "show",
        ("subscription", "ls") => "list",
        ("subscription", "new") => "add",
        ("subscription", "cat") => "show",
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
            } else if BOOLEAN.contains(&name) || DEPRECATED_BOOLEAN.contains(&name) {
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
    fn single(&self, name: &str) -> Result<Option<&str>> {
        match self.flags.get(name) {
            None => Ok(None),
            Some(values) if values.len() == 1 => Ok(values.first().map(String::as_str)),
            Some(_) => bail!("--{name} may be given at most once"),
        }
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

    /// Priority accepts the durable numeric key and the canonical operator
    /// vocabulary. Symbols store their band anchor; explicit integers retain
    /// controlled within-band ordering for compatibility.
    fn optional_priority(&self) -> Result<Option<i64>> {
        let Some(raw) = self.one("priority") else {
            return Ok(None);
        };
        match raw.to_ascii_lowercase().as_str() {
            "p0" => Ok(Some(0)),
            "p1" => Ok(Some(3)),
            "p2" => Ok(Some(6)),
            _ => raw.parse::<i64>().map(Some).map_err(|_| {
                anyhow::anyhow!(
                    "--priority must be P0, P1, P2, or an integer from 0 through 9, got {raw:?}"
                )
            }),
        }
    }

    fn priority(&self, fallback: i64) -> Result<i64> {
        Ok(self.optional_priority()?.unwrap_or(fallback))
    }

    /// The body text, from `--body` or from a file.
    ///
    /// A plan is an epic's body, and a plan is markdown measured in kilobytes.
    /// Passing that on a command line works and is miserable, so `--body-file`
    /// reads it from disk instead.
    ///
    /// Giving both is refused rather than ranked: they are two answers to one
    /// question, only one is what the caller meant, and nothing in the receipt
    /// would say which was stored — the same rule the board selectors follow.
    fn body(&self) -> Result<Option<String>> {
        match (self.one("body"), self.one("body-file")) {
            (Some(_), Some(_)) => bail!(
                "--body and --body-file both give the body; pass one, because \
                 picking between them is not something a receipt can explain"
            ),
            (Some(text), None) => Ok(Some(text.to_owned())),
            (None, Some(path)) => {
                let text =
                    fs::read_to_string(path).with_context(|| format!("read body from {path}"))?;
                Ok(Some(text))
            }
            (None, None) => Ok(None),
        }
    }

    /// `--limit`, refusing a value SQL would read as the opposite of a bound.
    ///
    /// `LIMIT -1` means *no limit* in SQLite, so `--limit -1` returned every
    /// row a caller had explicitly asked to bound, and reported success. That
    /// is the same defect as a `--max-chars` that is accepted and ignored: the
    /// caller asked for a bounded answer, got an unbounded one, and has no way
    /// to tell from the result.
    ///
    /// Every other numeric flag here already carries a band — `--priority`,
    /// `--max-chars`, `--keep`, `--lease-minutes`, `--stale-minutes`. This was
    /// the one that did not, which is why it is a helper rather than a check
    /// repeated at each call site.
    ///
    /// Zero is allowed. It asks for nothing and returns nothing, which is
    /// exactly what it says; a script computing a limit that comes out zero is
    /// not making a mistake the way a negative one is.
    fn limit(&self, fallback: i64) -> Result<i64> {
        let value = self.integer("limit", fallback)?;
        if value < 0 {
            bail!(
                "--limit must be zero or more, got {value}: a negative limit reads as no limit at all"
            );
        }
        Ok(value)
    }

    /// The TCP port `serve` listens on, bounded to the real range.
    ///
    /// A port outside 1-65535 cannot be bound, and 0 asks the kernel to choose
    /// one — which for a server nginx reaches by number means listening
    /// somewhere nobody can find. Both are refused here rather than turning
    /// into an opaque bind failure, or worse a server that starts and is
    /// unreachable. Privileged ports are allowed: this binds loopback and the
    /// operator may have reason to.
    fn port(&self, fallback: u16) -> Result<u16> {
        let value = self.integer("port", fallback as i64)?;
        u16::try_from(value)
            .ok()
            .filter(|port| *port != 0)
            .with_context(|| {
                format!(
                    "--port must be between 1 and 65535, got {value}: port 0 asks the \
                     kernel to pick one, and nginx reaches this server by number"
                )
            })
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

    /// Fail when a command was not given a positional it cannot work without.
    ///
    /// This has to run in the parse phase, not where the value is read. Every
    /// command that takes a positional read it *after* `open_store`, so
    /// `task add --db /new/board.db` with no title created and migrated a
    /// 372736-byte board — and every parent directory above it — and only then
    /// reported that the title was missing. A command that fails must leave
    /// nothing behind, and the only way to guarantee that is to refuse before
    /// anything is opened.
    ///
    /// The names come from `COMMANDS`, the same row the arity check reads, so a
    /// command cannot declare a positional here and forget to require it.
    fn reject_missing_positionals(&self, words: &[&str], positionals: &[&str]) -> Result<()> {
        let supplied = self.positionals.len().saturating_sub(words.len());
        let missing = positionals
            .iter()
            .enumerate()
            .filter(|(index, name)| *index >= supplied && !name.starts_with('?'))
            .map(|(_, name)| (*name).to_owned())
            .collect::<Vec<_>>();
        if missing.is_empty() {
            return Ok(());
        }
        let usage = positionals
            .iter()
            .map(|name| match name.strip_prefix('?') {
                Some(optional) => format!("[{}]", optional.to_uppercase()),
                None => name.to_uppercase(),
            })
            .collect::<Vec<_>>()
            .join(" ");
        bail!(
            "{} is required.\nusage: kanban {} {usage}",
            missing.join(" and "),
            words.join(" ")
        );
    }

    /// Fail on a single-valued flag given more than once.
    ///
    /// Taking the last occurrence is a common convention and the wrong one
    /// here: the values disagree, only one of them is what the caller meant,
    /// and nothing in the receipt says which was used.
    fn reject_repeated_for(&self, command: Option<&str>) -> Result<()> {
        let mut repeated = self
            .flags
            .iter()
            .filter(|(name, values)| {
                let repeatable = REPEATABLE.contains(&name.as_str())
                    || (command == Some("watch") && WATCH_REPEATABLE.contains(&name.as_str()))
                    || (command == Some("subscription")
                        && SUBSCRIPTION_REPEATABLE.contains(&name.as_str()));
                values.len() > 1 && !repeatable
            })
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
    ///
    /// `ignored` is the board selectors this command discards, from
    /// [`IGNORED_SELECTORS`]. They are global flags, so the exemption above
    /// would otherwise wave them through and — worse — the "accepted here" line
    /// would advertise `--db` on a `doctor` that refuses it.
    /// `reject_ignored_selectors` reaches them first with the better message;
    /// subtracting them here is the second lock, so a selector cannot be
    /// refused by one guard and offered by the other.
    fn reject_unknown(&self, allowed: &[&str], ignored: &[&str]) -> Result<()> {
        let mut unknown = self
            .flags
            .keys()
            .filter(|name| {
                !allowed.contains(&name.as_str())
                    && (!GLOBAL_FLAGS.contains(&name.as_str()) || ignored.contains(&name.as_str()))
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
            .filter(|name| !ignored.contains(name))
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
pub(crate) fn nearest<'a>(value: &str, candidates: &[&'a str]) -> Option<&'a str> {
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
    emit(&serde_json::to_string_pretty(value)?)
}

/// Write one line to stdout, and let a closed pipe be a normal ending.
///
/// `println!` panics when the reader has gone — `kb task list --json | head`
/// printed a Rust panic and a backtrace note over the output it had just
/// produced, and exited non-zero, so a shell pipeline could not tell a closed
/// pipe from a real failure. Every Unix tool ends quietly when its reader
/// leaves; the error is returned here and recognised in [`entrypoint`].
fn emit(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut out = io::stdout().lock();
    writeln!(out, "{text}")?;
    // Explicit, because a buffered line lost at exit is output that was
    // reported as written and never arrived.
    out.flush()?;
    Ok(())
}

/// Whether an error is only "the reader hung up".
fn reader_left(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|io| io.kind() == io::ErrorKind::BrokenPipe)
}

/// Whether the command line asked for `--json`.
///
/// Read from the raw arguments rather than from [`Args`], because the error
/// this decides the shape of may be the parse itself failing. Recognises the
/// two spellings [`Args::parse`] does -- `--json` and `--json=VALUE` -- and
/// nothing looser, so an abbreviation the parser would refuse is not treated
/// as a request here.
fn json_requested() -> bool {
    env::args()
        .skip(1)
        .any(|argument| argument == "--json" || argument.starts_with("--json="))
}

fn cwd() -> Result<PathBuf> {
    env::current_dir().context("read current directory")
}

/// Comma-listed project names, for error messages. Empty when the registry is
/// empty, so a first-run error is not padded with a pointless "known projects:".
fn known_projects(registry: &Registry) -> Result<String> {
    let names = registry
        .projects_active()?
        .into_iter()
        .map(|project| project.name)
        .collect::<Vec<_>>();
    Ok(if names.is_empty() {
        String::new()
    } else {
        format!("\nknown projects: {}", names.join(", "))
    })
}

pub(crate) fn project_candidates(projects: &[ProjectRecord]) -> String {
    projects
        .iter()
        .map(|project| {
            let retired = if project.archived {
                match project
                    .archived_note
                    .as_deref()
                    .map(str::trim)
                    .filter(|note| !note.is_empty())
                {
                    Some(note) => format!(" (retired: {note})"),
                    None => " (retired)".to_owned(),
                }
            } else {
                String::new()
            };
            if project.workspace_roots.is_empty() {
                format!("{} (rootless){retired}", project.name)
            } else {
                format!(
                    "{} [{}]{retired}",
                    project.name,
                    project.workspace_roots.join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
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
        [] => {
            let retired = registry.by_name_all(name)?;
            match retired.as_slice() {
                [] => bail!(
                    "no Kanban project named {name}{}",
                    known_projects(registry)?
                ),
                [project] => bail!(
                    "{}",
                    retired_board_message(
                        &project.name,
                        project.archived_note.as_deref(),
                        "addressing it"
                    )
                ),
                many => bail!(
                    "{} retired Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                    many.len(),
                    project_candidates(many)
                ),
            }
        }
        many => bail!(
            "{} Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
            many.len(),
            project_candidates(many)
        ),
    }
}

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
pub(crate) fn schema() -> Value {
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
                    let kind = if REPEATABLE.contains(flag)
                        || (*command == "watch" && WATCH_REPEATABLE.contains(flag))
                        || (*command == "subscription" && SUBSCRIPTION_REPEATABLE.contains(flag))
                    {
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
                "longRunning": LONG_RUNNING.contains(command),
                "flags": flags,
                // Named and in order, so an adapter can build an argument
                // list rather than guess at what the slots mean. A leading
                // `?` marks one the command can do without.
                "positionals": positionals,
                "readOnly": read_only,
                // Distinct from `readOnly`, which asks whether the operation
                // writes anything anywhere. This asks the narrower question the
                // board resolver actually needs: may naming a `--db` path that
                // is not there bring one into existence.
                "createsBoard": board_creation(command, *sub) == BoardCreation::Permitted,
                // The board selectors this operation refuses. Every other
                // command honours all three, so an adapter can offer them
                // everywhere this list is empty and nowhere it is not.
                "ignoredSelectors": ignored_selectors(command, *sub).0,
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

/// Where this invocation is standing, as git sees it.
///
/// Resolved once per command from the working directory and handed to whatever
/// records provenance. It is the *caller's* location, not the addressed
/// project's: for a driver lane those differ, and the lane is the answer to
/// "where is this work happening".
fn here() -> Option<gitctx::GitContext> {
    gitctx::resolve(&cwd().ok()?)
}

/// Whether this invocation may bring a board file into existence.
///
/// Deliberately not derived from `readOnly`. That bit answers a different
/// question — whether an operation writes *anything, anywhere* — which is why
/// `backup` and `todo` are not read-only despite changing no work state. Ask it
/// about board creation and it answers about file writes, and the two diverge
/// exactly where it hurts: `todo` and `archive --dry-run` are both `readOnly:
/// false`, both accept `--db`, and both stood a 372736-byte board up at a
/// mistyped path and exited 0 — `archive` from a flag whose whole promise is to
/// change nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoardCreation {
    /// The command's purpose is to put work state into the board it names, so
    /// naming a path that is not there is a request to start one.
    Permitted,
    /// A board file that is not there is reported, never conjured.
    Refused,
}

/// The only commands that may bring a board file into existence.
///
/// An allowlist rather than a sixth column on [`CommandRow`], because the two
/// shapes fail differently. A per-row bit is forty-five bits of which
/// forty-four must be `false`, and the way it breaks is someone copying a
/// neighbouring row and inheriting a `true` — silently, in the dangerous
/// direction. Absence from a list cannot be got wrong that way: a command
/// that is not written here cannot create anything, including a command that
/// does not exist yet, and including one whose author never read this file.
///
/// `task add` is the entry because it is the only command whose point is to put
/// the first work state into a board. Everything else addresses rows that would
/// have to be there already — you cannot usefully `claim`, `move`,
/// `checkpoint`, `archive` or `todo` a board into being. `init` is absent
/// because it never consults `--db` at all; it creates through the registry.
///
/// Widening this is a deliberate act, and `the_only_board_creator_is_declared`
/// fails until the new entry is written down in the test too.
const BOARD_CREATORS: [(&str, Option<&str>); 1] = [("task", Some("add"))];

/// Which of the selectors this invocation addresses a board with.
///
/// Resolved in exactly one place so the store, the read-only store, the
/// board-name lookup and the data-root lock can never disagree about what was
/// addressed.
///
/// Every flag the caller typed outranks every environment default. A default is
/// what applies when nothing was asked for, so reading one ahead of a flag lets
/// the environment silently outvote the command line: with `KANBAN_DB` set,
/// `task list --project alpha` read the environment's file, reported `[]`, and
/// created that file on the way. Two typed flags never reach here —
/// `reject_conflicting_board_selectors` refuses that command line first — so
/// this order only ever resolves a flag against a default, or a flag against the
/// working directory, and neither of those is a second request.
enum BoardSelection {
    /// A board file named straight by path, bypassing the registry entirely.
    ///
    /// `explicit` separates `--db PATH`, which is how a board outside the
    /// registry is made, from `KANBAN_DB`, which is only a default for it.
    Db { path: PathBuf, explicit: bool },
    /// A registered project by name, addressable from anywhere.
    Project(String),
    /// `--workspace PATH`, or the working directory when nothing named a board.
    Workspace(Option<PathBuf>),
}

fn board_selection(args: &Args) -> BoardSelection {
    if let Some(path) = args.one("db") {
        return BoardSelection::Db {
            path: PathBuf::from(path),
            explicit: true,
        };
    }
    if let Some(name) = args.one("project") {
        return BoardSelection::Project(name.to_owned());
    }
    if let Some(workspace) = args.one("workspace") {
        return BoardSelection::Workspace(Some(PathBuf::from(workspace)));
    }
    if let Some(path) = env::var_os("KANBAN_DB") {
        return BoardSelection::Db {
            path: PathBuf::from(path),
            explicit: false,
        };
    }
    if let Some(name) = env::var("KANBAN_PROJECT")
        .ok()
        .filter(|value| !value.is_empty())
    {
        return BoardSelection::Project(name);
    }
    BoardSelection::Workspace(None)
}

/// A board named straight by path, bypassing the registry entirely.
///
/// Read by both the board resolver and the data-root lock, so the two can
/// never disagree about whether an invocation is registry-addressed.
fn direct_db(args: &Args) -> Option<PathBuf> {
    direct_board(args).map(|(path, _)| path)
}

/// The same board, with whether the caller named it or the environment did.
///
/// `watch` needs both halves: it opens the path itself rather than going
/// through `store_path_readonly`, so without the origin it cannot tell a
/// mistyped `KANBAN_DB` from a `--db` the caller typed.
fn direct_board(args: &Args) -> Option<(PathBuf, bool)> {
    match board_selection(args) {
        BoardSelection::Db { path, explicit } => Some((path, explicit)),
        BoardSelection::Project(_) | BoardSelection::Workspace(_) => None,
    }
}

/// The commands from [`BOARD_CREATORS`], for an error that names the way out.
fn board_creator_names() -> String {
    BOARD_CREATORS
        .iter()
        .map(|(command, sub)| match sub {
            Some(sub) => format!("{command} {sub}"),
            None => (*command).to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether this command may create the board it names.
///
/// Fails closed twice over: a command absent from [`BOARD_CREATORS`] cannot
/// create, and so cannot a command absent from `COMMANDS` entirely. That second
/// case is unreachable today — `command_spec` refuses an unknown command a few
/// lines after this is called — but it is guaranteed here rather than left to
/// the order two statements happen to be in.
fn board_creation(command: &str, sub: Option<&str>) -> BoardCreation {
    if BOARD_CREATORS.contains(&(command, sub)) {
        BoardCreation::Permitted
    } else {
        BoardCreation::Refused
    }
}

/// Refuse to conjure a board file nobody asked for.
///
/// Opening a board creates it, and ADR-008 records that as the aggravating half
/// of the wrong-board defect, not as a feature: a `--db` path that did not exist
/// was "conjured empty and answered from", so the caller got a board with
/// nothing in it and an exit status of zero. Creation therefore needs all three
/// of: the file is absent, the caller named the path themselves, and the command
/// is one whose purpose is to put work state there.
///
/// Each missing condition is its own defect. A command that only reports
/// creating its subject answers from the file it just made — `doctor` did that
/// to a registered board and certified the result healthy, and `todo` and
/// `archive --dry-run` did it to a path. And `KANBAN_DB` is a default, not a
/// request: one inherited from a parent process or mistyped in a profile
/// standing a board up as a side effect is the wrong-board write ADR-007 exists
/// to prevent, reached without anyone naming the file on the command line.
fn require_board_file(path: &Path, explicit: bool, creation: BoardCreation) -> Result<()> {
    match board_file(&path.to_string_lossy()) {
        BoardFile::Board => return Ok(()),
        // Refused for every command, `task add` included. Permission to start a
        // board where there is nothing is not permission to overwrite a file
        // that is already there.
        BoardFile::Foreign => return Err(foreign_board_error(&path.to_string_lossy())),
        BoardFile::Unreadable(reason) => {
            return Err(unreadable_board_error(&path.to_string_lossy(), &reason));
        }
        // An interrupted creation left a database with nothing in it. Finishing
        // the job is the recovery, and it needs the same two conditions
        // creating one does: the caller typed the path, and the command is one
        // whose purpose is to put work state there. Anything else reports,
        // because otherwise a stale `KANBAN_DB` silently adopts the wreckage.
        BoardFile::Unfinished => {
            return match (creation, explicit) {
                (BoardCreation::Permitted, true) => Ok(()),
                _ => Err(unfinished_board_error(&path.to_string_lossy())),
            };
        }
        BoardFile::Absent => {}
    }
    // The environment case first: when both apply, the actionable half is that
    // nobody typed this path.
    if !explicit {
        bail!(
            "KANBAN_DB names {}, which does not exist, and an environment default is not a \
             request to create a board.\n\
             Create it deliberately:  kanban {} ... --db {}\n\
             Or address another one:  --project NAME, or unset KANBAN_DB.",
            path.display(),
            board_creator_names(),
            path.display()
        );
    }
    match creation {
        BoardCreation::Permitted => Ok(()),
        BoardCreation::Refused => bail!(
            "board file {} does not exist, and this command never creates one: it would answer \
             from a board it had just made, which is indistinguishable from the empty board you \
             meant.\n\
             Address an existing board with --project NAME, or start this one with: kanban {} \
             ... --db {}",
            path.display(),
            board_creator_names(),
            path.display()
        ),
    }
}

/// Board selection, most explicit first:
///   1. `--db PATH`           — a board file directly
///   2. `--project NAME`      — a registered project by name, from anywhere
///   3. `--workspace PATH`    — the project containing PATH
///   4. `KANBAN_DB`           — the default for (1)
///   5. `KANBAN_PROJECT`      — the default for (2)
///   6. the current directory — the project containing it
///
/// Flags come before defaults, not interleaved with them: see [`BoardSelection`]
/// for why an environment default must never outrank a typed flag.
///
/// (2) and (3) are what make the CLI usable outside a registered tree: an agent
/// in an unrelated cage, a cron line, or any shell in $HOME can address a board
/// without cd-ing into it.
fn store_path(args: &Args, creation: BoardCreation) -> Result<PathBuf> {
    match board_selection(args) {
        BoardSelection::Db { path, explicit } => {
            require_board_file(&path, explicit, creation)?;
            if path.exists()
                && let Some(BoardPathState::Retired { name, note }) =
                    Registry::board_path_state_if_available(&path)?
            {
                bail!(
                    "{}",
                    retired_board_message(&name, note.as_deref(), "addressing it")
                );
            }
            Ok(path)
        }
        BoardSelection::Project(name) => {
            let registry = Registry::open()?;
            let path = board_by_name(&registry, &name)?;
            require_registered_board(&path.to_string_lossy())?;
            Ok(path)
        }
        BoardSelection::Workspace(workspace) => {
            let mut registry = Registry::open()?;
            let workspace = match workspace {
                Some(path) => path,
                None => cwd()?,
            };
            if let Some(record) = registry.resolve(&workspace)? {
                require_registered_board(&record.board_path)?;
                return Ok(PathBuf::from(record.board_path));
            }
            bail!(
                "no Kanban project contains {}; address one from anywhere with --project NAME or KANBAN_PROJECT, or run 'kanban init' there{}",
                workspace.display(),
                known_projects(&registry)?
            )
        }
    }
}

/// Resolve a board without updating registry recency or migrating either DB.
fn store_path_readonly(args: &Args) -> Result<PathBuf> {
    let path = match board_selection(args) {
        BoardSelection::Db { path, explicit } => {
            // Nothing reached through here writes, so a missing file is
            // reported whichever selector named it.
            require_board_file(&path, explicit, BoardCreation::Refused)?;
            if path.exists()
                && let Some(BoardPathState::Retired { name, note }) =
                    Registry::board_path_state_if_available(&path)?
            {
                bail!(
                    "{}",
                    retired_board_message(&name, note.as_deref(), "addressing it")
                );
            }
            return Ok(path);
        }
        BoardSelection::Project(name) => {
            let registry = Registry::open_readonly()?;
            let matches = registry.by_name(&name)?;
            match matches.as_slice() {
                [project] => PathBuf::from(&project.board_path),
                [] => {
                    let retired = registry.by_name_all(&name)?;
                    match retired.as_slice() {
                        [] => bail!(
                            "no Kanban project named {name}{}",
                            known_projects(&registry)?
                        ),
                        [project] => bail!(
                            "{}",
                            retired_board_message(
                                &project.name,
                                project.archived_note.as_deref(),
                                "addressing it"
                            )
                        ),
                        many => bail!(
                            "{} retired Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                            many.len(),
                            project_candidates(many)
                        ),
                    }
                }
                many => bail!(
                    "{} Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                    many.len(),
                    project_candidates(many)
                ),
            }
        }
        BoardSelection::Workspace(workspace) => {
            let registry = Registry::open_readonly()?;
            let workspace = match workspace {
                Some(path) => path,
                None => cwd()?,
            };
            registry
                .resolve_readonly(&workspace)?
                .map(|record| PathBuf::from(record.board_path))
                .with_context(|| {
                    format!(
                        "no Kanban project contains {}; address one from anywhere with --project NAME or KANBAN_PROJECT{}",
                        workspace.display(),
                        known_projects(&registry).unwrap_or_default()
                    )
                })?
        }
    };
    require_registered_board(&path.to_string_lossy())?;
    Ok(path)
}

/// The 16 bytes every SQLite database begins with.
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// What is actually at a board path.
///
/// Existence was the wrong question. `is_file` says yes to any file, and
/// opening a board *migrates* it, so a path that named something else was
/// rewritten into a database: `task list --db notes.txt` against a 0-byte file
/// answered `[]` and left 372736 bytes of SQLite where the operator's file had
/// been. Two harms in one command — a plausible wrong answer, and a file
/// destroyed — and no way to tell it from the empty board they meant.
///
/// "Is this SQLite" was not enough either, and the gap was worse than the one
/// it closed. `migrate` starts from the file's own `PRAGMA user_version`, so a
/// stranger's database at version 0 gets the whole ladder run into it: an
/// 8192-byte browser database came back at 376832 bytes, `user_version` 23,
/// with 26 kanban tables grafted alongside its own. The question has to be "is
/// this *our* board", so the last step looks for the schema.
///
/// The order is three widening steps, each cheaper than the next, so the common
/// answers cost the least: stat, sixteen bytes, then SQLite.
enum BoardFile {
    /// Nothing there. For a `--db` path on a creating command this is the
    /// ordinary way a new board starts, so it is not by itself an error.
    Absent,
    /// Something there that is not a Kanban board — a zero-length file, a
    /// directory, a text file, or another application's database.
    /// Never opened, never migrated, never overwritten.
    Foreign,
    /// Something there that cannot be read, carrying the reason.
    ///
    /// Distinct from `Foreign` because saying "this is not a Kanban board"
    /// about an intact board at mode 000 — or about one whose WAL is being
    /// checkpointed by another agent's exiting command — is simply a false
    /// statement, and it points the operator at the wrong problem.
    Unreadable(String),
    /// A database with no tables: an interrupted board creation, and nothing
    /// else. Safe to finish, because there is nothing in it to lose.
    Unfinished,
    /// A Kanban board.
    Board,
}

fn board_file(board_path: &str) -> BoardFile {
    let path = Path::new(board_path);
    // stat before open. `open(O_RDONLY)` on a FIFO blocks until a writer
    // appears and Rust passes no `O_NONBLOCK`, so one FIFO among the registered
    // boards would hang `doctor`, `backup` and every other survey partway
    // through, with no output and no timeout. `is_file` is false for a FIFO, a
    // directory and a socket alike, and answers in microseconds.
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return BoardFile::Foreign,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return BoardFile::Absent,
        Err(error) => return BoardFile::Unreadable(error.to_string()),
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return BoardFile::Unreadable(error.to_string()),
    };
    let mut header = [0u8; SQLITE_MAGIC.len()];
    // A file shorter than the header cannot be a database, so an empty one is
    // rejected here without a special case.
    if file.read_exact(&mut header).is_err() || &header != SQLITE_MAGIC {
        return BoardFile::Foreign;
    }
    match db::probe_board_schema(path) {
        Ok(db::BoardSchema::Board) => BoardFile::Board,
        Ok(db::BoardSchema::Unwritten) => BoardFile::Unfinished,
        Ok(db::BoardSchema::Other) => BoardFile::Foreign,
        // A SQLite failure is not evidence about what the file holds. Saying
        // "not a board" here would be the `Unreadable` lie again, arriving
        // through a lock rather than a permission bit — and transient, so it
        // would read as a flake rather than as the false refusal it is.
        Err(error) => BoardFile::Unreadable(error.to_string()),
    }
}

/// What a survey should say about one registered board.
///
/// Opening a board creates it, which is right for `--db` on a creating command
/// — that is how a board outside the registry is made — and wrong for one the
/// registry already knows about. A registered board file that has gone missing
/// was destroyed, and standing an empty one up in its place turns recoverable
/// data loss into a board that reports itself fine. This check exists because
/// `doctor` did exactly that: it recreated the file it was asked to inspect,
/// then certified the result healthy.
///
/// Commands that do work on one board refuse. Commands that survey every board
/// — `doctor`, `dashboard`, `backup` — report the gap and carry on, because
/// dying on the first missing board is no use to whoever has to fix it, and
/// `restore` would otherwise be unable to repair the very thing that stops it
/// from running. A registered path holding something that is not a database
/// counts as a gap for them too, which is the honest answer: it is not a board
/// they can read, and a survey that dies on it helps nobody.
///
/// The answer is three-way because the boolean this replaced could not tell
/// "the data is gone" from "this process could not open it". Measured on the
/// same board and the same command, a board at mode 000 with its data intact
/// and a board that had been deleted produced byte-identical receipts. The move
/// an operator makes after reading `missing` is to restore a snapshot over the
/// path — so on the unreadable board, the receipt was the cause of the data
/// loss. Boards are created `0600`, which makes one written by another user
/// unreadable and perfectly healthy at once.
///
/// This reads [`board_file`], the same classifier the single-board resolvers
/// use, rather than a second one beside it: two classifiers of the same
/// question drift, and the drift is what produced this.
enum SurveyBoard {
    /// Opened, and it holds a board. The survey reads it.
    Readable,
    /// There, and this process could not look inside it. The reason travels
    /// with it, because `Permission denied` and a locked database are different
    /// problems with different fixes. Never reported as missing: nothing here
    /// is evidence that anything is wrong with the data.
    Unreadable(String),
    /// Nothing at the path, or something there that is not a board.
    Missing,
}

fn survey_board(board_path: &str) -> SurveyBoard {
    match board_file(board_path) {
        BoardFile::Board => SurveyBoard::Readable,
        BoardFile::Unreadable(reason) => SurveyBoard::Unreadable(reason),
        // `Foreign` and `Unfinished` keep the bucket they have always had.
        // Neither is a board a survey can read, and separating them is a
        // different question from this one.
        BoardFile::Absent | BoardFile::Foreign | BoardFile::Unfinished => SurveyBoard::Missing,
    }
}

/// One row of the `unreadableBoards` list the surveys carry.
fn unreadable_board(project: &ProjectRecord, reason: String) -> UnreadableBoard {
    UnreadableBoard {
        name: project.name.clone(),
        board_path: project.board_path.clone(),
        reason,
    }
}

/// Resolve a path as far as the filesystem allows, so two spellings of one file
/// compare equal. A path that does not exist yet cannot be canonicalized and
/// compares by its literal form, which is the right answer for a destination.
fn resolved(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

/// What `restore` will find at one path it is about to rename over.
///
/// The question is "can this file be copied out of the way", not "is this a
/// healthy board". Those come apart exactly where it matters: a board with
/// corrupt pages, or a damaged 16-byte header, is the disaster `restore` exists
/// to recover from, and refusing to run because SQLite cannot parse it blocks
/// the one command that would fix it. That refusal was measured — `database
/// disk image is malformed`, followed by advice to check permissions that were
/// never wrong.
///
/// So the split is by what a copy needs. Anything readable can be moved out of
/// the way, so the restore proceeds; only a file this process cannot read at
/// all is genuinely unrescuable. That also makes the refusal's "very likely
/// intact — could not open it to look" a true sentence for every case that
/// still reaches it, rather than a guess that was wrong for corruption.
enum RestoreTarget {
    /// Nothing to lose. Either no file, or a database with no tables in it —
    /// an interrupted board creation, which holds no rows by definition.
    Vacant,
    /// A board SQLite can open. Copied with the online backup API, which is
    /// WAL-correct: a byte copy of the `.db` alone would drop commits still
    /// sitting in the write-ahead log, and those are exactly the recent work
    /// the rescue copy exists to keep.
    Rescue,
    /// Readable bytes SQLite will not open as a board — corrupt pages, a
    /// damaged header, or a file that was never a board. Copied verbatim,
    /// carrying why it would not open.
    Copy(String),
    /// The bytes cannot be read, so nothing can copy it. The one case left that
    /// has to stop the restore.
    Blocked(String),
}

fn restore_target(path: &Path) -> RestoreTarget {
    match board_file(&path.to_string_lossy()) {
        BoardFile::Absent | BoardFile::Unfinished => RestoreTarget::Vacant,
        BoardFile::Board => RestoreTarget::Rescue,
        // `BoardFile::Foreign`'s contract is "Never opened, never migrated,
        // never overwritten", and it exists because `task list --db notes.txt`
        // once left 372736 bytes of SQLite where an operator's file had been.
        // Copying it verbatim keeps that promise where it counts — the file is
        // still there afterwards, in the rescue snapshot — without blocking the
        // restore. Blocking would be worse than it sounds: a board whose header
        // is damaged classifies as foreign too, so refusing here would refuse
        // the recovery of the very thing that broke. It is never replaced
        // silently; the receipt and the rescue manifest both name it.
        BoardFile::Foreign => RestoreTarget::Copy("not a Kanban board".to_owned()),
        BoardFile::Unreadable(reason) => match fs::File::open(path) {
            // SQLite would not open it, but the bytes are there to copy. A
            // rescue copy needs the file read, not parsed.
            Ok(_) => RestoreTarget::Copy(reason),
            Err(error) => RestoreTarget::Blocked(error.to_string()),
        },
    }
}

/// A registered board that is gone, no longer a board, or unreadable.
fn require_registered_board(board_path: &str) -> Result<()> {
    match board_file(board_path) {
        BoardFile::Board => Ok(()),
        // `init` commits the registry row before `Store::open` runs the
        // migrations, so an interrupt in that window leaves a *registered*
        // board with no tables in it. Refusing would strand it permanently, in
        // the registry, with no command able to open it. The next ordinary
        // command finishes the migrations, which is the recovery the registry
        // is already assuming; `store_path_readonly` still declines, and says
        // to run one ordinary command first.
        BoardFile::Unfinished => Ok(()),
        BoardFile::Absent => Err(missing_board_error(board_path)),
        BoardFile::Foreign => Err(foreign_board_error(board_path)),
        BoardFile::Unreadable(reason) => Err(unreadable_board_error(board_path, &reason)),
    }
}

/// Report what was observed, and both things that produce it.
///
/// The earlier wording called this "what an interrupted board creation leaves
/// behind" and offered "deleting it loses no work". Both sentences are false
/// while another process is inside `migrate`'s first transaction: under WAL
/// this probe sees last-committed state, so a creation happening right now
/// presents exactly as one abandoned an hour ago. Nothing distinguishes them
/// from here — not the table count, not the schema version, not a second look —
/// and the second sentence is destructive advice stated as fact.
///
/// So this says what it saw and names both causes. Deleting is mentioned only
/// with the condition that makes it safe, because the operator is the one who
/// can check it and this process cannot.
fn unfinished_board_error(board_path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "board file {board_path} is a database with no tables in it.\n\
         A board looks like this while it is being created, between the file appearing and its \
         first migration committing — so this is either a creation that was interrupted, or one \
         running in another process right now. From here the two are identical.\n\
         If one is in progress:  it will finish on its own; run the command again.\n\
         To finish it yourself:  kanban {} ... --db {board_path}\n\
         Before removing the file, confirm no other process is creating it.",
        board_creator_names()
    )
}

fn foreign_board_error(board_path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{board_path} exists but is not a Kanban board: it is not a SQLite database, or it is one \
         that does not carry a board's tables.\n\
         Opening it would migrate it into a board and leave its own contents inside the result, so \
         this refuses instead.\n\
         Name an existing board with --db, or a path that does not exist yet. (An empty file can \
         also be a board creation caught before it wrote anything — if you remove it, confirm \
         first that no other process is creating it.)"
    )
}

/// Say the file cannot be read, rather than something false about what it holds.
///
/// A board at mode 000 is still a board. Reporting it as "not a Kanban board"
/// is a false statement about intact data, and it sends the operator to look
/// for a corrupt file instead of a permission bit.
fn unreadable_board_error(board_path: &str, reason: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "board file {board_path} cannot be read: {reason}.\n\
         Nothing is wrong with the file as far as this can tell — it could not be opened to look. \
         Check its permissions and the directories above it."
    )
}

fn missing_board_error(board_path: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "board file {board_path} is registered but missing.\n\
         Recover it:      kanban restore --from SNAPSHOT --force\n\
         Or start over:   kanban init   (in the project, recreates it empty)"
    )
}

fn open_store(args: &Args, creation: BoardCreation) -> Result<Store> {
    Store::open(&store_path(args, creation)?)
}

/// The board selectors this command discards, and what it addresses instead.
///
/// Empty for every command that resolves a board, which is the whole rest of
/// the surface. Read from one table by the refusal, by `reject_unknown`, by the
/// manifest and by the MCP tool builder, so a flag cannot be refused by the CLI
/// and advertised by an adapter.
pub(crate) fn ignored_selectors(
    command: &str,
    sub: Option<&str>,
) -> (&'static [&'static str], &'static str) {
    IGNORED_SELECTORS
        .iter()
        .find(|(name, expected, ..)| *name == command && *expected == sub)
        .map(|(_, _, ignored, subject)| (*ignored, *subject))
        .unwrap_or((&[], ""))
}

/// Fail when a command line names a board the command will never look at.
///
/// `reject_unknown` already refuses a flag a command does not define. This is
/// the same refusal for one it defines and cannot honour: inapplicable rather
/// than unknown, and silently discarded rather than reported, which is the
/// worse of the two because the command then answers about something else and
/// exits zero. See [`IGNORED_SELECTORS`] for the measured `doctor` case.
///
/// Runs before the `version`, `schema` and `mcp` early returns and before the
/// data-root lock, because two of those answer without validating anything and
/// the lock itself reads the `--db` path being refused.
fn reject_ignored_selectors(args: &Args, command: &str, sub: Option<&str>) -> Result<()> {
    let (ignored, subject) = ignored_selectors(command, sub);
    let given = ignored
        .iter()
        .filter(|flag| args.flags.contains_key(**flag))
        .map(|flag| format!("--{flag}"))
        .collect::<Vec<_>>();
    if given.is_empty() {
        return Ok(());
    }
    let words = match sub {
        Some(sub) => format!("{command} {sub}"),
        None => command.to_owned(),
    };
    bail!(
        "{} {} a board, and `{words}` {subject}; remove it rather than have it \
         discarded, because a receipt computed about something else is read as an \
         answer about the board you named",
        given.join(" and "),
        if given.len() == 1 { "names" } else { "name" }
    )
}

fn reject_all_boards_selector(args: &Args) -> Result<()> {
    let explicit = BOARD_SELECTORS
        .iter()
        .filter(|name| args.flags.contains_key(**name))
        .map(|name| format!("--{name}"))
        .collect::<Vec<_>>();
    let mut selectors = explicit;
    if env::var_os("KANBAN_DB").is_some() {
        selectors.push("KANBAN_DB".to_owned());
    }
    if env::var_os("KANBAN_PROJECT").is_some() {
        selectors.push("KANBAN_PROJECT".to_owned());
    }
    if !selectors.is_empty() {
        bail!(
            "--all-boards cannot be combined with a board selector ({}); unset or remove it",
            selectors.join(", ")
        );
    }
    Ok(())
}

fn search_options(args: &Args, query: &str) -> Result<SearchOptions> {
    let limit = args.limit(10)?;
    let max_chars = args.integer("max-chars", 12_000)?;
    if !(1..=100).contains(&limit) {
        bail!("--limit must be between 1 and 100, got {limit}");
    }
    if !(256..=100_000).contains(&max_chars) {
        bail!("--max-chars must be between 256 and 100000, got {max_chars}");
    }
    let after = args.optional_integer("after")?;
    let before = args.optional_integer("before")?;
    if after
        .zip(before)
        .is_some_and(|(after, before)| after > before)
    {
        bail!("--after must not be later than --before");
    }
    if let Some(source) = args.one("source") {
        const SOURCES: [&str; 8] = [
            "task",
            "note",
            "checkpoint",
            "handoff",
            "attention",
            "sitrep",
            "rule",
            "event",
        ];
        if !SOURCES.contains(&source) {
            bail!("invalid --source {source}; expected {}", SOURCES.join(", "));
        }
    }
    Ok(SearchOptions {
        query: query.to_owned(),
        source: option_string(args, "source"),
        status: option_string(args, "status"),
        tags: args.many("tag"),
        lane: option_string(args, "lane"),
        after,
        before,
        include_archived: args.has("all"),
        limit: limit as usize,
        max_chars: max_chars as usize,
    })
}

fn search_command(args: &Args, query: &str, creation: BoardCreation) -> Result<SearchReceipt> {
    let options = search_options(args, query)?;
    if args.has("all-boards") {
        reject_all_boards_selector(args)?;
        let registry = Registry::open_readonly()?;
        let projects = if args.has("all") {
            registry.projects()?
        } else {
            registry.projects_active()?
        };
        let mut results = Vec::new();
        let mut boards = Vec::new();
        let mut missing = Vec::new();
        let mut unreadable = Vec::new();
        for project in projects {
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {}
                SurveyBoard::Unreadable(reason) => {
                    unreadable.push(unreadable_board(&project, reason));
                    continue;
                }
                SurveyBoard::Missing => {
                    missing.push(project.name);
                    continue;
                }
            }
            let store = Store::open(Path::new(&project.board_path))?;
            results.extend(store.search(&project.name, &options)?);
            boards.push(project.name);
        }
        results.extend(search::search_rules(
            &registry.rules(options.include_archived)?,
            &options,
        ));
        let mut seen = HashSet::new();
        results.retain(|result| seen.insert(result.citation.clone()));
        return Ok(search::bound_receipt(
            query,
            boards,
            missing,
            unreadable,
            results,
            options.limit,
            options.max_chars,
        ));
    }

    let registry = Registry::open()?;
    let board_name = selected_board_name(args)?;
    let store = open_store(args, creation)?;
    let board = board_name
        .clone()
        .or(store.board_name()?)
        .unwrap_or_else(|| "unregistered".to_owned());
    let mut results = store.search(&board, &options)?;
    results.extend(search::search_rules(
        &registry.rules_targeting_board(board_name.as_deref(), options.include_archived)?,
        &options,
    ));
    Ok(search::bound_receipt(
        query,
        vec![board],
        Vec::new(),
        // One named board, already resolved: an unreadable one refused before
        // reaching here.
        Vec::new(),
        results,
        options.limit,
        options.max_chars,
    ))
}

fn rebuild_search_command(args: &Args, creation: BoardCreation) -> Result<Value> {
    let actor = args.require("as")?;
    if args.has("all-boards") {
        reject_all_boards_selector(args)?;
        let registry = Registry::open()?;
        let mut reports = Vec::new();
        let mut missing = Vec::new();
        let mut unreadable = Vec::new();
        for project in registry.projects_active()? {
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {}
                SurveyBoard::Unreadable(reason) => {
                    unreadable.push(unreadable_board(&project, reason));
                    continue;
                }
                SurveyBoard::Missing => {
                    missing.push(project.name);
                    continue;
                }
            }
            let mut store = Store::open(Path::new(&project.board_path))?;
            reports.push(store.rebuild_search(&project.name, actor)?);
        }
        // An index left unrebuilt because the file would not open is not the
        // same as one whose board is gone: the first is retried after a chmod.
        return Ok(
            json!({"reports":reports,"missingBoards":missing,"unreadableBoards":unreadable}),
        );
    }
    let mut store = open_store(args, creation)?;
    let board = selected_board_name(args)?
        .or(store.board_name()?)
        .unwrap_or_else(|| "unregistered".to_owned());
    Ok(serde_json::to_value(store.rebuild_search(&board, actor)?)?)
}

/// Rules are one registry-owned document. Bodies remain lazy; this is only the
/// applicable table of contents for the addressed board and optional task.
fn selected_board_name(args: &Args) -> Result<Option<String>> {
    match board_selection(args) {
        BoardSelection::Db { path, .. } => match Registry::board_path_state_if_available(&path)? {
            Some(BoardPathState::Active(name)) => Ok(Some(name)),
            Some(BoardPathState::Retired { name, note }) => bail!(
                "{}",
                retired_board_message(&name, note.as_deref(), "addressing it")
            ),
            Some(BoardPathState::External) | None => Ok(None),
        },
        BoardSelection::Project(name) => Ok(Some(name)),
        BoardSelection::Workspace(workspace) => {
            let mut registry = Registry::open()?;
            let workspace = match workspace {
                Some(path) => path,
                None => cwd()?,
            };
            Ok(registry.resolve(&workspace)?.map(|record| record.name))
        }
    }
}

fn effective_rule_summaries(
    args: &Args,
    store: &Store,
    task_id: Option<&str>,
) -> Result<Vec<RuleSummary>> {
    let board_name = selected_board_name(args)?;
    let task_tags = task_id
        .map(|id| store.require_task(id).map(|task| task.tags))
        .transpose()?
        .unwrap_or_default()
        .into_iter()
        .collect::<HashSet<_>>();
    Registry::open()?.applicable_rule_summaries(
        board_name.as_deref(),
        task_id.map(|_| &task_tags),
        false,
    )
}

fn option_string(args: &Args, name: &str) -> Option<String> {
    args.one(name).map(str::to_owned)
}

fn subscription_values(args: &Args, name: &str) -> Vec<String> {
    args.many(name)
}

/// Serialize a struct that is always a JSON object, so callers can extend it.
fn object_of<T: Serialize>(value: &T) -> Result<Map<String, Value>> {
    match serde_json::to_value(value)? {
        Value::Object(map) => Ok(map),
        other => bail!("expected a JSON object, got {other}"),
    }
}

fn list_json(
    store: &Store,
    status: Option<&str>,
    tag: Option<&str>,
    lane: Option<&str>,
    relations: bool,
    include_archived: bool,
) -> Result<Value> {
    let tasks = store.list_tasks(status, tag, lane, include_archived)?;
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

/// The keys of one `task list` row, exactly as a caller sees them.
///
/// `--fields` is checked against this list rather than against the rows that
/// came back, so an empty listing refuses a misspelt key the same way a full
/// one does. The test module serializes a row and compares, so the list cannot
/// drift from the struct.
const TASK_FIELDS: [&str; 20] = [
    "id",
    "type",
    "parentID",
    "title",
    "body",
    "assignee",
    "lane",
    "deliverable",
    "staleMinutes",
    "driverOnly",
    "status",
    "priority",
    "priorityLevel",
    "createdAt",
    "updatedAt",
    "completedAt",
    "archived",
    "archivedAt",
    "metadata",
    "tags",
];

/// The one key `task list --with-relations` adds to every row.
const TASK_RELATION_FIELD: &str = "dependencies";

/// The keys of one `attention list` row, exactly as a caller sees them.
const ATTENTION_FIELDS: [&str; 17] = [
    "id",
    "taskID",
    "kind",
    "body",
    "raisedBy",
    "createdAt",
    "status",
    "priority",
    "priorityLevel",
    "resolvedAt",
    "resolvedBy",
    "resolution",
    "reopenedAt",
    "reopenedBy",
    "reopenNote",
    "archived",
    "tags",
];

/// The keys `--fields` or `--no-body` keep of every row in a listing, or
/// `None` for the default of the whole row.
///
/// A listing carrying every body is the bulk of what crosses the wire from a
/// remote caller — measured at 1 MB for 702 tasks — and a caller choosing
/// what to read is the only projection that shrinks it without lying about
/// what is on the board. Bodies stay reachable per row through `task show`
/// and `context`.
///
/// `--fields` and `--no-body` together are two answers to one question, and
/// are refused rather than ranked (ADR-008).
fn projection(args: &Args, fields: &[&str]) -> Result<Option<Vec<String>>> {
    match (args.one("fields"), args.has("no-body")) {
        (Some(_), true) => bail!(
            "--fields and --no-body both choose the keys of every row; pass one: \
             --fields names the keys to keep, --no-body keeps every key but body"
        ),
        (None, false) => Ok(None),
        (None, true) => Ok(Some(
            fields
                .iter()
                .filter(|field| **field != "body")
                .map(|field| (*field).to_owned())
                .collect(),
        )),
        (Some(raw), false) => {
            let mut keep = Vec::new();
            for name in raw.split(',').map(str::trim) {
                if name.is_empty() {
                    bail!(
                        "--fields {raw:?} has an empty entry; pass a comma list of row keys \
                         such as --fields id,title,status, or drop --fields for whole rows"
                    );
                }
                if !fields.contains(&name) {
                    bail!(
                        "--fields names {name}, which is not a key of these rows; \
                         the keys are {}",
                        fields.join(", ")
                    );
                }
                keep.push(name.to_owned());
            }
            Ok(Some(keep))
        }
    }
}

/// Drop every key of every row that `keep` does not name.
fn project(rows: &mut Value, keep: &[String]) {
    let Value::Array(rows) = rows else {
        return;
    };
    for row in rows {
        if let Value::Object(row) = row {
            row.retain(|key, _| keep.iter().any(|kept| kept == key));
        }
    }
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

const SNAPSHOT_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotFile {
    path: String,
    kind: String,
    project: Option<String>,
    bytes: u64,
    sha256: String,
    schema_version: usize,
    audit: crate::audit::AuditReport,
}

/// A file copied out of the way byte for byte, because SQLite would not open it
/// as a database.
///
/// Deliberately outside `files` and outside the `boards` directory. Every entry
/// in `files` carries a schema version and an audit head, and both can only be
/// read from a database that opens; `verify_snapshot_manifest` also enumerates
/// `boards/*.db` and demands that set match `files` exactly, so a copy that
/// cannot be described that way would make its own rescue snapshot fail
/// verification. These sit under `unparsed/` and are described by what can
/// actually be known of them: size, digest, and why they would not open.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnparsedFile {
    path: String,
    original_path: String,
    bytes: u64,
    sha256: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotManifest {
    format_version: u32,
    created_at: i64,
    files: Vec<SnapshotFile>,
    missing_boards: Vec<String>,
    /// Registered boards that exist and would not open, so are absent from
    /// `files` while their data is very likely intact. A snapshot missing one
    /// of these is incomplete in a way a later restore must not silently paper
    /// over.
    ///
    /// `default` rather than a format bump: every manifest written before this
    /// field existed recorded no unreadable boards, which an empty list states
    /// exactly, and bumping the version would make those snapshots unrestorable
    /// to buy nothing. Unknown fields are ignored on the way in, so an older
    /// binary still reads a manifest written by this one.
    #[serde(default)]
    unreadable_boards: Vec<UnreadableBoard>,
    /// Files copied verbatim because they would not open as databases. Same
    /// `default` reasoning as above: a manifest written before this field
    /// existed recorded none, which an empty list states exactly.
    #[serde(default)]
    unparsed_files: Vec<UnparsedFile>,
}

fn load_snapshot_manifest(path: &Path) -> Result<SnapshotManifest> {
    let path = if path.is_dir() {
        path.join("manifest.json")
    } else {
        path.to_owned()
    };
    let bytes = fs::read(&path).with_context(|| format!("read audit anchor {}", path.display()))?;
    let manifest: SnapshotManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse audit anchor {}", path.display()))?;
    if manifest.format_version != SNAPSHOT_MANIFEST_VERSION {
        bail!(
            "snapshot manifest version {} is not supported (expected {})",
            manifest.format_version,
            SNAPSHOT_MANIFEST_VERSION
        );
    }
    Ok(manifest)
}

fn apply_snapshot_anchor(
    connection: &rusqlite::Connection,
    table: &str,
    report: &mut audit::AuditReport,
    anchor: &audit::AuditReport,
) -> Result<()> {
    let anchored_hash = if anchor.last_seq == 0 {
        Some(anchor.head.clone())
    } else {
        audit::hash_at(connection, table, anchor.last_seq)?
    };
    if report.last_seq < anchor.last_seq {
        report.errors.push(format!(
            "journal ends at sequence {}, before anchored sequence {}",
            report.last_seq, anchor.last_seq
        ));
    } else if anchored_hash.as_deref() != Some(anchor.head.as_str()) {
        report.errors.push(format!(
            "sequence {} does not match the retained anchor",
            anchor.last_seq
        ));
    }
    report.healthy = report.errors.is_empty();
    Ok(())
}

fn snapshot_file(
    directory: &Path,
    path: &Path,
    kind: &str,
    project: Option<String>,
) -> Result<SnapshotFile> {
    let relative = path
        .strip_prefix(directory)
        .with_context(|| {
            format!(
                "{} is outside snapshot {}",
                path.display(),
                directory.display()
            )
        })?
        .to_string_lossy()
        .into_owned();
    let (schema_version, audit) = match kind {
        "registry" => {
            let connection = db::open_registry_readonly(path)?;
            (
                db::schema_version(&connection)?,
                audit::verify_registry(&connection)?,
            )
        }
        "board" => {
            let connection = db::open_board_readonly(path)?;
            (
                db::schema_version(&connection)?,
                audit::verify_board(&connection)?,
            )
        }
        _ => bail!("unknown snapshot file kind {kind}"),
    };
    if !audit.healthy {
        bail!(
            "{} has an invalid audit chain: {:?}",
            path.display(),
            audit.errors
        );
    }
    Ok(SnapshotFile {
        path: relative,
        kind: kind.to_owned(),
        project,
        bytes: fs::metadata(path)?.len(),
        sha256: audit::file_sha256(path)?,
        schema_version,
        audit,
    })
}

fn write_snapshot_manifest(
    directory: &Path,
    registry_path: &Path,
    // The project name is optional because a file the restore is about to
    // overwrite need not be registered at all, and inventing a name for one
    // would put a fiction in the manifest.
    boards: &[(Option<String>, PathBuf)],
    missing_boards: &[String],
    unreadable_boards: &[UnreadableBoard],
    unparsed_files: &[UnparsedFile],
) -> Result<(PathBuf, String)> {
    let mut files = vec![snapshot_file(directory, registry_path, "registry", None)?];
    for (project, path) in boards {
        files.push(snapshot_file(directory, path, "board", project.clone())?);
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let manifest = SnapshotManifest {
        format_version: SNAPSHOT_MANIFEST_VERSION,
        created_at: now_ms(),
        files,
        missing_boards: missing_boards.to_vec(),
        unreadable_boards: unreadable_boards.to_vec(),
        unparsed_files: unparsed_files.to_vec(),
    };
    let path = directory.join("manifest.json");
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    serde_json::to_writer_pretty(&mut output, &manifest)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    let digest = audit::file_sha256(&path)?;
    Ok((path, digest))
}

fn verify_snapshot_manifest(directory: &Path) -> Result<(SnapshotManifest, String)> {
    let path = directory.join("manifest.json");
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "snapshot {} has no readable manifest.json; legacy unmanifested snapshots cannot be verified",
            directory.display()
        )
    })?;
    let manifest: SnapshotManifest =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if manifest.format_version != SNAPSHOT_MANIFEST_VERSION {
        bail!(
            "snapshot manifest version {} is not supported (expected {})",
            manifest.format_version,
            SNAPSHOT_MANIFEST_VERSION
        );
    }
    let mut expected = manifest
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    expected.sort();
    expected.dedup();
    if expected.len() != manifest.files.len() {
        bail!("snapshot manifest contains duplicate file paths");
    }
    let mut actual = vec!["registry.db".to_owned()];
    for entry in fs::read_dir(directory.join("boards"))
        .with_context(|| format!("read {}/boards", directory.display()))?
    {
        let entry = entry?;
        if entry.path().extension().and_then(|value| value.to_str()) == Some("db") {
            actual.push(format!("boards/{}", entry.file_name().to_string_lossy()));
        }
    }
    actual.sort();
    if expected != actual {
        bail!(
            "snapshot database set differs from manifest: expected {expected:?}, found {actual:?}"
        );
    }
    for record in &manifest.files {
        let relative = Path::new(&record.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            bail!("unsafe path in snapshot manifest: {}", record.path);
        }
        let file = directory.join(relative);
        let bytes = fs::metadata(&file)
            .with_context(|| format!("stat manifested file {}", file.display()))?
            .len();
        if bytes != record.bytes {
            bail!("snapshot file {} size differs from manifest", record.path);
        }
        let digest = audit::file_sha256(&file)?;
        if digest != record.sha256 {
            bail!(
                "snapshot file {} SHA-256 differs from manifest",
                record.path
            );
        }
        let observed = snapshot_file(directory, &file, &record.kind, record.project.clone())?;
        if observed.schema_version != record.schema_version
            || observed.audit.head != record.audit.head
            || observed.audit.entries != record.audit.entries
        {
            bail!(
                "snapshot file {} audit anchor differs from manifest",
                record.path
            );
        }
    }
    Ok((manifest, audit::file_sha256(&path)?))
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
    let (manifest, manifest_sha256) = verify_snapshot_manifest(&source)?;
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

    // Every path this is about to rename over, derived once and used both to
    // build the rescue copy and to do the replacing, so the two cannot come to
    // disagree about which files are at risk.
    //
    // What gets destroyed is decided by the snapshot and the filesystem, never
    // by the registry: the replacement below writes `<root>/boards/<file name>`
    // for every board file in the snapshot, listed or not. Keying the rescue
    // off `registry.projects()` protected the wrong set, and measurably so.
    // Restoring an older snapshot drops a project from the registry while
    // leaving its file on disk; restoring a newer one then renames over that
    // file, which by then is registered nowhere, so nothing classified it and
    // nothing copied it. Work committed after the snapshot went with it — and
    // the unreadable-board refusal could not fire either, because it was keyed
    // to the registry too.
    let overwrites = boards
        .iter()
        .map(|path| {
            (
                path.clone(),
                root.join("boards")
                    .join(path.file_name().unwrap_or_default()),
            )
        })
        .collect::<Vec<_>>();

    // `Registry::open` can migrate the live registry and re-assert its mode, so
    // this is not "before anything is written". What holds is narrower and is
    // what the risk actually needs: no rescue directory is created and no board
    // file is touched until every refusal below has had its say.
    let registry = Registry::open()?;
    let registered = registry.projects()?;
    let name_at = |target: &Path| -> Option<String> {
        let target = resolved(target);
        registered
            .iter()
            .find(|project| resolved(Path::new(&project.board_path)) == target)
            .map(|project| project.name.clone())
    };

    let mut rescue_sources: Vec<(Option<String>, PathBuf)> = Vec::new();
    let mut verbatim = Vec::new();
    let mut blocked = Vec::new();
    for (_, target) in &overwrites {
        match restore_target(target) {
            RestoreTarget::Vacant => {}
            RestoreTarget::Rescue => rescue_sources.push((name_at(target), target.clone())),
            RestoreTarget::Copy(reason) => verbatim.push((target.clone(), reason)),
            RestoreTarget::Blocked(reason) => {
                blocked.push((name_at(target), target.clone(), reason));
            }
        }
    }

    // Registered boards outside the overwrite set keep the rescue copy they
    // have always had: the registry row that reaches them is being replaced, so
    // the copy is part of undoing a mistaken restore. Their files are not
    // touched, though, so one that will not open is recorded in the manifest
    // rather than refused — refusing there would block a restore that destroys
    // nothing.
    let targeted = overwrites
        .iter()
        .map(|(_, target)| resolved(target))
        .collect::<HashSet<_>>();
    let mut rescue_missing = Vec::new();
    let mut rescue_unreadable = Vec::new();
    for project in &registered {
        if targeted.contains(&resolved(Path::new(&project.board_path))) {
            continue;
        }
        match survey_board(&project.board_path) {
            SurveyBoard::Readable => rescue_sources.push((
                Some(project.name.clone()),
                PathBuf::from(&project.board_path),
            )),
            // A board that is already gone is what a restore is often for; it
            // cannot be a precondition of running one.
            SurveyBoard::Missing => rescue_missing.push(project.name.clone()),
            SurveyBoard::Unreadable(reason) => {
                rescue_unreadable.push(unreadable_board(project, reason));
            }
        }
    }

    // Measured before this refusal existed: a live board at mode 000, holding
    // work committed after the snapshot was taken, was skipped by the rescue
    // copy as "missing", then replaced anyway — `replace_database` renames over
    // the path, which needs the directory's permissions and not the file's. The
    // command exited 0, the rescue snapshot had no `boards` directory at all,
    // and the task added after the backup was gone with nothing to recover it
    // from. The rescue copy is the only thing that makes `--force` reversible,
    // so a file it cannot copy stops the restore rather than becoming a line in
    // a manifest nobody reads until afterwards.
    if !blocked.is_empty() {
        let listed = blocked
            .iter()
            .map(|(name, path, reason)| {
                format!(
                    "  {} ({}): {reason}",
                    name.as_deref().unwrap_or("not in the registry"),
                    path.display()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "restore would overwrite files it cannot copy into the rescue snapshot first.\n\
             That snapshot is the only thing that makes --force reversible, so a file whose bytes \
             cannot be read stops the restore:\n{listed}\n\
             Nothing here says the data is damaged — this could not open the file to look, so it \
             is very likely intact. Fix its permissions and run this again, or move it aside to \
             discard what is in it.\n\
             A corrupt or unrecognisable board does not reach this: it is copied out of the way \
             byte for byte and the restore proceeds, which is what a restore is for."
        );
    }

    // The same file can be both an overwrite target and a registered project;
    // collapsing those is safe because it is one file either way.
    let mut seen_source = HashSet::new();
    rescue_sources.retain(|(_, path)| seen_source.insert(resolved(path)));

    // Two *different* files sharing a base name would copy to the same path
    // inside the rescue snapshot, and the second would land on the first: one
    // board rescued, one silently not, which is the failure everything above
    // exists to prevent. It also produces a manifest with a duplicate path,
    // which `verify_snapshot_manifest` rejects — so the rescue snapshot would
    // be unrestorable, discovered only after the live files were gone.
    let mut seen_name = HashSet::new();
    let collisions = rescue_sources
        .iter()
        .filter(|(_, path)| !seen_name.insert(path.file_name().unwrap_or_default().to_owned()))
        .map(|(_, path)| format!("  {}", path.display()))
        .collect::<Vec<_>>();
    if !collisions.is_empty() {
        bail!(
            "restore cannot build a rescue snapshot: these board files share a file name with \
             another board it is also rescuing, so one copy would overwrite the other:\n{}\n\
             Move or rename one of them, then run this again.",
            collisions.join("\n")
        );
    }

    let rescue = root
        .join("backups")
        .join(format!("pre-restore-{}", now_ms()));
    let rescue_registry = rescue.join("registry.db");
    registry.backup(&rescue_registry)?;
    let mut rescue_boards = Vec::new();
    for (name, path) in rescue_sources {
        let file_name = path
            .file_name()
            .with_context(|| format!("board path has no file name: {}", path.display()))?;
        let destination = rescue.join("boards").join(file_name);
        // Read-only, because this is a copy and nothing else: opening writable
        // runs migrations, and running a migration into a board that is about
        // to be rescued for being damaged can only make the copy worse.
        //
        // The online backup is WAL-correct, so it is the first choice for any
        // board that opens at all. It can still fail on a file the classifier
        // accepted: `probe_board_schema` reads the schema out of the first
        // page, so corruption further into the file is invisible to it and
        // surfaces only here, when every page is read. Measured — a board with
        // its header and schema intact and its later pages overwritten passed
        // classification and then failed the backup with `database disk image
        // is malformed`, aborting the restore that was recovering it. The
        // fallback keeps the guarantee whole: whatever the damage turns out to
        // be, the file is copied out of the way before anything replaces it.
        //
        // Describing the copy is part of making it. Every manifest entry
        // carries a schema version and an audit head, both read back out of the
        // copy, and that read walks rows the page-level backup never validated:
        // a board whose corruption sits in free pages copied cleanly and then
        // failed here, aborting the restore just the same. Anything that cannot
        // be copied *and* described as a board becomes a verbatim copy instead.
        match Store::open_readonly(&path)
            .and_then(|store| store.backup(&destination))
            .and_then(|()| snapshot_file(&rescue, &destination, "board", name.clone()))
        {
            Ok(_) => rescue_boards.push((name, destination)),
            Err(error) => {
                // Leave nothing half-described behind: an unmanifested `.db`
                // under `boards/` is exactly what `verify_snapshot_manifest`
                // rejects, so the failed copy goes before the verbatim one
                // takes its place.
                let _ = fs::remove_file(&destination);
                for suffix in ["-wal", "-shm"] {
                    let mut sidecar = destination.as_os_str().to_owned();
                    sidecar.push(suffix);
                    let _ = fs::remove_file(Path::new(&sidecar));
                }
                verbatim.push((path, format!("{error:#}")));
            }
        }
    }
    // Copied rather than exported, because SQLite will not open these. The
    // sidecars go too: `replace_database` deletes the `-wal` and `-shm` beside
    // the file it replaces, and for a corrupt main database the write-ahead log
    // is often the part a recovery would still want.
    let mut rescue_unparsed = Vec::new();
    for (path, reason) in &verbatim {
        let file_name = path
            .file_name()
            .with_context(|| format!("overwrite target has no file name: {}", path.display()))?;
        let destination = rescue.join("unparsed").join(file_name);
        db::create_private_dir_all(&rescue.join("unparsed"))?;
        fs::copy(path, &destination)
            .with_context(|| format!("copy {} to {}", path.display(), destination.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_owned();
            sidecar.push(suffix);
            let sidecar = PathBuf::from(sidecar);
            if sidecar.is_file() {
                let mut beside = destination.as_os_str().to_owned();
                beside.push(suffix);
                fs::copy(&sidecar, Path::new(&beside))
                    .with_context(|| format!("copy {}", sidecar.display()))?;
            }
        }
        rescue_unparsed.push(UnparsedFile {
            path: format!("unparsed/{}", file_name.to_string_lossy()),
            original_path: path.to_string_lossy().into_owned(),
            bytes: fs::metadata(&destination)?.len(),
            sha256: audit::file_sha256(&destination)?,
            reason: reason.clone(),
        });
    }
    let (rescue_manifest, rescue_manifest_sha256) = write_snapshot_manifest(
        &rescue,
        &rescue_registry,
        &rescue_boards,
        &rescue_missing,
        &rescue_unreadable,
        &rescue_unparsed,
    )?;
    drop(registry);

    let mut restored = Vec::new();
    for (from, to) in std::iter::once((registry_source.clone(), root.join("registry.db")))
        .chain(overwrites.iter().cloned())
    {
        db::replace_database(&from, &to)?;
        restored.push(to.to_string_lossy().into_owned());
    }
    let restore_id = format!("restore-{}", now_ms());
    let actor = args.one("as").unwrap_or("system@cli");
    Registry::open()?.record_system_event(
        "snapshot_restored",
        actor,
        json!({
            "restoreID":restore_id,
            "from":source,
            "manifestSha256":manifest_sha256,
            "rescueSnapshot":rescue,
            "rescueManifestSha256":rescue_manifest_sha256,
        }),
    )?;
    for (_, destination) in &overwrites {
        Store::open(destination)?.record_system_event(
            "snapshot_restored",
            actor,
            json!({
                "restoreID":restore_id,
                "from":source,
                "manifestSha256":manifest_sha256,
            }),
        )?;
    }
    let restored_heads = manifest
        .files
        .iter()
        .map(|file| json!({"path":file.path,"kind":file.kind,"project":file.project,"entries":file.audit.entries,"head":file.audit.head}))
        .collect::<Vec<_>>();
    print(
        &json!({
            "restored":restored,
            "from":source,
            "manifestSha256":manifest_sha256,
            "restoredHeads":restored_heads,
            "rescueSnapshot":rescue,
            "rescueManifest":rescue_manifest,
            "rescueManifestSha256":rescue_manifest_sha256,
            // Replaced without ever being opened as a database. Named here so
            // that overwriting a corrupt board, or a file that was never a
            // board, is something the operator is told rather than something
            // they find out later.
            "rescuedUnparsed":rescue_unparsed,
        }),
        args.has("json"),
    )
}

/// Shared entry point for both installed binaries, `kanban` and `kb`.
pub fn entrypoint() -> ! {
    match run() {
        Ok(()) => std::process::exit(0),
        // A closed pipe is not a failure: `kb task list --json | head` closes
        // stdout the moment head has what it wants, and every other Unix tool
        // ends quietly at that point rather than reporting an error the user
        // did not cause.
        Err(error) if reader_left(&error) => std::process::exit(0),
        Err(error) => {
            let message = format!("{error:#}");
            let _ = writeln!(io::stderr(), "Error: {message}");
            // A `--json` caller reads stdout and the exit status, and a
            // pipeline such as `... --json | jq` never sees stderr at all.
            // `claim --candidates --json` without `--as` left stdout empty,
            // and an empty stdout piped into a parser reads as "no
            // candidates" -- absence rendered identically to error. So the
            // refusal reaches stdout too, as the object a parser will see, and
            // stderr keeps the prose for the MCP layer and humans.
            if json_requested() {
                let _ = print(&json!({ "error": message }), true);
            }
            std::process::exit(1)
        }
    }
}

/// Entry point for the dedicated durable-subscription worker binary.
pub fn dispatcher_entrypoint() -> ! {
    match dispatcher::command(env::args_os().skip(1).collect()) {
        Ok(()) => std::process::exit(0),
        Err(error) if reader_left(&error) => std::process::exit(0),
        Err(error) => {
            let _ = writeln!(io::stderr(), "Error: {error:#}");
            std::process::exit(1)
        }
    }
}

/// Entry point for the Codex queue adapter binary.
pub fn codex_queue_adapter_entrypoint() -> ! {
    match codex_queue_adapter::entrypoint() {
        Ok(()) => std::process::exit(0),
        Err(error) if reader_left(&error) => std::process::exit(0),
        Err(error) => {
            let _ = writeln!(io::stderr(), "Error: {error:#}");
            std::process::exit(1)
        }
    }
}

/// Entry point for the Codex app-server adapter binary.
pub fn codex_app_server_adapter_entrypoint() -> ! {
    match codex_app_server_adapter::entrypoint() {
        Ok(()) => std::process::exit(0),
        // This structured adapter must not apply the human CLI's reader-left
        // exception: BrokenPipe can come from Codex child stdin, and a closed
        // response pipe means the delivery acknowledgement did not arrive.
        Err(error) => {
            let _ = writeln!(io::stderr(), "Error: {error:#}");
            std::process::exit(1)
        }
    }
}

/// Entry point for the Claude Code print adapter binary.
pub fn claude_print_adapter_entrypoint() -> ! {
    match claude_print_adapter::entrypoint() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            let _ = writeln!(io::stderr(), "Error: {error:#}");
            std::process::exit(1)
        }
    }
}

fn read_transfer_bundle(path: &Path) -> Result<Vec<u8>> {
    const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("read rule transfer bundle {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("rule transfer bundle must be a regular file");
    }
    if metadata.len() > MAX_BUNDLE_BYTES {
        bail!(
            "rule transfer bundle is too large: {} bytes",
            metadata.len()
        );
    }
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open rule transfer bundle {}", path.display()))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        bail!("rule transfer bundle changed while reading");
    }
    Ok(bytes)
}

fn run() -> Result<()> {
    let args = Args::parse(env::args().skip(1).collect())?;
    if args.has("version") {
        emit(&version_string())?;
        return Ok(());
    }
    if !args.positionals.is_empty()
        && canonical_command(args.positionals[0].as_str()) == WORKSPACE_ADOPT_HELPER_COMMAND
    {
        return run_workspace_adopt_helper();
    }
    if args.positionals.is_empty() || args.has("help") {
        emit(HELP)?;
        return Ok(());
    }
    let command = canonical_command(args.positionals[0].as_str());
    let sub = args
        .positionals
        .get(1)
        .map(String::as_str)
        .map(|value| canonical_sub(command, value));
    let rest = args.positionals.get(2..).unwrap_or(&[]);
    let spec_sub = sub.filter(|_| SUBCOMMAND_GROUPS.contains(&command));

    // Ahead of the selector refusal below, because a flag that no longer
    // exists is the more actionable complaint about the same command line:
    // `rule list --global --project ONE` must still say `--global` was
    // superseded rather than start with the `--project` that is merely
    // inapplicable.
    if args.has("global") && (command == "rule" || command == "events") {
        bail!("--global is superseded; all /kb rules are one tag-scoped collection");
    }

    // Ahead of everything else, including the three commands below that answer
    // without validating anything and the data-root lock that reads `--db`
    // itself: a selector this command cannot honour is a refusal, not
    // something to discard on the way to a confident answer.
    reject_ignored_selectors(&args, command, spec_sub)?;

    if command == "version" {
        emit(&version_string())?;
        return Ok(());
    }

    // Describes the binary, not a board, so it resolves none and takes no lock.
    if command == "schema" {
        return print(&schema(), args.has("json"));
    }

    // Speaks the protocol on stdout, so it must reach it before anything else
    // can print there, and it resolves each call's board per call instead.
    if command == "mcp" {
        return mcp::serve();
    }

    let creation = board_creation(command, spec_sub);
    match command_spec(command, spec_sub) {
        Some((allowed, positionals)) => {
            args.reject_unknown(allowed, ignored_selectors(command, spec_sub).0)?;
            args.reject_repeated_for(Some(command))?;
            args.reject_extra_positionals(arity(spec_sub, positionals))?;
            // Before the data-root lock and before any store: a command that
            // cannot run must not have created a board on its way to saying so.
            let mut words = vec![command];
            words.extend(spec_sub);
            args.reject_missing_positionals(&words, positionals)?;
            args.reject_conflicting_board_selectors()?;
        }
        None => bail!("unknown command; run kanban --help"),
    }

    require_sane_clock()?;

    let adopt_request = if command == "workspace" && sub == Some("adopt") {
        let source = PathBuf::from(args.require("from-board")?);
        let name = args.require("name")?;
        let actor = args.require("as")?.to_owned();
        let rootless = args.has("rootless");
        let workspace = args.one("workspace").map(PathBuf::from);
        if rootless && workspace.is_some() {
            bail!("--rootless cannot be combined with --workspace");
        }
        if !rootless && workspace.is_none() {
            bail!("workspace adopt requires either --workspace PATH or --rootless");
        }
        Some((
            PreparedAdoption::prepare(&source, name)?,
            workspace,
            rootless,
            actor,
        ))
    } else {
        None
    };
    if adopt_request.is_some() {
        preflight_live_root_for_adoption()?;
    }

    // Held until `run` returns. `restore` replaces database files behind
    // SQLite's back, so it needs the data root to itself; everything else
    // only needs the assurance that no restore is doing so underneath it.
    // Acquired here rather than inside `Registry::open`, which `restore`
    // itself calls to write its rescue snapshot — an flock conflicts with a
    // second descriptor in the same process, so a lower-level acquire would
    // deadlock restore against itself.
    // A command that discards `--db` must not have its locking decided by one.
    // `touches_data_root` asks whether the board this invocation addresses lies
    // outside the data root, and for these commands the answer is that they
    // address the data root itself whatever any `--db` says. Reading the
    // discarded selector anyway made the flag's *only* effect the suppression
    // of the lock: `KANBAN_DB=/tmp/elsewhere.db kanban restore --from SNAP
    // --force` replaced the entire data root with no exclusive lock, and
    // `backup`, `init` and `doctor` skipped the shared one the same way.
    //
    // Refusing the flag cannot close this. `reject_ignored_selectors` counts
    // typed flags only — correctly, because every agent cage exports
    // `KANBAN_DB` and an exported default must not break `doctor` — so the
    // environment reaches `board_selection` untouched. The lock decision is
    // where it has to be fixed, and `None` is the fail-closed answer:
    // `touches_data_root(None)` is true, so the lock is taken.
    let addressed_board = match ignored_selectors(command, spec_sub).0.contains(&"db") {
        true => None,
        false => direct_db(&args),
    };
    let _data_root = if lock::touches_data_root(addressed_board.as_deref()) {
        Some(
            if command == "restore" || (command == "workspace" && sub == Some("adopt")) {
                lock::exclusive()?
            } else {
                lock::shared()?
            },
        )
    } else {
        None
    };

    // Adoption validates and snapshots the source before any live write. Once
    // the source is pinned, the exclusive data-root lock above stays in force
    // across root preparation, helper execution, registry commit, and cleanup.
    if let Some((prepared, workspace, rootless, actor)) = adopt_request {
        if let Err(error) = prepare_live_root_for_adoption() {
            return Err(prepared.abort(error));
        }
        let record =
            match spawn_workspace_adopt_helper(&prepared, workspace.as_deref(), rootless, &actor) {
                Ok(record) => record,
                Err(error) => return Err(prepared.abort(error)),
            };
        prepared.cleanup()?;
        return print(&record, args.has("json"));
    }

    if command == "init" {
        if args.has("rootless") && args.one("workspace").is_some() {
            bail!("--rootless cannot be combined with --workspace");
        }
        let workspace = if args.has("rootless") {
            None
        } else {
            Some(args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?))
        };
        let _initialization = lock::initialization()?;
        let mut registry = Registry::open()?;
        let record = registry.register(
            workspace.as_deref(),
            args.require("name")?,
            args.has("force"),
            args.one("as").unwrap_or("system@cli"),
        )?;
        let mut store = Store::open(Path::new(&record.board_path))?;
        store.initialize(&record.name, args.one("as").unwrap_or("system@cli"))?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("list") {
        return print(&Registry::open()?.list(args.has("all"))?, args.has("json"));
    }
    if command == "workspace" && sub == Some("attach") {
        let workspace = args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?);
        let mut registry = Registry::open()?;
        let record = registry.attach(
            &workspace,
            args.require("to")?,
            args.one("as").unwrap_or("system@cli"),
        )?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("detach") {
        let mut registry = Registry::open()?;
        let record = registry.detach(args.require("root")?, args.require("as")?)?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("retire") {
        let mut registry = Registry::open()?;
        let name = rest.first().context("workspace name is required")?;
        let record = registry.retire(name, args.require("as")?, args.require("note")?)?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("unretire") {
        let mut registry = Registry::open()?;
        let name = rest.first().context("workspace name is required")?;
        let record = registry.unretire(name, args.require("as")?)?;
        return print(&record, args.has("json"));
    }
    if command == "workspace" && sub == Some("repoint") {
        let mut registry = Registry::open()?;
        // Without --root, repoint every broken row: the usual cause is one tree
        // moving, which breaks its project root and each lane beneath it at
        // once, and repairing them one command at a time invites stopping half
        // way. Naming one is for the case where only that one should move.
        let broken = registry.unreachable_roots()?;
        let targets = match args.one("root") {
            Some(root) => vec![root.to_owned()],
            None => {
                if broken.is_empty() {
                    bail!("every registered root already resolves to itself; nothing to repoint");
                }
                broken.iter().map(|item| item.root_path.clone()).collect()
            }
        };
        let mut repointed = Vec::new();
        for root in targets {
            repointed.push(registry.repoint(&root, args.one("as").unwrap_or("system@cli"))?);
        }
        return print(&repointed, args.has("json"));
    }
    if command == "dashboard" {
        let registry = Registry::open()?;
        let projects = if args.has("all") {
            registry.projects()?
        } else {
            registry.projects_active()?
        };
        let mut output = Vec::new();
        for project in projects {
            let mut value = object_of(&project)?;
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {
                    value.insert("boardState".into(), json!("readable"));
                }
                SurveyBoard::Unreadable(reason) => {
                    // Deliberately not `boardMissing`. That flag is the one a
                    // reader acts on, and acting on it here means restoring a
                    // snapshot over a file that is still there.
                    value.insert("boardState".into(), json!("unreadable"));
                    value.insert("boardUnreadableReason".into(), json!(reason));
                    output.push((
                        i64::MAX,
                        i64::MAX,
                        project.name.clone(),
                        Value::Object(value),
                    ));
                    continue;
                }
                SurveyBoard::Missing => {
                    value.insert("boardState".into(), json!("missing"));
                    value.insert("boardMissing".into(), json!(true));
                    output.push((
                        i64::MAX,
                        i64::MAX,
                        project.name.clone(),
                        Value::Object(value),
                    ));
                    continue;
                }
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let tasks = store.list_tasks(None, None, None, false)?;
            let handoffs = store.handoffs(None, Some("pending"), None, 100, false)?;
            let attention = store.attention(Some("open"), None, None, None, None, 1000, false)?;
            let mut counts = Map::new();
            for status in TASK_STATUSES {
                counts.insert(
                    status.into(),
                    json!(tasks.iter().filter(|task| task.status == status).count()),
                );
            }
            value.insert("taskCounts".into(), Value::Object(counts));
            value.insert("pendingHandoffs".into(), json!(handoffs.len()));
            // The count an operator most needs to see without being asked: a
            // record raised for them that nobody has settled.
            value.insert("openAttention".into(), json!(attention.len()));
            value.insert("totalTasks".into(), json!(tasks.len()));
            value.insert("staleTasks".into(), json!(store.stale_tasks()?.len()));
            let queued = tasks
                .iter()
                .filter(|task| task.status == "todo")
                .map(|task| (task.priority, task.created_at))
                .chain(
                    attention
                        .iter()
                        .map(|item| (item.priority, item.created_at)),
                )
                .chain(handoffs.iter().map(|item| (item.priority, item.created_at)))
                .collect::<Vec<_>>();
            let highest = queued.iter().map(|(priority, _)| *priority).min();
            let oldest_at_highest = highest
                .and_then(|priority| {
                    queued
                        .iter()
                        .filter(|(candidate, _)| *candidate == priority)
                        .map(|(_, created_at)| *created_at)
                        .min()
                })
                .unwrap_or(i64::MAX);
            value.insert("highestPriority".into(), json!(highest));
            value.insert(
                "highestPriorityLevel".into(),
                json!(highest.and_then(priority_level)),
            );
            output.push((
                highest.unwrap_or(i64::MAX),
                oldest_at_highest,
                project.name.clone(),
                Value::Object(value),
            ));
        }
        output.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        return print(
            &output
                .into_iter()
                .map(|(_, _, _, value)| value)
                .collect::<Vec<_>>(),
            args.has("json"),
        );
    }
    if command == "audit" && sub == Some("verify") {
        let anchor = args
            .one("against")
            .map(|path| load_snapshot_manifest(Path::new(path)))
            .transpose()?;
        let registry = Registry::open_readonly()?;
        let mut registry_audit = registry.audit()?;
        if let Some(record) = anchor
            .as_ref()
            .and_then(|manifest| manifest.files.iter().find(|file| file.kind == "registry"))
        {
            apply_snapshot_anchor(
                &registry.connection,
                "rule_events",
                &mut registry_audit,
                &record.audit,
            )?;
        }
        let mut healthy = registry_audit.healthy;
        let mut boards = Vec::new();
        let mut missing = Vec::new();
        let mut unreadable = Vec::new();
        for project in registry.projects()? {
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {}
                SurveyBoard::Unreadable(reason) => {
                    // Not healthy: an unopened ledger has had nothing verified
                    // about it, and this command's whole output is a claim
                    // about ledgers it verified.
                    healthy = false;
                    unreadable.push(unreadable_board(&project, reason));
                    continue;
                }
                SurveyBoard::Missing => {
                    healthy = false;
                    missing.push(project.name);
                    continue;
                }
            }
            let store = Store::open_readonly(Path::new(&project.board_path))?;
            let mut audit = store.audit()?;
            if let Some(record) = anchor.as_ref().and_then(|manifest| {
                let current_name = Path::new(&project.board_path).file_name()?;
                manifest.files.iter().find(|file| {
                    file.kind == "board" && Path::new(&file.path).file_name() == Some(current_name)
                })
            }) {
                apply_snapshot_anchor(&store.connection, "events", &mut audit, &record.audit)?;
            }
            healthy &= audit.healthy;
            boards.push(json!({"name":project.name,"boardPath":project.board_path,"audit":audit}));
        }
        let receipt = json!({
            "healthy": healthy,
            "registry": registry_audit,
            "boards": boards,
            "missingBoards": missing,
            "unreadableBoards": unreadable,
        });
        print(&receipt, args.has("json"))?;
        if !healthy {
            bail!("Kanban audit verification failed");
        }
        return Ok(());
    }
    if command == "doctor" {
        let registry = Registry::open()?;
        let registry_check = registry.integrity()?;
        let registry_audit = registry.audit()?;
        let active_rule_selectors = registry.active_rule_selector_health()?;
        let registry_schema = db::schema_version(&registry.connection)?;
        let registry_projects = if args.has("all") {
            registry.projects()?
        } else {
            registry.projects_active()?
        };
        let mut projects = Vec::new();
        // Roots are discovery hints, not board identity (ADR-028). Keep stale
        // hints visible so an operator can repoint or retire them, but do not
        // fail an otherwise healthy board that remains reachable by name.
        let unreachable = registry.unreachable_roots()?;
        let mut healthy =
            registry_check == vec!["ok"] && registry_audit.healthy && active_rule_selectors.healthy;
        for project in registry_projects {
            // Checked before opening, because opening would create it.
            //
            // `present` keeps the value it has always carried: true exactly
            // when this run opened the path and found a board, false otherwise.
            // That is unchanged for both a healthy board and a deleted one, so
            // no adapter reading it sees a different answer than before. What
            // was missing is a way to tell the two `false` cases apart, and
            // `boardState` is that field — read it rather than `present` to
            // decide whether anything needs recovering.
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {}
                SurveyBoard::Unreadable(reason) => {
                    // Unhealthy, and not because anything is known to be wrong
                    // with the board. Nothing about it was checked at all:
                    // integrity, orphans, future dating, the search index and
                    // the audit chain each need the file open. Reporting
                    // healthy here would be certifying a file this process
                    // never read.
                    healthy = false;
                    let mut value = object_of(&project)?;
                    value.insert("present".into(), json!(false));
                    value.insert("boardState".into(), json!("unreadable"));
                    value.insert("unreadableReason".into(), json!(reason));
                    value.insert("rootless".into(), json!(project.workspace_roots.is_empty()));
                    projects.push(Value::Object(value));
                    continue;
                }
                SurveyBoard::Missing => {
                    healthy = false;
                    let mut value = object_of(&project)?;
                    value.insert("present".into(), json!(false));
                    value.insert("boardState".into(), json!("missing"));
                    value.insert("rootless".into(), json!(project.workspace_roots.is_empty()));
                    projects.push(Value::Object(value));
                    continue;
                }
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let board_schema = db::schema_version(&store.connection)?;
            let check = store.integrity()?;
            // `integrity_check` validates the b-tree and nothing about what
            // the rows mean, so a structurally perfect board can still hold a
            // note on a task that is gone, or work stamped in the future whose
            // lease no sweep will ever retire.
            let orphans = store.foreign_key_violations()?;
            let future = store.future_dated_tasks()?;
            let search_index = store.search_health()?;
            let audit = store.audit()?;
            healthy &= check == vec!["ok"]
                && orphans.is_empty()
                && future.is_empty()
                && search_index.healthy
                && audit.healthy;
            let mut value = object_of(&project)?;
            value.insert("present".into(), json!(true));
            value.insert("boardState".into(), json!("readable"));
            value.insert("schemaVersion".into(), json!(board_schema));
            value.insert(
                "supportedSchemaVersion".into(),
                json!(db::BOARD_SCHEMA_VERSION),
            );
            value.insert("integrity".into(), json!(check));
            value.insert("orphanedRows".into(), json!(orphans));
            value.insert("futureDatedTasks".into(), json!(future));
            value.insert("searchIndex".into(), json!(search_index));
            value.insert("audit".into(), json!(audit));
            value.insert("rootless".into(), json!(project.workspace_roots.is_empty()));
            projects.push(Value::Object(value));
        }
        let result = json!({
            "healthy": healthy,
            "registry": registry_check,
            "registryAudit": registry_audit,
            "activeRuleSelectors": active_rule_selectors,
            "registrySchemaVersion": registry_schema,
            "supportedRegistrySchemaVersion": db::REGISTRY_SCHEMA_VERSION,
            "supportedBoardSchemaVersion": db::BOARD_SCHEMA_VERSION,
            "unreachableRoots": unreachable,
            "projects": projects,
        });
        print(&result, args.has("json"))?;
        if !healthy {
            bail!("Kanban integrity check failed");
        }
        return Ok(());
    }
    if command == "serve" {
        return serve::serve(
            args.port(serve::DEFAULT_PORT)?,
            args.single("actor-header")?.map(str::to_owned),
        );
    }
    if command == "watch" {
        return watch::run(&args);
    }
    if command == "backup" {
        let registry = Registry::open()?;
        let directory = args
            .one("output")
            .map(PathBuf::from)
            .unwrap_or(data_root()?.join("backups").join(now_ms().to_string()));
        let registry_path = directory.join("registry.db");
        registry.backup(&registry_path)?;
        let mut board_files = Vec::new();
        let mut missing = Vec::new();
        let mut unreadable = Vec::new();
        for project in registry.projects()? {
            // A snapshot of what is still here beats refusing to snapshot
            // anything, but it has to say what it could not include — and
            // "missing", about a file that is sitting right there, is not
            // saying it. Backup stays permissive because refusing to snapshot
            // eight healthy boards over a ninth's permission bit throws away
            // the recovery this command exists to provide; `restore` is where
            // the refusal belongs, because `restore` is the destructive half.
            match survey_board(&project.board_path) {
                SurveyBoard::Readable => {}
                SurveyBoard::Unreadable(reason) => {
                    unreadable.push(unreadable_board(&project, reason));
                    continue;
                }
                SurveyBoard::Missing => {
                    missing.push(project.board_path.clone());
                    continue;
                }
            }
            let store = Store::open(Path::new(&project.board_path))?;
            let file_name = Path::new(&project.board_path)
                .file_name()
                .with_context(|| format!("board path has no file name: {}", project.board_path))?;
            let destination = directory.join("boards").join(file_name);
            store.backup(&destination)?;
            board_files.push((Some(project.name), destination));
        }
        let (manifest, manifest_sha256) = write_snapshot_manifest(
            &directory,
            &registry_path,
            &board_files,
            &missing,
            &unreadable,
            // `backup` opens every board it copies, so it never produces one.
            &[],
        )?;
        let boards = board_files
            .iter()
            .map(|(_, path)| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let pruned = match args.one("keep") {
            Some(_) => prune_backups(args.integer("keep", 0)?, &directory)?,
            None => Vec::new(),
        };
        return print(
            &json!({
                "directory":directory,
                "registry":registry_path,
                "boards":boards,
                "missingBoards":missing,
                "unreadableBoards":unreadable,
                "manifest":manifest,
                "manifestSha256":manifest_sha256,
                "pruned":pruned,
            }),
            args.has("json"),
        );
    }
    if command == "restore" {
        return restore(&args);
    }

    if command == "rule" && sub == Some("export") {
        let boards = args.many("board");
        if boards.is_empty() {
            bail!("rule export requires at least one --board");
        }
        let bundle = Registry::open_readonly()?.export_rules(args.require("as")?, &boards)?;
        if let Some(output) = args.one("output") {
            let source_boards = bundle.source_boards.clone();
            let rules_exported = bundle.rules.len();
            let source_registry_audit_head = bundle.source_registry_audit.head.clone();
            fs::write(output, serde_json::to_vec_pretty(&bundle)?)?;
            return print(
                &json!({
                    "written": output,
                    "sourceBoards": source_boards,
                    "rulesExported": rules_exported,
                    "sourceRegistryAuditHead": source_registry_audit_head,
                }),
                args.has("json"),
            );
        }
        return print(&bundle, args.has("json"));
    }

    if command == "rule" && sub == Some("import") {
        let path = rest
            .first()
            .context("rule transfer bundle path is required")?;
        let bundle: crate::model::RuleTransferBundle =
            serde_json::from_slice(&read_transfer_bundle(Path::new(path))?)?;
        return print(
            &Registry::open()?.import_rules(args.require("as")?, bundle)?,
            args.has("json"),
        );
    }

    if command == "rule" && sub == Some("consolidate") {
        // The board selectors are refused by `reject_ignored_selectors` from
        // the table, before this branch is reached. `--global` needs no loop
        // here either: it is refused for every `rule` and `events` invocation
        // above, as superseded rather than inapplicable.
        return print(
            &Registry::open()?.consolidate_board_rules(args.require("as")?)?,
            args.has("json"),
        );
    }

    if command == "events" && (args.one("rule").is_some() || args.has("registry")) {
        if args.has("task") || (args.one("rule").is_some() && args.has("registry")) {
            bail!("--task, --rule and --registry address different event trails; pass one");
        }
        if args.has("after") || args.has("before") {
            bail!("--after and --before only apply to board events");
        }
        // `watch --registry` has refused these since it was written; `events`
        // read the same registry trail and accepted them, so the two disagreed
        // about the same command line.
        if args.has("project") || args.has("workspace") || args.has("db") {
            bail!(
                "--project, --workspace and --db address boards; --registry and --rule read the registry trail"
            );
        }
        return print(
            &Registry::open_readonly()?.rule_events(
                args.one("rule"),
                args.one("kind"),
                args.limit(50)?,
            )?,
            args.has("json"),
        );
    }

    if command == "rule" {
        // Board selectors are refused from the table, before dispatch.
        let mut registry = Registry::open()?;
        if sub == Some("add") {
            let flagged = args.body()?;
            let positional = rest.first();
            let body = match (positional, flagged.as_deref()) {
                (Some(_), Some(_)) => bail!(
                    "rule body was given as both a positional and --body/--body-file; pass one"
                ),
                (Some(body), None) => body.as_str(),
                (None, Some(body)) => body,
                (None, None) => bail!("rule body is required"),
            };
            let tags = registry.canonical_rule_tags(
                &args.many("board"),
                &args.many("except-board"),
                &args.many("tag"),
            )?;
            return print(
                &registry.add_rule(body, args.require("as")?, &tags)?,
                args.has("json"),
            );
        }
        if sub == Some("list") {
            if args.has("full") {
                return print(&registry.rules(args.has("all"))?, args.has("json"));
            }
            return print(&registry.rule_summaries(args.has("all"))?, args.has("json"));
        }
        if sub == Some("show") {
            return print(
                &registry.rule(rest.first().context("rule id is required")?)?,
                args.has("json"),
            );
        }
        if sub == Some("update") {
            if args.has("tag") && args.has("clear-tags") {
                bail!("--tag and --clear-tags are mutually exclusive");
            }
            let id = rest.first().context("rule id is required")?;
            let body = args.body()?;
            let targets_changed = args.has("board") || args.has("except-board");
            let selector_tags = targets_changed
                .then(|| {
                    registry.canonical_board_tags(&args.many("board"), &args.many("except-board"))
                })
                .transpose()?;
            let subsystem_tags = if args.has("clear-tags") {
                Some(Vec::new())
            } else if args.has("tag") {
                Some(registry.canonical_rule_task_tags(&args.many("tag"))?)
            } else {
                None
            };
            return print(
                &registry.update_rule(
                    id,
                    body.as_deref(),
                    selector_tags.as_deref(),
                    subsystem_tags.as_deref(),
                    args.require("as")?,
                )?,
                args.has("json"),
            );
        }
        if sub == Some("retire") {
            return print(
                &registry.retire_rule(
                    rest.first().context("rule id is required")?,
                    args.require("as")?,
                )?,
                args.has("json"),
            );
        }
    }

    if command == "search" {
        let query = args
            .positionals
            .get(1)
            .context("search query is required")?;
        return print(&search_command(&args, query, creation)?, args.has("json"));
    }
    if command == "search-rebuild" {
        return print(&rebuild_search_command(&args, creation)?, args.has("json"));
    }

    if command == "claim" && args.has("candidates") {
        if sub.is_some() || args.has("next") {
            bail!(
                "claim --candidates is read-only and cannot be combined with a task id or --next"
            );
        }
        if args.has("session") || args.has("lease-minutes") {
            bail!(
                "claim --candidates creates no lease; --session and --lease-minutes do not apply"
            );
        }
        let store = Store::open_readonly(&store_path_readonly(&args)?)?;
        let options = ClaimOptions {
            git: None,
            agent_id: args.require("as")?.into(),
            session_id: None,
            lease_ms: 1000,
            caller_lane: option_string(&args, "lane"),
            role_filter: option_string(&args, "role"),
            caller_scope: option_string(&args, "caller-scope"),
            cross_lane: !args.has("no-cross-lane"),
            allow_reassign: args.has("allow-reassign"),
        };
        let limit =
            usize::try_from(args.limit(100)?).context("--limit is too large for this platform")?;
        return print(
            &store.claim_candidates(&options, args.one("tag"), limit)?,
            args.has("json"),
        );
    }

    if command == "subscription" && sub == Some("list") {
        let store = Store::open_readonly(&store_path_readonly(&args)?)?;
        return print(
            &store.subscriptions(args.one("status"), args.one("consumer"), args.has("all"))?,
            args.has("json"),
        );
    }
    if command == "subscription" && sub == Some("show") {
        let store = Store::open_readonly(&store_path_readonly(&args)?)?;
        return print(
            &store.require_subscription(rest.first().context("subscription id is required")?)?,
            args.has("json"),
        );
    }

    let mut store = open_store(&args, creation)?;
    if command == "subscription" && sub == Some("add") {
        for required in [
            "consumer",
            "action",
            "timeout-ms",
            "max-retries",
            "rate-per-minute",
            "max-concurrency",
            "as",
        ] {
            args.require(required)?;
        }
        let subject_task_id = match args.one("subject") {
            None => None,
            Some(value) => Some(
                value
                    .strip_prefix("task:")
                    .filter(|id| !id.is_empty())
                    .context("--subject must be task:ID")?
                    .to_owned(),
            ),
        };
        return print(
            &store.add_subscription(AddSubscription {
                id: option_string(&args, "id"),
                subject_task_id,
                relations: subscription_values(&args, "relation"),
                kinds: subscription_values(&args, "kind"),
                prior_statuses: subscription_values(&args, "prior-status"),
                current_statuses: subscription_values(&args, "current-status"),
                tags: args.many("tag"),
                consumer_id: args.require("consumer")?.to_owned(),
                action_id: args.require("action")?.to_owned(),
                timeout_ms: args.integer("timeout-ms", 0)?,
                max_retries: args.integer("max-retries", -1)?,
                rate_per_minute: args.integer("rate-per-minute", 0)?,
                max_concurrency: args.integer("max-concurrency", 0)?,
                secret_ref: option_string(&args, "secret-ref"),
                actor: args.require("as")?.to_owned(),
            })?,
            args.has("json"),
        );
    }
    if command == "subscription" && sub == Some("pause") {
        return print(
            &store.pause_subscription(
                rest.first().context("subscription id is required")?,
                args.require("as")?,
            )?,
            args.has("json"),
        );
    }
    if command == "subscription" && sub == Some("resume") {
        return print(
            &store.resume_subscription(
                rest.first().context("subscription id is required")?,
                args.require("as")?,
            )?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("start") {
        return print(
            &store.start_deployment(StartDeployment {
                task_id: option_string(&args, "task"),
                repo: args.require("repo")?.to_owned(),
                commit_sha: args.require("commit")?.to_owned(),
                branch: option_string(&args, "branch"),
                tier: args.require("tier")?.to_owned(),
                environment: args.require("environment")?.to_owned(),
                host: args.require("host")?.to_owned(),
                url: args.require("url")?.to_owned(),
                mechanism: option_string(&args, "mechanism"),
                operation_id: option_string(&args, "operation-id"),
                retry_of: option_string(&args, "retry-of"),
                actor: args.require("as")?.to_owned(),
                lane: option_string(&args, "lane"),
            })?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("finish") {
        return print(
            &store.finish_deployment(FinishDeployment {
                id: rest.first().context("deployment id is required")?.clone(),
                capability_token: args.require("token")?.to_owned(),
                result: args.require("result")?.to_owned(),
                phase: option_string(&args, "phase"),
                receipt: option_string(&args, "receipt"),
                artifact_uri: option_string(&args, "artifact-uri"),
                served_commit: option_string(&args, "served-commit"),
                actor: args.require("as")?.to_owned(),
            })?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("abandon") {
        return print(
            &store.abandon_deployment(
                rest.first().context("deployment id is required")?,
                args.one("token"),
                args.has("force"),
                args.require("note")?,
                args.require("as")?,
            )?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("show") {
        return print(
            &store.require_deployment(rest.first().context("deployment id is required")?)?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("list") {
        return print(
            &store.deployments(
                args.one("status"),
                args.one("tier"),
                args.has("all"),
                args.limit(100)?,
            )?,
            args.has("json"),
        );
    }
    if command == "deploy" && sub == Some("current") {
        return print(&store.current_deployments()?, args.has("json"));
    }
    if command == "archive" {
        let days = args.integer("older-than-days", 90)?;
        if days < 1 {
            bail!("--older-than-days must be at least 1");
        }
        let age_ms = days
            .checked_mul(24 * 60 * 60 * 1000)
            .context("--older-than-days is too large")?;
        return print(
            &store.archive_settled(now_ms() - age_ms, args.require("as")?, args.has("dry-run"))?,
            args.has("json"),
        );
    }
    if command == "task" && sub == Some("add") {
        let title = rest.first().context("task title is required")?.clone();
        let task = store.add_task(crate::model::AddTask {
            tags: args.many("tag"),
            id: option_string(&args, "id"),
            task_type: args.one("type").unwrap_or("task").into(),
            parent_id: option_string(&args, "parent"),
            title,
            actor: Some(args.one("as").unwrap_or("system@cli").to_owned()),
            body: args.body()?,
            assignee: option_string(&args, "assignee"),
            lane: option_string(&args, "lane"),
            deliverable: option_string(&args, "deliverable"),
            stale_minutes: args.optional_integer("stale-minutes")?,
            driver_only: args.has("driver-only"),
            status: args.one("status").unwrap_or("todo").into(),
            priority: args.priority(6)?,
            dependencies: args.many("depends-on"),
            metadata: json!({}),
        })?;
        return print(&task, args.has("json"));
    }
    if command == "task" && sub == Some("list") {
        let relations = args.has("with-relations");
        let mut fields = TASK_FIELDS.to_vec();
        if relations {
            fields.push(TASK_RELATION_FIELD);
        }
        // Checked before the query, so a misspelt key is refused without
        // reading the board.
        let keep = projection(&args, &fields)?;
        let mut rows = list_json(
            &store,
            args.one("status"),
            args.one("tag"),
            args.one("lane"),
            relations,
            args.has("all"),
        )?;
        if let Some(keep) = &keep {
            project(&mut rows, keep);
        }
        return print(&rows, args.has("json"));
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
            serde_json::to_value(store.handoffs(Some(id), None, None, 100, true)?)?,
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
            ("tag", "clear-tags"),
        ] {
            if args.has(a) && args.has(b) {
                bail!("--{a} and --{b} are mutually exclusive");
            }
        }
        let input = UpdateTask {
            tags: if args.has("clear-tags") {
                Some(Vec::new())
            } else if args.flags.contains_key("tag") {
                Some(args.many("tag"))
            } else {
                None
            },
            parent_id: if let Some(v) = args.one("parent") {
                Some(Some(v.into()))
            } else if args.has("clear-parent") {
                Some(None)
            } else {
                None
            },
            title: option_string(&args, "title"),
            body: args.body()?.map(Some),
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
            priority: args.optional_priority()?,
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
        let mut value = store.claim(
            id,
            ClaimOptions {
                git: here(),
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
        value.rules = effective_rule_summaries(&args, &store, Some(&value.claim.task_id))?;
        return print(&value, args.has("json"));
    }
    if command == "heartbeat" {
        let id = sub.context("task id is required")?;
        return print(
            &store.heartbeat(
                id,
                args.require("lease")?,
                lease_ms(&args)?,
                here().as_ref(),
            )?,
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
        // Captured, not asked for: these columns were 100% empty because they
        // depended on the caller passing them. An explicit flag still wins.
        let git = here();
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
            repo_path: option_string(&args, "repo")
                .or_else(|| git.as_ref().map(|g| g.worktree.clone())),
            branch: option_string(&args, "branch")
                .or_else(|| git.as_ref().and_then(|g| g.branch.clone())),
            head_sha: option_string(&args, "head").or_else(|| git.as_ref().map(|g| g.head.clone())),
            dirty_summary: option_string(&args, "dirty")
                .or_else(|| git.as_ref().map(gitctx::dirty_summary)),
            root_head: git.as_ref().and_then(|g| g.root_head.clone()),
        })?;
        return print(&value, args.has("json"));
    }
    if command == "handoff" && sub == Some("create") {
        // No task id makes it a session handoff: about the work as a whole
        // rather than one row of it. The store refuses an id without its lease
        // and a lease without its id, since neither half means anything alone.
        let git = here();
        let value = store.create_handoff(HandoffInput {
            task_id: rest.first().map(|id| (*id).to_owned()),
            lease_token: args.one("lease").map(str::to_owned),
            from_agent: args.require("as")?.into(),
            from_session: option_string(&args, "session"),
            from_model: option_string(&args, "model"),
            to_agent: option_string(&args, "to"),
            reason: args.one("reason").unwrap_or("token_pressure").into(),
            priority: args.priority(6)?,
            summary: args.require("summary")?.into(),
            intent: args.require("intent")?.into(),
            next_action: args.require("next-action")?.into(),
            blockers: args.many("blocker"),
            validations: args.many("validation"),
            repo_path: option_string(&args, "repo")
                .or_else(|| git.as_ref().map(|g| g.worktree.clone())),
            branch: option_string(&args, "branch")
                .or_else(|| git.as_ref().and_then(|g| g.branch.clone())),
            head_sha: option_string(&args, "head").or_else(|| git.as_ref().map(|g| g.head.clone())),
            dirty_summary: option_string(&args, "dirty")
                .or_else(|| git.as_ref().map(gitctx::dirty_summary)),
            root_head: git.as_ref().and_then(|g| g.root_head.clone()),
        })?;
        return print(&value, args.has("json"));
    }
    if command == "handoff" && sub == Some("list") {
        return print(
            &store.handoffs(
                args.one("task"),
                args.one("status"),
                args.one("to"),
                100,
                args.has("all"),
            )?,
            args.has("json"),
        );
    }
    if command == "handoff" && sub == Some("accept") {
        let id = rest.first().context("handoff id is required")?;
        let git = here();
        let (handoff, claim) = store.accept_handoff(
            id,
            args.require("as")?,
            option_string(&args, "session"),
            lease_ms(&args)?,
            args.one("caller-scope"),
            git,
        )?;
        let rules = effective_rule_summaries(
            &args,
            &store,
            claim
                .as_ref()
                .map(|claim| claim.task_id.as_str())
                .or(handoff.task_id.as_deref()),
        )?;
        return print(
            &json!({"handoff":handoff,"claim":claim,"rules":rules}),
            args.has("json"),
        );
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
    if command == "tag" && sub == Some("add") {
        let name = rest.first().context("tag name is required")?;
        return print(
            &store.add_tag(
                name,
                args.one("description"),
                Some(args.one("as").unwrap_or("system@cli")),
            )?,
            args.has("json"),
        );
    }
    if command == "tag" && sub == Some("list") {
        return print(&store.tags()?, args.has("json"));
    }
    if command == "tag" && sub == Some("remove") {
        let name = rest.first().context("tag name is required")?;
        let rule_uses = Registry::open()?
            .rules(false)?
            .iter()
            .filter(|rule| rule.tags.iter().any(|tag| tag == name))
            .count();
        if rule_uses > 0 {
            bail!(
                "tag {name} scopes {rule_uses} active rule{}; update or retire those rules before removing the master entry, because stripping it would silently widen their scope",
                if rule_uses == 1 { "" } else { "s" }
            );
        }
        store.remove_tag(
            name,
            Some(args.one("as").unwrap_or("system@cli")),
            args.has("force"),
        )?;
        return print(&json!({ "removed": name }), args.has("json"));
    }
    if command == "attention" && sub == Some("raise") {
        let text = rest.first().context("attention text is required")?;
        return print(
            &store.raise_attention(
                text,
                args.one("kind").unwrap_or("decision"),
                args.require("as")?,
                args.one("task"),
                args.priority(6)?,
                &args.many("tag"),
            )?,
            args.has("json"),
        );
    }
    if command == "attention" && sub == Some("list") {
        let keep = projection(&args, &ATTENTION_FIELDS)?;
        let mut rows = serde_json::to_value(store.attention(
            args.one("status"),
            args.one("kind"),
            args.one("task"),
            args.one("tag"),
            args.one("lane"),
            args.limit(100)?,
            args.has("all"),
        )?)?;
        if let Some(keep) = &keep {
            project(&mut rows, keep);
        }
        return print(&rows, args.has("json"));
    }
    if command == "attention" && sub == Some("update") {
        if args.has("tag") && args.has("clear-tags") {
            bail!("--tag and --clear-tags are mutually exclusive");
        }
        let id = rest.first().context("attention id is required")?;
        let body = args.body()?;
        let tags = if args.has("clear-tags") {
            Some(Vec::new())
        } else if args.has("tag") {
            Some(args.many("tag"))
        } else {
            None
        };
        return print(
            &store.update_attention(id, body.as_deref(), tags.as_deref(), args.require("as")?)?,
            args.has("json"),
        );
    }
    if command == "attention" && sub == Some("resolve") {
        let id = rest.first().context("attention id is required")?;
        return print(
            &store.resolve_attention(id, args.require("as")?, args.one("note"))?,
            args.has("json"),
        );
    }
    if command == "attention" && sub == Some("reopen") {
        let id = rest.first().context("attention id is required")?;
        return print(
            &store.reopen_attention(id, args.require("as")?, args.require("note")?)?,
            args.has("json"),
        );
    }
    if command == "sitrep" && sub == Some("post") {
        let text = rest.first().context("sitrep text is required")?;
        return print(
            &store.post_sitrep(
                args.require("lane")?,
                text,
                args.require("as")?,
                args.one("task"),
                here().as_ref(),
            )?,
            args.has("json"),
        );
    }
    if command == "sitrep" && sub == Some("list") {
        return print(
            &store.sitreps(
                args.one("lane"),
                args.has("all"),
                args.one("task"),
                args.limit(20)?,
            )?,
            args.has("json"),
        );
    }
    if command == "events" {
        let after = args.optional_integer("after")?;
        let before = args.optional_integer("before")?;
        if after.is_some_and(|value| value < 0) {
            bail!("--after must be non-negative");
        }
        if before.is_some_and(|value| value < 0) {
            bail!("--before must be non-negative");
        }
        if after
            .zip(before)
            .is_some_and(|(after, before)| after > before)
        {
            bail!("--after must not be later than --before");
        }
        return print(
            &store.events_with_bounds(
                args.one("task"),
                args.one("kind"),
                after,
                before,
                args.limit(50)?,
                args.has("all"),
            )?,
            args.has("json"),
        );
    }
    if command == "context" {
        let id = sub.context("task id is required")?;
        let mut packet = store.context_packet(id)?;
        packet.rules = effective_rule_summaries(&args, &store, Some(id))?;
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
        emit(&render_context(&packet, max_chars as usize)?)?;
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

fn version_string() -> String {
    format!(
        "kanban {} (board schema {}; registry schema {})",
        env!("CARGO_PKG_VERSION"),
        db::BOARD_SCHEMA_VERSION,
        db::REGISTRY_SCHEMA_VERSION
    )
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
    fn a_limit_cannot_ask_for_the_opposite_of_a_bound() {
        assert_eq!(args(&["--limit", "5"]).limit(9).unwrap(), 5);
        assert_eq!(args(&[]).limit(9).unwrap(), 9, "the default still applies");
        // Zero asks for nothing and gets nothing, which is what it says.
        assert_eq!(args(&["--limit", "0"]).limit(9).unwrap(), 0);
        for bad in ["-1", "-5000"] {
            let error = args(&["--limit", bad])
                .limit(9)
                .expect_err("a negative limit must be refused")
                .to_string();
            assert!(error.contains("--limit"), "{error}");
            assert!(error.contains(bad), "{error}");
        }
    }

    #[test]
    fn every_limit_is_read_through_the_bounded_reader() {
        // Read the file back so the two cannot drift: a call site that goes
        // straight to `integer("limit", ..)` skips the floor, and SQLite reads
        // the negative it lets through as no limit at all -- silently handing
        // back every row the caller had asked to bound.
        const SOURCE: &str = include_str!("lib.rs");
        // Only the shipping half of the file: the tests below legitimately
        // exercise the generic reader, and counting those in would make the
        // guard fire on its own coverage.
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);
        // Assembled rather than written out, because this test reads its own
        // source and a literal would match itself.
        let bypass = format!(".{}(\"{}\"", "integer", "limit");
        assert_eq!(
            shipped.matches(bypass.as_str()).count(),
            1,
            "a --limit call site bypasses `limit()` and loses the floor; \
             only `limit()` itself may read the flag directly"
        );
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
    fn actor_header_is_single_valued_at_startup() {
        assert_eq!(
            args(&["serve", "--actor-header", "X-Kanban-Actor"])
                .single("actor-header")
                .unwrap(),
            Some("X-Kanban-Actor")
        );
        assert!(
            args(&[
                "serve",
                "--actor-header",
                "X-Kanban-Actor",
                "--actor-header",
                "X-Other"
            ])
            .single("actor-header")
            .is_err()
        );
    }

    #[test]
    fn unknown_flags_are_rejected_and_globals_are_not() {
        let allowed = ["status", "with-relations"];
        assert!(
            args(&["--status", "todo"])
                .reject_unknown(&allowed, &[])
                .is_ok()
        );
        for global in GLOBAL_FLAGS {
            assert!(
                args(&[&format!("--{global}"), "x"])
                    .reject_unknown(&allowed, &[])
                    .is_ok(),
                "--{global} must be accepted everywhere"
            );
        }
        let error = args(&["--projct", "x"])
            .reject_unknown(&allowed, &[])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown flag --projct"), "{error}");
        assert!(error.contains("did you mean --project?"), "{error}");
    }

    #[test]
    fn a_selector_a_command_ignores_is_neither_exempt_nor_advertised() {
        // Second lock behind `reject_ignored_selectors`, which reaches these
        // command lines first with a better message. If it is ever bypassed,
        // an ignored selector must still not be waved through as a global.
        let allowed: [&str; 0] = [];
        let error = args(&["--db", "/tmp/somewhere.db"])
            .reject_unknown(&allowed, &["db", "project", "workspace"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown flag --db"), "{error}");
        // And the "accepted here" line must not offer what was just refused.
        for selector in BOARD_SELECTORS {
            assert!(
                !error.contains(&format!("--{selector},")),
                "the refusal still advertises --{selector}: {error}"
            );
        }
        assert!(error.contains("--help"), "{error}");
        assert!(error.contains("--json"), "{error}");
        // A selector this command does honour stays exempt.
        assert!(
            args(&["--project", "Alpha"])
                .reject_unknown(&allowed, &["db"])
                .is_ok()
        );
    }

    #[test]
    fn every_ignored_selector_row_names_a_real_command_and_a_real_selector() {
        for (command, sub, ignored, subject) in IGNORED_SELECTORS {
            assert!(
                command_spec(command, *sub).is_some(),
                "{command} {sub:?} is declared selector-blind but is not a command"
            );
            assert!(
                !ignored.is_empty(),
                "{command} {sub:?} declares an empty ignored-selector list"
            );
            assert!(
                !subject.is_empty(),
                "{command} {sub:?} does not say what it addresses instead"
            );
            let mut seen = ignored.to_vec();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(
                before,
                seen.len(),
                "{command} {sub:?} lists a selector twice"
            );
            for flag in *ignored {
                assert!(
                    BOARD_SELECTORS.contains(flag),
                    "{command} {sub:?} ignores --{flag}, which is not a board selector"
                );
            }
        }
        // No row is declared twice, which would make the second one dead.
        let mut keys = IGNORED_SELECTORS
            .iter()
            .map(|(command, sub, ..)| (*command, *sub))
            .collect::<Vec<_>>();
        keys.sort_unstable();
        let unique = keys.len();
        keys.dedup();
        assert_eq!(
            unique,
            keys.len(),
            "a command is declared twice in IGNORED_SELECTORS"
        );
        // The two commands that honour one selector and discard the others are
        // the reason this is a per-command list rather than a set of names.
        assert_eq!(ignored_selectors("init", None).0, ["db", "project"]);
        assert_eq!(
            ignored_selectors("workspace", Some("attach")).0,
            ["db", "project"]
        );
        // And a command that resolves a board declares nothing.
        assert_eq!(ignored_selectors("task", Some("list")).0, [] as [&str; 0]);
    }

    #[test]
    fn an_ignored_selector_is_refused_by_name() {
        let error = reject_ignored_selectors(&args(&["--db", "/tmp/somewhere.db"]), "doctor", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--db"), "{error}");
        assert!(error.contains("doctor"), "{error}");
        assert!(
            error.contains("checks the registry and every board in it"),
            "the refusal does not say what doctor addresses instead: {error}"
        );
        // Both, when both were given. `reject_conflicting_board_selectors`
        // refuses that pair too, but this guard runs first and must not name
        // only half of what it is refusing.
        let error = reject_ignored_selectors(
            &args(&["--db", "/tmp/somewhere.db", "--project", "Alpha"]),
            "doctor",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--db and --project"), "{error}");
        // The selector it honours is not refused.
        assert!(
            reject_ignored_selectors(&args(&["--workspace", "/tmp/tree"]), "init", None).is_ok()
        );
        assert!(reject_ignored_selectors(&args(&["--db", "/tmp/b.db"]), "init", None).is_err());
        // A command with no row is untouched.
        assert!(
            reject_ignored_selectors(&args(&["--db", "/tmp/b.db"]), "task", Some("list")).is_ok()
        );
    }

    #[test]
    fn every_command_declares_its_flags_without_duplicates() {
        for (command, sub, flags, positionals, _) in COMMANDS {
            let arity = arity(*sub, positionals);
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
            // A named positional is either required or explicitly optional;
            // nothing else is a marker, and an empty name is not a name.
            for name in *positionals {
                let bare = name.strip_prefix('?').unwrap_or(name);
                assert!(
                    !bare.is_empty(),
                    "{command} {sub:?} has an unnamed positional"
                );
                assert!(
                    bare.chars().all(|c| c.is_ascii_lowercase()),
                    "{command} {sub:?}: positional {name} is not a plain lowercase name"
                );
            }
            // A command has to leave room for the words that name it, and
            // for whatever positional the dispatcher then reads.
            assert!(
                arity > usize::from(sub.is_some()),
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
    fn every_clearing_flag_is_refused_alongside_the_flag_it_clears() {
        // `--tag infra --clear-tags` is two answers to one question, and the
        // receipt would not say which was stored (ADR-008). Every clearing flag
        // is therefore declared mutually exclusive with the flag it undoes --
        // and the pair list is prose inside a dispatch arm, so nothing but a
        // read-back stops the next `--clear-x` from shipping without its pair.
        const SOURCE: &str = include_str!("lib.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);
        for flag in BOOLEAN {
            // Only the `clear-` family. `--no-cross-lane` reads the same way
            // but has no positive counterpart to contradict -- cross-lane is
            // the default and there is no `--cross-lane` -- so requiring it a
            // pair would be requiring a flag nobody needs.
            if !flag.starts_with("clear-") {
                continue;
            }
            // The positive half is not derivable -- `--clear-dependencies`
            // undoes `--depends-on` -- so the guard checks that the clearing
            // flag is paired with *something*, and leaves naming it to the
            // pair list. Assembled rather than written out, because this test
            // reads its own source and a literal would match itself.
            let paired = format!(", \"{flag}\")");
            assert!(
                shipped.contains(&paired),
                "--{flag} undoes another flag but is in no mutually-exclusive \
                 pair, so passing both silently prefers one"
            );
        }
    }

    /// The usage lines `--help` prints for one command: its `  kanban NAME`
    /// line and the indented continuation lines under it.
    fn usage_block(command: &str, sub: Option<&str>) -> String {
        let heading = match sub {
            Some(sub) => format!("  kanban {command} {sub} "),
            None => format!("  kanban {command} "),
        };
        let mut lines = HELP.lines().skip_while(|line| !line.starts_with(&heading));
        let mut block = lines
            .next()
            .unwrap_or_else(|| panic!("--help has no usage line starting `{heading}`"))
            .to_owned();
        for line in lines.take_while(|line| line.starts_with("             ")) {
            block.push('\n');
            block.push_str(line);
        }
        block
    }

    #[test]
    fn help_documents_every_flag_checkpoint_and_handoff_create_accept() {
        // Both handlers read --repo, --branch, --head and --dirty ahead of git
        // capture, and for a caller on a host where capture returns nothing
        // those four are the only way to store a head at all -- yet --help
        // named none of them, so the /session skill guessed the surface and
        // guessed two of them wrong. The parser's flag row is the surface;
        // this reads every flag on it back out of the usage block.
        for (command, sub) in [("checkpoint", None), ("handoff", Some("create"))] {
            let (flags, _) = command_spec(command, sub).unwrap();
            let block = usage_block(command, sub);
            for flag in flags {
                assert!(
                    block.contains(&format!("--{flag}")),
                    "`{command} {}` accepts --{flag} but its --help usage does not name it:\n{block}",
                    sub.unwrap_or("")
                );
            }
        }
    }

    #[test]
    fn the_skill_documents_aliases_that_actually_resolve() {
        // The skill ships in this repository precisely so it versions with the
        // binary (ADR-014), and that only holds if something checks. Writing
        // `rm`=remove into the tag row before the alias existed produced a
        // documented command that answers "unknown command" -- the drift
        // ADR-010 exists to prevent, arriving through documentation.
        const SKILL: &str = include_str!("../skills/kb/SKILL.md");

        // | `att`, `attn` | `attention` |
        let mut checked = 0;
        for line in SKILL.lines() {
            let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
            if cells.len() != 4 || !cells[0].is_empty() {
                continue;
            }
            let (shorts, full) = (cells[1], cells[2]);
            if !full.starts_with('`') || !full.ends_with('`') {
                continue;
            }
            let full = full.trim_matches('`');
            if full.contains('=') || full.contains(' ') {
                continue;
            }
            // A command-alias row: every short form on the left must resolve.
            if COMMANDS.iter().any(|(name, ..)| *name == full) && !shorts.contains('=') {
                for short in shorts.split(',') {
                    let short = short.trim().trim_matches('`');
                    if short.is_empty() || short == "Short" {
                        continue;
                    }
                    assert_eq!(
                        canonical_command(short),
                        full,
                        "the skill documents `{short}` as {full}, and it resolves elsewhere"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked >= 10, "only {checked} command aliases were checked");

        // | `task` | `ls`=list `mv`=move … |
        let mut subs = 0;
        for line in SKILL.lines() {
            let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
            if cells.len() != 4 || !cells[0].is_empty() || !cells[2].contains('=') {
                continue;
            }
            let group = cells[1].trim_matches('`');
            assert!(
                SUBCOMMAND_GROUPS.contains(&group),
                "the skill documents subcommands for `{group}`, which takes none"
            );
            for pair in cells[2].split_whitespace() {
                let pair = pair.trim_matches('`');
                let Some((short, full)) = pair.split_once('=') else {
                    continue;
                };
                let (short, full) = (short.trim_matches('`'), full.trim_matches('`'));
                assert_eq!(
                    canonical_sub(group, short),
                    full,
                    "the skill documents `{group} {short}` as {full}"
                );
                assert!(
                    COMMANDS
                        .iter()
                        .any(|(name, sub, ..)| *name == group && *sub == Some(full)),
                    "the skill documents `{group} {full}`, which is not a command"
                );
                subs += 1;
            }
        }
        assert!(subs >= 10, "only {subs} subcommand aliases were checked");
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
                // Watch owns additional list-valued filters while `--kind`
                // remains scalar on notes, events and other operations.
                if *command == "watch" && WATCH_REPEATABLE.contains(flag) {
                    continue;
                }
                if *command == "subscription" && SUBSCRIPTION_REPEATABLE.contains(flag) {
                    continue;
                }
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
    fn watch_is_marked_long_running_in_the_manifest() {
        assert!(LONG_RUNNING.contains(&"watch"));
        let schema = schema();
        let watch = schema
            .get("operations")
            .and_then(Value::as_array)
            .and_then(|operations| {
                operations
                    .iter()
                    .find(|operation| operation["name"] == "watch")
            })
            .expect("watch operation in schema");
        assert_eq!(watch["longRunning"], true);
        assert_eq!(watch["readOnly"], true);
    }

    #[test]
    fn watch_follow_is_a_presence_boolean() {
        assert!(BOOLEAN.contains(&"follow"));
        let parsed =
            Args::parse(vec!["watch".to_owned(), "--follow".to_owned()]).expect("parse watch args");
        assert_eq!(parsed.one("follow"), Some("true"));

        let schema = schema();
        let watch = schema
            .get("operations")
            .and_then(Value::as_array)
            .and_then(|operations| {
                operations
                    .iter()
                    .find(|operation| operation["name"] == "watch")
            })
            .expect("watch operation in schema");
        let follow = watch["flags"]
            .as_array()
            .and_then(|flags| flags.iter().find(|flag| flag["name"] == "follow"))
            .expect("follow flag in watch schema");
        assert_eq!(follow["kind"], "boolean");
    }

    #[test]
    fn watch_filter_lists_are_repeatable_in_parser_and_schema() {
        let parsed = Args::parse(
            [
                "watch",
                "--kind",
                "task_moved",
                "--kind",
                "task_added",
                "--relation",
                "parent:s-1",
                "--prior-status",
                "todo",
                "--current-status",
                "done",
                "--tag",
                "infra",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
        .expect("parse repeatable watch filters");
        for (flag, expected) in [
            ("kind", 2),
            ("relation", 1),
            ("prior-status", 1),
            ("current-status", 1),
            ("tag", 1),
        ] {
            assert_eq!(parsed.many(flag).len(), expected, "--{flag} was not parsed");
            assert!(
                REPEATABLE.contains(&flag) || WATCH_REPEATABLE.contains(&flag),
                "--{flag} is not repeatable"
            );
        }
        let schema = schema();
        let watch = schema["operations"]
            .as_array()
            .and_then(|operations| operations.iter().find(|op| op["name"] == "watch"))
            .expect("watch operation in schema");
        for flag in ["kind", "relation", "prior-status", "current-status", "tag"] {
            let descriptor = watch["flags"]
                .as_array()
                .and_then(|flags| flags.iter().find(|item| item["name"] == flag))
                .expect("watch filter in schema");
            assert_eq!(descriptor["kind"], "list", "--{flag} schema kind");
        }
    }

    #[test]
    fn repeating_a_single_valued_flag_is_refused() {
        assert!(
            args(&["--project", "alpha"])
                .reject_repeated_for(None)
                .is_ok()
        );
        assert!(args(&[]).reject_repeated_for(None).is_ok());
        // A list-valued flag is exactly what repeating is for.
        assert!(
            args(&["--blocker", "a", "--blocker", "b"])
                .reject_repeated_for(None)
                .is_ok()
        );
        let error = args(&["--project", "alpha", "--project", "beta"])
            .reject_repeated_for(None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--project (alpha, beta)"), "{error}");
    }

    #[test]
    fn kind_is_repeatable_only_for_watch_and_subscription_add() {
        let repeated = args(&["--kind", "a", "--kind", "b"]);
        assert!(repeated.reject_repeated_for(Some("watch")).is_ok());
        assert!(repeated.reject_repeated_for(Some("subscription")).is_ok());
        let error = repeated
            .reject_repeated_for(Some("events"))
            .expect_err("events --kind must remain scalar")
            .to_string();
        assert!(error.contains("--kind (a, b)"), "{error}");

        let schema = schema();
        for (operation, list) in [
            ("watch", true),
            ("subscription add", true),
            ("events", false),
            ("note", false),
        ] {
            let descriptor = schema["operations"]
                .as_array()
                .and_then(|operations| {
                    operations.iter().find(|item| {
                        item["name"] == operation
                            && item["flags"]
                                .as_array()
                                .is_some_and(|flags| flags.iter().any(|f| f["name"] == "kind"))
                    })
                })
                .expect("operation with kind");
            let kind = descriptor["flags"]
                .as_array()
                .and_then(|flags| flags.iter().find(|f| f["name"] == "kind"))
                .expect("kind descriptor");
            assert_eq!(kind["kind"], if list { "list" } else { "value" });
        }
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

    fn task_row() -> Value {
        serde_json::to_value(
            serde_json::from_value::<crate::model::Task>(json!({
                "id": "t-1",
                "type": "task",
                "title": "Row",
                "body": "long body",
                "lane": "driver-2",
                "driverOnly": false,
                "status": "todo",
                "priority": 3,
                "createdAt": 1,
                "updatedAt": 1,
                "archived": false,
                "metadata": {},
                "tags": ["kanban"],
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn attention_row() -> Value {
        serde_json::to_value(crate::model::Attention {
            id: "a-1".into(),
            task_id: Some("t-1".into()),
            kind: "decision".into(),
            body: "long body".into(),
            raised_by: "worker@driver-2".into(),
            created_at: 1,
            status: "open".into(),
            priority: 0,
            priority_level: Some("P0".into()),
            resolved_at: None,
            resolved_by: None,
            resolution: None,
            reopened_at: None,
            reopened_by: None,
            reopen_note: None,
            archived: false,
            tags: vec![],
        })
        .unwrap()
    }

    fn keys(row: &Value) -> Vec<&str> {
        row.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    #[test]
    fn the_field_lists_name_exactly_the_keys_a_row_carries() {
        // `--fields` validates against these lists, not against the rows that
        // came back, so the lists must be the rows: a key added to the struct
        // and not here would be refused as unknown, and a key removed from the
        // struct and not here would be accepted and silently absent.
        let mut expected = TASK_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys(&task_row()), expected);
        let mut expected = ATTENTION_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys(&attention_row()), expected);
    }

    #[test]
    fn fields_keeps_exactly_the_named_keys_and_nothing_else() {
        let keep = projection(&args(&["--fields", "id,title, lane"]), &TASK_FIELDS)
            .unwrap()
            .unwrap();
        let mut rows = Value::Array(vec![task_row(), task_row()]);
        project(&mut rows, &keep);
        for row in rows.as_array().unwrap() {
            assert_eq!(keys(row), ["id", "lane", "title"]);
            assert_eq!(row["lane"], "driver-2");
        }
    }

    #[test]
    fn no_body_drops_only_the_body() {
        let keep = projection(&args(&["--no-body"]), &ATTENTION_FIELDS)
            .unwrap()
            .unwrap();
        let mut rows = Value::Array(vec![attention_row()]);
        project(&mut rows, &keep);
        let mut expected = ATTENTION_FIELDS.to_vec();
        expected.retain(|field| *field != "body");
        expected.sort_unstable();
        assert_eq!(keys(&rows[0]), expected);
        assert_eq!(rows[0]["raisedBy"], "worker@driver-2");
    }

    #[test]
    fn the_default_is_the_whole_row() {
        assert!(projection(&args(&[]), &TASK_FIELDS).unwrap().is_none());
    }

    #[test]
    fn an_unknown_field_is_refused_naming_the_keys_that_exist() {
        let error = projection(&args(&["--fields", "id,bodyy"]), &ATTENTION_FIELDS)
            .unwrap_err()
            .to_string();
        assert!(error.contains("bodyy"), "{error}");
        for field in ATTENTION_FIELDS {
            assert!(error.contains(field), "{error} does not name {field}");
        }
        // `dependencies` exists only when --with-relations adds it, and the
        // refusal must not offer it otherwise.
        let error = projection(&args(&["--fields", "dependencies"]), &TASK_FIELDS)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("the keys are dependencies"), "{error}");
        let mut with_relations = TASK_FIELDS.to_vec();
        with_relations.push(TASK_RELATION_FIELD);
        assert_eq!(
            projection(&args(&["--fields", "dependencies"]), &with_relations)
                .unwrap()
                .unwrap(),
            ["dependencies"]
        );
        let error = projection(&args(&["--fields", "id,,title"]), &TASK_FIELDS)
            .unwrap_err()
            .to_string();
        assert!(error.contains("empty entry"), "{error}");
    }

    #[test]
    fn fields_and_no_body_are_two_answers_to_one_question() {
        let error = projection(&args(&["--fields", "id", "--no-body"]), &TASK_FIELDS)
            .unwrap_err()
            .to_string();
        assert!(error.contains("pass one"), "{error}");
    }
}
