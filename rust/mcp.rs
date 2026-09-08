//! An MCP server over stdio, generated from the command surface.
//!
//! Two decisions shape everything here, both recorded in ADR-011.
//!
//! **A tool call runs the binary.** Every call spawns the executable and lets
//! the ordinary CLI parse, validate and answer it. There is no second code
//! path that can drift from the one operators use, no second validation to
//! keep in step, and the errors an agent sees are the errors a person sees.
//! It also means an updated binary is serving calls from the very next one.
//!
//! **The server holds nothing between requests.** Durable state is in SQLite
//! and the protocol is strict request/response, so the process is idle and
//! empty the moment it has answered. That is what makes it safe to replace
//! itself in place: see `Reload`.

use crate::COMMANDS;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The protocol revision this server speaks.
///
/// A client that asks for a different one is answered in its own, because the
/// methods used here — `initialize`, `tools/list`, `tools/call` — are stable
/// across every revision that exists, and refusing a client over a version
/// string it will not renegotiate helps nobody.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Read newline-delimited JSON from a file descriptor, buffering only what we
/// have actually been handed.
///
/// The buffering matters more than it looks. `Reload` replaces this process
/// with `execve`, which keeps the file descriptors and discards everything in
/// memory — so any bytes a convenience reader had buffered but not yet parsed
/// would be lost in the swap, silently eating a request. This keeps leftovers
/// where we can see them, and the reload is skipped whenever any remain.
struct Frames {
    input: ManuallyDrop<File>,
    buffer: Vec<u8>,
}

impl Frames {
    /// Borrow standard input without owning it: dropping a `File` built from a
    /// raw descriptor would close fd 0, which we still need after an `execve`.
    fn stdin() -> Self {
        Self {
            input: ManuallyDrop::new(unsafe { File::from_raw_fd(0) }),
            buffer: Vec::new(),
        }
    }

    /// Whether anything has been read but not yet consumed.
    fn idle(&self) -> bool {
        self.buffer.iter().all(u8::is_ascii_whitespace)
    }

    /// The next line, or `None` once the client closes the stream.
    fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line = self.buffer.drain(..=end).collect::<Vec<_>>();
                let text = String::from_utf8_lossy(&line).trim().to_owned();
                if text.is_empty() {
                    continue;
                }
                return Ok(Some(text));
            }
            let mut chunk = [0u8; 8192];
            match self.input.read(&mut chunk)? {
                0 => {
                    // A final line with no newline is still a request.
                    if self.buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
                        let text = String::from_utf8_lossy(&self.buffer).trim().to_owned();
                        self.buffer.clear();
                        return Ok(Some(text));
                    }
                    return Ok(None);
                }
                read => self.buffer.extend_from_slice(&chunk[..read]),
            }
        }
    }
}

/// What the executable looked like when this process started.
///
/// Replacing a binary is a rename over the path, so the running process keeps
/// the old inode and would serve stale code until the client happened to
/// restart it. Comparing identity — not just modification time, which a
/// same-second write can repeat — is what notices the swap.
#[derive(PartialEq, Eq, Clone, Copy)]
struct Fingerprint {
    device: u64,
    inode: u64,
    size: u64,
    modified: i64,
}

impl Fingerprint {
    fn read(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.size(),
            modified: meta.mtime(),
        })
    }
}

/// Replace this process with a newer build of the same binary, in place.
///
/// The client owns a stdio server's lifetime — it spawned us and holds the
/// pipe — so there is no restarting without disturbing it. `execve` sidesteps
/// that: the process image is replaced while the process id and the open file
/// descriptors survive, so from the far end of the pipe nothing happened.
///
/// Three conditions gate it, and all three matter:
///
/// - **Only between requests.** Swapping mid-request would drop a reply the
///   client is waiting on.
/// - **Only with an empty buffer.** Anything read but unparsed dies with the
///   old image; see `Frames`.
/// - **Only after the new binary answers.** A broken build that cannot start
///   takes the pipe down with it and the client sees a crashed server, so the
///   candidate is run once first. If it fails we keep serving the old image
///   and say so on stderr, which is not the protocol channel.
struct Reload {
    path: PathBuf,
    started_as: Option<Fingerprint>,
}

impl Reload {
    fn new() -> Self {
        // `current_exe` resolves /proc/self/exe, which reports the original
        // file even once it has been replaced — and appends " (deleted)" when
        // the old inode is unlinked. The path as written is what we must watch.
        let path = std::env::current_exe().unwrap_or_default();
        let path = match path
            .to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
        {
            Some(trimmed) => PathBuf::from(trimmed),
            None => path,
        };
        let started_as = Fingerprint::read(&path);
        Self { path, started_as }
    }

    fn changed(&self) -> bool {
        match (self.started_as, Fingerprint::read(&self.path)) {
            (Some(before), Some(now)) => before != now,
            _ => false,
        }
    }

    /// Returns only on failure; on success this process no longer exists.
    fn take_over(&self) {
        let probe = Command::new(&self.path).arg("version").output();
        let healthy = probe
            .as_ref()
            .map(|output| output.status.success() && output.stdout.starts_with(b"kanban "))
            .unwrap_or(false);
        if !healthy {
            eprintln!(
                "kanban mcp: {} changed but does not run; still serving the previous build",
                self.path.display()
            );
            return;
        }
        let error = Command::new(&self.path)
            .arg("mcp")
            .env("KANBAN_MCP_RELOADED", "1")
            .exec();
        eprintln!("kanban mcp: reload failed, still serving the previous build: {error}");
    }
}

/// The global flags a tool call may carry.
///
/// `--json` is supplied by this layer on every call, so accepting it again
/// produced "given more than once" — a refusal naming a flag the caller never
/// passed twice. `--help` was worse: it answered the tool call with the usage
/// page and reported success, so an agent asking for a task list received the
/// manual and nothing said the operation had not run.
///
/// Both were kept out of the generated schema and both were still accepted,
/// because the schema filtered them and the argument check did not. One list
/// now feeds both, so a flag cannot be advertised and honoured differently.
const TOOL_GLOBALS: [&str; 3] = ["db", "project", "workspace"];

/// A single argument value, or a refusal.
///
/// An array or an object where one value belongs is not a value: passing
/// `["a", "b"]` as a title used to record the title `["a","b"]`, reported as
/// success. Silently stringifying a caller's mistake into durable state is the
/// defect this ledger exists to prevent, so it is refused instead.
fn scalar(name: &str, value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(flag) => Ok(flag.to_string()),
        _ => bail!("{name} takes a single value, not {value}"),
    }
}

/// An MCP tool name: the operation with its spaces and dashes flattened.
///
/// A subcommand may itself be multi-word ("principal bind"), so both the
/// separator (`_` becomes a space in the name) and any inner spaces are
/// flattened to `_`; `access principal bind` is one tool, `access_principal_bind`.
pub(crate) fn tool_name(command: &str, sub: Option<&str>) -> String {
    match sub {
        Some(sub) => format!("{command}_{sub}"),
        None => command.to_owned(),
    }
    .replace(['-', ' '], "_")
}

/// Every operation, as a tool an agent can be handed.
///
/// Built from `COMMANDS` for the reason ADR-010 gives: a hand-written tool list
/// is a second description of the surface and drifts from the first one
/// silently. `readOnly` travels with each tool so a harness can withhold
/// mutation without keeping its own list of which calls write.
///
/// Two entries are not a command's flags: `batch`, which carries several of
/// these calls on one request, and `transact`, which carries several writes
/// through one transaction. Both are described by hand — an ordered list of
/// calls is not a set of flags — and both are appended here so `tools/list`
/// and `tools/call` offer and answer exactly the same set.
fn tools() -> Vec<Value> {
    let mut tools = COMMANDS
        .iter()
        .filter(|(command, ..)| !crate::LONG_RUNNING.contains(command))
        // `transact` has a `COMMANDS` row, which is what makes `schema --json`
        // publish it and what makes the read-only `batch` refuse it with no
        // new code (ADR-041 §11). Only its *schema* is hand-written: generated
        // from that row it would advertise `--items` as a string an agent has
        // to serialize by hand, and `--items-file` as a path this layer owns.
        .filter(|(command, sub, ..)| tool_name(command, *sub) != TRANSACT)
        .map(|(command, sub, flags, positionals, read_only)| {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for positional in *positionals {
                let optional = positional.starts_with('?');
                let name = positional.trim_start_matches('?');
                properties.insert(
                    name.to_owned(),
                    json!({ "type": "string", "description": format!("Positional argument {name}.") }),
                );
                if !optional {
                    required.push(name.to_owned());
                }
            }
            // A selector the CLI refuses on this operation must not be
            // offered as an input: the tool would take it, the binary would
            // reject it, and the agent would read a refusal for an argument
            // this schema told it to send.
            let ignored = crate::ignored_selectors(command, *sub).0;
            for flag in flags
                .iter()
                .chain(TOOL_GLOBALS.iter())
                .filter(|flag| !ignored.contains(*flag))
            {
                let (kind, description): (Value, String) =
                    if crate::repeatable(command, *sub, flag) {
                    (
                        json!({ "type": "array", "items": { "type": "string" } }),
                        format!("--{flag}, repeatable."),
                    )
                } else if crate::BOOLEAN.contains(flag) {
                    (json!({ "type": "boolean" }), format!("--{flag}."))
                } else {
                    (json!({ "type": "string" }), format!("--{flag}."))
                };
                let mut property = kind.as_object().cloned().unwrap_or_default();
                property.insert("description".into(), json!(description));
                properties.insert((*flag).to_owned(), Value::Object(property));
            }
            let usage = std::iter::once(*command)
                .chain(*sub)
                .map(str::to_owned)
                .chain(positionals.iter().map(|p| {
                    let name = p.trim_start_matches('?');
                    if p.starts_with('?') {
                        format!("[{name}]")
                    } else {
                        format!("<{name}>")
                    }
                }))
                .collect::<Vec<_>>()
                .join(" ");
            json!({
                "name": tool_name(command, *sub),
                "description": format!(
                    "kanban {usage}. {} Returns the command's JSON.",
                    if *read_only { "Reads only; writes nothing." } else { "Writes." }
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": Value::Object(properties),
                    "required": required,
                },
                "annotations": { "readOnlyHint": read_only },
            })
        })
        .collect::<Vec<_>>();
    tools.push(batch_tool());
    tools.push(transact_tool());
    tools
}

/// Turn a tool call into the argument list the CLI would have been given.
pub(crate) fn arguments_for(name: &str, arguments: &Value) -> Result<Vec<String>> {
    let Some((command, sub, flags, positionals, _)) = COMMANDS
        .iter()
        .find(|(command, sub, ..)| tool_name(command, *sub) == name)
    else {
        bail!("no such tool {name}");
    };
    let empty = serde_json::Map::new();
    // Anything else was read as "no arguments", so a call whose arguments were
    // malformed ran unconstrained and reported success.
    let supplied = match arguments {
        Value::Object(supplied) => supplied,
        Value::Null => &empty,
        other => bail!("arguments must be an object, not {other}"),
    };
    let mut argv = vec![(*command).to_owned()];
    // A multi-word subcommand ("principal bind") is one argv word per CLI word,
    // never one word with an embedded space.
    for word in sub.iter().flat_map(|sub| sub.split_whitespace()) {
        argv.push(word.to_owned());
    }
    for positional in *positionals {
        let key = positional.trim_start_matches('?');
        match supplied.get(key) {
            Some(Value::Null) | None => {}
            Some(value) => argv.push(scalar(key, value)?),
        }
    }
    let ignored = crate::ignored_selectors(command, *sub).0;
    let allowed = flags
        .iter()
        .copied()
        .chain(TOOL_GLOBALS.iter().copied())
        .filter(|flag| !ignored.contains(flag))
        .collect::<Vec<_>>();
    for (key, value) in supplied {
        if positionals
            .iter()
            .any(|p| p.trim_start_matches('?') == key.as_str())
        {
            continue;
        }
        if !allowed.contains(&key.as_str()) {
            // The CLI refuses an unknown flag rather than ignoring it, and so
            // does this: a silently dropped argument is the defect ADR-008
            // exists to prevent, and passing it through would only relocate
            // the refusal somewhere less obvious.
            bail!("{name} has no argument {key}");
        }
        let repeatable = crate::repeatable(command, *sub, key.as_str());
        match value {
            Value::Null => {}
            // A boolean flag is present or absent; false means absent.
            Value::Bool(false) if crate::BOOLEAN.contains(&key.as_str()) => {}
            Value::Bool(true) if crate::BOOLEAN.contains(&key.as_str()) => {
                argv.push(format!("--{key}"))
            }
            Value::Array(values) if repeatable => {
                for value in values {
                    argv.push(format!("--{key}"));
                    argv.push(scalar(key, value)?);
                }
            }
            value => {
                argv.push(format!("--{key}"));
                argv.push(scalar(key, value)?);
            }
        }
    }
    argv.push("--json".to_owned());
    Ok(argv)
}

/// Run one tool call by running the binary, and report what it said.
///
/// A failed command is a tool result marked `isError`, not a JSON-RPC error:
/// the refusal text is the most useful thing an agent can be given, and this
/// CLI's refusals name the fix. A transport-level error would hide it.
fn call(path: &Path, name: &str, arguments: &Value) -> Value {
    // `batch` is the one tool that names no command, so the one that cannot be
    // answered by running the binary. Dispatched here rather than in
    // `respond` so a batched entry and a standalone call are the same function
    // call and cannot drift apart: byte-identical results are the property the
    // benchmark's equivalence check rests on. A batch inside a batch is
    // impossible — `batch` has no `COMMANDS` row, and validation refuses any
    // name that has none.
    if name == BATCH {
        return batch(path, arguments);
    }
    // `transact` names a command and is still answered here, because the item
    // list has to reach the binary as a file rather than as a flag value and
    // because the whole list is one run. A transact inside a batch is
    // impossible for the ordinary reason — its row says it writes, and the
    // read-only batch refuses a write — and a transact inside a transact is
    // refused by the binary's own pre-flight (`rust/lib.rs:4740`).
    if name == TRANSACT {
        return transact(path, arguments);
    }
    let argv = match arguments_for(name, arguments) {
        Ok(argv) => argv,
        Err(error) => return error_result(&error.to_string()),
    };
    match Command::new(path).args(&argv).output() {
        Ok(output) if output.status.success() => json!({
            "content": [{ "type": "text", "text": String::from_utf8_lossy(&output.stdout) }],
            "isError": false,
        }),
        Ok(output) => {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            error_result(if message.is_empty() {
                "the command failed without a message"
            } else {
                &message
            })
        }
        Err(error) => error_result(&format!("could not run kanban: {error}")),
    }
}

fn error_result(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true,
    })
}

/// The most calls one `batch` may carry.
///
/// A bound rather than none, because a batch is answered by running the binary
/// once per entry: an unbounded array would put an unbounded amount of work
/// behind a single request, on a server whose safety rests on being idle and
/// empty the moment it has answered. The representative agent loop is twelve
/// reads (`docs/testing/graphql-agent-loop-benchmark.md`); this leaves room
/// above it without letting one frame become a job.
pub(crate) const BATCH_LIMIT: usize = 32;

/// The one tool name that is not an operation.
const BATCH: &str = "batch";

/// Whether a tool name is one `tools/list` offers, and whether its operation
/// writes: `None` when no listed tool has that name.
///
/// Filtered on the same two tables `tools` reads, so what is advertised and
/// what a batch accepts cannot answer differently. `batch` itself has no row,
/// so it answers `None` and a batch cannot nest.
fn listed_read_only(name: &str) -> Option<bool> {
    COMMANDS
        .iter()
        .filter(|(command, ..)| !crate::LONG_RUNNING.contains(command))
        .find(|(command, sub, ..)| tool_name(command, *sub) == name)
        .map(|(.., read_only)| *read_only)
}

/// The `batch` tool as `tools/list` offers it.
///
/// Hand-written, unlike every other tool, because it describes no command: its
/// input is a list of calls, not a command's flags. The one thing that could
/// drift — which operations a batch will carry — is still read off `COMMANDS`
/// at call time by `listed_read_only`, so it is not written down twice.
fn batch_tool() -> Value {
    json!({
        "name": BATCH,
        "description": format!(
            "Run up to {BATCH_LIMIT} read-only tool calls in one request, in the order given. \
             Every call must name a read-only tool from this list, or the whole batch is \
             refused and none of it runs. Each result is exactly what that call returns on \
             its own. Returns {{\"results\":[{{\"ok\":true,\"result\":…}}|{{\"ok\":false,\"error\":…}}]}} \
             in order, so one failing call hides no other. A transport shortcut, not a \
             capability: it can do nothing a separate call could not."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "calls": {
                    "type": "array",
                    "maxItems": BATCH_LIMIT,
                    "description": format!("The calls to run, in order; at most {BATCH_LIMIT}."),
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {
                                "type": "string",
                                "description": "A read-only tool name from tools/list.",
                            },
                            "arguments": {
                                "type": "object",
                                "description": "That tool's arguments, exactly as tools/call takes them.",
                            },
                        },
                        "required": ["name"],
                        "additionalProperties": false,
                    },
                },
            },
            "required": ["calls"],
        },
        "annotations": { "readOnlyHint": true },
    })
}

/// Answer several read-only tool calls on one request.
///
/// The loop this exists for is twelve reads at one round trip each. With
/// payload projection in place the bytes are no longer the cost and the
/// latency is: 2.45 s p50 for one agent loop from the MBP to the board home,
/// measured 2026-09-07, twelve reads paying ~190 ms of round trip apiece. A
/// batch is those round trips collapsed into one and nothing else.
///
/// Two properties make it a shortcut rather than a second surface, and both
/// are load-bearing:
///
/// - **Every entry runs the standalone `call`.** Same resolution, same
///   argument handling, same open path, so a batched result is byte-identical
///   to the result the same call returns alone. Anything less and a batch
///   would be a second way to read the board, free to drift from the first.
/// - **The whole batch is checked before any of it runs.** A batch that names
///   one operation it may not perform performs none of them, so a refusal
///   leaves nothing half-applied to reason about.
///
/// A per-entry failure is not a batch failure: it travels as that entry's
/// `error` beside every other entry's result, because a batch that gave up on
/// the first refusal would hide eleven answers behind one.
fn batch(path: &Path, arguments: &Value) -> Value {
    let empty = serde_json::Map::new();
    let supplied = match arguments {
        Value::Object(supplied) => supplied,
        Value::Null => &empty,
        other => return error_result(&format!("arguments must be an object, not {other}")),
    };
    if let Some(unknown) = supplied.keys().find(|key| key.as_str() != "calls") {
        return error_result(&format!("{BATCH} has no argument {unknown}"));
    }
    let calls = match supplied.get("calls") {
        Some(Value::Array(calls)) => calls,
        Some(other) => {
            return error_result(&format!("{BATCH} calls must be an array, not {other}"));
        }
        None => return error_result(&format!("{BATCH} needs a calls array")),
    };
    if calls.len() > BATCH_LIMIT {
        return error_result(&format!(
            "{BATCH} takes at most {BATCH_LIMIT} calls and was given {}; nothing in it ran",
            calls.len()
        ));
    }
    let mut planned = Vec::with_capacity(calls.len());
    for (index, entry) in calls.iter().enumerate() {
        let Value::Object(fields) = entry else {
            return error_result(&format!(
                "{BATCH} call {index} must be an object, not {entry}; nothing in it ran"
            ));
        };
        if let Some(unknown) = fields
            .keys()
            .find(|key| !matches!(key.as_str(), "name" | "arguments"))
        {
            return error_result(&format!(
                "{BATCH} call {index} has no field {unknown}; nothing in it ran"
            ));
        }
        let Some(name) = fields.get("name").and_then(Value::as_str) else {
            return error_result(&format!(
                "{BATCH} call {index} needs a name; nothing in it ran"
            ));
        };
        match listed_read_only(name) {
            Some(true) => {}
            Some(false) => {
                return error_result(&format!(
                    "{BATCH} call {index} names {name}, which writes; a batch carries read-only \
                     tools only, so nothing in it ran"
                ));
            }
            None => {
                return error_result(&format!(
                    "{BATCH} call {index} names no such tool {name}; nothing in it ran"
                ));
            }
        }
        planned.push((name, fields.get("arguments").unwrap_or(&Value::Null)));
    }
    let results = planned
        .into_iter()
        .map(|(name, arguments)| {
            let result = call(path, name, arguments);
            // Absent is treated as failed: a result this layer cannot read is
            // not a result it may report as success.
            if result["isError"].as_bool().unwrap_or(true) {
                json!({ "ok": false, "error": result })
            } else {
                json!({ "ok": true, "result": result })
            }
        })
        .collect::<Vec<_>>();
    json!({
        "content": [{ "type": "text", "text": json!({ "results": results }).to_string() }],
        "isError": false,
    })
}

/// The one writing tool whose input is a list of calls rather than flags.
///
/// Unlike `batch` it *is* an operation — `("transact", None, &["items",
/// "items-file"], &[], false)` in `COMMANDS` — which is what makes
/// `schema --json` publish it, what makes `listed_read_only` answer
/// `Some(false)`, and therefore what makes the read-only `batch` refuse it as
/// an entry with no code of its own (ADR-041 §6, §11).
const TRANSACT: &str = "transact";

/// The items of one `transact`, on disk for exactly as long as the call needs
/// them.
///
/// A file rather than an argument, always: `--items` travels as a single argv
/// string, Linux caps one argv string at 128 KiB (`MAX_ARG_STRLEN`) and macOS
/// caps the whole block at `ARG_MAX`, so a full batch carrying checkpoint
/// bodies would die as `E2BIG` before the binary saw it (ADR-041 §8). Choosing
/// per call would mean two paths and one of them exercised rarely.
///
/// Created `O_EXCL` and `0600`, because the list holds a caller's writes —
/// checkpoint prose, lease tokens — and the OS temp directory is shared. `Drop`
/// removes it, so every way out of [`transact`] takes the file with it: a
/// spawn that failed, a write that failed halfway, an early return, or the
/// ordinary answer.
struct TempItems(PathBuf);

impl TempItems {
    fn write(items: &[Value]) -> Result<Self> {
        // The process id, a counter and the clock: two calls in one process
        // cannot collide even inside one nanosecond, and `create_new` refuses
        // rather than truncating if some other file already holds the name.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "kanban-transact-{}-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default(),
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("create {}", path.display()))?;
        // Owned before the first byte is written, so a failure mid-write
        // removes the file too rather than leaving a half-list behind.
        let items_file = Self(path);
        file.write_all(serde_json::to_string(items)?.as_bytes())
            .with_context(|| format!("write {}", items_file.0.display()))?;
        file.flush()?;
        Ok(items_file)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempItems {
    fn drop(&mut self) {
        // Best effort and unconditional. There is nothing useful to do with a
        // failure here and nothing to report it on: stdout is the protocol
        // channel.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The `transact` tool as `tools/list` offers it.
///
/// Hand-written for the same reason `batch_tool` is — its input is an ordered
/// list of calls, not a command's flags — and hand-written *only* for that:
/// which operations a batch may carry, the bound, and every `$ref` are still
/// the binary's to judge at call time, so they are not written down twice.
///
/// The board selectors are offered because `transact` addresses one board like
/// any other board command (its `ignoredSelectors` is empty, pinned by
/// `the_schema_lists_transact_as_a_writing_operation`) and because an item that
/// names a board of its own is refused with "pass the selector to `transact`
/// itself" (`rust/lib.rs:4857`) — a refusal naming a fix this schema has to
/// make possible.
fn transact_tool() -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "items".to_owned(),
        json!({
            "type": "array",
            "maxItems": BATCH_LIMIT,
            "description": format!("The calls to run, in order; at most {BATCH_LIMIT}."),
            "items": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "A tool name from tools/list; not batch, and not transact.",
                    },
                    "arguments": {
                        "type": "object",
                        "description": "That tool's arguments, exactly as tools/call takes them, \
                                        except that no item may name a board of its own. Any \
                                        value may be {\"$ref\": {\"item\": N, \"path\": \
                                        \"/json/pointer\"}}, replaced by the value at that RFC \
                                        6901 pointer inside item N's result, where N is strictly \
                                        earlier.",
                    },
                },
                "required": ["name"],
                "additionalProperties": false,
            },
        }),
    );
    let ignored = crate::ignored_selectors(TRANSACT, None).0;
    for flag in TOOL_GLOBALS.iter().filter(|flag| !ignored.contains(*flag)) {
        properties.insert(
            (*flag).to_owned(),
            json!({ "type": "string", "description": format!("--{flag}.") }),
        );
    }
    json!({
        "name": TRANSACT,
        "description": format!(
            "kanban transact. Run up to {BATCH_LIMIT} tool calls in one request, in the order \
             given, through one transaction on one board: either every item lands or none of \
             them does. Each item names a tool from this list other than batch and transact, \
             and passes no board selector of its own -- db, project and workspace belong to \
             transact itself. Answers with one envelope, {{\"ok\", \"batchId\", \"failedIndex\", \
             \"rolledBack\", \"results\": [{{\"index\", \"ok\", \"result\"}} | {{\"index\", \
             \"ok\": false, \"error\"}} | {{\"index\", \"ok\": false, \"skipped\": true}}]}}, and \
             is an error whenever ok is false. Execution stops at the first failure, every item \
             before it is rolled back, and every item after it is skipped rather than attempted, \
             so rolledBack true means the board is exactly where the batch found it; \
             rolledBack false on a failure means the list was refused before anything ran. Any \
             argument value may be {{\"$ref\": {{\"item\": N, \"path\": \"/json/pointer\"}}}}, \
             replaced by the value at that pointer in item N's result, which is how the lease \
             token a claim returns reaches a later checkpoint without passing through the \
             caller. Each item is authorized exactly as if it had arrived alone. There is no \
             idempotency key: a replayed batch is not a no-op, though one that begins with a \
             claim is refused at index 0 and therefore lands nothing."
        ),
        "inputSchema": {
            "type": "object",
            "properties": Value::Object(properties),
            "required": ["items"],
        },
        "annotations": { "readOnlyHint": false },
    })
}

/// Answer one writing batch by running the binary **once** for the whole list.
///
/// This is deliberately not `batch`'s loop, and the difference is the whole
/// point of the tool. A process per item is a connection per item, and two
/// connections cannot share a transaction, so a per-entry spawn would offer
/// atomicity it could not deliver (ADR-041 §3.4). One run keeps ADR-011's
/// invariant — "a tool call runs the binary" — literally true while the
/// binary keeps ownership of the ordering, the rollback and the ledger stamps.
///
/// Only two things happen here that the binary does not do again: the item
/// list is staged in a file, because a list is not a flag value and an argv
/// string has a ceiling (§8); and the shape of that list is checked far enough
/// to answer a caller who sent something that is not a list of calls at all.
/// Names, the bound and every `$ref` are checked by the binary, pre-flight,
/// before it opens the board — re-checking them here would be the second
/// validation ADR-011 exists to keep out, free to drift from the first.
fn transact(path: &Path, arguments: &Value) -> Value {
    let empty = serde_json::Map::new();
    let supplied = match arguments {
        Value::Object(supplied) => supplied,
        Value::Null => &empty,
        other => return error_result(&format!("arguments must be an object, not {other}")),
    };
    // `items-file` is this layer's business and is not offered, so a caller
    // naming it is told that rather than having it silently become a second
    // `--items-file` on the command line.
    if let Some(unknown) = supplied
        .keys()
        .find(|key| key.as_str() != "items" && !TOOL_GLOBALS.contains(&key.as_str()))
    {
        return error_result(&format!("{TRANSACT} has no argument {unknown}"));
    }
    let items = match supplied.get("items") {
        Some(Value::Array(items)) => items,
        Some(other) => {
            return error_result(&format!("{TRANSACT} items must be an array, not {other}"));
        }
        None => return error_result(&format!("{TRANSACT} needs an items array")),
    };
    // The binary would refuse each of these too, in the same words, but a
    // refusal that costs a process start is exactly what this tool exists to
    // avoid — and a call whose `items` is not a list of calls has nothing to
    // gain from reaching a board.
    for (index, item) in items.iter().enumerate() {
        let Value::Object(fields) = item else {
            return error_result(&format!(
                "{TRANSACT} item {index} must be an object, not {item}; nothing in it ran"
            ));
        };
        if !matches!(fields.get("name"), Some(Value::String(_))) {
            return error_result(&format!(
                "{TRANSACT} item {index} needs a name; nothing in it ran"
            ));
        }
        // Absent and null are how an item with no arguments is spelled, so
        // they are accepted here exactly as the binary accepts them: a tool
        // stricter than the command it runs refuses legal calls.
        match fields.get("arguments") {
            None | Some(Value::Null) | Some(Value::Object(_)) => {}
            Some(other) => {
                return error_result(&format!(
                    "{TRANSACT} item {index} has arguments that are not an object: {other}; \
                     nothing in it ran"
                ));
            }
        }
    }
    // The selectors travel through `arguments_for` like every other tool's, so
    // this layer holds no second opinion about them; only the item list is
    // handled here.
    let selectors = supplied
        .iter()
        .filter(|(key, _)| key.as_str() != "items")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<String, Value>>();
    let argv = match arguments_for(TRANSACT, &Value::Object(selectors)) {
        Ok(argv) => argv,
        Err(error) => return error_result(&error.to_string()),
    };
    let items_file = match TempItems::write(items) {
        Ok(items_file) => items_file,
        Err(error) => {
            return error_result(&format!("could not stage the {TRANSACT} items: {error:#}"));
        }
    };
    // `kanban transact --items-file PATH [selectors] --json`: the list goes in
    // beside the operation, ahead of the trailing `--json` `arguments_for`
    // always ends with. Kept as `OsString` so a temp directory whose name is
    // not UTF-8 still names the file it wrote rather than a lossy near-miss.
    let mut argv = argv.into_iter().map(OsString::from).collect::<Vec<_>>();
    argv.splice(
        1..1,
        [
            OsString::from("--items-file"),
            items_file.path().as_os_str().to_owned(),
        ],
    );
    let output = match Command::new(path).args(&argv).output() {
        Ok(output) => output,
        Err(error) => return error_result(&format!("could not run kanban: {error}")),
    };
    // The envelope is the answer — `ok`, `failedIndex` and `rolledBack` are
    // what a caller acts on — so it is passed through exactly as the CLI
    // printed it, for both verdicts. `error_result` would replace it with a
    // sentence and throw away the one document that says how far the batch
    // got.
    let envelope = String::from_utf8_lossy(&output.stdout).into_owned();
    if envelope.trim().is_empty() {
        // No envelope means the binary died before printing one, so its stderr
        // line is all there is to hand back.
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return error_result(if message.is_empty() {
            "the transact failed without a message"
        } else {
            &message
        });
    }
    // The envelope's own verdict decides `isError`, so a caller reading the
    // flag and a caller reading the document cannot disagree. It is also the
    // CLI's exit status, which is nonzero exactly when `ok` is false. An
    // answer this layer cannot parse is a failure, the same rule `batch`
    // applies to a result it cannot read.
    let landed = serde_json::from_str::<Value>(&envelope)
        .ok()
        .and_then(|parsed| parsed["ok"].as_bool())
        .unwrap_or(false);
    json!({
        "content": [{ "type": "text", "text": envelope }],
        "isError": !landed,
    })
}

/// Answer one request, or `None` when it was a notification.
fn respond(path: &Path, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    // A notification carries no id and must never be answered, or the client
    // sees a reply to a request it never made.
    id.as_ref()?;
    let id = id.unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            let asked = request
                .get("params")
                .and_then(|params| params.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            Ok(json!({
                "protocolVersion": asked,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "kanban", "version": env!("CARGO_PKG_VERSION") },
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let params = request.get("params").cloned().unwrap_or(Value::Null);
            match params.get("name").and_then(Value::as_str) {
                Some(name) => Ok(call(
                    path,
                    name,
                    params.get("arguments").unwrap_or(&Value::Null),
                )),
                None => Err((-32602, "tools/call needs a name".to_owned())),
            }
        }
        other => Err((-32601, format!("no such method {other}"))),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }),
    })
}

/// Serve MCP over stdio until the client closes the stream.
pub fn serve() -> Result<()> {
    let reload = Reload::new();
    let mut frames = Frames::stdin();
    let mut output = std::io::stdout();
    loop {
        // Between requests, with nothing buffered, is the one moment a swap is
        // invisible. Checked before blocking on the next read so a client that
        // goes quiet does not pin an old build indefinitely.
        if frames.idle() && reload.changed() {
            reload.take_over();
        }
        let Some(line) = frames.next_line()? else {
            return Ok(());
        };
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => respond(&reload.path, &request),
            Err(error) => Some(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": { "code": -32700, "message": format!("invalid JSON: {error}") },
            })),
        };
        if let Some(response) = response {
            // One frame per line, flushed, because the client is blocking on it.
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temp-directory name no concurrent test can also pick.
    ///
    /// The clock alone was not enough: `SystemTime::now()` reports
    /// microseconds here, and two tests on two threads inside one microsecond
    /// wrote the same script over each other.
    fn unique_name(kind: &str, extension: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "kanban-mcp-{kind}-{}-{}-{}.{extension}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    struct TempExecutable(PathBuf);

    impl TempExecutable {
        fn new(body: &str) -> Self {
            let path = unique_name("stub", "sh");
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            Self(path)
        }
    }

    impl AsRef<Path> for TempExecutable {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempExecutable {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn tool(name: &str) -> Value {
        tools()
            .into_iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing tool named {name}"))
    }

    /// A file a stub executable writes to, removed with the test.
    struct TempLog(PathBuf);

    impl TempLog {
        fn new(label: &str) -> Self {
            Self(unique_name(label, "log"))
        }

        fn shell_path(&self) -> String {
            self.0.display().to_string()
        }

        /// Every line written so far; empty when the stub never ran, which is
        /// how "without spawning" is observed.
        fn lines(&self) -> Vec<String> {
            fs::read_to_string(&self.0)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// A stub `kanban` that records one line per invocation — its whole
    /// argument list, then the bytes of the file `--items-file` named — and
    /// answers with `envelope` and `code`.
    ///
    /// The recorded argv is how a spawn is counted, and reading the items file
    /// back is what proves the file existed *while* the child ran rather than
    /// merely having been created.
    fn counting_stub(log: &TempLog, envelope: &str, code: i32) -> TempExecutable {
        TempExecutable::new(&format!(
            r#"#!/bin/sh
{{
  printf 'argv'
  for arg in "$@"; do printf '\t%s' "$arg"; done
  printf '\n'
}} >> '{log}'
printf 'items\t%s\n' "$(cat "$3")" >> '{log}'
printf '%s' '{envelope}'
exit {code}
"#,
            log = log.shell_path(),
        ))
    }

    /// Serializes the tests that count files in the shared temp directory, so
    /// one test's staged items cannot be mistaken for another's leak.
    fn temp_items_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The staged item files this process is currently holding.
    fn staged_item_files() -> Vec<PathBuf> {
        let prefix = format!("kanban-transact-{}-", std::process::id());
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect()
    }

    #[test]
    fn a_tool_only_offers_globals_the_cli_accepts() {
        // TOOL_GLOBALS is a subset of the real global flags with the two this
        // layer supplies or must never forward removed. Drifting from
        // GLOBAL_FLAGS would advertise a flag the CLI refuses.
        for flag in TOOL_GLOBALS {
            assert!(
                crate::GLOBAL_FLAGS.contains(&flag),
                "--{flag} is offered on every tool but is not a global flag"
            );
        }
        for excluded in ["json", "help"] {
            assert!(
                !TOOL_GLOBALS.contains(&excluded),
                "--{excluded} must never be forwarded from a tool call"
            );
            assert!(
                crate::GLOBAL_FLAGS.contains(&excluded),
                "{excluded} is excluded from a set it was never in"
            );
        }
    }

    #[test]
    fn a_tool_offers_no_selector_the_cli_refuses_for_that_operation() {
        // The CLI refuses `doctor --db`; advertising `db` on the doctor tool
        // would have an agent send an argument the binary then rejects, and
        // the agent would read the refusal as its own mistake.
        for (command, sub, ..) in COMMANDS {
            if crate::LONG_RUNNING.contains(command) {
                continue;
            }
            let ignored = crate::ignored_selectors(command, *sub).0;
            let properties = tool(&tool_name(command, *sub))["inputSchema"]["properties"].clone();
            for flag in TOOL_GLOBALS {
                assert_eq!(
                    properties.get(flag).is_some(),
                    !ignored.contains(&flag),
                    "{command} {sub:?} offers or withholds {flag} against what the CLI does"
                );
            }
        }
        // And a call that sends one anyway is refused here rather than
        // relocated into the child process.
        let error = arguments_for("doctor", &json!({ "db": "/tmp/somewhere.db" }))
            .expect_err("doctor has no db argument")
            .to_string();
        assert!(error.contains("doctor has no argument db"), "{error}");
        // A selector the operation honours still travels.
        assert!(
            arguments_for(
                "workspace_attach",
                &json!({ "workspace": "/tmp/tree", "to": "Alpha" })
            )
            .unwrap()
            .contains(&"--workspace".to_owned())
        );
        assert!(
            arguments_for("task_list", &json!({ "db": "/tmp/b.db" }))
                .unwrap()
                .contains(&"--db".to_owned())
        );
    }

    #[test]
    fn a_value_that_is_not_one_value_is_refused() {
        assert_eq!(scalar("title", &json!("a title")).unwrap(), "a title");
        assert_eq!(scalar("priority", &json!(3)).unwrap(), "3");
        for bad in [json!(["a", "b"]), json!({ "a": 1 })] {
            let error = scalar("title", &bad)
                .expect_err("a composite value must not be flattened into one")
                .to_string();
            assert!(error.contains("title"), "{error}");
            assert!(error.contains("single value"), "{error}");
        }
    }

    #[test]
    fn tool_names_are_flattened_and_reversible() {
        assert_eq!(tool_name("task", Some("add")), "task_add");
        assert_eq!(
            tool_name("import", Some("atmux-sqlite")),
            "import_atmux_sqlite"
        );
        assert_eq!(tool_name("claim", None), "claim");
        // Every operation must round-trip, or a tool exists that cannot be
        // called: `tools/list` advertises the name and `tools/call` looks it up.
        for (command, sub, ..) in COMMANDS {
            let name = tool_name(command, *sub);
            assert!(
                COMMANDS.iter().any(|(c, s, ..)| tool_name(c, *s) == name),
                "{name} does not resolve back to an operation"
            );
            assert!(
                name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{name} is not a usable MCP tool name"
            );
        }
        // And no two operations collapse onto one name.
        let mut names = COMMANDS
            .iter()
            .map(|(c, s, ..)| tool_name(c, *s))
            .collect::<Vec<_>>();
        names.sort();
        let total = names.len();
        names.dedup();
        assert_eq!(total, names.len(), "two operations share a tool name");
        // The listed set is every callable operation plus exactly one name
        // that is not an operation. Spelled out rather than relaxed: a second
        // hand-written tool must be added here to be legal, and `batch` must
        // not collide with a command, or `tools/call batch` would answer for
        // an operation instead.
        assert!(
            !names.contains(&BATCH.to_owned()),
            "an operation is named {BATCH} and the batch tool would shadow it"
        );
        let generated = COMMANDS
            .iter()
            .filter(|(command, ..)| !crate::LONG_RUNNING.contains(command))
            .map(|(c, s, ..)| tool_name(c, *s))
            .collect::<std::collections::BTreeSet<_>>();
        let listed = tools()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        let mut expected = generated;
        expected.insert(BATCH.to_owned());
        assert_eq!(
            listed, expected,
            "tools/list is not the operations plus {BATCH}"
        );
        assert_eq!(listed.len(), tools().len(), "tools/list repeats a name");
        // And the name that is not an operation resolves to no operation, which
        // is what stops a batch from carrying a batch.
        assert_eq!(listed_read_only(BATCH), None);
        assert_eq!(listed_read_only("task_list"), Some(true));
        assert_eq!(listed_read_only("task_add"), Some(false));
        // A long-running command is not listed, so a batch may not name one
        // even though `arguments_for` can still build its argv.
        assert_eq!(listed_read_only("watch"), None);
    }

    #[test]
    fn the_manifest_records_required_optional_list_and_boolean_shapes() {
        let claim = tool("claim");
        assert_eq!(claim["inputSchema"]["required"], json!([]));
        assert_eq!(claim["inputSchema"]["properties"]["id"]["type"], "string");
        assert_eq!(
            claim["inputSchema"]["properties"]["id"]["description"],
            "Positional argument id."
        );

        let add = tool("task_add");
        assert_eq!(add["inputSchema"]["required"], json!(["title"]));
        assert_eq!(
            add["inputSchema"]["properties"]["depends-on"]["type"],
            "array"
        );
        assert_eq!(
            add["inputSchema"]["properties"]["driver-only"]["type"],
            "boolean"
        );
        assert_eq!(
            add["inputSchema"]["properties"]["priority"]["type"],
            "string"
        );

        let list = tool("task_list");
        assert_eq!(list["inputSchema"]["properties"]["all"]["type"], "boolean");
        for flag in ["with-relations", "with-claims"] {
            assert_eq!(
                list["inputSchema"]["properties"][flag]["type"], "boolean",
                "--{flag}"
            );
        }
    }

    #[test]
    fn arguments_translate_null_false_repeatable_and_unknown_shapes() {
        let argv = arguments_for(
            "task_add",
            &json!({
                "title": "Write the test",
                "id": null,
                "depends-on": ["t-1", "t-2"],
                "driver-only": false,
            }),
        )
        .unwrap();
        assert_eq!(argv[0], "task");
        assert_eq!(argv[1], "add");
        assert_eq!(argv.last().map(String::as_str), Some("--json"));
        assert!(!argv.iter().any(|arg| arg == "null"));
        assert!(!argv.iter().any(|arg| arg == "--id"));
        assert!(!argv.iter().any(|arg| arg == "--driver-only"));
        assert_eq!(
            argv.iter()
                .filter(|arg| arg.as_str() == "--depends-on")
                .count(),
            2
        );
        assert!(argv.iter().any(|arg| arg == "t-1"));
        assert!(argv.iter().any(|arg| arg == "t-2"));

        let absent = arguments_for("task_list", &json!({"all": false})).unwrap();
        assert_eq!(absent, vec!["task", "list", "--json"]);

        let unknown = arguments_for("task_add", &json!({"title": "x", "frobnicate": true}))
            .expect_err("an unknown argument must be rejected");
        let unknown = unknown.to_string();
        assert!(unknown.contains("frobnicate"), "{unknown}");

        let malformed = arguments_for("task_list", &json!("not-an-object"))
            .expect_err("a non-object arguments payload must be rejected");
        let malformed = malformed.to_string();
        assert!(malformed.contains("must be an object"), "{malformed}");
    }

    #[test]
    fn respond_handles_notifications_default_initialize_ping_list_missing_name_and_unknown_method()
    {
        let path = Path::new("/tmp/kanban-mcp-unused");

        assert!(respond(path, &json!({"jsonrpc": "2.0", "method": "ping"})).is_none());

        let initialize = respond(
            path,
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}),
        )
        .unwrap();
        assert_eq!(initialize["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(initialize["result"]["serverInfo"]["name"], "kanban");
        assert_eq!(
            initialize["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );

        let ping = respond(path, &json!({"jsonrpc": "2.0", "id": 2, "method": "ping"})).unwrap();
        assert_eq!(ping["result"], json!({}));

        let listed = respond(
            path,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        )
        .unwrap();
        assert!(!listed["result"]["tools"].as_array().unwrap().is_empty());

        let missing_name = respond(
            path,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"arguments": {}}}),
        )
        .unwrap();
        assert_eq!(missing_name["error"]["code"], -32602);
        assert!(
            missing_name["error"]["message"]
                .as_str()
                .unwrap()
                .contains("name")
        );

        let unknown_method = respond(
            path,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "no/such"}),
        )
        .unwrap();
        assert_eq!(unknown_method["error"]["code"], -32601);
        assert!(
            unknown_method["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no such method")
        );
    }

    #[test]
    fn call_translates_success_nonzero_and_exec_failures_safely() {
        let ok = TempExecutable::new(
            r#"#!/bin/sh
printf '%s\n' "$@"
"#,
        );
        let succeeded = call(
            ok.as_ref(),
            "task_add",
            &json!({
                "title": "Cover the shim",
                "id": null,
                "depends-on": ["t-a", "t-b"],
                "driver-only": true
            }),
        );
        assert_eq!(succeeded["isError"], false);
        let text = succeeded["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("task"), "{text}");
        assert!(text.contains("add"), "{text}");
        assert!(text.contains("--depends-on"), "{text}");
        assert!(text.contains("t-a"), "{text}");
        assert!(text.contains("t-b"), "{text}");
        assert!(text.contains("--driver-only"), "{text}");
        assert!(text.contains("--json"), "{text}");

        let failed = TempExecutable::new(
            r#"#!/bin/sh
printf 'refused by shim\n' >&2
exit 7
"#,
        );
        let refused = call(failed.as_ref(), "task_list", &json!({"all": false}));
        assert_eq!(refused["isError"], true);
        assert_eq!(refused["content"][0]["text"], "refused by shim");

        let missing = call(
            Path::new("/definitely/not/a/binary"),
            "task_list",
            &json!({}),
        );
        assert_eq!(missing["isError"], true);
        assert!(
            missing["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("could not run kanban")
        );
    }

    #[test]
    fn the_transact_tool_is_hand_written_once_and_says_it_writes() {
        let listed = tools();
        assert_eq!(
            listed
                .iter()
                .filter(|tool| tool["name"] == TRANSACT)
                .count(),
            1,
            "transact is advertised more than once, so a client sees two schemas for it"
        );
        let transact = tool(TRANSACT);
        assert_eq!(transact["annotations"]["readOnlyHint"], false);
        // Hand-written, not generated off the `COMMANDS` row: the row's flags
        // would arrive as two strings -- `items` an array an agent has to
        // serialize by hand, and `items-file` a path only this layer may name.
        let properties = &transact["inputSchema"]["properties"];
        assert_eq!(properties["items"]["type"], "array");
        assert_eq!(properties["items"]["maxItems"], BATCH_LIMIT);
        assert!(
            properties.get("items-file").is_none(),
            "the tool offers the file path this layer owns: {transact}"
        );
        assert_eq!(transact["inputSchema"]["required"], json!(["items"]));
        let item = &properties["items"]["items"];
        assert_eq!(item["required"], json!(["name"]));
        assert_eq!(item["additionalProperties"], false);
        assert_eq!(item["properties"]["name"]["type"], "string");
        assert_eq!(item["properties"]["arguments"]["type"], "object");
        // It addresses one board, and an item naming a board of its own is
        // refused telling the caller to pass the selector to `transact`
        // itself, so all three have to be passable.
        for flag in TOOL_GLOBALS {
            assert_eq!(properties[flag]["type"], "string", "--{flag}");
        }
        // ADR-041 freezes the read-only batch. Asserted beside the new tool
        // because this is the pair a harness withholds mutation by.
        assert_eq!(tool(BATCH)["annotations"]["readOnlyHint"], true);
        assert_eq!(listed_read_only(TRANSACT), Some(false));
    }

    #[test]
    fn a_transact_runs_the_binary_once_with_the_whole_list_in_a_file() {
        let _serialized = temp_items_lock();
        let log = TempLog::new("transact-once");
        let stub = counting_stub(&log, r#"{"ok":true,"results":[]}"#, 0);
        let items = (0..5)
            .map(|index| json!({ "name": "note", "arguments": { "id": "t-1", "text": index } }))
            .collect::<Vec<_>>();

        let answered = transact(
            stub.as_ref(),
            &json!({ "items": items, "project": "BOARD" }),
        );
        assert_eq!(answered["isError"], false);
        assert_eq!(
            answered["content"][0]["text"], r#"{"ok":true,"results":[]}"#,
            "the envelope was not passed through as the CLI printed it"
        );

        let lines = log.lines();
        let argv = lines
            .iter()
            .filter(|line| line.starts_with("argv\t"))
            .collect::<Vec<_>>();
        assert_eq!(
            argv.len(),
            1,
            "a five-item list ran the binary {} times: {lines:?}",
            argv.len()
        );
        let words = argv[0].split('\t').skip(1).collect::<Vec<_>>();
        assert_eq!(words[0], TRANSACT);
        assert_eq!(words[1], "--items-file");
        assert_eq!(&words[3..], ["--project", "BOARD", "--json"]);

        // The child read the whole list out of the file, so the file was there
        // while it ran -- and it is not there now.
        let seen = lines
            .iter()
            .find_map(|line| line.strip_prefix("items\t"))
            .expect("the stub read its items file");
        assert_eq!(
            serde_json::from_str::<Value>(seen).unwrap(),
            Value::Array(items)
        );
        assert!(
            !Path::new(words[2]).exists(),
            "{} outlived the call that staged it",
            words[2]
        );
        assert_eq!(staged_item_files(), Vec::<PathBuf>::new());
    }

    #[test]
    fn the_items_file_is_gone_on_success_on_failure_and_on_an_early_return() {
        let _serialized = temp_items_lock();
        assert_eq!(
            staged_item_files(),
            Vec::<PathBuf>::new(),
            "a previous call leaked its items file"
        );
        let items = vec![json!({ "name": "stale", "arguments": {} })];

        // The file the guard writes: only the owner may read it, because the
        // list holds a caller's writes and the temp directory is shared.
        let staged = TempItems::write(&items).unwrap();
        let path = staged.path().to_owned();
        assert!(path.exists());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            serde_json::from_str::<Value>(&fs::read_to_string(&path).unwrap()).unwrap(),
            json!(items)
        );
        // Two calls in one process do not collide.
        let second = TempItems::write(&items).unwrap();
        assert_ne!(second.path(), path);
        drop(second);
        drop(staged);
        assert!(!path.exists(), "the guard left {} behind", path.display());

        // An early return between the write and the spawn: the guard's scope
        // ends without a child ever running, and the file goes with it.
        let abandoned = {
            let staged = TempItems::write(&items).unwrap();
            staged.path().to_owned()
        };
        assert!(!abandoned.exists());

        // A refused batch: the binary answers nonzero with an envelope.
        let log = TempLog::new("transact-refused");
        let refused_stub = counting_stub(
            &log,
            r#"{"ok":false,"failedIndex":0,"rolledBack":true,"results":[]}"#,
            1,
        );
        let refused = transact(refused_stub.as_ref(), &json!({ "items": items }));
        assert_eq!(refused["isError"], true);
        assert_eq!(staged_item_files(), Vec::<PathBuf>::new());

        // And a spawn that never happened at all.
        let unspawnable = transact(
            Path::new("/definitely/not/a/binary"),
            &json!({ "items": items }),
        );
        assert_eq!(unspawnable["isError"], true);
        assert!(
            unspawnable["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("could not run kanban"),
            "{unspawnable}"
        );
        assert_eq!(
            staged_item_files(),
            Vec::<PathBuf>::new(),
            "a failed spawn left its items file in the temp directory"
        );
    }

    #[test]
    fn a_list_that_is_not_a_list_of_calls_is_refused_without_running_the_binary() {
        // Held because the staged-file snapshots in the tests above must not
        // see this test's own temporary items.
        let _serialized = temp_items_lock();
        let log = TempLog::new("transact-malformed");
        let stub = counting_stub(&log, r#"{"ok":true,"results":[]}"#, 0);

        for (arguments, expected) in [
            (json!({}), "transact needs an items array"),
            (json!(null), "transact needs an items array"),
            (json!("nope"), "arguments must be an object"),
            (json!({ "items": "[]" }), "items must be an array"),
            (
                json!({ "items": [], "items-file": "/tmp/elsewhere.json" }),
                "transact has no argument items-file",
            ),
            (json!({ "items": [], "frobnicate": true }), "no argument"),
            (json!({ "items": ["note"] }), "item 0 must be an object"),
            (
                json!({ "items": [{ "arguments": {} }] }),
                "item 0 needs a name",
            ),
            (
                json!({ "items": [{ "name": ["note"] }] }),
                "item 0 needs a name",
            ),
            (
                json!({ "items": [{ "name": "note", "arguments": "id=t-1" }] }),
                "item 0 has arguments that are not an object",
            ),
            (
                json!({ "items": [{ "name": "stale" }, { "name": "note", "arguments": 7 }] }),
                "item 1 has arguments that are not an object",
            ),
        ] {
            let refused = transact(stub.as_ref(), &arguments);
            assert_eq!(refused["isError"], true, "{arguments} was accepted");
            let text = refused["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(expected), "{arguments}: {text}");
            assert_eq!(
                log.lines(),
                Vec::<String>::new(),
                "{arguments} cost a process start to be refused"
            );
        }

        // An item with no arguments at all is legal -- the CLI reads absent and
        // null the same way -- so it must reach the binary rather than be
        // refused by a tool stricter than the command it runs.
        let ran = transact(stub.as_ref(), &json!({ "items": [{ "name": "stale" }] }));
        assert_eq!(ran["isError"], false, "{ran}");
        assert_eq!(
            log.lines()
                .iter()
                .filter(|line| line.starts_with("argv\t"))
                .count(),
            1
        );
    }

    #[test]
    fn the_envelopes_own_verdict_decides_is_error() {
        // Same reason as above: this test stages items of its own.
        let _serialized = temp_items_lock();
        let log = TempLog::new("transact-verdict");
        let items = json!({ "items": [{ "name": "stale" }] });

        let landed = counting_stub(&log, r#"{"ok":true,"batchId":"b","results":[]}"#, 0);
        assert_eq!(transact(landed.as_ref(), &items)["isError"], false);

        // The failing answer keeps its envelope: `failedIndex` and
        // `rolledBack` are how far the batch got, and a sentence in their
        // place would throw that away.
        let rolled_back = counting_stub(
            &log,
            r#"{"ok":false,"failedIndex":1,"rolledBack":true,"results":[]}"#,
            1,
        );
        let failed = transact(rolled_back.as_ref(), &items);
        assert_eq!(failed["isError"], true);
        let envelope: Value =
            serde_json::from_str(failed["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(envelope["failedIndex"], 1);
        assert_eq!(envelope["rolledBack"], true);

        // An answer this layer cannot read is a failure, not a success whose
        // verdict it guessed.
        let unreadable = counting_stub(&log, "not an envelope", 0);
        let unreadable = transact(unreadable.as_ref(), &items);
        assert_eq!(unreadable["isError"], true);
        assert_eq!(unreadable["content"][0]["text"], "not an envelope");

        // Nothing on stdout means the binary died before printing an
        // envelope, so its stderr line is what the caller gets.
        let silent = TempExecutable::new(
            r#"#!/bin/sh
printf 'the board is locked by another writer\n' >&2
exit 9
"#,
        );
        let silent = transact(silent.as_ref(), &items);
        assert_eq!(silent["isError"], true);
        assert_eq!(
            silent["content"][0]["text"],
            "the board is locked by another writer"
        );

        let mute = TempExecutable::new(
            r#"#!/bin/sh
exit 9
"#,
        );
        let mute = transact(mute.as_ref(), &items);
        assert_eq!(mute["isError"], true);
        assert_eq!(
            mute["content"][0]["text"],
            "the transact failed without a message"
        );
    }
}
