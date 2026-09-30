use crate::adapter_process::{child_exited_unreaped, terminate_and_reap};
use crate::adapter_protocol::{AdapterRequest, AdapterResponse, decode_request, encode_request};
use anyhow::{Result, bail};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "kanban-zcode-notify-adapter --sink ABSOLUTE_PATH --notify-timeout-ms N";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_SINK_PATH_BYTES: usize = 512;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
// The sink inherits nothing: the dispatcher may have handed this adapter a
// subscription secret in its own environment, and a notification sink has no
// business seeing it. A minimal PATH is kept so a `/bin/sh` sink can still
// call the ordinary system tools it was written against.
const CHILD_PATH: &str = "/usr/bin:/bin";
// ZCode is wired as a notification consumer, not a turn runner. The action
// vocabulary says so out loud: there is no `enqueue-turn` or
// `start-readonly-turn` here, so a subscription that means to drive a turn
// cannot be pointed at this binary by editing an action ID -- the delivery is
// refused before the sink is started.
const ZCODE_CONSUMER_ID: &str = "zcode.notify";
const POST_NOTIFICATION_ACTION_ID: &str = "post-notification";

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
                "kanban-zcode-notify-adapter {}",
                env!("CARGO_PKG_VERSION")
            )?;
            Ok(())
        }
        Outcome::Args(args) => run(&args),
    }
}

/// Exit status for one classified notification failure.
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

fn run(args: &Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(args.notify_timeout_ms);
    let request = decode_request_from_stdin()?;
    let notified = notify(&args.sink, &request, deadline)?;
    // The acknowledgement the dispatcher decodes is computed from the request
    // that arrived on stdin. There is deliberately no path by which the sink
    // can influence it: `decode_response` is not imported here, and the sink's
    // own output never reaches this process at all (see [`SinkOutput`]).
    let mut stdout = io::stdout();
    stdout.write_all(&serde_json::to_vec(&acknowledge(&request))?)?;
    stdout.flush()?;
    // The operator report is best-effort on purpose: the notification has left
    // and the sink acknowledged it, so a closed stderr must not turn a
    // completed delivery into a retryable failure.
    let _ = writeln!(io::stderr(), "{}", report(&args.sink, notified));
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    sink: PathBuf,
    notify_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

/// The payload an ingress-capable adapter would act on.
///
/// This enum has no variants, so no expression in Rust produces a value of
/// this type: `Option<Ingress>` has exactly one inhabitant, `None`, and that
/// is a fact the compiler enforces rather than a convention a comment asks
/// for. Turning this adapter into an ingress therefore starts here, with a
/// variant, which immediately breaks the empty `match` in [`report`] and
/// stops the crate compiling until someone writes -- in the open, in a diff a
/// reviewer must read -- the code that acts on what a sink said.
#[derive(Debug)]
enum Ingress {}

/// Where a spawned sink's own output goes.
///
/// One variant, and [`SinkOutput::stdio`] is the only place in this module
/// where a `Stdio` for the sink's output streams is built. Handing the sink a
/// pipe back into this process is not a flag flip or an argument: it needs a
/// second variant here and a second arm in that `match`, which is a type
/// change.
///
/// The discard is enforced by the kernel, not by restraint: `Stdio::null()`
/// opens the sink's stdout and stderr on `/dev/null`, so `Child::stdout` and
/// `Child::stderr` are `None` from the moment of spawn. There is no reader in
/// this process to read, and no descriptor a later edit could take one from.
enum SinkOutput {
    Discard,
}

impl SinkOutput {
    fn stdio(self) -> Stdio {
        match self {
            Self::Discard => Stdio::null(),
        }
    }
}

/// Everything one notification attempt hands back.
///
/// A notify-only adapter learns exactly two things: how many bytes of notice
/// it handed over, and that the sink's exit status acknowledged them. It does
/// not learn what the sink thinks, because the sink cannot tell it.
#[derive(Debug)]
struct Notified {
    notice_bytes: usize,
    /// The payload this adapter acted on.
    ///
    /// [`Ingress`] is uninhabited, so this is `None` in every reachable state
    /// and in every state that could ever be written.
    acted_on: Option<Ingress>,
}

/// One classified way a notification attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    SinkUnreachable,
    SinkRefused,
    DeadlineExceeded,
    AcknowledgementMalformed,
}

impl FailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::SinkUnreachable => "zcode_sink_unreachable",
            Self::SinkRefused => "zcode_sink_refused",
            Self::DeadlineExceeded => "zcode_deadline_exceeded",
            Self::AcknowledgementMalformed => "zcode_acknowledgement_malformed",
        }
    }

    /// Whether a later attempt with byte-identical notice bytes can succeed.
    /// See [`classify_exit`] for the reasoning behind each answer.
    const fn retryable(self) -> bool {
        match self {
            Self::SinkUnreachable | Self::DeadlineExceeded => true,
            Self::SinkRefused | Self::AcknowledgementMalformed => false,
        }
    }

    /// Exit statuses are chosen to mean the same thing they mean in the
    /// OpenCode adapter: 10 is "the peer could not be reached", 11 is "the
    /// peer refused this exact delivery", 13 is "the deadline elapsed", 14 is
    /// "the peer's answer cannot be believed". 12 -- OpenCode's transient
    /// mid-response endpoint failure -- has no analogue here, because there
    /// is no response channel that could fail halfway.
    const fn exit_code(self) -> i32 {
        match self {
            Self::SinkUnreachable => 10,
            Self::SinkRefused => 11,
            Self::DeadlineExceeded => 13,
            Self::AcknowledgementMalformed => 14,
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

/// The one-line operator report for a delivered notification.
///
/// The `Some` arm is the structural guard, not the prose. [`Ingress`] has no
/// variants, so that arm's body is an empty `match` -- an expression that
/// compiles only while the type stays uninhabited. It is also why the report
/// can state `acted-on=none` as a measurement rather than a promise: there is
/// no reachable execution in which it prints anything else.
fn report(sink: &Path, notified: Notified) -> String {
    let Notified {
        notice_bytes,
        acted_on,
    } = notified;
    let acted_on = match acted_on {
        None => "none",
        Some(ingress) => match ingress {},
    };
    // `sink-exit=0` is a constant because a non-zero status never reaches
    // this line: it is classified as a refusal or a malformed acknowledgement
    // instead. It is printed anyway so one log line answers the operator's
    // whole question without a cross-reference.
    format!(
        "notified: sink={} notice-bytes={notice_bytes} sink-exit=0 sink-output=discarded reply-bytes-read=0 acted-on={acted_on}",
        sink.display()
    )
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

    let mut sink = None;
    let mut notify_timeout_ms = None;

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

        // The whole accepted surface is these two arms. Every other flag is
        // refused by name, so no spelling of `--allow-ingress`,
        // `--read-reply` or `--capture-response` can be set by an operator,
        // a stale runbook, or a subscription's `args` list: an unknown flag
        // is an error and the sink is never started.
        match flag {
            "--sink" => assign_once(&mut sink, parse_sink(value)?, flag)?,
            "--notify-timeout-ms" => {
                assign_once(&mut notify_timeout_ms, parse_timeout_ms(value)?, flag)?
            }
            _ => bail!("unknown argument: {flag}"),
        }
        index += 1;
    }

    Ok(Outcome::Args(Args {
        sink: sink.ok_or_else(|| anyhow::anyhow!("missing required flag: --sink"))?,
        notify_timeout_ms: notify_timeout_ms
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --notify-timeout-ms"))?,
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

fn parse_timeout_ms(value: &str) -> Result<u64> {
    let timeout_ms: u64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--notify-timeout-ms must be an integer"))?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("--notify-timeout-ms must be in {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

/// Parse the one sink the host configuration pins.
///
/// The sink is an explicit absolute path and is never read from the
/// environment or resolved through `PATH`: the dispatcher clears the child
/// environment before every invocation, and a relative program name would
/// let whatever happens to sit in the working directory receive private
/// ledger events.
///
/// Unlike the Claude print adapter, this one does not pin the sink's inode,
/// ownership and mode before spawning it. That check exists there because
/// that adapter *parses the child's stdout* and acts on it, so who wrote the
/// binary decides what it believes. Here the sink's output is discarded by
/// the kernel and the only thing it can return is one exit status, so
/// swapping the binary buys an attacker no influence over this process --
/// while opening the filesystem to validate it would add a reader this module
/// deliberately does not have.
fn parse_sink(value: &str) -> Result<PathBuf> {
    if value.is_empty() || value.len() > MAX_SINK_PATH_BYTES {
        bail!("--sink must be 1..={MAX_SINK_PATH_BYTES} bytes");
    }
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("--sink must be an absolute path");
    }
    // Checked on the raw text rather than on `Path::components`, which
    // silently normalises an interior `.` away: what the operator wrote is
    // what is judged, so a configured path means one thing to the reader and
    // to `execve`.
    if value
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        bail!("--sink must not contain relative segments");
    }
    Ok(path.to_owned())
}

/// Read the one delivery this process is given.
///
/// `Read` is called through its fully qualified path instead of being
/// imported, so the trait never enters this module's namespace. The single
/// bounded read below is then the only read in the module that can be written
/// without adding an import, which is what makes "this adapter reads nothing
/// but its own stdin" checkable rather than merely stated.
fn decode_request_from_stdin() -> Result<AdapterRequest> {
    let bounded = io::Read::take(io::stdin().lock(), (MAX_STDIN_BYTES + 1) as u64);
    // A delivery is JSON, so it is UTF-8 by construction; a byte soup is
    // refused here with std's own message instead of reaching the decoder.
    let text = io::read_to_string(bounded)?;
    if text.len() > MAX_STDIN_BYTES {
        bail!("adapter request exceeds {MAX_STDIN_BYTES} bytes");
    }
    let request = decode_request(text.as_bytes())?;
    validate_request_target(&request)?;
    Ok(request)
}

fn validate_request_target(request: &AdapterRequest) -> Result<()> {
    if request.target.consumer_id != ZCODE_CONSUMER_ID {
        bail!("adapter target consumer ID must be {ZCODE_CONSUMER_ID}");
    }
    if request.target.action_id != POST_NOTIFICATION_ACTION_ID {
        bail!("adapter target action ID must be {POST_NOTIFICATION_ACTION_ID}");
    }
    Ok(())
}

/// The acknowledgement the dispatcher reads, derived only from the request.
fn acknowledge(request: &AdapterRequest) -> AdapterResponse {
    AdapterResponse {
        protocol_version: 1,
        subscription_id: request.delivery.subscription_id.clone(),
        event_id: request.delivery.event_id.clone(),
        created_at: request.delivery.created_at,
        replay: request.delivery.attempt > 1,
    }
}

/// The exact bytes the sink is notified with.
///
/// This is the canonical delivery document from `adapter_protocol`, validated
/// and re-encoded, with one trailing newline so a line-oriented sink gets a
/// complete notice without waiting for EOF. No second protocol and no second
/// error vocabulary: what the dispatcher wrote is what the sink is told.
fn notice_bytes(request: &AdapterRequest) -> Result<Vec<u8>> {
    let mut bytes = encode_request(request)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Start the sink and hand it the notice, one way.
fn notify(sink: &Path, request: &AdapterRequest, deadline: Instant) -> Result<Notified> {
    let notice = notice_bytes(request)?;
    let notice_len = notice.len();
    let mut child = spawn_sink(sink)?;
    let Some(mut pipe) = child.stdin.take() else {
        let _ = terminate_and_reap(&mut child);
        return Err(failed(
            FailureClass::SinkUnreachable,
            format!(
                "the sink {} was started without a notice pipe",
                sink.display()
            ),
        ));
    };

    // A notice runs up to a megabyte, which is larger than a pipe buffer, so
    // a sink that never reads would park this write forever. The write lives
    // on its own thread: when the deadline below kills the sink, the write
    // fails with a broken pipe and the thread ends, so the attempt is bounded
    // by the deadline rather than by the sink's manners. Dropping the pipe at
    // the end of the closure is what gives the sink its EOF.
    let writer = match thread::Builder::new()
        .name("kanban-zcode-notice".into())
        .spawn(move || -> io::Result<()> {
            pipe.write_all(&notice)?;
            pipe.flush()
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = terminate_and_reap(&mut child);
            bail!("spawning the notification writer thread: {error}");
        }
    };

    let waited = wait_for_exit(&mut child, deadline);
    // Joined on every path: the notice writer is only unblocked by the sink
    // reading, exiting, or being killed above, and all three have happened by
    // now. Leaving it detached would let a live thread outlast the attempt
    // that owns it.
    let written = writer
        .join()
        .map_err(|_| anyhow::anyhow!("the notification writer thread panicked"))?;
    classify_exit(sink, waited?, written, notice_len)
}

fn spawn_sink(sink: &Path) -> Result<Child> {
    Command::new(sink)
        // No arguments: the notice is the delivery document on stdin, and a
        // second configurable channel is a second thing to get wrong.
        .env_clear()
        .env("PATH", CHILD_PATH)
        .stdin(Stdio::piped())
        .stdout(SinkOutput::Discard.stdio())
        .stderr(SinkOutput::Discard.stdio())
        // A private process group so a sink that forks is contained and
        // reaped with its leader instead of outliving this attempt.
        .process_group(0)
        .spawn()
        .map_err(|error| {
            failed(
                FailureClass::SinkUnreachable,
                format!("starting the sink {}: {error}", sink.display()),
            )
        })
}

/// Wait for the sink's exit status, or the deadline, whichever comes first.
fn wait_for_exit(child: &mut Child, deadline: Instant) -> Result<ExitStatus> {
    loop {
        match child_exited_unreaped(child.id()) {
            // The leader is an unreaped zombie holding its PID: terminating
            // the group kills any descendant that is still running before the
            // PID can be recycled, and returns the leader's real status.
            Ok(true) => {
                return terminate_and_reap(child).ok_or_else(|| {
                    failed(
                        FailureClass::AcknowledgementMalformed,
                        "the sink's exit status was lost while reaping it",
                    )
                });
            }
            Ok(false) => {}
            Err(error) => {
                let _ = terminate_and_reap(child);
                return Err(failed(
                    FailureClass::AcknowledgementMalformed,
                    format!("waiting for the sink: {error}"),
                ));
            }
        }
        if Instant::now() >= deadline {
            // The sink may have exited in the gap between the poll and the
            // deadline check, in which case its real status is honoured
            // rather than overwritten by our own kill.
            return match terminate_and_reap(child) {
                Some(status) => Ok(status),
                None => Err(failed(
                    FailureClass::DeadlineExceeded,
                    "the sink did not finish before the notify deadline",
                )),
            };
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Turn the sink's exit status -- the entire acknowledgement channel -- into
/// either a delivered notification or one classified failure.
///
/// The dispatcher records exactly one error code per failed attempt and
/// discards the adapter's stderr, so this classification is the operator's
/// only clue about what to do next. Every class is therefore a distinct code
/// and a distinct process exit status.
///
/// Retryable, because a later attempt with byte-identical notice bytes can
/// succeed:
///
/// * `zcode_sink_unreachable` -- the sink could not be started at all, raised
///   in [`spawn_sink`]. Nothing about the delivery is wrong: the sink is being
///   installed or replaced, or the configured path is not there yet. The
///   operator's next move is to fix the host, not to inspect the event.
/// * `zcode_deadline_exceeded` -- `--notify-timeout-ms` elapsed and the sink
///   was killed, raised in [`wait_for_exit`]. A wedged sink says nothing about
///   the delivery, and the ledger already stores a timeout separately from
///   other failures, so this must not look like a refusal.
///
/// Terminal, because retrying identical bytes reproduces the same answer and
/// would spend the subscription's retry budget on a notice that can never
/// land:
///
/// * `zcode_sink_refused` -- the sink ran and exited non-zero. It understood
///   the notice and rejected it; this adapter sends byte-identical bytes on
///   every attempt, so the answer cannot change.
/// * `zcode_acknowledgement_malformed` -- the status is not an
///   acknowledgement this adapter is willing to believe: either the sink died
///   on a signal, so there is no exit code at all, or it exited zero while
///   closing the notice pipe early, so it claims to have taken a notice it
///   demonstrably did not read. Both mean a broken or mismatched sink rather
///   than a busy one. An externally killed sink -- an OOM reaper, say -- lands
///   here too and is the accepted cost: classing it retryable would let a sink
///   that crashes on every notice consume the whole retry budget while the
///   crash itself stayed invisible, and the distinct code keeps the operator
///   pointed at the sink either way.
fn classify_exit(
    sink: &Path,
    status: ExitStatus,
    written: io::Result<()>,
    notice_bytes: usize,
) -> Result<Notified> {
    match status.code() {
        Some(0) => {}
        Some(code) => {
            return Err(failed(
                FailureClass::SinkRefused,
                format!(
                    "the sink {} refused the notification and exited {code}",
                    sink.display()
                ),
            ));
        }
        None => {
            return Err(failed(
                FailureClass::AcknowledgementMalformed,
                format!(
                    "the sink {} was terminated before it could acknowledge the notification: {status}",
                    sink.display()
                ),
            ));
        }
    }
    if let Err(error) = written {
        return Err(failed(
            FailureClass::AcknowledgementMalformed,
            format!(
                "the sink {} exited 0 without taking all {notice_bytes} notice bytes: {error}",
                sink.display()
            ),
        ));
    }
    Ok(Notified {
        notice_bytes,
        // Nothing else can be written here. `Ingress` is uninhabited, so
        // `None` is not a default that a later edit can quietly change -- it
        // is the only value this field has.
        acted_on: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt as _;

    fn request() -> AdapterRequest {
        let event_id = "b".repeat(64);
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".to_owned(),
                event_id: event_id.clone(),
                attempt: 1,
                created_at: 1_720_000_000,
            },
            target: AdapterTarget {
                consumer_id: ZCODE_CONSUMER_ID.to_owned(),
                action_id: POST_NOTIFICATION_ACTION_ID.to_owned(),
            },
            event: json!({
                "eventID": event_id,
                "eventHash": event_id,
                "timestamp": 1_720_000_000_i64,
            }),
        }
    }

    #[test]
    fn every_failure_class_reports_a_distinct_code_and_exit_status() {
        let classes = [
            FailureClass::SinkUnreachable,
            FailureClass::SinkRefused,
            FailureClass::DeadlineExceeded,
            FailureClass::AcknowledgementMalformed,
        ];
        let mut codes: Vec<&str> = classes.iter().map(|class| class.code()).collect();
        let mut statuses: Vec<i32> = classes.iter().map(|class| class.exit_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        statuses.sort_unstable();
        statuses.dedup();
        assert_eq!(codes.len(), classes.len(), "duplicate failure code");
        assert_eq!(statuses.len(), classes.len(), "duplicate exit status");
        assert!(
            statuses.iter().all(|status| *status > 1),
            "a class reused the unclassified exit status"
        );
        assert_eq!(FailureClass::SinkUnreachable.exit_code(), 10);
        assert_eq!(FailureClass::SinkRefused.exit_code(), 11);
        assert_eq!(FailureClass::DeadlineExceeded.exit_code(), 13);
        assert_eq!(FailureClass::AcknowledgementMalformed.exit_code(), 14);
    }

    #[test]
    fn retryable_classes_are_exactly_the_ones_a_resend_can_fix() {
        assert!(FailureClass::SinkUnreachable.retryable());
        assert!(FailureClass::DeadlineExceeded.retryable());
        assert!(!FailureClass::SinkRefused.retryable());
        assert!(!FailureClass::AcknowledgementMalformed.retryable());
        assert_eq!(
            FailureClass::SinkUnreachable.to_string(),
            "zcode_sink_unreachable (retryable)"
        );
        assert_eq!(
            FailureClass::SinkRefused.to_string(),
            "zcode_sink_refused (terminal)"
        );
        assert_eq!(
            FailureClass::DeadlineExceeded.to_string(),
            "zcode_deadline_exceeded (retryable)"
        );
        assert_eq!(
            FailureClass::AcknowledgementMalformed.to_string(),
            "zcode_acknowledgement_malformed (terminal)"
        );
    }

    #[test]
    fn classified_failures_carry_their_exit_status_through_anyhow() {
        for class in [
            FailureClass::SinkUnreachable,
            FailureClass::SinkRefused,
            FailureClass::DeadlineExceeded,
            FailureClass::AcknowledgementMalformed,
        ] {
            let error = failed(class, "detail");
            assert_eq!(exit_code(&error), class.exit_code());
            assert!(
                format!("{error:#}").starts_with(class.code()),
                "{error:#} does not lead with {}",
                class.code()
            );
        }
        assert_eq!(exit_code(&anyhow::anyhow!("unclassified")), 1);
    }

    #[test]
    fn an_exit_status_is_the_whole_acknowledgement_channel() {
        let sink = Path::new("/sink");
        let notified = classify_exit(sink, ExitStatus::from_raw(0), Ok(()), 42).unwrap();
        assert_eq!(notified.notice_bytes, 42);
        assert!(notified.acted_on.is_none());

        let refused = classify_exit(sink, ExitStatus::from_raw(7 << 8), Ok(()), 42).unwrap_err();
        assert_eq!(exit_code(&refused), FailureClass::SinkRefused.exit_code());
        assert!(format!("{refused:#}").contains("exited 7"), "{refused:#}");

        // Raw wait status with no exit code: killed by SIGKILL.
        let signalled = classify_exit(sink, ExitStatus::from_raw(9), Ok(()), 42).unwrap_err();
        assert_eq!(
            exit_code(&signalled),
            FailureClass::AcknowledgementMalformed.exit_code()
        );

        let short = classify_exit(
            sink,
            ExitStatus::from_raw(0),
            Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            42,
        )
        .unwrap_err();
        assert_eq!(
            exit_code(&short),
            FailureClass::AcknowledgementMalformed.exit_code()
        );
        assert!(
            format!("{short:#}").contains("without taking all 42 notice bytes"),
            "{short:#}"
        );

        // A non-zero status outranks a failed write: the sink said no, which
        // is a clearer answer than the broken pipe that non-zero exit caused.
        let both = classify_exit(
            sink,
            ExitStatus::from_raw(3 << 8),
            Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            42,
        )
        .unwrap_err();
        assert_eq!(exit_code(&both), FailureClass::SinkRefused.exit_code());
    }

    #[test]
    fn the_report_states_that_no_reply_was_read() {
        let line = report(
            Path::new("/opt/zcode/notify"),
            Notified {
                notice_bytes: 321,
                acted_on: None,
            },
        );
        assert_eq!(
            line,
            "notified: sink=/opt/zcode/notify notice-bytes=321 sink-exit=0 sink-output=discarded reply-bytes-read=0 acted-on=none"
        );
    }

    #[test]
    fn only_the_zcode_notify_consumer_and_action_are_delivered() {
        assert!(validate_request_target(&request()).is_ok());
        let mut wrong = request();
        wrong.target.consumer_id = "opencode.server".to_owned();
        assert!(
            validate_request_target(&wrong)
                .unwrap_err()
                .to_string()
                .contains("consumer ID must be zcode.notify")
        );
        let mut wrong = request();
        wrong.target.action_id = "enqueue-turn".to_owned();
        assert!(
            validate_request_target(&wrong)
                .unwrap_err()
                .to_string()
                .contains("action ID must be post-notification")
        );
    }

    #[test]
    fn the_notice_is_the_canonical_delivery_document_and_nothing_else() {
        let request = request();
        let notice = notice_bytes(&request).unwrap();
        assert_eq!(notice.last(), Some(&b'\n'));
        assert_eq!(
            &notice[..notice.len() - 1],
            encode_request(&request).unwrap()
        );
        assert_eq!(
            decode_request(&notice[..notice.len() - 1]).unwrap(),
            request
        );
    }

    #[test]
    fn the_acknowledgement_is_derived_from_the_request() {
        let request = request();
        let response = acknowledge(&request);
        assert_eq!(response.protocol_version, 1);
        assert_eq!(response.subscription_id, request.delivery.subscription_id);
        assert_eq!(response.event_id, request.delivery.event_id);
        assert_eq!(response.created_at, request.delivery.created_at);
        assert!(!response.replay, "a first attempt is not a replay");

        let mut retried = request.clone();
        retried.delivery.attempt = 2;
        assert!(
            acknowledge(&retried).replay,
            "a later attempt must acknowledge as a replay"
        );
    }

    #[test]
    fn only_an_absolute_sink_path_without_relative_segments_is_accepted() {
        assert_eq!(
            parse_sink("/opt/zcode/notify").unwrap(),
            PathBuf::from("/opt/zcode/notify")
        );
        for (value, expected) in [
            ("zcode-notify", "absolute path"),
            ("./notify", "absolute path"),
            ("/opt/../etc/notify", "relative segments"),
            ("/opt/./notify", "relative segments"),
            ("", "1..=512 bytes"),
        ] {
            let error = parse_sink(value).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{value} reported {error}, expected {expected}"
            );
        }
        let long = format!("/{}", "a".repeat(MAX_SINK_PATH_BYTES));
        assert!(
            parse_sink(&long)
                .unwrap_err()
                .to_string()
                .contains("1..=512 bytes")
        );
    }

    #[test]
    fn arguments_are_exact_and_unrepeatable() {
        assert_eq!(
            parse_outcome(vec!["bin".into(), "--help".into()]).unwrap(),
            Outcome::Help
        );
        assert_eq!(
            parse_outcome(vec!["bin".into(), "--version".into()]).unwrap(),
            Outcome::Version
        );
        assert_eq!(
            parse_outcome(vec![
                "bin".into(),
                "--sink".into(),
                "/opt/zcode/notify".into(),
                "--notify-timeout-ms".into(),
                "5000".into(),
            ])
            .unwrap(),
            Outcome::Args(Args {
                sink: PathBuf::from("/opt/zcode/notify"),
                notify_timeout_ms: 5_000,
            })
        );

        for (tokens, expected) in [
            (vec!["bin".into(), "extra".into()], "positional"),
            (vec!["bin".into(), "--sink".into()], "missing value"),
            (
                vec!["bin".into(), "--sink".into(), "--notify-timeout-ms".into()],
                "missing value",
            ),
            (
                vec![
                    "bin".into(),
                    "--sink".into(),
                    "/a".into(),
                    "--sink".into(),
                    "/b".into(),
                ],
                "argument repeated",
            ),
            (
                vec!["bin".into(), "--notify-timeout-ms".into(), "5000".into()],
                "missing required flag: --sink",
            ),
            (
                vec!["bin".into(), "--sink".into(), "/a".into()],
                "missing required flag: --notify-timeout-ms",
            ),
            (
                vec![
                    "bin".into(),
                    "--sink".into(),
                    "/a".into(),
                    "--notify-timeout-ms".into(),
                    "999".into(),
                ],
                "must be in 1000..=300000",
            ),
            (
                vec![
                    "bin".into(),
                    "--sink".into(),
                    "/a".into(),
                    "--notify-timeout-ms".into(),
                    "300001".into(),
                ],
                "must be in 1000..=300000",
            ),
            (
                vec![
                    "bin".into(),
                    "--sink".into(),
                    "/a".into(),
                    "--notify-timeout-ms".into(),
                    "abc".into(),
                ],
                "must be an integer",
            ),
        ] {
            let error = parse_outcome(tokens).unwrap_err().to_string();
            assert!(error.contains(expected), "{error} lacks {expected}");
        }
        let non_utf8: OsString = std::os::unix::ffi::OsStringExt::from_vec(vec![0xff]);
        let error = parse_outcome(vec!["bin".into(), non_utf8])
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-UTF-8 argument"), "{error}");
    }

    #[test]
    fn no_argument_can_ask_this_adapter_for_a_reply() {
        // The refusal is by name, so an operator cannot enable ingress by
        // guessing a plausible flag, and a subscription that carries one
        // fails loudly instead of quietly delivering with a reply channel.
        for flag in [
            "--ingress",
            "--allow-ingress",
            "--read-reply",
            "--capture-reply",
            "--capture-response",
            "--response",
            "--reply",
            "--bidirectional",
            "--pipe-stdout",
            "--sink-stdout",
            "--act-on-reply",
        ] {
            let error = parse_outcome(vec![
                "bin".into(),
                "--sink".into(),
                "/opt/zcode/notify".into(),
                "--notify-timeout-ms".into(),
                "5000".into(),
                flag.into(),
                "1".into(),
            ])
            .unwrap_err()
            .to_string();
            assert_eq!(error, format!("unknown argument: {flag}"));
        }
    }
}
