mod context;
mod db;
mod import;
mod model;
mod registry;
mod store;

use crate::context::{render_context, render_todo};
use crate::import::{import_json, import_sqlite};
use crate::model::*;
use crate::registry::{Registry, data_root, now_ms};
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
  kanban init [--name NAME] [--workspace PATH]
  kanban workspace list [--json]
  kanban workspace attach --to REGISTERED_PATH [--workspace PATH]
  kanban dashboard [--json]
  kanban doctor [--json]
  kanban backup [--output DIRECTORY] [--json]
  kanban task add TITLE [--id ID] [--type epic|story|task] [--parent ID]
             [--body TEXT] [--status STATUS] [--priority N] [--depends-on ID ...]
             [--assignee AGENT] [--lane LANE] [--deliverable TEXT]
             [--stale-minutes N] [--driver-only]
  kanban task list [--status STATUS] [--with-relations] [--json]
  kanban task show ID [--json]
  kanban task move ID STATUS --as ACTOR [--metadata-patch-json JSON_OBJECT]
  kanban task remove ID --as ACTOR
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
  kanban import atmux-json|atmux-sqlite PATH --as ACTOR [--reconcile] [--json]
  kanban context ID [--max-chars N] [--json]
  kanban todo [--output PATH]

SQLite is authoritative. Generated TODO files are read-only projections."#;

const BOOLEAN: [&str; 15] = [
    "help",
    "json",
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
];

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
    fn integer(&self, name: &str, fallback: i64) -> Result<i64> {
        self.one(name)
            .map(str::parse::<i64>)
            .transpose()
            .with_context(|| format!("{name} must be an integer"))
            .map(|v| v.unwrap_or(fallback))
    }
}

fn print<T: Serialize>(value: &T, _pretty: bool) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn cwd() -> Result<PathBuf> {
    env::current_dir().context("read current directory")
}

fn store_path(args: &Args) -> Result<PathBuf> {
    if let Some(path) = args
        .one("db")
        .map(PathBuf::from)
        .or_else(|| env::var_os("KANBAN_DB").map(PathBuf::from))
    {
        return Ok(path);
    }
    let mut registry = Registry::open()?;
    registry
        .resolve(&cwd()?)?
        .map(|record| PathBuf::from(record.board_path))
        .context("no Kanban workspace contains the current directory; run 'kanban init' first")
}

fn open_store(args: &Args) -> Result<Store> {
    Store::open(&store_path(args)?)
}

fn option_string(args: &Args, name: &str) -> Option<String> {
    args.one(name).map(str::to_owned)
}

fn list_json(store: &Store, status: Option<&str>, relations: bool) -> Result<Value> {
    let tasks = store.list_tasks(status)?;
    if !relations {
        return Ok(serde_json::to_value(tasks)?);
    }
    Ok(Value::Array(
        tasks
            .into_iter()
            .map(|task| {
                let mut value = serde_json::to_value(&task)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone();
                value.insert(
                    "dependencies".into(),
                    json!(
                        store
                            .dependencies(&task.id)
                            .unwrap()
                            .into_iter()
                            .map(|d| d.id)
                            .collect::<Vec<_>>()
                    ),
                );
                Value::Object(value)
            })
            .collect(),
    ))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse(env::args().skip(1).collect())?;
    if args.positionals.is_empty() || args.has("help") {
        println!("{HELP}");
        return Ok(());
    }
    let command = args.positionals[0].as_str();
    let sub = args.positionals.get(1).map(String::as_str);
    let rest = args.positionals.get(2..).unwrap_or(&[]);

    if command == "init" {
        let workspace = args.one("workspace").map(PathBuf::from).unwrap_or(cwd()?);
        let fallback = workspace
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("project");
        let mut registry = Registry::open()?;
        let record = registry.register(&workspace, args.one("name").unwrap_or(fallback))?;
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
            let store = Store::open(Path::new(&project.board_path))?;
            let tasks = store.list_tasks(None)?;
            let mut counts = Map::new();
            for status in TASK_STATUSES {
                counts.insert(
                    status.into(),
                    json!(tasks.iter().filter(|task| task.status == status).count()),
                );
            }
            let mut value = serde_json::to_value(&project)?.as_object().unwrap().clone();
            value.insert("taskCounts".into(), Value::Object(counts));
            value.insert(
                "pendingHandoffs".into(),
                json!(store.handoffs(None, Some("pending"), 100)?.len()),
            );
            value.insert("totalTasks".into(), json!(tasks.len()));
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
            let store = Store::open(Path::new(&project.board_path))?;
            let check = store.integrity()?;
            healthy &= check == vec!["ok"];
            projects.push(
                json!({"name":project.name,"boardPath":project.board_path,"integrity":check}),
            );
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
        for project in registry.projects()? {
            let store = Store::open(Path::new(&project.board_path))?;
            let destination = directory
                .join("boards")
                .join(Path::new(&project.board_path).file_name().unwrap());
            store.backup(&destination)?;
            boards.push(destination.to_string_lossy().into_owned());
        }
        return print(
            &json!({"directory":directory,"registry":registry_path,"boards":boards}),
            args.has("json"),
        );
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
            stale_minutes: args.one("stale-minutes").map(str::parse).transpose()?,
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
        let mut value = serde_json::to_value(task)?.as_object().unwrap().clone();
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
        let task = store.move_task(id, status, args.require("as")?, patch)?;
        return print(&task, args.has("json"));
    }
    if command == "task" && sub == Some("remove") {
        let id = rest.first().context("task id is required")?;
        store.remove_task(id, args.require("as")?)?;
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
            stale_minutes: args
                .one("stale-minutes")
                .map(|v| v.parse().map(Some))
                .transpose()?,
            driver_only: if args.has("driver-only") {
                Some(true)
            } else if args.has("no-driver-only") {
                Some(false)
            } else {
                None
            },
            priority: args.one("priority").map(str::parse).transpose()?,
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
        let id = if args.has("next") { None } else { sub };
        if id.is_none() && !args.has("next") {
            bail!("task id or --next is required");
        }
        let value = store.claim(
            id,
            ClaimOptions {
                agent_id: args.require("as")?.into(),
                session_id: option_string(&args, "session"),
                lease_ms: args.integer("lease-minutes", 15)? * 60_000,
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
            &store.heartbeat(
                id,
                args.require("lease")?,
                args.integer("lease-minutes", 15)? * 60_000,
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
            args.integer("lease-minutes", 15)? * 60_000,
            args.one("caller-scope"),
        )?;
        return print(&json!({"handoff":handoff,"claim":claim}), args.has("json"));
    }
    if command == "import" && (sub == Some("atmux-json") || sub == Some("atmux-sqlite")) {
        let path = rest.first().context("import path is required")?;
        let actor = args.require("as")?;
        let reconcile = args.has("reconcile");
        let receipt = if sub == Some("atmux-json") {
            import_json(&mut store, Path::new(path), actor, reconcile)?
        } else {
            import_sqlite(&mut store, Path::new(path), actor, reconcile)?
        };
        return print(&receipt, args.has("json"));
    }
    if command == "context" {
        let id = sub.context("task id is required")?;
        let packet = store.context_packet(id)?;
        if args.has("json") {
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
