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
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
fn tool_name(command: &str, sub: Option<&str>) -> String {
    match sub {
        Some(sub) => format!("{command}_{sub}"),
        None => command.to_owned(),
    }
    .replace('-', "_")
}

/// Every operation, as a tool an agent can be handed.
///
/// Built from `COMMANDS` for the reason ADR-010 gives: a hand-written tool list
/// is a second description of the surface and drifts from the first one
/// silently. `readOnly` travels with each tool so a harness can withhold
/// mutation without keeping its own list of which calls write.
fn tools() -> Vec<Value> {
    COMMANDS
        .iter()
        .filter(|(command, ..)| *command != "mcp")
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
            for flag in flags.iter().chain(TOOL_GLOBALS.iter()) {
                let (kind, description): (Value, String) = if crate::REPEATABLE.contains(flag) {
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
        .collect()
}

/// Turn a tool call into the argument list the CLI would have been given.
fn arguments_for(name: &str, arguments: &Value) -> Result<Vec<String>> {
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
    argv.extend(sub.map(str::to_owned));
    for positional in *positionals {
        let key = positional.trim_start_matches('?');
        match supplied.get(key) {
            Some(Value::Null) | None => {}
            Some(value) => argv.push(scalar(key, value)?),
        }
    }
    let allowed = flags
        .iter()
        .copied()
        .chain(TOOL_GLOBALS.iter().copied())
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
        let repeatable = crate::REPEATABLE.contains(&key.as_str());
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
    }
}
