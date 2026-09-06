//! Serialized Cursor worker bridge: exactly one turn at a time per worker.
//!
//! The Cursor worker is a host-configured executable that runs one agent turn
//! per invocation. It is given the delivery on stdin and answers with an
//! `AdapterResponse` on stdout; nothing about the event travels in `argv`,
//! which is world-readable in the process table while a ledger event is
//! private. The worker's `--state-dir` is its `HOME` and its working
//! directory, so a turn mutates session, history and credential state there.
//!
//! That shared state is why this adapter exists as its own binary rather than
//! as another spawn-a-turn clone: two turns running against one state
//! directory interleave writes into the same session files, and the visible
//! symptom is an acknowledgement for the *other* delivery — which
//! [`FailureClass::ResponseMismatched`] exists to refuse rather than report as
//! delivered. See [`acquire_turn_slot`] for the mechanism and
//! [`classify_turn_failure`] for the retry disposition of every failure.

use crate::adapter_protocol::{
    AdapterRequest, AdapterResponse, decode_request, decode_response, encode_request,
};
use anyhow::{Result, bail};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "kanban-cursor-worker-adapter --worker ABSOLUTE_PATH --state-dir ABSOLUTE_PATH --turn-timeout-ms N --queue-wait-ms N";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_RESPONSE_BYTES: usize = 1 << 16;
const MAX_STDERR_BYTES: usize = 1 << 13;
const READ_CHUNK_BYTES: usize = 4096;
const MIN_TURN_TIMEOUT_MS: u64 = 1_000;
const MAX_TURN_TIMEOUT_MS: u64 = 600_000;
const MAX_QUEUE_WAIT_MS: u64 = 600_000;
const POLL: Duration = Duration::from_millis(25);
const CHILD_PATH: &str = "/usr/bin:/bin";
/// Name of the turn slot inside `--state-dir`; see [`acquire_turn_slot`].
const TURN_SLOT_FILE: &str = ".kanban-cursor-turn.lock";
// Cursor's worker runs the turn inside the invocation rather than queueing it
// for a session that is already open, so `enqueue-turn` would name the wrong
// thing; this is the `start`-capability vocabulary the print bridge already
// speaks, without the read-only promise a Cursor worker cannot make.
const CURSOR_CONSUMER_ID: &str = "cursor.worker";
const START_TURN_ACTION_ID: &str = "start-turn";
/// The whole `argv` the worker is invoked with. Fixed, and free of event
/// content: the delivery travels on stdin precisely so a private ledger event
/// never appears in a process listing.
const WORKER_ARGS: [&str; 3] = ["--headless", "--protocol-version", "1"];

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
                "kanban-cursor-worker-adapter {}",
                env!("CARGO_PKG_VERSION")
            )?;
            Ok(())
        }
        Outcome::Args(args) => run(&args),
    }
}

/// Exit status for one classified delivery failure.
///
/// The dispatcher collapses every non-zero adapter exit into `adapter_exit`
/// and discards adapter stderr, so the exit status is the only machine-visible
/// part of the classification. Unclassified local errors -- bad arguments, an
/// undecodable delivery on stdin -- keep the other adapters' plain `1`.
pub(crate) fn exit_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<FailureClass>()
        .map_or(1, |class| class.exit_code())
}

/// One delivery: read it, take the worker's turn slot, run exactly one turn.
///
/// The order is deliberate. The delivery is read from stdin *before* the slot
/// is taken, because a dispatcher that writes it slowly would otherwise keep a
/// healthy worker idle while holding its slot. The slot is released *before*
/// the acknowledgement is written to stdout, for the mirror reason: a
/// dispatcher slow to read our answer must not pin the worker either. The turn
/// itself is the only thing the slot covers.
fn run(args: &Args) -> Result<()> {
    let request = decode_request_from_stdin()?;
    let worker = validate_worker(args)?;
    let slot = acquire_turn_slot(&worker.state_dir, args.queue_wait_ms)?;
    let response = run_turn(&worker, &request, args.turn_timeout_ms);
    drop(slot);
    let response = response?;
    let mut stdout = io::stdout();
    stdout.write_all(&render_response(&response)?)?;
    stdout.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    worker: PathBuf,
    state_dir: PathBuf,
    turn_timeout_ms: u64,
    queue_wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

/// The resolved worker executable and the state directory it owns.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Worker {
    path: PathBuf,
    state_dir: PathBuf,
}

/// One classified way a Cursor worker turn failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    WorkerBusy,
    WorkerUnavailable,
    TurnFailed,
    DeadlineExceeded,
    ResponseInvalid,
    ResponseMismatched,
}

impl FailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::WorkerBusy => "cursor_worker_busy",
            Self::WorkerUnavailable => "cursor_worker_unavailable",
            Self::TurnFailed => "cursor_worker_turn_failed",
            Self::DeadlineExceeded => "cursor_worker_deadline_exceeded",
            Self::ResponseInvalid => "cursor_worker_response_invalid",
            Self::ResponseMismatched => "cursor_worker_response_mismatched",
        }
    }

    /// Whether a later attempt with byte-identical delivery bytes can succeed.
    /// See [`classify_turn_failure`] for the reasoning behind each answer.
    const fn retryable(self) -> bool {
        match self {
            Self::WorkerBusy
            | Self::WorkerUnavailable
            | Self::TurnFailed
            | Self::DeadlineExceeded => true,
            Self::ResponseInvalid | Self::ResponseMismatched => false,
        }
    }

    const fn exit_code(self) -> i32 {
        match self {
            Self::WorkerBusy => 20,
            Self::WorkerUnavailable => 21,
            Self::TurnFailed => 22,
            Self::DeadlineExceeded => 23,
            Self::ResponseInvalid => 24,
            Self::ResponseMismatched => 25,
        }
    }
}

impl fmt::Display for FailureClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let disposition = if self.retryable() {
            "retryable"
        } else {
            "terminal"
        };
        write!(formatter, "{} ({disposition})", self.code())
    }
}

/// Attach a failure class to one detail so `{error:#}` reports the code first.
fn failed(class: FailureClass, detail: impl fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{detail}").context(class)
}

fn unavailable(detail: impl fmt::Display) -> anyhow::Error {
    failed(FailureClass::WorkerUnavailable, detail)
}

/// Why each class is retryable or terminal, and why there are six of them.
///
/// The dispatcher records exactly one error code per failed attempt and
/// discards the adapter's stderr, so this classification is the operator's
/// only clue about what to do next. Every class is therefore a distinct code
/// and a distinct process exit status: a single generic failure would leave a
/// stuck worker, a crashed turn and a worker answering for somebody else's
/// delivery all looking the same, and those three demand different actions.
///
/// Retryable, because a later attempt with byte-identical bytes can succeed:
///
/// * `cursor_worker_busy` -- the whole `--queue-wait-ms` budget elapsed with
///   another turn holding the slot; raised in [`acquire_turn_slot`]. The
///   holder finishes on its own, so nothing about this delivery is wrong and
///   the next attempt is likely to find the worker free.
/// * `cursor_worker_unavailable` -- the worker executable or its state
///   directory is not something we will exec into, or the spawn itself failed;
///   raised in [`validate_executable`], [`validate_state_dir`] and [`spawn`].
///   This is a host condition, not a property of the delivery: the operator
///   installs the worker or fixes the mode, and the same bytes then land.
/// * `cursor_worker_turn_failed` -- the worker ran and exited non-zero, or
///   died on a signal; raised here. An agent turn's exit status carries no
///   refusal semantics: a crashed, killed or rate-limited turn is exactly the
///   transient fault retries exist for. It stays retryable even though a
///   deterministically broken worker will burn the subscription's retry
///   budget, because the alternative -- dropping a delivery the first time an
///   agent process fell over -- loses work that would have landed.
/// * `cursor_worker_deadline_exceeded` -- the turn outran `--turn-timeout-ms`
///   and was killed; raised in [`wait_for_turn`]. A wedged turn says nothing
///   about the delivery, and the ledger already stores a timeout separately
///   from other failures, so this must not look like a rejection.
///
/// Terminal, because retrying identical bytes reproduces the same answer and
/// would spend the subscription's retry budget on a delivery that can never
/// land:
///
/// * `cursor_worker_response_invalid` -- the turn succeeded but its stdout is
///   not a valid `AdapterResponse` for this delivery: not JSON, the wrong
///   shape, an unsupported protocol version, or over the byte cap. That is a
///   worker speaking the wrong contract, which is a configuration fault; the
///   same executable answers the same way next time.
/// * `cursor_worker_response_mismatched` -- stdout *is* a well-formed
///   protocol-1 acknowledgement, but it names a different subscription, event
///   or timestamp. This is the cross-talk signature that serialization is
///   there to prevent, and it is kept apart from the invalid case because it
///   means something quite different: the worker is answering for another
///   delivery, so this one must never be recorded as delivered no matter how
///   healthy the answer looks. Retrying cannot help, and marking it retryable
///   would re-run a turn that has already been run once for someone.
fn classify_turn_failure(status: &ExitStatus, stderr: &[u8]) -> anyhow::Error {
    let cause = match (status.code(), status.signal()) {
        (Some(code), _) => format!("exited with status {code}"),
        (None, Some(signal)) => format!("died on signal {signal}"),
        (None, None) => "ended without a status".to_owned(),
    };
    failed(
        FailureClass::TurnFailed,
        format!("the worker turn {cause}{}", diagnostics(stderr)),
    )
}

/// The worker's stderr, quoted for the operator when a turn fails.
///
/// Non-empty stderr on a *successful* turn is deliberately not a failure: an
/// agent CLI logs progress there, and the print bridge's stricter rule works
/// only because it drives a program whose output format it pins exactly.
fn diagnostics(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("; the worker's stderr was: {text}")
    }
}

/// Take the worker's one turn slot, waiting a bounded time for it.
///
/// **Keyed on the state directory.** The slot is an exclusive `flock` on
/// `<state-dir>/.kanban-cursor-turn.lock`, so the identity that serializes is
/// the resolved state directory's inode -- not the host, not the subscription,
/// not the worker executable. That is the right key because the state
/// directory is the worker's `HOME` and the only thing a turn mutates: two
/// deliveries pointed at one state directory must not both run even if they
/// name different executables, and two pointed at different state directories
/// are independent workers that should run in parallel. Keying on the
/// subscription instead would let two subscriptions share one worker and
/// interleave; keying on the host would serialize workers that share nothing.
///
/// **Wait, bounded, then refuse.** A second delivery waits up to
/// `--queue-wait-ms` and is refused with [`FailureClass::WorkerBusy`] if the
/// slot never comes free. Waiting is the default because a turn is seconds
/// long while the dispatcher's retry backoff is much longer: refusing at once
/// would burn a retry attempt and park a delivery behind a backoff for a
/// worker about to go idle, and it would lose the arrival ordering that
/// queueing gives for free -- the slot is granted in the order attempts reach
/// it. The wait is bounded because a wedged holder must not pin this process,
/// and through it the dispatcher's lease on the delivery, indefinitely;
/// `--queue-wait-ms 0` is the honest spelling of "refuse immediately" for a
/// host that prefers it.
///
/// **A holder that died without releasing.** There is nothing to reap. An
/// `flock` lives on the open file description, so the kernel drops it when the
/// descriptor closes -- on a clean exit, on a panic, on `process::exit`, and
/// on `SIGKILL` alike -- and the next waiter is granted the slot on its next
/// poll. That is why the slot is a lock and not a lease file holding a PID and
/// a timestamp: a lease would need a liveness guess, and every possible guess
/// is wrong in one direction or the other. Too patient and a crashed turn
/// wedges the worker until an operator deletes a file; too eager and two turns
/// run at once, which is the failure this adapter exists to prevent.
///
/// The lock file is created if absent, never truncated, and never unlinked:
/// its contents are irrelevant but its inode *is* the slot, and unlinking it
/// would let the next process create a different inode and take a slot that
/// excludes nobody.
fn acquire_turn_slot(state_dir: &Path, queue_wait_ms: u64) -> Result<TurnSlot> {
    let path = state_dir.join(TURN_SLOT_FILE);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|error| unavailable(format!("open the turn slot {}: {error}", path.display())))?;
    let deadline = Instant::now() + Duration::from_millis(queue_wait_ms);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(TurnSlot { _file: file }),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(POLL),
            Err(TryLockError::WouldBlock) => {
                return Err(failed(
                    FailureClass::WorkerBusy,
                    format!(
                        "another turn held {} for the whole {queue_wait_ms}ms queue wait",
                        path.display()
                    ),
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(unavailable(format!(
                    "lock the turn slot {}: {error}",
                    path.display()
                )));
            }
        }
    }
}

/// One held turn slot. Releasing it is closing the descriptor, so it happens
/// on every exit path including an unwind.
#[derive(Debug)]
struct TurnSlot {
    _file: File,
}

/// Run exactly one turn, with the slot already held.
///
/// The turn deadline is measured from here rather than from process start, so
/// a delivery that queued politely behind another turn still gets its whole
/// `--turn-timeout-ms` budget instead of being killed for someone else's slow
/// turn.
fn run_turn(
    worker: &Worker,
    request: &AdapterRequest,
    turn_timeout_ms: u64,
) -> Result<AdapterResponse> {
    let body = encode_request(request)?;
    let mut child = spawn(worker)?;
    let deadline = Instant::now() + Duration::from_millis(turn_timeout_ms);
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| unavailable("the worker was spawned without a stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| unavailable("the worker was spawned without a stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| unavailable("the worker was spawned without a stderr pipe"))?;

    // The delivery runs up to a megabyte, past any pipe buffer, so a worker
    // that never reads its stdin would park this write until the deadline
    // killed the child. Writing from its own thread keeps the deadline poll
    // below responsive, and the write's own result is deliberately dropped:
    // an acknowledgement has to name this subscription, event and timestamp,
    // which a worker that did not receive the delivery cannot know. The
    // identity check is the proof of receipt, so a failure class here would be
    // an unreachable branch restating it.
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let reading_stdout = thread::spawn(move || bounded_stdout(stdout));
    let reading_stderr = thread::spawn(move || bounded_stderr(stderr));

    let waited = wait_for_turn(&mut child, deadline);
    let captured_stdout = reading_stdout.join();
    let captured_stderr = reading_stderr.join();
    let _ = writer.join();

    let status = waited?;
    let (stdout, truncated) = flatten(captured_stdout, "stdout")?;
    let (stderr, _) = flatten(captured_stderr, "stderr")?;
    if !status.success() {
        return Err(classify_turn_failure(&status, &stderr));
    }
    if truncated {
        return Err(failed(
            FailureClass::ResponseInvalid,
            format!("the worker's acknowledgement exceeds {MAX_RESPONSE_BYTES} bytes"),
        ));
    }
    read_acknowledgement(&stdout, request)
}

fn spawn(worker: &Worker) -> Result<Child> {
    Command::new(&worker.path)
        .args(WORKER_ARGS)
        .current_dir(&worker.state_dir)
        // The dispatcher's own environment is not the worker's: cleared, plus
        // the state directory as `HOME` -- which is what makes the slot's key
        // and the worker's private state the same directory -- and a fixed
        // `PATH` so a turn cannot pick up an executable from an inherited one.
        .env_clear()
        .env("HOME", &worker.state_dir)
        .env("PATH", CHILD_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            unavailable(format!(
                "spawn the worker {}: {error}",
                worker.path.display()
            ))
        })
}

/// Wait for the turn, ending it when the deadline passes.
///
/// Ending it is not a courtesy: the slot is held until this returns, so a
/// wedged turn left running would block every later delivery for this worker
/// and not only this one.
///
/// The end is [`crate::adapter_process::terminate_and_reap`], the same
/// escalation the dispatcher uses on its own adapter timeout: `SIGTERM`, a
/// bounded grace window, then `SIGKILL`, then a reap. A worker that traps
/// `SIGTERM` gets the chance to leave its session files consistent for the
/// next turn, which a bare `SIGKILL` mid-write does not. It also answers the
/// race where the turn finishes in the same instant the deadline passes: the
/// helper returns the real exit status when the worker had already exited
/// before any signal, and that status is reported instead of a fictional
/// timeout.
///
/// Descendants the worker spawned and did not wait for are not chased here.
/// The dispatcher spawns this adapter as its own process-group leader and
/// group-kills that whole tree on its own timeout, so the worker's children
/// are already covered by an outer net; taking a private group for the worker
/// would remove them from it, which trades a bounded leak for an unbounded
/// one.
fn wait_for_turn(child: &mut Child, deadline: Instant) -> Result<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                return Err(failed(
                    FailureClass::TurnFailed,
                    format!("waiting for the worker turn: {error}"),
                ));
            }
        }
        if Instant::now() >= deadline {
            if let Some(status) = crate::adapter_process::terminate_and_reap(child) {
                return Ok(status);
            }
            return Err(failed(
                FailureClass::DeadlineExceeded,
                "the worker turn did not finish before the deadline and was ended",
            ));
        }
        thread::sleep(POLL);
    }
}

fn flatten(
    joined: thread::Result<Result<(Vec<u8>, bool)>>,
    label: &str,
) -> Result<(Vec<u8>, bool)> {
    joined.map_err(|_| unavailable(format!("the worker {label} capture panicked")))?
}

fn bounded_stdout(stdout: ChildStdout) -> Result<(Vec<u8>, bool)> {
    bounded(stdout, MAX_RESPONSE_BYTES)
}

fn bounded_stderr(stderr: ChildStderr) -> Result<(Vec<u8>, bool)> {
    bounded(stderr, MAX_STDERR_BYTES)
}

/// Read a child pipe, keeping at most `limit` bytes and reporting whether the
/// child produced more.
///
/// The stream is drained rather than abandoned at the cap so the worker never
/// blocks on a full pipe and can reach its own exit; what is bounded is the
/// memory this process commits, and the turn deadline bounds the time.
fn bounded<R: io::Read>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut truncated = false;
    loop {
        let count = match reader.read(&mut chunk) {
            Ok(0) => return Ok((kept, truncated)),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(unavailable(format!("reading a worker pipe: {error}"))),
        };
        let take = count.min(limit.saturating_sub(kept.len()));
        kept.extend_from_slice(&chunk[..take]);
        truncated |= take < count;
    }
}

/// Accept the worker's acknowledgement, or classify why it cannot be trusted.
fn read_acknowledgement(bytes: &[u8], request: &AdapterRequest) -> Result<AdapterResponse> {
    match decode_response(bytes, request) {
        Ok(response) => Ok(response),
        Err(error) if names_another_delivery(bytes) => Err(failed(
            FailureClass::ResponseMismatched,
            format!("the worker acknowledged a different delivery: {error:#}"),
        )),
        Err(error) => Err(failed(
            FailureClass::ResponseInvalid,
            format!("the worker did not acknowledge this delivery: {error:#}"),
        )),
    }
}

/// Whether the refused bytes are a well-formed protocol-1 acknowledgement.
///
/// `decode_response` refuses for exactly two reasons: the bytes are not a
/// protocol-1 `AdapterResponse` at all, or they are one and name a different
/// delivery. Asking the first question here is what separates the two classes
/// without restating `decode_response`'s acceptance rule, which stays the one
/// authority on whether an acknowledgement counts.
fn names_another_delivery(bytes: &[u8]) -> bool {
    serde_json::from_slice::<AdapterResponse>(bytes)
        .is_ok_and(|response| response.protocol_version == 1)
}

fn validate_worker(args: &Args) -> Result<Worker> {
    Ok(Worker {
        path: validate_executable(&args.worker)?,
        state_dir: validate_state_dir(&args.state_dir)?,
    })
}

/// Resolve `--worker` and refuse anything we should not exec into.
///
/// One spawn happens per process, immediately after this, so unlike the print
/// bridge -- which probes the same executable several times and must prove the
/// inode never changed between probes -- there is no second invocation to pin
/// an identity for. What is checked is the misconfiguration this can actually
/// see: a symlink standing in for the executable, a non-file, a file nobody
/// can exec, a mode that lets another account rewrite the program we are about
/// to run, and an owner that is neither this account nor root.
fn validate_executable(path: &Path) -> Result<PathBuf> {
    let link = fs::symlink_metadata(path)
        .map_err(|error| unavailable(format!("--worker {}: {error}", path.display())))?;
    if link.file_type().is_symlink() {
        return Err(unavailable("--worker must not be a symlink"));
    }
    let resolved = fs::canonicalize(path)
        .map_err(|error| unavailable(format!("--worker {}: {error}", path.display())))?;
    let metadata = fs::metadata(&resolved)
        .map_err(|error| unavailable(format!("--worker {}: {error}", resolved.display())))?;
    if !metadata.is_file() {
        return Err(unavailable("--worker must be a regular file"));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(unavailable("--worker must be executable"));
    }
    if mode & 0o022 != 0 {
        return Err(unavailable("--worker must not be group- or world-writable"));
    }
    trusted_owner(&metadata, "--worker")?;
    Ok(resolved)
}

/// Resolve `--state-dir`, which becomes the worker's `HOME`, its working
/// directory, and the identity the turn slot is keyed on.
///
/// It must be private (`0o077` clear) rather than merely non-writable: it
/// holds the worker's session and credential state, and it is also where the
/// turn slot lives, so an account that can create files there can create a
/// second lock file and defeat the serialization.
fn validate_state_dir(path: &Path) -> Result<PathBuf> {
    let link = fs::symlink_metadata(path)
        .map_err(|error| unavailable(format!("--state-dir {}: {error}", path.display())))?;
    if link.file_type().is_symlink() {
        return Err(unavailable("--state-dir must not be a symlink"));
    }
    let resolved = fs::canonicalize(path)
        .map_err(|error| unavailable(format!("--state-dir {}: {error}", path.display())))?;
    let metadata = fs::metadata(&resolved)
        .map_err(|error| unavailable(format!("--state-dir {}: {error}", resolved.display())))?;
    if !metadata.is_dir() {
        return Err(unavailable("--state-dir must be a directory"));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(unavailable("--state-dir must not be accessible to others"));
    }
    trusted_owner(&metadata, "--state-dir")?;
    Ok(resolved)
}

fn trusted_owner(metadata: &fs::Metadata, label: &str) -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid && metadata.uid() != 0 {
        return Err(unavailable(format!("{label} owner is not trusted")));
    }
    Ok(())
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

    let mut worker = None;
    let mut state_dir = None;
    let mut turn_timeout_ms = None;
    let mut queue_wait_ms = None;

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
            "--worker" => assign_once(&mut worker, absolute(value, flag)?, flag)?,
            "--state-dir" => assign_once(&mut state_dir, absolute(value, flag)?, flag)?,
            "--turn-timeout-ms" => assign_once(
                &mut turn_timeout_ms,
                parse_millis(value, flag, MIN_TURN_TIMEOUT_MS, MAX_TURN_TIMEOUT_MS)?,
                flag,
            )?,
            "--queue-wait-ms" => assign_once(
                &mut queue_wait_ms,
                parse_millis(value, flag, 0, MAX_QUEUE_WAIT_MS)?,
                flag,
            )?,
            _ => bail!("unknown argument: {flag}"),
        }
        index += 1;
    }

    Ok(Outcome::Args(Args {
        worker: worker.ok_or_else(|| anyhow::anyhow!("missing required flag: --worker"))?,
        state_dir: state_dir
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --state-dir"))?,
        turn_timeout_ms: turn_timeout_ms
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --turn-timeout-ms"))?,
        queue_wait_ms: queue_wait_ms
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --queue-wait-ms"))?,
    }))
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

/// Absolute paths only, and never from the environment: the dispatcher clears
/// the child environment, and a relative path would resolve against whatever
/// working directory the dispatcher happened to run in.
fn absolute(value: &str, flag: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("{flag} must be an absolute path");
    }
    Ok(path.to_path_buf())
}

fn parse_millis(value: &str, flag: &str, low: u64, high: u64) -> Result<u64> {
    let millis: u64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("{flag} must be an integer"))?;
    if !(low..=high).contains(&millis) {
        bail!("{flag} must be in {low}..={high}");
    }
    Ok(millis)
}

fn decode_request_from_stdin() -> Result<AdapterRequest> {
    let mut stdin = io::stdin().lock().take((MAX_STDIN_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    stdin.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_STDIN_BYTES {
        bail!("adapter request exceeds {MAX_STDIN_BYTES} bytes");
    }
    let request = decode_request(&bytes)?;
    validate_request_target(&request)?;
    Ok(request)
}

fn validate_request_target(request: &AdapterRequest) -> Result<()> {
    if request.target.consumer_id != CURSOR_CONSUMER_ID {
        bail!("adapter target consumer ID must be {CURSOR_CONSUMER_ID}");
    }
    if request.target.action_id != START_TURN_ACTION_ID {
        bail!("adapter target action ID must be {START_TURN_ACTION_ID}");
    }
    Ok(())
}

fn render_response(response: &AdapterResponse) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bail!("adapter response exceeds {MAX_RESPONSE_BYTES} bytes");
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    const CLASSES: [FailureClass; 6] = [
        FailureClass::WorkerBusy,
        FailureClass::WorkerUnavailable,
        FailureClass::TurnFailed,
        FailureClass::DeadlineExceeded,
        FailureClass::ResponseInvalid,
        FailureClass::ResponseMismatched,
    ];

    static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "kanban-cursor-adapter-unit-{label}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock moved backwards")
                .as_nanos(),
            NEXT_ROOT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&path).expect("create the temporary state directory");
        let mut permissions = fs::metadata(&path)
            .expect("stat the directory")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions).expect("tighten the directory");
        TempDir(path)
    }

    fn args(values: &[&str]) -> Result<Outcome> {
        parse_outcome(
            std::iter::once(OsString::from("kanban-cursor-worker-adapter"))
                .chain(values.iter().map(OsString::from)),
        )
    }

    fn full_args() -> Vec<&'static str> {
        vec![
            "--worker",
            "/opt/cursor/worker",
            "--state-dir",
            "/var/lib/cursor",
            "--turn-timeout-ms",
            "30000",
            "--queue-wait-ms",
            "5000",
        ]
    }

    fn request() -> AdapterRequest {
        let event_id = "a".repeat(64);
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".to_owned(),
                event_id: event_id.clone(),
                attempt: 1,
                created_at: 1_720_000_000,
            },
            target: AdapterTarget {
                consumer_id: CURSOR_CONSUMER_ID.to_owned(),
                action_id: START_TURN_ACTION_ID.to_owned(),
            },
            event: json!({
                "eventID": event_id,
                "eventHash": event_id,
                "timestamp": 1_720_000_000_i64,
            }),
        }
    }

    #[test]
    fn a_full_argument_set_parses_and_every_malformed_spelling_is_refused() {
        assert_eq!(
            args(&full_args()).expect("the documented argument set parses"),
            Outcome::Args(Args {
                worker: PathBuf::from("/opt/cursor/worker"),
                state_dir: PathBuf::from("/var/lib/cursor"),
                turn_timeout_ms: 30_000,
                queue_wait_ms: 5_000,
            })
        );
        assert_eq!(args(&["--help"]).unwrap(), Outcome::Help);
        assert_eq!(args(&["--version"]).unwrap(), Outcome::Version);

        // A zero queue wait is the supported "refuse immediately" spelling, so
        // it must parse where an out-of-range turn timeout does not.
        let mut zero_wait = full_args();
        zero_wait[7] = "0";
        assert!(matches!(
            args(&zero_wait).unwrap(),
            Outcome::Args(Args {
                queue_wait_ms: 0,
                ..
            })
        ));

        for (spelling, expected) in [
            (vec!["run", "--worker", "/opt/cursor/worker"], "positional"),
            (vec!["--nope", "1"], "unknown argument"),
            (vec!["--worker"], "missing value for --worker"),
            (
                vec!["--worker", "--state-dir", "/var/lib/cursor"],
                "missing value for --worker",
            ),
            (vec!["--worker", "cursor/worker"], "absolute path"),
            (vec!["--state-dir", "var/lib/cursor"], "absolute path"),
            (vec!["--turn-timeout-ms", "soon"], "must be an integer"),
            (vec!["--turn-timeout-ms", "999"], "must be in 1000..=600000"),
            (
                vec!["--turn-timeout-ms", "600001"],
                "must be in 1000..=600000",
            ),
            (vec!["--queue-wait-ms", "600001"], "must be in 0..=600000"),
            (vec!["--queue-wait-ms", "-1"], "must be an integer"),
        ] {
            let error = format!(
                "{:#}",
                args(&spelling).expect_err("this spelling must be refused")
            );
            assert!(
                error.contains(expected),
                "{spelling:?} reported {error}, which does not name {expected}"
            );
        }

        for flag in ["--worker", "--state-dir"] {
            let mut repeated = full_args();
            repeated.extend_from_slice(&[flag, "/tmp"]);
            let error = format!("{:#}", args(&repeated).expect_err("a repeat is refused"));
            assert!(
                error.contains(&format!("argument repeated: {flag}")),
                "{error}"
            );
        }

        for missing in [
            "--worker",
            "--state-dir",
            "--turn-timeout-ms",
            "--queue-wait-ms",
        ] {
            let full = full_args();
            let position = full.iter().position(|token| *token == missing).unwrap();
            let mut without = full.clone();
            without.drain(position..position + 2);
            let error = format!("{:#}", args(&without).expect_err("an omission is refused"));
            assert!(
                error.contains(&format!("missing required flag: {missing}")),
                "{error}"
            );
        }
    }

    #[test]
    fn every_failure_class_reports_a_distinct_code_and_exit_status() {
        let mut codes: Vec<&str> = CLASSES.iter().map(|class| class.code()).collect();
        let mut statuses: Vec<i32> = CLASSES.iter().map(|class| class.exit_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        statuses.sort_unstable();
        statuses.dedup();
        assert_eq!(codes.len(), CLASSES.len(), "duplicate failure code");
        assert_eq!(statuses.len(), CLASSES.len(), "duplicate exit status");
        assert!(
            statuses.iter().all(|status| *status > 1),
            "a class reused the unclassified exit status"
        );
        assert_eq!(
            exit_code(&anyhow::anyhow!("a local error carries no class")),
            1
        );
        assert_eq!(
            exit_code(&failed(FailureClass::ResponseMismatched, "detail")),
            25
        );
    }

    /// The dispatcher decides whether to re-deliver from the class alone, so
    /// the split is a contract and not an implementation detail.
    #[test]
    fn the_retry_disposition_of_every_class_is_the_documented_one() {
        for (class, retryable) in [
            (FailureClass::WorkerBusy, true),
            (FailureClass::WorkerUnavailable, true),
            (FailureClass::TurnFailed, true),
            (FailureClass::DeadlineExceeded, true),
            (FailureClass::ResponseInvalid, false),
            (FailureClass::ResponseMismatched, false),
        ] {
            assert_eq!(class.retryable(), retryable, "{} flipped", class.code());
            let shown = class.to_string();
            let disposition = if retryable { "retryable" } else { "terminal" };
            assert_eq!(shown, format!("{} ({disposition})", class.code()));
        }
    }

    #[test]
    fn a_delivery_for_another_consumer_or_action_is_refused() {
        validate_request_target(&request()).expect("the bridge's own target is accepted");
        let mut foreign_consumer = request();
        foreign_consumer.target.consumer_id = "claude.print".to_owned();
        assert!(
            format!(
                "{:#}",
                validate_request_target(&foreign_consumer).expect_err("refused")
            )
            .contains("consumer ID must be cursor.worker")
        );
        let mut foreign_action = request();
        foreign_action.target.action_id = "enqueue-turn".to_owned();
        assert!(
            format!(
                "{:#}",
                validate_request_target(&foreign_action).expect_err("refused")
            )
            .contains("action ID must be start-turn")
        );
    }

    /// Two turns against one state directory contend; two against different
    /// state directories do not. A slot keyed on anything coarser would
    /// serialize unrelated workers, and anything narrower would let two turns
    /// share one worker `HOME`.
    #[test]
    fn a_turn_slot_is_keyed_on_the_state_directory() {
        let first = temp_dir("slot-a");
        let second = temp_dir("slot-b");
        let held = acquire_turn_slot(&first.0, 0).expect("the first turn takes the slot");

        let error = format!(
            "{:#}",
            acquire_turn_slot(&first.0, 0).expect_err("a second turn must not take the same slot")
        );
        assert!(error.contains("cursor_worker_busy (retryable)"), "{error}");
        assert!(error.contains(TURN_SLOT_FILE), "{error}");

        let other_worker =
            acquire_turn_slot(&second.0, 0).expect("a different worker is not blocked");
        drop(other_worker);
        drop(held);
    }

    /// The claim that a dead holder needs no reaping: closing the descriptor
    /// is the release, so the next waiter proceeds with no expiry heuristic.
    #[test]
    fn a_slot_whose_holder_vanished_is_immediately_available() {
        let state_dir = temp_dir("slot-release");
        let held = acquire_turn_slot(&state_dir.0, 0).expect("the first turn takes the slot");
        drop(held);
        acquire_turn_slot(&state_dir.0, 0).expect("the released slot is free at once");
    }

    /// A bounded wait is the documented policy, so it has to actually wait:
    /// a refusal that ignored the budget would turn every brief overlap into a
    /// burnt retry attempt.
    #[test]
    fn a_queue_wait_is_spent_before_the_slot_is_refused() {
        let state_dir = temp_dir("slot-wait");
        let held = acquire_turn_slot(&state_dir.0, 0).expect("the first turn takes the slot");
        let started = Instant::now();
        let error = acquire_turn_slot(&state_dir.0, 250).expect_err("the slot never came free");
        let waited = started.elapsed();
        drop(held);
        assert!(
            waited >= Duration::from_millis(250),
            "the refusal came after only {waited:?}"
        );
        assert!(
            format!("{error:#}").contains("250ms queue wait"),
            "{error:#}"
        );
    }

    #[test]
    fn a_state_directory_open_to_others_is_refused_before_a_turn_runs() {
        let state_dir = temp_dir("state-mode");
        let mut permissions = fs::metadata(&state_dir.0).unwrap().permissions();
        permissions.set_mode(0o707);
        fs::set_permissions(&state_dir.0, permissions).unwrap();
        let error = format!(
            "{:#}",
            validate_state_dir(&state_dir.0).expect_err("an open state directory is refused")
        );
        assert!(
            error.contains("cursor_worker_unavailable (retryable)"),
            "{error}"
        );
        assert!(
            error.contains("must not be accessible to others"),
            "{error}"
        );
    }

    #[test]
    fn a_worker_that_is_not_an_executable_file_is_refused() {
        let root = temp_dir("worker-mode");
        let directory = root.0.join("not-a-file");
        fs::create_dir(&directory).unwrap();
        assert!(
            format!(
                "{:#}",
                validate_executable(&directory).expect_err("refused")
            )
            .contains("must be a regular file")
        );

        let plain = root.0.join("plain");
        fs::write(&plain, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&plain).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&plain, permissions).unwrap();
        assert!(
            format!("{:#}", validate_executable(&plain).expect_err("refused"))
                .contains("must be executable")
        );

        let mut permissions = fs::metadata(&plain).unwrap().permissions();
        permissions.set_mode(0o757);
        fs::set_permissions(&plain, permissions).unwrap();
        assert!(
            format!("{:#}", validate_executable(&plain).expect_err("refused"))
                .contains("must not be group- or world-writable")
        );

        let mut permissions = fs::metadata(&plain).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&plain, permissions).unwrap();
        assert_eq!(
            validate_executable(&plain).expect("a private executable is accepted"),
            fs::canonicalize(&plain).unwrap()
        );

        let link = root.0.join("link");
        std::os::unix::fs::symlink(&plain, &link).unwrap();
        assert!(
            format!("{:#}", validate_executable(&link).expect_err("refused"))
                .contains("must not be a symlink")
        );
        assert!(
            format!("{:#}", validate_state_dir(&link).expect_err("refused"))
                .contains("must not be a symlink")
        );
    }

    /// An acknowledgement for somebody else's delivery must not be reported as
    /// this delivery's, and must not be reported as a malformed one either:
    /// the two classes send the operator to different places.
    #[test]
    fn an_acknowledgement_is_accepted_matched_and_otherwise_classified() {
        let request = request();
        let accepted = json!({
            "protocolVersion": 1,
            "subscriptionID": "sub-test",
            "eventID": "a".repeat(64),
            "createdAt": 1_720_000_000_i64,
            "replay": false
        });
        let response = read_acknowledgement(&serde_json::to_vec(&accepted).unwrap(), &request)
            .expect("an acknowledgement for this delivery is accepted");
        assert_eq!(response.event_id, "a".repeat(64));
        // The adapter re-serializes the decoded acknowledgement rather than
        // forwarding the worker's bytes, so what must survive is the content.
        assert_eq!(
            serde_json::from_slice::<Value>(&render_response(&response).unwrap()).unwrap(),
            accepted
        );

        for (label, body) in [
            (
                "subscription",
                json!({
                    "protocolVersion": 1,
                    "subscriptionID": "sub-other",
                    "eventID": "a".repeat(64),
                    "createdAt": 1_720_000_000_i64,
                    "replay": false
                }),
            ),
            (
                "event",
                json!({
                    "protocolVersion": 1,
                    "subscriptionID": "sub-test",
                    "eventID": "f".repeat(64),
                    "createdAt": 1_720_000_000_i64,
                    "replay": false
                }),
            ),
            (
                "timestamp",
                json!({
                    "protocolVersion": 1,
                    "subscriptionID": "sub-test",
                    "eventID": "a".repeat(64),
                    "createdAt": 1_720_000_001_i64,
                    "replay": false
                }),
            ),
        ] {
            let error = format!(
                "{:#}",
                read_acknowledgement(&serde_json::to_vec(&body).unwrap(), &request)
                    .expect_err("a foreign acknowledgement is refused")
            );
            assert!(
                error.contains("cursor_worker_response_mismatched (terminal)"),
                "the {label} mismatch reported {error}"
            );
        }

        for (label, bytes) in [
            ("truncated JSON", br#"{"protocolVersion":1,"#.to_vec()),
            ("empty stdout", Vec::new()),
            ("prose", b"turn complete\n".to_vec()),
            (
                "an unsupported protocol version",
                serde_json::to_vec(&json!({
                    "protocolVersion": 2,
                    "subscriptionID": "sub-test",
                    "eventID": "a".repeat(64),
                    "createdAt": 1_720_000_000_i64,
                    "replay": false
                }))
                .unwrap(),
            ),
            (
                "a trailing document",
                [
                    serde_json::to_vec(&accepted).unwrap(),
                    b" {\"protocolVersion\":1}".to_vec(),
                ]
                .concat(),
            ),
        ] {
            let error = format!(
                "{:#}",
                read_acknowledgement(&bytes, &request).expect_err("refused")
            );
            assert!(
                error.contains("cursor_worker_response_invalid (terminal)"),
                "{label} reported {error}"
            );
        }
    }

    /// A turn that fails has to say how it failed: the stderr tail is the only
    /// diagnostic the operator gets, and a signalled turn must not be reported
    /// as a clean non-zero exit.
    #[test]
    fn a_failed_turn_reports_its_status_and_the_workers_stderr() {
        let exited = classify_turn_failure(&ExitStatus::from_raw(23 << 8), b"  auth expired  ");
        let shown = format!("{exited:#}");
        assert!(
            shown.contains("cursor_worker_turn_failed (retryable)"),
            "{shown}"
        );
        assert!(shown.contains("exited with status 23"), "{shown}");
        assert!(
            shown.contains("the worker's stderr was: auth expired"),
            "{shown}"
        );

        let signalled = format!("{:#}", classify_turn_failure(&ExitStatus::from_raw(9), b""));
        assert!(signalled.contains("died on signal 9"), "{signalled}");
        assert!(
            !signalled.contains("stderr was"),
            "an empty stderr was quoted anyway: {signalled}"
        );
    }

    /// The response cap is a memory bound, not a reason to leave a worker
    /// blocked on a full pipe: everything is drained, only the head is kept.
    #[test]
    fn a_bounded_read_keeps_its_cap_and_reports_the_overflow() {
        let (kept, truncated) = bounded(&b"short"[..], 16).unwrap();
        assert_eq!(kept, b"short");
        assert!(!truncated);

        let flood = vec![b'x'; READ_CHUNK_BYTES * 3];
        let (kept, truncated) = bounded(&flood[..], 8).unwrap();
        assert_eq!(kept, vec![b'x'; 8]);
        assert!(truncated);
    }
}
