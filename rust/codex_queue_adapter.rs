use crate::adapter_protocol::{
    AdapterRequest, AdapterResponse, decode_request, validate_request as validate_protocol_request,
};
use anyhow::{Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

const HELP: &str = "kanban-codex-queue-adapter --codex PATH --codex-home PATH --thread NAME --required-version VER";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_CAPTURED_OUTPUT_BYTES: usize = 1 << 16;
const MAX_RENDER_BYTES: usize = 1 << 16;
const CODEX_QUEUE_CONSUMER_ID: &str = "codex.queue";
const ENQUEUE_TURN_ACTION_ID: &str = "enqueue-turn";
const AT_LEAST_ONCE_INSTRUCTION: &str = "At-least-once delivery; deduplicate by idempotency key.";

pub(crate) fn entrypoint() -> Result<()> {
    match parse_outcome(std::env::args_os())? {
        Outcome::Help => {
            let mut stdout = io::stdout();
            writeln!(stdout, "{HELP}")?;
            Ok(())
        }
        Outcome::Version => {
            let mut stdout = io::stdout();
            writeln!(
                stdout,
                "kanban-codex-queue-adapter {}",
                env!("CARGO_PKG_VERSION")
            )?;
            Ok(())
        }
        Outcome::Args(args) => {
            let validated = validate_paths(&args)?;
            probe_codex_version(&validated)?;
            probe_codex_queue_help(&validated)?;
            let request = decode_request_from_stdin()?;
            let message = String::from_utf8(render_message(&request)?)
                .map_err(|_| anyhow::anyhow!("adapter render must be UTF-8"))?;
            run_codex_queue(&validated, &message)?;
            let response = response_for(&request);
            let rendered = render_response(&response)?;
            let mut stdout = io::stdout();
            stdout.write_all(&rendered)?;
            stdout.flush()?;
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Args {
    codex: PathBuf,
    codex_home: PathBuf,
    thread: String,
    required_version: String,
}

#[derive(Debug)]
pub(crate) struct Validated {
    canonical_codex: PathBuf,
    canonical_codex_file: fs::File,
    canonical_codex_identity: FileIdentity,
    canonical_codex_home: PathBuf,
    canonical_codex_home_file: fs::File,
    canonical_codex_home_identity: FileIdentity,
    thread: String,
    required_version: String,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

fn parse_outcome<I>(args: I) -> Result<Outcome>
where
    I: IntoIterator<Item = OsString>,
{
    let tokens: Vec<OsString> = args.into_iter().skip(1).collect();

    if matches!(tokens.as_slice(), [one] if one == "--help") {
        return Ok(Outcome::Help);
    }

    if matches!(tokens.as_slice(), [one] if one == "--version") {
        return Ok(Outcome::Version);
    }

    let mut codex = None;
    let mut codex_home = None;
    let mut thread = None;
    let mut required_version = None;

    let mut index = 0;
    while index < tokens.len() {
        let flag = token_to_str(&tokens[index], "argument")?;
        if !flag.starts_with("--") {
            bail!("positional argument is not allowed: {flag}");
        }

        index += 1;
        let value = tokens
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))?;
        let value = token_to_str(value, "value")?;
        if value.starts_with("--") {
            bail!("missing value for {flag}");
        }

        match flag {
            "--codex" => assign_once(&mut codex, parse_absolute_path(value, "--codex")?, flag)?,
            "--codex-home" => assign_once(
                &mut codex_home,
                parse_absolute_path(value, "--codex-home")?,
                flag,
            )?,
            "--thread" => assign_once(&mut thread, parse_thread(value)?, flag)?,
            "--required-version" => {
                assign_once(&mut required_version, parse_required_version(value)?, flag)?
            }
            _ => bail!("unknown argument: {flag}"),
        }

        index += 1;
    }

    let args = Args {
        codex: codex.ok_or_else(|| anyhow::anyhow!("missing required flag: --codex"))?,
        codex_home: codex_home
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --codex-home"))?,
        thread: thread.ok_or_else(|| anyhow::anyhow!("missing required flag: --thread"))?,
        required_version: required_version
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --required-version"))?,
    };

    Ok(Outcome::Args(args))
}

fn assign_once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if slot.is_some() {
        bail!("argument repeated: {flag}");
    }
    *slot = Some(value);
    Ok(())
}

fn token_to_str<'a>(token: &'a OsStr, label: &str) -> Result<&'a str> {
    token
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 {label}"))
}

fn parse_absolute_path(value: &str, flag: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("{flag} must be an absolute path");
    }
    Ok(path.to_path_buf())
}

fn parse_thread(value: &str) -> Result<String> {
    if value.is_empty() {
        bail!("--thread must be 1..=128 ASCII printable characters");
    }
    if value.len() > 128 {
        bail!("--thread must be 1..=128 ASCII printable characters");
    }
    if value.starts_with('-') {
        bail!("--thread must not start with '-'");
    }
    if value.trim().is_empty() {
        bail!("--thread must not be whitespace-only");
    }
    if !value
        .chars()
        .all(|c| c.is_ascii() && (!c.is_control() || c == ' '))
    {
        bail!("--thread must contain only ASCII printable characters");
    }
    Ok(value.to_owned())
}

fn parse_required_version(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > 32 {
        bail!("--required-version must be 1..=32 ASCII digits and dots");
    }
    if value.starts_with('.') || value.ends_with('.') || value.contains("..") {
        bail!("--required-version must not start, end, or repeat dots");
    }
    if !value.chars().all(|c| c.is_ascii_digit() || c == '.') {
        bail!("--required-version must contain only ASCII digits and dots");
    }
    Ok(value.to_owned())
}

pub(crate) fn validate_paths(args: &Args) -> Result<Validated> {
    let codex_lstat = fs::symlink_metadata(&args.codex)?;
    if !(codex_lstat.file_type().is_file() || codex_lstat.file_type().is_symlink()) {
        bail!("--codex must be a regular file or a symlink to one");
    }

    // This validates the current inode snapshot; it does not claim an atomic open-time guarantee.
    let canonical_codex = fs::canonicalize(&args.codex)?;
    let codex_stat = fs::metadata(&canonical_codex)?;
    if !codex_stat.file_type().is_file() {
        bail!("--codex must resolve to a regular file");
    }

    let codex_mode = codex_stat.permissions().mode();
    if codex_mode & 0o111 == 0 {
        bail!("--codex target must have at least one execute bit");
    }
    if codex_mode & 0o022 != 0 {
        bail!("--codex target must not be group- or other-writable");
    }

    let effective_uid = unsafe { libc::geteuid() };
    let codex_uid = codex_stat.uid();
    if codex_uid != effective_uid && codex_uid != 0 {
        bail!("--codex target must be owned by the effective user or root");
    }

    let codex_home_lstat = fs::symlink_metadata(&args.codex_home)?;
    if codex_home_lstat.file_type().is_symlink() {
        bail!("--codex-home must not be a symlink");
    }
    if !codex_home_lstat.file_type().is_dir() {
        bail!("--codex-home must be an existing directory");
    }

    let canonical_codex_home = fs::canonicalize(&args.codex_home)?;
    let codex_home_stat = fs::metadata(&canonical_codex_home)?;
    if !codex_home_stat.file_type().is_dir() {
        bail!("--codex-home must resolve to a directory");
    }

    let codex_home_mode = codex_home_stat.permissions().mode();
    if codex_home_mode & 0o077 != 0 {
        bail!("--codex-home must not be accessible by group or others");
    }
    let codex_home_uid = codex_home_stat.uid();
    if codex_home_uid != effective_uid && codex_home_uid != 0 {
        bail!("--codex-home must be owned by the effective user or root");
    }

    let canonical_codex_file = fs::File::open(&canonical_codex)?;
    let canonical_codex_identity = file_identity(&canonical_codex_file.metadata()?);
    if canonical_codex_identity != file_identity(&codex_stat) {
        bail!("--codex must resolve to a regular file");
    }

    let canonical_codex_home_file = fs::File::open(&canonical_codex_home)?;
    let canonical_codex_home_identity = file_identity(&canonical_codex_home_file.metadata()?);
    if canonical_codex_home_identity != file_identity(&codex_home_stat) {
        bail!("--codex-home must resolve to a directory");
    }

    let validated = Validated {
        canonical_codex,
        canonical_codex_file,
        canonical_codex_identity,
        canonical_codex_home,
        canonical_codex_home_file,
        canonical_codex_home_identity,
        thread: args.thread.clone(),
        required_version: args.required_version.clone(),
    };

    validate_canonical_path_trust(&validated)?;

    Ok(validated)
}

fn validate_canonical_ancestor_chain(path: &Path, label: &str) -> Result<()> {
    let effective_uid = unsafe { libc::geteuid() };

    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::metadata(ancestor)?;
        if !metadata.file_type().is_dir() {
            bail!(
                "{label} ancestor must be a directory: {}",
                ancestor.display()
            );
        }

        let uid = metadata.uid();
        if uid != effective_uid && uid != 0 {
            bail!(
                "{label} ancestor must be owned by the effective user or root: {}",
                ancestor.display()
            );
        }

        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            let sticky_owned = mode & 0o1000 != 0 && (uid == effective_uid || uid == 0);
            if !sticky_owned {
                bail!(
                    "{label} ancestor must not be group- or other-writable unless sticky and owned by the effective user or root: {}",
                    ancestor.display()
                );
            }
        }
    }

    Ok(())
}

fn validate_request_target(request: &AdapterRequest) -> Result<()> {
    if request.target.consumer_id != CODEX_QUEUE_CONSUMER_ID {
        bail!(
            "adapter target consumer ID must be {}",
            CODEX_QUEUE_CONSUMER_ID
        );
    }
    if request.target.action_id != ENQUEUE_TURN_ACTION_ID {
        bail!(
            "adapter target action ID must be {}",
            ENQUEUE_TURN_ACTION_ID
        );
    }
    Ok(())
}

fn validate_request(request: &AdapterRequest) -> Result<()> {
    validate_protocol_request(request)
}

fn read_bounded_stdin(limit: usize) -> Result<Vec<u8>> {
    let mut stdin = io::stdin().lock().take((limit + 1) as u64);
    let mut bytes = Vec::new();
    stdin.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("adapter request exceeds {limit} bytes");
    }
    Ok(bytes)
}

fn decode_request_bytes(bytes: &[u8]) -> Result<AdapterRequest> {
    if bytes.len() > MAX_STDIN_BYTES {
        bail!("adapter request exceeds {MAX_STDIN_BYTES} bytes");
    }
    let request = decode_request(bytes)?;
    validate_request(&request)?;
    validate_request_target(&request)?;
    Ok(request)
}

fn decode_request_from_stdin() -> Result<AdapterRequest> {
    let bytes = read_bounded_stdin(MAX_STDIN_BYTES)?;
    decode_request_bytes(&bytes)
}

fn read_bounded_stream<R: io::Read>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut exceeded = false;
    let mut scratch = [0u8; 8192];

    loop {
        let read = reader.read(&mut scratch)?;
        if read == 0 {
            break;
        }

        if bytes.len() < limit {
            let remaining = limit - bytes.len();
            let take = remaining.min(read);
            bytes.extend_from_slice(&scratch[..take]);
            if take < read {
                exceeded = true;
            }
        } else {
            exceeded = true;
        }
    }

    Ok((bytes, exceeded))
}

fn capture_output(mut child: std::process::Child, label: &str) -> Result<Output> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stdout pipe for {label}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stderr pipe for {label}"))?;

    let stdout_thread =
        thread::spawn(move || read_bounded_stream(stdout, MAX_CAPTURED_OUTPUT_BYTES));
    let stderr_thread =
        thread::spawn(move || read_bounded_stream(stderr, MAX_CAPTURED_OUTPUT_BYTES));

    let status = child.wait()?;

    let (stdout, stdout_exceeded) = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stdout capture panicked"))??;
    let (stderr, stderr_exceeded) = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stderr capture panicked"))??;

    if stdout_exceeded {
        bail!("{label} stdout exceeds {MAX_CAPTURED_OUTPUT_BYTES} bytes");
    }
    if stderr_exceeded {
        bail!("{label} stderr exceeds {MAX_CAPTURED_OUTPUT_BYTES} bytes");
    }

    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn codex_command(validated: &Validated, args: &[OsString]) -> Command {
    let mut command = Command::new(&validated.canonical_codex);
    command
        .env_clear()
        .env("CODEX_HOME", &validated.canonical_codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    command
}

fn validate_canonical_path_trust(validated: &Validated) -> Result<()> {
    validate_canonical_ancestor_chain(&validated.canonical_codex, "--codex")?;
    validate_canonical_ancestor_chain(&validated.canonical_codex_home, "--codex-home")?;
    Ok(())
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.permissions().mode(),
    }
}

fn validate_codex_identity(validated: &Validated) -> Result<()> {
    let codex_stat = fs::metadata(&validated.canonical_codex)?;
    if !codex_stat.file_type().is_file() {
        bail!("codex target is no longer trusted");
    }

    let mode = codex_stat.permissions().mode();
    if mode & 0o111 == 0 {
        bail!("codex target is no longer trusted");
    }
    if mode & 0o022 != 0 {
        bail!("codex target is no longer trusted");
    }

    let effective_uid = unsafe { libc::geteuid() };
    let uid = codex_stat.uid();
    if uid != effective_uid && uid != 0 {
        bail!("codex target is no longer trusted");
    }

    let pinned_identity = file_identity(&validated.canonical_codex_file.metadata()?);
    if pinned_identity != validated.canonical_codex_identity {
        bail!("codex target is no longer trusted");
    }

    if file_identity(&codex_stat) != pinned_identity {
        bail!("codex target is no longer trusted");
    }

    Ok(())
}

fn validate_codex_home_identity(validated: &Validated) -> Result<()> {
    let codex_home_stat = fs::metadata(&validated.canonical_codex_home)?;
    if !codex_home_stat.file_type().is_dir() {
        bail!("codex home is no longer trusted");
    }

    let mode = codex_home_stat.permissions().mode();
    if mode & 0o077 != 0 {
        bail!("codex home is no longer trusted");
    }

    let effective_uid = unsafe { libc::geteuid() };
    let uid = codex_home_stat.uid();
    if uid != effective_uid && uid != 0 {
        bail!("codex home is no longer trusted");
    }

    let pinned_identity = file_identity(&validated.canonical_codex_home_file.metadata()?);
    if pinned_identity != validated.canonical_codex_home_identity {
        bail!("codex home is no longer trusted");
    }

    if file_identity(&codex_home_stat) != pinned_identity {
        bail!("codex home is no longer trusted");
    }

    Ok(())
}

fn run_codex_command(validated: &Validated, args: &[OsString], label: &str) -> Result<Output> {
    validate_canonical_path_trust(validated)?;
    validate_codex_identity(validated)?;
    validate_codex_home_identity(validated)?;
    let child = codex_command(validated, args).spawn()?;
    capture_output(child, label)
}

fn version_args() -> Vec<OsString> {
    vec![OsString::from("--version")]
}

fn queue_help_args() -> Vec<OsString> {
    vec![OsString::from("queue"), OsString::from("--help")]
}

fn queue_args(thread: &str, message: &str) -> Vec<OsString> {
    vec![
        OsString::from("queue"),
        OsString::from("--thread"),
        OsString::from(thread),
        OsString::from("--message"),
        OsString::from(message),
    ]
}

fn validate_version_probe(output: &Output, required_version: &str) -> Result<()> {
    if !output.status.success() {
        bail!("codex version probe failed");
    }
    if !output.stderr.is_empty() {
        bail!("codex version probe wrote to stderr");
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("codex version probe stdout must be UTF-8"))?;
    if stdout.trim() != format!("codex-cli {required_version}") {
        bail!("codex version probe returned an unexpected version");
    }
    Ok(())
}

fn validate_queue_help_probe(output: &Output) -> Result<()> {
    if !output.status.success() {
        bail!("codex queue help probe failed");
    }
    if !output.stderr.is_empty() {
        bail!("codex queue help probe wrote to stderr");
    }

    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("codex queue help probe stdout must be UTF-8"))?;
    let lines = stdout.lines().collect::<Vec<_>>();
    let expected_prefix = [
        "Queue a message for an existing session",
        "",
        "Usage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>",
        "",
        "Options:",
    ];
    if lines.len() < expected_prefix.len() {
        bail!("codex queue help probe returned an unexpected help layout");
    }
    if lines[..expected_prefix.len()] != expected_prefix {
        bail!("codex queue help probe returned an unexpected help layout");
    }

    let tail = &lines[expected_prefix.len()..];
    let thread_positions = tail
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == "      --thread <THREAD>").then_some(index))
        .collect::<Vec<_>>();
    let message_positions = tail
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == "      --message <TEXT>").then_some(index))
        .collect::<Vec<_>>();
    if thread_positions.len() != 1
        || message_positions.len() != 1
        || thread_positions[0] >= message_positions[0]
    {
        bail!("codex queue help probe returned an unexpected help layout");
    }

    Ok(())
}

fn probe_codex_version(validated: &Validated) -> Result<()> {
    let output = run_codex_command(validated, &version_args(), "codex version probe")?;
    validate_version_probe(&output, &validated.required_version)
}

fn probe_codex_queue_help(validated: &Validated) -> Result<()> {
    let output = run_codex_command(validated, &queue_help_args(), "codex queue help probe")?;
    validate_queue_help_probe(&output)
}

fn run_codex_queue(validated: &Validated, message: &str) -> Result<()> {
    let args = queue_args(&validated.thread, message);
    let output = run_codex_command(validated, &args, "codex queue invocation")?;
    if !output.status.success() {
        bail!("codex queue invocation failed");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
struct QueueMessage {
    instruction: &'static str,
    #[serde(rename = "idempotencyKey")]
    idempotency_key: String,
    #[serde(rename = "subscriptionID")]
    subscription_id: String,
    #[serde(rename = "eventID")]
    event_id: String,
    attempt: i64,
    event: Value,
}

fn serialize_bounded<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_RENDER_BYTES {
        bail!("adapter render exceeds {MAX_RENDER_BYTES} bytes");
    }
    Ok(bytes)
}

fn render_message(request: &AdapterRequest) -> Result<Vec<u8>> {
    let message = QueueMessage {
        instruction: AT_LEAST_ONCE_INSTRUCTION,
        idempotency_key: format!(
            "{}:{}",
            request.delivery.subscription_id, request.delivery.event_id
        ),
        subscription_id: request.delivery.subscription_id.clone(),
        event_id: request.delivery.event_id.clone(),
        attempt: request.delivery.attempt,
        event: request.event.clone(),
    };
    serialize_bounded(&message)
}

fn response_for(request: &AdapterRequest) -> AdapterResponse {
    AdapterResponse {
        protocol_version: 1,
        subscription_id: request.delivery.subscription_id.clone(),
        event_id: request.delivery.event_id.clone(),
        created_at: request.delivery.created_at,
        replay: request.delivery.attempt > 1,
    }
}

fn render_response(response: &AdapterResponse) -> Result<Vec<u8>> {
    serialize_bounded(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;
    use std::fs::{self, File};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn assert_err_contains(args: Vec<OsString>, needle: &str) {
        let err = parse_outcome(args).unwrap_err().to_string();
        assert!(
            err.contains(needle),
            "expected error containing {needle:?}, got {err:?}"
        );
    }

    fn request() -> AdapterRequest {
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".into(),
                event_id: "a".repeat(64),
                attempt: 2,
                created_at: 123,
            },
            target: AdapterTarget {
                consumer_id: CODEX_QUEUE_CONSUMER_ID.into(),
                action_id: ENQUEUE_TURN_ACTION_ID.into(),
            },
            event: json!({
                "eventHash": "a".repeat(64),
                "eventID": "a".repeat(64),
                "timestamp": 123
            }),
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kanban-codex-queue-{prefix}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, mode: u32) {
        File::create(path).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_dir(path: &Path, mode: u32) {
        fs::create_dir(path).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_script(path: &Path, mode: u32, body: &str) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn make_valid_paths(
        prefix: &str,
        codex_mode: u32,
        home_mode: u32,
    ) -> (PathBuf, PathBuf, PathBuf) {
        let root = temp_dir(prefix);
        let codex = root.join("codex");
        let home = root.join("home");
        write_dir(&home, home_mode);
        write_file(&codex, codex_mode);
        (root, codex, home)
    }

    fn output(stdout: &[u8], stderr: &[u8], success: bool) -> Output {
        use std::os::unix::process::ExitStatusExt;

        Output {
            status: if success {
                std::process::ExitStatus::from_raw(0)
            } else {
                std::process::ExitStatus::from_raw(1 << 8)
            },
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn bounded_stream(bytes: &[u8], limit: usize) -> (Vec<u8>, bool) {
        read_bounded_stream(std::io::Cursor::new(bytes.to_vec()), limit).unwrap()
    }

    #[test]
    fn parses_success() {
        let outcome = parse_outcome(os(&[
            "prog",
            "--required-version",
            "1.2.3",
            "--thread",
            "queue-42",
            "--codex-home",
            "/tmp/codex-home",
            "--codex",
            "/tmp/codex",
        ]))
        .unwrap();

        let Outcome::Args(args) = outcome else {
            panic!("expected args outcome");
        };

        assert_eq!(args.codex, PathBuf::from("/tmp/codex"));
        assert_eq!(args.codex_home, PathBuf::from("/tmp/codex-home"));
        assert_eq!(args.thread, "queue-42");
        assert_eq!(args.required_version, "1.2.3");
    }

    #[test]
    fn recognizes_help_and_version_only() {
        assert_eq!(
            parse_outcome(os(&["prog", "--help"])).unwrap(),
            Outcome::Help
        );
        assert_eq!(
            parse_outcome(os(&["prog", "--version"])).unwrap(),
            Outcome::Version
        );
    }

    #[test]
    fn rejects_positional_arguments() {
        assert_err_contains(
            os(&["prog", "positional"]),
            "positional argument is not allowed",
        );
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert_err_contains(os(&["prog", "--bogus", "value"]), "unknown argument");
    }

    #[test]
    fn rejects_repeated_arguments() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex",
                "/tmp/codex-2",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
                "--required-version",
                "1.2.3",
            ]),
            "argument repeated",
        );
    }

    #[test]
    fn rejects_missing_required_flag() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
            ]),
            "missing required flag: --required-version",
        );
    }

    #[test]
    fn rejects_missing_value() {
        assert_err_contains(
            os(&["prog", "--codex", "--codex-home", "/tmp/codex-home"]),
            "missing value for --codex",
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_value() {
        use std::os::unix::ffi::OsStringExt;

        let mut args = vec![OsString::from("prog"), OsString::from("--thread")];
        args.push(OsStringExt::from_vec(vec![0xFF, 0xFE]));
        args.extend(os(&[
            "--codex",
            "/tmp/codex",
            "--codex-home",
            "/tmp/codex-home",
            "--required-version",
            "1.2.3",
        ]));

        assert_err_contains(args, "non-UTF-8 value");
    }

    #[test]
    fn rejects_relative_codex_path() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "relative",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
                "--required-version",
                "1.2.3",
            ]),
            "--codex must be an absolute path",
        );
    }

    #[test]
    fn rejects_relative_codex_home_path() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "relative",
                "--thread",
                "queue-42",
                "--required-version",
                "1.2.3",
            ]),
            "--codex-home must be an absolute path",
        );
    }

    #[test]
    fn rejects_empty_thread() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "",
                "--required-version",
                "1.2.3",
            ]),
            "--thread must be 1..=128 ASCII printable characters",
        );
    }

    #[test]
    fn rejects_whitespace_only_thread() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "   ",
                "--required-version",
                "1.2.3",
            ]),
            "--thread must not be whitespace-only",
        );
    }

    #[test]
    fn rejects_dash_prefixed_thread() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "-queue",
                "--required-version",
                "1.2.3",
            ]),
            "--thread must not start with '-'",
        );
    }

    #[test]
    fn rejects_control_characters_in_thread() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue\n42",
                "--required-version",
                "1.2.3",
            ]),
            "--thread must contain only ASCII printable characters",
        );
    }

    #[test]
    fn rejects_long_thread() {
        let thread = "a".repeat(129);
        let err = parse_outcome(os(&[
            "prog",
            "--codex",
            "/tmp/codex",
            "--codex-home",
            "/tmp/codex-home",
            "--thread",
            &thread,
            "--required-version",
            "1.2.3",
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("1..=128 ASCII printable characters"));
    }

    #[test]
    fn rejects_bad_version_characters() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
                "--required-version",
                "1.2a",
            ]),
            "--required-version must contain only ASCII digits and dots",
        );
    }

    #[test]
    fn rejects_bad_version_dots() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
                "--required-version",
                "1..2",
            ]),
            "--required-version must not start, end, or repeat dots",
        );
    }

    #[test]
    fn rejects_empty_version() {
        assert_err_contains(
            os(&[
                "prog",
                "--codex",
                "/tmp/codex",
                "--codex-home",
                "/tmp/codex-home",
                "--thread",
                "queue-42",
                "--required-version",
                "",
            ]),
            "--required-version must be 1..=32 ASCII digits and dots",
        );
    }

    #[test]
    fn rejects_long_version() {
        let version = "1".repeat(33);
        let err = parse_outcome(os(&[
            "prog",
            "--codex",
            "/tmp/codex",
            "--codex-home",
            "/tmp/codex-home",
            "--thread",
            "queue-42",
            "--required-version",
            &version,
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("1..=32 ASCII digits and dots"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_paths_accepts_symlink_codex_when_target_is_safe() {
        let (root, codex, home) = make_valid_paths("safe-link", 0o755, 0o700);
        let link = root.join("codex-link");
        symlink(&codex, &link).unwrap();

        let args = Args {
            codex: link,
            codex_home: home.clone(),
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };

        let validated = validate_paths(&args).unwrap();
        assert_eq!(validated.canonical_codex, fs::canonicalize(&codex).unwrap());
        let codex_stat = fs::metadata(&codex).unwrap();
        assert_eq!(
            validated.canonical_codex_identity,
            file_identity(&codex_stat)
        );
        assert_eq!(validated.canonical_codex_identity.uid, codex_stat.uid());
        assert_eq!(
            validated.canonical_codex_identity.mode,
            codex_stat.permissions().mode()
        );
        assert_eq!(
            validated.canonical_codex_home,
            fs::canonicalize(&home).unwrap()
        );
        assert_eq!(
            validated.canonical_codex_home_identity,
            file_identity(&fs::metadata(&home).unwrap())
        );
        let home_stat = fs::metadata(&home).unwrap();
        assert_eq!(validated.canonical_codex_home_identity.uid, home_stat.uid());
        assert_eq!(
            validated.canonical_codex_home_identity.mode,
            home_stat.permissions().mode()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn validate_paths_rejects_unsafe_writable_non_sticky_ancestor() {
        let root = temp_dir("unsafe-ancestor");
        let unsafe_parent = root.join("unsafe-parent");
        write_dir(&unsafe_parent, 0o777);
        let codex = unsafe_parent.join("codex");
        write_file(&codex, 0o755);

        let home = root.join("home");
        write_dir(&home, 0o700);

        let args = Args {
            codex,
            codex_home: home,
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };

        let err = validate_paths(&args).unwrap_err().to_string();
        assert!(
            err.contains("ancestor must not be group- or other-writable"),
            "{err}"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn validate_codex_home_identity_rejects_replaced_home() {
        let (root, codex, home) = make_valid_paths("home-replace", 0o755, 0o700);
        let args = Args {
            codex,
            codex_home: home.clone(),
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };

        let validated = validate_paths(&args).unwrap();

        write_file(&home.join("marker"), 0o600);

        validate_codex_home_identity(&validated).unwrap();

        fs::remove_dir_all(&home).unwrap();
        write_dir(&home, 0o700);
        write_file(&home.join("marker"), 0o600);

        let err = validate_codex_home_identity(&validated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no longer trusted"), "{err}");

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn validate_paths_stores_canonical_home() {
        let (root, codex, home) = make_valid_paths("home-canonical", 0o755, 0o700);
        let home_input = home.join(".");

        let args = Args {
            codex,
            codex_home: home_input.clone(),
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };

        let validated = validate_paths(&args).unwrap();
        let canonical_home = fs::canonicalize(&home_input).unwrap();
        assert_eq!(validated.canonical_codex_home, canonical_home);
        assert_eq!(
            validated.canonical_codex_home_identity,
            file_identity(&fs::metadata(&canonical_home).unwrap())
        );

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn validate_codex_identity_rejects_replaced_codex() {
        let (root, codex, home) = make_valid_paths("codex-replace", 0o755, 0o700);
        let replacement = root.join("codex.replacement");

        let args = Args {
            codex: codex.clone(),
            codex_home: home,
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };

        let validated = validate_paths(&args).unwrap();

        validate_codex_identity(&validated).unwrap();

        let original_identity = validated.canonical_codex_identity;
        fs::remove_file(&codex).unwrap();
        write_script(&replacement, 0o755, "#!/bin/sh\necho replaced\n");
        fs::rename(&replacement, &codex).unwrap();

        let replacement_identity = file_identity(&fs::metadata(&codex).unwrap());
        assert_ne!(replacement_identity, original_identity);

        let err = validate_codex_identity(&validated).unwrap_err().to_string();
        assert!(err.contains("no longer trusted"), "{err}");

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn validate_paths_rejects_nonexec_and_group_writable_codex_targets() {
        let cases = [
            (0o644, "--codex target must have at least one execute bit"),
            (0o775, "--codex target must not be group- or other-writable"),
        ];

        for (mode, needle) in cases {
            let (root, codex, home) = make_valid_paths("codex-mode", mode, 0o700);
            let args = Args {
                codex,
                codex_home: home,
                thread: "queue-42".into(),
                required_version: "1.2.3".into(),
            };
            let err = validate_paths(&args).unwrap_err().to_string();
            assert!(err.contains(needle), "mode {mode:o} produced {err:?}");
            let _ = fs::remove_dir_all(root);
        }
    }

    #[cfg(unix)]
    #[test]
    fn validate_paths_rejects_codex_home_symlink_and_world_access() {
        let (root, codex, home) = make_valid_paths("home-safety", 0o755, 0o700);
        let symlink_home = root.join("home-link");
        symlink(&home, &symlink_home).unwrap();

        let args = Args {
            codex,
            codex_home: symlink_home,
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };
        let err = validate_paths(&args).unwrap_err().to_string();
        assert!(err.contains("--codex-home must not be a symlink"));
        let _ = fs::remove_dir_all(root);

        let (root, codex, home) = make_valid_paths("home-world", 0o755, 0o711);
        let args = Args {
            codex,
            codex_home: home,
            thread: "queue-42".into(),
            required_version: "1.2.3".into(),
        };
        let err = validate_paths(&args).unwrap_err().to_string();
        assert!(err.contains("--codex-home must not be accessible by group or others"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn validate_request_target_rejects_mismatch() {
        let mut consumer_request = request();
        consumer_request.target.consumer_id = "other.consumer".into();
        assert!(validate_request_target(&consumer_request).is_err());

        let mut action_request = request();
        action_request.target.action_id = "other-action".into();
        assert!(validate_request_target(&action_request).is_err());
    }

    #[test]
    fn render_message_uses_the_expected_compact_fields() {
        let message = String::from_utf8(render_message(&request()).unwrap()).unwrap();
        assert_eq!(
            message,
            "{\"instruction\":\"At-least-once delivery; deduplicate by idempotency key.\",\"idempotencyKey\":\"sub-test:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"subscriptionID\":\"sub-test\",\"eventID\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"attempt\":2,\"event\":{\"eventHash\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"eventID\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"timestamp\":123}}"
        );
    }

    #[test]
    fn render_message_rejects_large_payloads() {
        let mut request = request();
        request.event["blob"] = json!("x".repeat(MAX_RENDER_BYTES));
        assert!(render_message(&request).is_err());
    }

    #[test]
    fn response_helper_marks_replays_and_serializes_compact_json() {
        let mut false_request = request();
        false_request.delivery.attempt = 1;
        let response = response_for(&false_request);
        assert!(!response.replay);

        let mut replay_request = request();
        replay_request.delivery.attempt = 3;
        let replay_response = response_for(&replay_request);
        assert!(replay_response.replay);

        let rendered = String::from_utf8(render_response(&replay_response).unwrap()).unwrap();
        assert_eq!(
            rendered,
            "{\"protocolVersion\":1,\"subscriptionID\":\"sub-test\",\"eventID\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"createdAt\":123,\"replay\":true}"
        );
    }

    #[test]
    fn request_decoder_enforces_size_target_and_protocol_validation() {
        let bytes = serde_json::to_vec(&request()).unwrap();
        let decoded = decode_request_bytes(&bytes).unwrap();
        assert_eq!(decoded, request());

        let too_large = vec![b' '; MAX_STDIN_BYTES + 1];
        assert!(decode_request_bytes(&too_large).is_err());

        let mut target_mismatch = request();
        target_mismatch.target.consumer_id = "not.codex".into();
        let bytes = serde_json::to_vec(&target_mismatch).unwrap();
        assert!(decode_request_bytes(&bytes).is_err());
    }

    #[test]
    fn version_probe_arg_vector_is_exact() {
        assert_eq!(version_args(), vec![OsString::from("--version")]);
    }

    #[test]
    fn queue_help_probe_arg_vector_is_exact() {
        assert_eq!(
            queue_help_args(),
            vec![OsString::from("queue"), OsString::from("--help")]
        );
    }

    #[test]
    fn queue_probe_arg_vector_is_exact() {
        assert_eq!(
            queue_args("queue-42", "{\"ok\":true}"),
            vec![
                OsString::from("queue"),
                OsString::from("--thread"),
                OsString::from("queue-42"),
                OsString::from("--message"),
                OsString::from("{\"ok\":true}"),
            ]
        );
    }

    #[test]
    fn version_probe_validator_accepts_exact_trimmed_stdout() {
        let output = output(b"codex-cli 1.2.3\n", b"", true);
        validate_version_probe(&output, "1.2.3").unwrap();
    }

    #[test]
    fn version_probe_validator_rejects_stderr_and_wrong_version() {
        let stderr_output = output(b"codex-cli 1.2.3\n", b"warn", true);
        assert!(validate_version_probe(&stderr_output, "1.2.3").is_err());

        let wrong_output = output(b"codex-cli 1.2.4\n", b"", true);
        assert!(validate_version_probe(&wrong_output, "1.2.3").is_err());
    }

    #[test]
    fn queue_help_probe_validator_requires_expected_markers() {
        let probe_output = output(
            b"Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --config <PATH>    Use a named config file\n      --thread <THREAD>\n      --message <TEXT>\n",
            b"",
            true,
        );
        validate_queue_help_probe(&probe_output).unwrap();

        let missing_output = output(
            b"Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --thread <THREAD>\n",
            b"",
            true,
        );
        assert!(validate_queue_help_probe(&missing_output).is_err());

        let description_only_output = output(
            b"Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --config <PATH>    Use a named config file\n",
            b"",
            true,
        );
        assert!(validate_queue_help_probe(&description_only_output).is_err());

        let reordered_output = output(
            b"Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --message <TEXT>\n      --thread <THREAD>\n",
            b"",
            true,
        );
        assert!(validate_queue_help_probe(&reordered_output).is_err());

        let duplicated_output = output(
            b"Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --thread <THREAD>\n      --thread <THREAD>\n      --message <TEXT>\n",
            b"",
            true,
        );
        assert!(validate_queue_help_probe(&duplicated_output).is_err());

        let header_only_output = output(b"Queue a message for an existing session\n", b"", true);
        assert!(validate_queue_help_probe(&header_only_output).is_err());
    }

    #[test]
    fn bounded_stream_keeps_data_below_limit() {
        let (bytes, exceeded) = bounded_stream(b"abc", 4);
        assert_eq!(bytes, b"abc");
        assert!(!exceeded);
    }

    #[test]
    fn bounded_stream_keeps_exact_limit() {
        let bytes = vec![b'x'; MAX_CAPTURED_OUTPUT_BYTES];
        let (captured, exceeded) = bounded_stream(&bytes, MAX_CAPTURED_OUTPUT_BYTES);
        assert_eq!(captured.len(), MAX_CAPTURED_OUTPUT_BYTES);
        assert_eq!(captured, bytes);
        assert!(!exceeded);
    }

    #[test]
    fn bounded_stream_marks_bytes_above_limit_without_storing_them() {
        let bytes = vec![b'x'; MAX_CAPTURED_OUTPUT_BYTES + 1];
        let (captured, exceeded) = bounded_stream(&bytes, MAX_CAPTURED_OUTPUT_BYTES);
        assert_eq!(captured.len(), MAX_CAPTURED_OUTPUT_BYTES);
        assert_eq!(captured, bytes[..MAX_CAPTURED_OUTPUT_BYTES]);
        assert!(exceeded);
    }
}
