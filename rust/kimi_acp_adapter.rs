//! Kimi ACP pubsub adapter: one delivery, one framed exchange, no guessing.
//!
//! # Framing
//!
//! The Agent Client Protocol carries JSON-RPC 2.0 over NDJSON on the peer's
//! stdio -- one complete JSON-RPC message per line, terminated by a single
//! `\n`, written to the peer's stdin and read back from its stdout. This
//! adapter speaks exactly that transport and nothing else: it writes one
//! request frame and reads one response frame.
//!
//! A partial frame can never be mistaken for a complete one, for two reasons
//! that hold together:
//!
//! * The terminator is the only thing that ends a frame. [`read_one_frame`]
//!   returns [`Frame::Complete`] only after it has observed the `\n`; end of
//!   file is not a terminator, so a peer that writes half an object and dies
//!   yields [`Frame::Truncated`] and is reported as an unanswered delivery.
//!   The bytes it did write are never handed to the decoder.
//! * The terminator cannot occur inside a frame. Compact `serde_json` output
//!   emits no insignificant whitespace, and a U+000A inside a JSON string is
//!   escaped as the two bytes `\` `n`. So the first `\n` in the stream is
//!   always the end of the first message, in both directions.
//!
//! Everything after that first terminator is left unread: this process asked
//! one question and accepts one answer, so a second frame is not part of the
//! answer and is never spliced onto it.
//!
//! # Scope
//!
//! One exchange means the adapter does not run the ACP `initialize` /
//! `session/new` handshake, and therefore does not drive a stock vendor CLI's
//! interactive session lifecycle. The pinned `--kimi` executable must speak
//! the ACP transport and accept the delivery method below as its first
//! message. That is the adapter's contract with the host configuration, and it
//! is why no argv is passed to the peer: the flag that puts a given vendor CLI
//! into ACP mode is a property of that CLI's release, not of this protocol,
//! and pinning an unverified flag here would fail against the real tool in a
//! way no hermetic test could catch.
//!
//! The method name is namespaced under `_kanban/`. The underscore prefix is
//! the ACP extension convention, so this method can never collide with a core
//! ACP method, and a peer that does not implement it answers with a JSON-RPC
//! error -- a legible refusal -- rather than silently doing something else.
//!
//! `params` is the `AdapterRequest` verbatim and the accepted `result` is an
//! `AdapterResponse`: the crate's one pubsub adapter protocol, not a second
//! one invented for this transport. Delivery is at-least-once and the peer
//! must deduplicate on `subscriptionID`:`eventID`, which the frame carries.

use crate::adapter_process::terminate_and_reap;
use crate::adapter_protocol::{AdapterRequest, AdapterResponse, decode_request, decode_response};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "kanban-kimi-acp-adapter --kimi ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --request-timeout-ms N";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_REQUEST_FRAME_BYTES: usize = 1 << 20;
const MAX_RESPONSE_FRAME_BYTES: usize = 1 << 16;
const READ_CHUNK_BYTES: usize = 4096;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
// Fixed, and never inherited: the dispatcher clears the child environment
// before every invocation, so the peer's PATH must be a compile-time literal
// naming only root-owned system directories.
const CHILD_PATH: &str = "/usr/bin:/bin";
const CLEANUP_WINDOW: Duration = Duration::from_millis(2_000);
const CLEANUP_POLL: Duration = Duration::from_millis(10);
const CONSUMER_ID: &str = "kimi.acp";
// The acknowledgement means the peer accepted the delivery; the turn it drives
// happens afterwards. That is the queue semantics `codex.queue` and
// `opencode.server` already speak, so this adapter reuses their action
// vocabulary instead of inventing a third word for the same thing.
const ACTION_ID: &str = "enqueue-turn";
const JSONRPC_VERSION: &str = "2.0";
const DELIVER_METHOD: &str = "_kanban/deliverEvent";
// One process delivers one event over one fresh pipe, so a fixed id keeps the
// frame byte-identical across attempts. Any other id in the answer means the
// peer replied to something this process never asked.
const DELIVERY_REQUEST_ID: i64 = 1;

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
                "kanban-kimi-acp-adapter {}",
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
/// untrusted peer path, an undecodable delivery on stdin, a peer that cannot
/// be spawned at all -- keep the other adapters' plain `1`.
pub(crate) fn exit_code(error: &anyhow::Error) -> i32 {
    error
        .downcast_ref::<FailureClass>()
        .map_or(1, |class| class.exit_code())
}

fn run(args: &Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(args.request_timeout_ms);
    let validated = validate_paths(args)?;
    let request = decode_request_from_stdin()?;
    let response = deliver(&validated, &request, deadline)?;
    let mut stdout = io::stdout();
    stdout.write_all(&render_response(&response)?)?;
    stdout.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    kimi: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    request_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

#[derive(Debug)]
struct Validated {
    kimi: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
}

/// One classified way a framed delivery to the ACP peer failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    PeerUnanswered,
    FrameMalformed,
    FrameOversized,
    IdentityMismatch,
    DeadlineExceeded,
    RequestRejected,
}

impl FailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::PeerUnanswered => "kimi_peer_unanswered",
            Self::FrameMalformed => "kimi_frame_malformed",
            Self::FrameOversized => "kimi_frame_oversized",
            Self::IdentityMismatch => "kimi_identity_mismatch",
            Self::DeadlineExceeded => "kimi_deadline_exceeded",
            Self::RequestRejected => "kimi_request_rejected",
        }
    }

    /// Whether a later attempt with a byte-identical delivery frame can
    /// succeed. This is the classification the operator and the retry budget
    /// both act on, so each answer is recorded here with its reasoning.
    ///
    /// The dispatcher records exactly one error code per failed attempt and
    /// discards this adapter's stderr, so every class is a distinct code and a
    /// distinct process exit status. A single generic failure would tell the
    /// operator to go read the peer's own logs, which is the thing this
    /// classification exists to avoid.
    ///
    /// Retryable, because nothing about the delivery is wrong and a later
    /// attempt with the same bytes can land:
    ///
    /// * `kimi_peer_unanswered` -- the peer closed stdout, exited, or faulted
    ///   before one complete frame arrived; raised in [`receive_frame`] and in
    ///   [`write_frame`]. A peer that is starting, restarting, crashed, or not
    ///   logged in says nothing about the event. This is also where a
    ///   *truncated* frame lands: a half-written object is not an answer, so
    ///   it is reported as no answer instead of being parsed, and the
    ///   at-least-once contract plus the peer's own `subscriptionID`:`eventID`
    ///   deduplication cover the case where the peer had in fact accepted the
    ///   delivery before dying.
    /// * `kimi_deadline_exceeded` -- the `--request-timeout-ms` budget elapsed
    ///   while writing the frame or waiting for the answer; raised in
    ///   [`remaining`], [`write_frame`] and [`receive_frame`]. A wedged or
    ///   merely slow peer says nothing about the delivery, and the ledger
    ///   already stores a timeout separately from other failures, so it must
    ///   not be reported as a refusal.
    ///
    /// Terminal, because retrying identical bytes reproduces the same answer
    /// and would spend the subscription's retry budget on a delivery that can
    /// never land:
    ///
    /// * `kimi_frame_malformed` -- the frame is not one JSON-RPC 2.0 response
    ///   object for a protocol-1 acknowledgement: unparseable, trailing bytes
    ///   after the object, an unrecognized envelope member, both or neither of
    ///   `result` and `error`, or a `result` that is not an acknowledgement
    ///   this adapter can read. That is a peer speaking a protocol this
    ///   adapter never verified -- a version or configuration defect -- and
    ///   this process sends byte-identical bytes on every attempt, so the
    ///   answer cannot change.
    /// * `kimi_frame_oversized` -- the answer passed the response frame cap
    ///   with no terminator in sight; raised in [`read_one_frame`]. The cap is
    ///   fixed and the peer's answer shape is a property of the peer, so a
    ///   retry reproduces it. Refusing here is also what keeps the read
    ///   bounded: the adapter stops at the cap rather than growing a buffer to
    ///   whatever the peer feels like sending.
    /// * `kimi_identity_mismatch` -- the answer does not name the delivery
    ///   this process sent: a JSON-RPC id other than the one it asked, or an
    ///   acknowledgement whose subscription, event, or timestamp belongs to
    ///   something else. This is the class that would silently corrupt the
    ///   ledger if it were ever accepted as success, so it is refused before
    ///   the payload is even inspected, and it is terminal because a peer that
    ///   answers about the wrong delivery will keep doing so; re-sending would
    ///   only burn attempts while the operator's real problem -- a
    ///   mis-multiplexed or shared peer -- stayed hidden behind a retry loop.
    /// * `kimi_request_rejected` -- the peer answered a well-formed JSON-RPC
    ///   error for this exact id: it understood the frame and refused it,
    ///   typically because the delivery method is not implemented. The frame
    ///   is byte-identical on every attempt, so the refusal is stable, and
    ///   reporting it as a malformed frame would send the operator hunting a
    ///   framing bug instead of a peer version mismatch.
    const fn retryable(self) -> bool {
        match self {
            Self::PeerUnanswered | Self::DeadlineExceeded => true,
            Self::FrameMalformed
            | Self::FrameOversized
            | Self::IdentityMismatch
            | Self::RequestRejected => false,
        }
    }

    const fn exit_code(self) -> i32 {
        match self {
            Self::PeerUnanswered => 10,
            Self::FrameMalformed => 11,
            Self::FrameOversized => 12,
            Self::IdentityMismatch => 13,
            Self::DeadlineExceeded => 14,
            Self::RequestRejected => 15,
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

    let mut kimi = None;
    let mut home = None;
    let mut cwd = None;
    let mut request_timeout_ms = None;

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
            "--kimi" => assign_once(&mut kimi, parse_absolute_path(value, flag)?, flag)?,
            "--home" => assign_once(&mut home, parse_absolute_path(value, flag)?, flag)?,
            "--cwd" => assign_once(&mut cwd, parse_absolute_path(value, flag)?, flag)?,
            "--request-timeout-ms" => {
                assign_once(&mut request_timeout_ms, parse_timeout_ms(value)?, flag)?
            }
            _ => bail!("unknown argument: {flag}"),
        }
        index += 1;
    }

    Ok(Outcome::Args(Args {
        kimi: kimi.ok_or_else(|| anyhow::anyhow!("missing required flag: --kimi"))?,
        home: home.ok_or_else(|| anyhow::anyhow!("missing required flag: --home"))?,
        cwd: cwd.ok_or_else(|| anyhow::anyhow!("missing required flag: --cwd"))?,
        request_timeout_ms: request_timeout_ms
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --request-timeout-ms"))?,
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

fn parse_absolute_path(value: &str, flag: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("{flag} must be an absolute path");
    }
    Ok(path.to_path_buf())
}

fn parse_timeout_ms(value: &str) -> Result<u64> {
    let timeout_ms: u64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--request-timeout-ms must be an integer"))?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("--request-timeout-ms must be in {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

fn identity(metadata: &fs::Metadata) -> (u64, u64, u32, u32) {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.permissions().mode(),
    )
}

fn validate_ancestors(path: &Path, label: &str) -> Result<()> {
    let effective_uid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::metadata(ancestor)?;
        let uid = metadata.uid();
        let mode = metadata.permissions().mode();
        if !metadata.is_dir()
            || (uid != effective_uid && uid != 0)
            || (mode & 0o022 != 0 && mode & 0o1000 == 0)
        {
            bail!("{label} ancestor is not trusted: {}", ancestor.display());
        }
    }
    Ok(())
}

/// Resolve one configured path and refuse it unless it is still trusted.
///
/// This validates the current inode snapshot; it does not claim an atomic
/// open-time guarantee. The adapter spawns immediately after validating, and
/// re-opening the canonical path to compare identities narrows the window
/// between the check and the exec.
fn pin(path: &Path, directory: bool, label: &str) -> Result<PathBuf> {
    let link = fs::symlink_metadata(path)?;
    if link.file_type().is_symlink() {
        bail!("{label} must not be a symlink");
    }
    let path = fs::canonicalize(path)?;
    let metadata = fs::metadata(&path)?;
    if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
        bail!("{label} has the wrong file type");
    }
    let mode = metadata.permissions().mode();
    if (!directory && (mode & 0o111 == 0 || mode & 0o022 != 0)) || (directory && mode & 0o077 != 0)
    {
        bail!("{label} permissions are not trusted");
    }
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        bail!("{label} owner is not trusted");
    }
    validate_ancestors(&path, label)?;
    if identity(&fs::File::open(&path)?.metadata()?) != identity(&metadata) {
        bail!("{label} identity changed");
    }
    Ok(path)
}

/// The peer's working directory must be an empty private directory: this
/// adapter hands the peer one event in one frame, so a peer that reads
/// ambient files from the dispatcher's tree is reaching outside the delivery.
fn validate_empty_cwd(path: &Path) -> Result<()> {
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        bail!("--cwd must be empty");
    }
    Ok(())
}

fn validate_paths(args: &Args) -> Result<Validated> {
    let cwd = pin(&args.cwd, true, "--cwd")?;
    validate_empty_cwd(&cwd)?;
    Ok(Validated {
        kimi: pin(&args.kimi, false, "--kimi")?,
        home: pin(&args.home, true, "--home")?,
        cwd,
    })
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
    if request.target.consumer_id != CONSUMER_ID {
        bail!("adapter target consumer ID must be {CONSUMER_ID}");
    }
    if request.target.action_id != ACTION_ID {
        bail!("adapter target action ID must be {ACTION_ID}");
    }
    Ok(())
}

fn render_response(response: &AdapterResponse) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_RESPONSE_FRAME_BYTES {
        bail!("adapter response exceeds {MAX_RESPONSE_FRAME_BYTES} bytes");
    }
    Ok(bytes)
}

#[derive(Debug, Serialize)]
struct DeliveryFrame<'a> {
    jsonrpc: &'static str,
    id: i64,
    method: &'static str,
    params: &'a AdapterRequest,
}

/// One JSON-RPC request frame, terminated exactly once.
///
/// The delivery on stdin was already validated by `decode_request`, so this
/// only serializes and bounds it. The `\n` appended here is the sole
/// terminator in the frame: compact JSON emits no insignificant whitespace
/// and escapes a U+000A inside a string as two bytes, so the peer's reader
/// sees one message and one end of message.
fn delivery_frame(request: &AdapterRequest) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(&DeliveryFrame {
        jsonrpc: JSONRPC_VERSION,
        id: DELIVERY_REQUEST_ID,
        method: DELIVER_METHOD,
        params: request,
    })?;
    if bytes.len() >= MAX_REQUEST_FRAME_BYTES {
        bail!("delivery frame exceeds {MAX_REQUEST_FRAME_BYTES} bytes");
    }
    bytes.push(b'\n');
    Ok(bytes)
}

/// One JSON-RPC response envelope. JSON-RPC 2.0 fixes the members of a
/// response object exactly, so an unrecognized member means the peer speaks
/// something this adapter never verified, and guessing at it is what corrupts
/// a ledger. `adapter_protocol` denies unknown fields for the same reason.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerFrame {
    jsonrpc: String,
    id: Value,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<PeerError>,
}

/// A JSON-RPC error object. Unknown members are *not* denied here: the
/// optional `data` member carries peer-specific diagnostics this adapter does
/// not interpret and must not refuse a legible rejection over. `code` and
/// `message` are the parts the operator's report quotes, and the response
/// frame cap bounds their size.
#[derive(Debug, Deserialize)]
struct PeerError {
    code: i64,
    message: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Complete(Vec<u8>),
    Oversized,
    Truncated(usize),
    Closed,
}

/// Read one newline-terminated frame, or say precisely why there is not one.
///
/// The cap is enforced before the buffer grows past it, so a peer that never
/// terminates its answer costs a bounded read rather than unbounded memory:
/// this never reads to end of stream. Bytes after the first terminator stay in
/// the pipe unread -- they are a message this process never asked for.
fn read_one_frame<R: io::Read>(mut reader: R) -> io::Result<Frame> {
    let mut frame = Vec::new();
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(if frame.is_empty() {
                Frame::Closed
            } else {
                Frame::Truncated(frame.len())
            });
        }
        let chunk = &chunk[..read];
        if let Some(end) = chunk.iter().position(|byte| *byte == b'\n') {
            if frame.len() + end > MAX_RESPONSE_FRAME_BYTES {
                return Ok(Frame::Oversized);
            }
            frame.extend_from_slice(&chunk[..end]);
            return Ok(Frame::Complete(frame));
        }
        if frame.len() + chunk.len() > MAX_RESPONSE_FRAME_BYTES {
            return Ok(Frame::Oversized);
        }
        frame.extend_from_slice(chunk);
    }
}

fn spawn_frame_reader(stdout: ChildStdout) -> mpsc::Receiver<io::Result<Frame>> {
    let (sender, receiver) = mpsc::channel();
    // The reader is detached on purpose: when the deadline fires, the peer is
    // still holding the pipe, and this process must report the timeout instead
    // of joining a thread that is blocked in `read`. Dropping the peer closes
    // the pipe and the thread ends on its own.
    thread::spawn(move || {
        let _ = sender.send(read_one_frame(stdout));
    });
    receiver
}

/// Time left before the deadline, or the breach itself as a distinct class.
fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(failed(
            FailureClass::DeadlineExceeded,
            "the request deadline elapsed",
        ));
    }
    Ok(left)
}

/// Write the one request frame, and hand the peer's stdin back.
///
/// A delivery runs up to a megabyte, which is larger than a pipe buffer, so a
/// peer that never reads would park this write forever. `ChildStdin` has no
/// write timeout, so the write happens on its own thread and the deadline is
/// enforced here. Stdin is deliberately kept open until the answer has been
/// read: a peer that treats end of stdin as shutdown must not be told to shut
/// down while its answer is still owed.
fn write_frame(mut stdin: ChildStdin, frame: Vec<u8>, deadline: Instant) -> Result<ChildStdin> {
    let left = remaining(deadline)?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = stdin.write_all(&frame).and_then(|()| stdin.flush());
        let _ = sender.send((result, stdin));
    });
    match receiver.recv_timeout(left) {
        Ok((Ok(()), stdin)) => Ok(stdin),
        Ok((Err(error), _)) => Err(failed(
            FailureClass::PeerUnanswered,
            format!("sending the delivery frame to the peer: {error}"),
        )),
        Err(RecvTimeoutError::Timeout) => Err(failed(
            FailureClass::DeadlineExceeded,
            "the peer did not accept the delivery frame before the deadline",
        )),
        Err(RecvTimeoutError::Disconnected) => Err(failed(
            FailureClass::PeerUnanswered,
            "the delivery writer ended without a result",
        )),
    }
}

fn receive_frame(frames: &mpsc::Receiver<io::Result<Frame>>, deadline: Instant) -> Result<Vec<u8>> {
    let left = remaining(deadline)?;
    match frames.recv_timeout(left) {
        Ok(Ok(Frame::Complete(frame))) => Ok(frame),
        Ok(Ok(Frame::Oversized)) => Err(failed(
            FailureClass::FrameOversized,
            format!(
                "the peer's answer passed {MAX_RESPONSE_FRAME_BYTES} bytes with no frame terminator, so it was refused unread"
            ),
        )),
        Ok(Ok(Frame::Truncated(read))) => Err(failed(
            FailureClass::PeerUnanswered,
            format!(
                "the peer closed its stdout after {read} bytes of an unterminated frame, which is not an answer"
            ),
        )),
        Ok(Ok(Frame::Closed)) => Err(failed(
            FailureClass::PeerUnanswered,
            "the peer closed its stdout without answering the delivery",
        )),
        Ok(Err(error)) => Err(failed(
            FailureClass::PeerUnanswered,
            format!("reading the peer's answer: {error}"),
        )),
        Err(RecvTimeoutError::Timeout) => Err(failed(
            FailureClass::DeadlineExceeded,
            "the peer did not answer the delivery before the deadline",
        )),
        Err(RecvTimeoutError::Disconnected) => Err(failed(
            FailureClass::PeerUnanswered,
            "the peer's stdout reader ended without an answer",
        )),
    }
}

/// Accept one frame, or refuse it with the class the operator acts on.
fn accept_frame(frame: &[u8], request: &AdapterRequest) -> Result<AdapterResponse> {
    let mut deserializer = serde_json::Deserializer::from_slice(frame);
    let peer = PeerFrame::deserialize(&mut deserializer).map_err(|error| {
        failed(
            FailureClass::FrameMalformed,
            format!("the peer's frame is not one JSON-RPC response object: {error}"),
        )
    })?;
    deserializer.end().map_err(|error| {
        failed(
            FailureClass::FrameMalformed,
            format!("the peer's frame carries trailing bytes after its response object: {error}"),
        )
    })?;
    if peer.jsonrpc != JSONRPC_VERSION {
        return Err(failed(
            FailureClass::FrameMalformed,
            format!(
                "the peer answered jsonrpc {}, not {JSONRPC_VERSION}",
                peer.jsonrpc
            ),
        ));
    }
    // Identity before content: a frame that does not name the request this
    // process sent is refused without its payload ever being inspected.
    if peer.id != Value::from(DELIVERY_REQUEST_ID) {
        return Err(failed(
            FailureClass::IdentityMismatch,
            format!(
                "the peer answered request id {}, not the id {DELIVERY_REQUEST_ID} this delivery asked",
                peer.id
            ),
        ));
    }
    match (peer.result, peer.error) {
        (Some(_), Some(_)) => Err(failed(
            FailureClass::FrameMalformed,
            "the peer's frame carries both a result and an error",
        )),
        (None, None) => Err(failed(
            FailureClass::FrameMalformed,
            "the peer's frame carries neither a result nor an error",
        )),
        (None, Some(error)) => Err(failed(
            FailureClass::RequestRejected,
            format!(
                "the peer refused the delivery with JSON-RPC error {}: {}",
                error.code, error.message
            ),
        )),
        (Some(result), None) => accept_acknowledgement(&result, request),
    }
}

fn accept_acknowledgement(result: &Value, request: &AdapterRequest) -> Result<AdapterResponse> {
    let bytes = serde_json::to_vec(result).map_err(|error| {
        failed(
            FailureClass::FrameMalformed,
            format!("the peer's result cannot be read back: {error}"),
        )
    })?;
    // Parse and version first, so a result that is not an acknowledgement at
    // all -- or is one this adapter cannot read -- reports a malformed frame
    // rather than an identity mismatch.
    let parsed: AdapterResponse = serde_json::from_slice(&bytes).map_err(|error| {
        failed(
            FailureClass::FrameMalformed,
            format!("the peer's result is not an adapter acknowledgement: {error}"),
        )
    })?;
    if parsed.protocol_version != 1 {
        return Err(failed(
            FailureClass::FrameMalformed,
            format!(
                "the peer acknowledged protocol version {}, which this adapter cannot read",
                parsed.protocol_version
            ),
        ));
    }
    // `decode_response` stays the crate's single acknowledgement gate. The
    // parse and the protocol version already passed on these exact bytes, and
    // the frame cap is far below the protocol's own message limit, so every
    // refusal it can still raise is an identity refusal: subscription, event,
    // or timestamp.
    decode_response(&bytes, request).map_err(|error| {
        failed(
            FailureClass::IdentityMismatch,
            format!("the peer acknowledged a delivery this process never sent: {error:#}"),
        )
    })
}

fn peer_command(validated: &Validated) -> Command {
    let mut command = Command::new(&validated.kimi);
    command
        .current_dir(&validated.cwd)
        .env_clear()
        .env("HOME", &validated.home)
        .env("PATH", CHILD_PATH)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // The peer's diagnostics are discarded rather than drained: this
        // adapter's own stderr is the classification channel the operator
        // reads, a peer banner interleaved into it would corrupt that one
        // clue, and a pipe nobody drains can wedge the peer mid-answer and
        // present itself as a timeout. The peer keeps its own logs.
        .stderr(Stdio::null())
        // A private process group so cleanup can reach descendants the peer
        // spawned without ever signalling this adapter's own group.
        .process_group(0);
    command
}

/// The spawned peer, and the guarantee that it is reaped exactly once.
struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    acknowledged: bool,
}

impl Drop for Peer {
    fn drop(&mut self) {
        // Closing stdin is the peer's shutdown signal: the exchange is over.
        self.stdin = None;
        if self.acknowledged {
            // The acknowledgement is the contract, and the turn it accepted
            // runs afterwards, so a peer that has answered is given a bounded
            // window to leave on its own before the group is terminated. What
            // it does after answering never turns a proven acknowledgement
            // into a failure -- and never makes this process hang either.
            let window = Instant::now() + CLEANUP_WINDOW;
            loop {
                match self.child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if Instant::now() < window => thread::sleep(CLEANUP_POLL),
                    _ => break,
                }
            }
        }
        terminate_and_reap(&mut self.child);
    }
}

fn deliver(
    validated: &Validated,
    request: &AdapterRequest,
    deadline: Instant,
) -> Result<AdapterResponse> {
    let frame = delivery_frame(request)?;
    let mut child = peer_command(validated)
        .spawn()
        .with_context(|| format!("spawn the ACP peer {}", validated.kimi.display()))?;
    let pipes = child.stdin.take().zip(child.stdout.take());
    let mut peer = Peer {
        child,
        stdin: None,
        acknowledged: false,
    };
    let Some((stdin, stdout)) = pipes else {
        bail!("the ACP peer was spawned without both stdio pipes");
    };
    let frames = spawn_frame_reader(stdout);
    peer.stdin = Some(write_frame(stdin, frame, deadline)?);
    let response = accept_frame(&receive_frame(&frames, deadline)?, request)?;
    peer.acknowledged = true;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;

    const EVENT_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CREATED_AT: i64 = 1_720_000_000;

    /// A reader that hands over one byte per call and remembers how far it
    /// was consumed, so a test can prove where the adapter stopped reading.
    struct ByteReader {
        bytes: Vec<u8>,
        position: usize,
    }

    impl ByteReader {
        fn new(bytes: impl Into<Vec<u8>>) -> Self {
            Self {
                bytes: bytes.into(),
                position: 0,
            }
        }
    }

    impl io::Read for ByteReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() || self.position == self.bytes.len() {
                return Ok(0);
            }
            buffer[0] = self.bytes[self.position];
            self.position += 1;
            Ok(1)
        }
    }

    fn request() -> AdapterRequest {
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".to_owned(),
                event_id: EVENT_ID.to_owned(),
                attempt: 2,
                created_at: CREATED_AT,
            },
            target: AdapterTarget {
                consumer_id: CONSUMER_ID.to_owned(),
                action_id: ACTION_ID.to_owned(),
            },
            event: json!({
                "eventID": EVENT_ID,
                "eventHash": EVENT_ID,
                "timestamp": CREATED_AT,
            }),
        }
    }

    fn parse(tokens: &[&str]) -> Result<Outcome> {
        parse_outcome(
            std::iter::once("kanban-kimi-acp-adapter")
                .chain(tokens.iter().copied())
                .map(OsString::from),
        )
    }

    fn acknowledgement(subscription: &str, event: &str, created_at: i64) -> String {
        format!(
            "{{\"protocolVersion\":1,\"subscriptionID\":\"{subscription}\",\"eventID\":\"{event}\",\"createdAt\":{created_at},\"replay\":true}}"
        )
    }

    fn frame(id: &str, payload: &str) -> String {
        format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},{payload}}}")
    }

    fn class(error: &anyhow::Error) -> FailureClass {
        *error
            .downcast_ref::<FailureClass>()
            .unwrap_or_else(|| panic!("unclassified failure: {error:#}"))
    }

    fn refuse(frame: &str) -> anyhow::Error {
        accept_frame(frame.as_bytes(), &request())
            .expect_err("the peer frame should have been refused")
    }

    #[test]
    fn a_complete_argument_list_parses_every_flag() {
        let parsed = parse(&[
            "--request-timeout-ms",
            "5000",
            "--cwd",
            "/tmp/cwd",
            "--kimi",
            "/usr/local/bin/kimi-acp",
            "--home",
            "/tmp/home",
        ])
        .unwrap();
        assert_eq!(
            parsed,
            Outcome::Args(Args {
                kimi: PathBuf::from("/usr/local/bin/kimi-acp"),
                home: PathBuf::from("/tmp/home"),
                cwd: PathBuf::from("/tmp/cwd"),
                request_timeout_ms: 5_000,
            })
        );
        assert_eq!(parse(&["--help"]).unwrap(), Outcome::Help);
        assert_eq!(parse(&["--version"]).unwrap(), Outcome::Version);
    }

    #[test]
    fn argument_refusals_name_the_offending_flag() {
        let complete: [&str; 8] = [
            "--kimi",
            "/bin/peer",
            "--home",
            "/tmp/home",
            "--cwd",
            "/tmp/cwd",
            "--request-timeout-ms",
            "5000",
        ];
        for (label, tokens, expected) in [
            (
                "a flag swallowed as a value",
                vec!["--kimi", "--home", "/tmp/home"],
                "missing value for --kimi",
            ),
            (
                "a value that is missing entirely",
                vec!["--kimi"],
                "missing value for --kimi",
            ),
            (
                "a repeated flag",
                vec!["--kimi", "/bin/a", "--kimi", "/bin/b"],
                "argument repeated: --kimi",
            ),
            (
                "an unknown flag",
                vec!["--peer", "/bin/peer"],
                "unknown argument: --peer",
            ),
            (
                "a positional argument",
                vec!["/bin/peer"],
                "positional argument is not allowed: /bin/peer",
            ),
            (
                "a relative executable",
                vec!["--kimi", "peer"],
                "--kimi must be an absolute path",
            ),
            (
                "a non-integer timeout",
                vec!["--request-timeout-ms", "5s"],
                "--request-timeout-ms must be an integer",
            ),
            (
                "a timeout under the floor",
                vec!["--request-timeout-ms", "999"],
                "--request-timeout-ms must be in 1000..=300000",
            ),
            (
                "a timeout over the ceiling",
                vec!["--request-timeout-ms", "300001"],
                "--request-timeout-ms must be in 1000..=300000",
            ),
            (
                "a missing required flag",
                vec!["--kimi", "/bin/peer"],
                "missing required flag: --home",
            ),
        ] {
            let error = parse(&tokens).expect_err(label);
            assert!(
                format!("{error:#}").contains(expected),
                "{label}: {error:#}"
            );
        }
        for boundary in ["1000", "300000"] {
            let mut tokens = complete.to_vec();
            tokens[7] = boundary;
            parse(&tokens).unwrap_or_else(|error| panic!("{boundary}ms refused: {error:#}"));
        }
    }

    #[test]
    fn the_delivery_frame_is_one_terminated_jsonrpc_line() {
        let request = request();
        let frame = delivery_frame(&request).unwrap();
        assert_eq!(frame.last(), Some(&b'\n'), "the frame is not terminated");
        let body = &frame[..frame.len() - 1];
        assert!(
            !body.contains(&b'\n'),
            "the frame carries an interior terminator"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(body).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "_kanban/deliverEvent",
                "params": serde_json::to_value(&request).unwrap(),
            })
        );
    }

    #[test]
    fn a_frame_stops_at_its_first_terminator_and_leaves_the_rest_unread() {
        let mut reader = ByteReader::new("{\"first\":1}\n{\"second\":2}\n");
        assert_eq!(
            read_one_frame(&mut reader).unwrap(),
            Frame::Complete(b"{\"first\":1}".to_vec())
        );
        assert_eq!(
            reader.position, 12,
            "the adapter read past the terminator of the answer it asked for"
        );
    }

    #[test]
    fn an_unterminated_answer_is_truncated_rather_than_complete() {
        assert_eq!(
            read_one_frame(&mut ByteReader::new("{\"jsonrpc\":\"2.0\"")).unwrap(),
            Frame::Truncated(16)
        );
        assert_eq!(
            read_one_frame(&mut ByteReader::new("")).unwrap(),
            Frame::Closed
        );
    }

    #[test]
    fn an_answer_is_refused_at_the_frame_cap_instead_of_growing_without_bound() {
        let mut at_cap = vec![b'x'; MAX_RESPONSE_FRAME_BYTES];
        at_cap.push(b'\n');
        assert_eq!(
            read_one_frame(at_cap.as_slice()).unwrap(),
            Frame::Complete(vec![b'x'; MAX_RESPONSE_FRAME_BYTES]),
            "an answer exactly at the cap was refused"
        );

        let mut over_cap = ByteReader::new(vec![b'x'; MAX_RESPONSE_FRAME_BYTES * 4]);
        assert_eq!(read_one_frame(&mut over_cap).unwrap(), Frame::Oversized);
        assert!(
            over_cap.position <= MAX_RESPONSE_FRAME_BYTES + 1,
            "the adapter read {} bytes past its own cap",
            over_cap.position - MAX_RESPONSE_FRAME_BYTES
        );

        let mut terminated_over_cap = vec![b'x'; MAX_RESPONSE_FRAME_BYTES + 1];
        terminated_over_cap.push(b'\n');
        assert_eq!(
            read_one_frame(terminated_over_cap.as_slice()).unwrap(),
            Frame::Oversized,
            "a terminated answer one byte over the cap was accepted"
        );
    }

    #[test]
    fn a_matching_acknowledgement_is_accepted() {
        let frame = frame(
            "1",
            &format!(
                "\"result\":{}",
                acknowledgement("sub-test", EVENT_ID, CREATED_AT)
            ),
        );
        let response = accept_frame(frame.as_bytes(), &request()).unwrap();
        assert_eq!(
            response,
            AdapterResponse {
                protocol_version: 1,
                subscription_id: "sub-test".to_owned(),
                event_id: EVENT_ID.to_owned(),
                created_at: CREATED_AT,
                replay: true,
            }
        );
    }

    #[test]
    fn an_answer_that_does_not_name_this_delivery_is_never_accepted() {
        for (label, id, subscription, event, created_at) in [
            (
                "an unasked request id",
                "2",
                "sub-test",
                EVENT_ID,
                CREATED_AT,
            ),
            (
                "a null request id",
                "null",
                "sub-test",
                EVENT_ID,
                CREATED_AT,
            ),
            (
                "a fractional request id",
                "1.0",
                "sub-test",
                EVENT_ID,
                CREATED_AT,
            ),
            (
                "another subscription",
                "1",
                "sub-other",
                EVENT_ID,
                CREATED_AT,
            ),
            (
                "another event",
                "1",
                "sub-test",
                &"0".repeat(64),
                CREATED_AT,
            ),
            (
                "another delivery timestamp",
                "1",
                "sub-test",
                EVENT_ID,
                CREATED_AT + 1,
            ),
        ] {
            let frame = frame(
                id,
                &format!(
                    "\"result\":{}",
                    acknowledgement(subscription, event, created_at)
                ),
            );
            let error = refuse(&frame);
            assert_eq!(
                class(&error),
                FailureClass::IdentityMismatch,
                "{label} was not refused as an identity mismatch: {error:#}"
            );
            assert!(!class(&error).retryable(), "{label} was made retryable");
        }
    }

    #[test]
    fn a_frame_this_adapter_cannot_read_is_malformed_rather_than_a_mismatch() {
        let acknowledged = acknowledgement("sub-test", EVENT_ID, CREATED_AT);
        for (label, frame) in [
            ("an unparseable frame", "{".to_owned()),
            (
                "trailing bytes after the response object",
                format!("{} {{}}", frame("1", &format!("\"result\":{acknowledged}"))),
            ),
            (
                "another JSON-RPC version",
                format!("{{\"jsonrpc\":\"1.0\",\"id\":1,\"result\":{acknowledged}}}"),
            ),
            (
                "an unrecognized envelope member",
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{acknowledged},\"_meta\":{{}}}}"
                ),
            ),
            (
                "both a result and an error",
                frame(
                    "1",
                    &format!(
                        "\"result\":{acknowledged},\"error\":{{\"code\":-1,\"message\":\"no\"}}"
                    ),
                ),
            ),
            (
                "neither a result nor an error",
                frame("1", "\"result\":null"),
            ),
            (
                "a result that is not an acknowledgement",
                frame("1", "\"result\":{\"stopReason\":\"end_turn\"}"),
            ),
            (
                "an acknowledgement protocol this adapter cannot read",
                frame(
                    "1",
                    &format!(
                        "\"result\":{}",
                        acknowledged.replacen("\"protocolVersion\":1", "\"protocolVersion\":2", 1)
                    ),
                ),
            ),
        ] {
            let error = refuse(&frame);
            assert_eq!(
                class(&error),
                FailureClass::FrameMalformed,
                "{label} was not refused as a malformed frame: {error:#}"
            );
        }
    }

    #[test]
    fn a_json_rpc_error_is_a_refusal_carrying_the_peer_s_own_code() {
        let error = refuse(&frame(
            "1",
            "\"error\":{\"code\":-32601,\"message\":\"Method not found\",\"data\":{\"method\":\"x\"}}",
        ));
        assert_eq!(class(&error), FailureClass::RequestRejected);
        assert!(
            format!("{error:#}").contains("-32601: Method not found"),
            "{error:#}"
        );
    }

    #[test]
    fn every_failure_class_reports_a_distinct_code_and_exit_status() {
        let classes = [
            FailureClass::PeerUnanswered,
            FailureClass::FrameMalformed,
            FailureClass::FrameOversized,
            FailureClass::IdentityMismatch,
            FailureClass::DeadlineExceeded,
            FailureClass::RequestRejected,
        ];
        let mut codes: Vec<&str> = classes.iter().map(|class| class.code()).collect();
        let mut statuses: Vec<i32> = classes.iter().map(|class| class.exit_code()).collect();
        codes.sort_unstable();
        statuses.sort_unstable();
        let unique_codes = codes.len();
        let unique_statuses = statuses.len();
        codes.dedup();
        statuses.dedup();
        assert_eq!(codes.len(), unique_codes, "two classes share one code");
        assert_eq!(
            statuses.len(),
            unique_statuses,
            "two classes share one exit status"
        );
        assert!(
            statuses.iter().all(|status| *status > 1),
            "a classified failure exits with the unclassified status"
        );
        assert_eq!(
            format!("{}", FailureClass::DeadlineExceeded),
            "kimi_deadline_exceeded (retryable)"
        );
        assert_eq!(
            format!("{}", FailureClass::IdentityMismatch),
            "kimi_identity_mismatch (terminal)"
        );
    }

    #[test]
    fn an_untrusted_working_directory_is_refused() {
        let root = std::env::temp_dir().join(format!(
            "kanban-kimi-acp-unit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        fs::set_permissions(&cwd, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_empty_cwd(&cwd).is_ok());

        fs::write(cwd.join("ambient.txt"), b"left behind").unwrap();
        let error = validate_empty_cwd(&cwd).expect_err("a dirty cwd should be refused");
        assert!(
            format!("{error:#}").contains("--cwd must be empty"),
            "{error:#}"
        );

        let link = root.join("link");
        std::os::unix::fs::symlink(&cwd, &link).unwrap();
        let error = pin(&link, true, "--cwd").expect_err("a symlinked cwd should be refused");
        assert!(
            format!("{error:#}").contains("--cwd must not be a symlink"),
            "{error:#}"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
