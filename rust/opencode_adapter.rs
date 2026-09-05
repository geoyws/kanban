use crate::adapter_protocol::{
    AdapterRequest, AdapterResponse, decode_request, decode_response, encode_request,
};
use anyhow::{Result, bail};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const HELP: &str = "kanban-opencode-adapter --endpoint http://LOOPBACK_IP:PORT/ABSOLUTE_PATH --request-timeout-ms N";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_HEAD_BYTES: usize = 1 << 13;
const MAX_BODY_BYTES: usize = 1 << 16;
const MAX_ENDPOINT_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 128;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const READ_CHUNK_BYTES: usize = 4096;
const SCHEME: &str = "http://";
// OpenCode serves the delivery over a local HTTP server rather than a spawned
// turn, but the queue semantics are the ones `codex.queue` already speaks: the
// POST returns an acknowledgement of receipt and the turn happens afterwards,
// so this adapter reuses that action vocabulary instead of inventing one.
const OPENCODE_CONSUMER_ID: &str = "opencode.server";
const ENQUEUE_TURN_ACTION_ID: &str = "enqueue-turn";

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
                "kanban-opencode-adapter {}",
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

fn run(args: &Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(args.request_timeout_ms);
    let request = decode_request_from_stdin()?;
    let response = post_delivery(&args.endpoint, &request, deadline)?;
    let mut stdout = io::stdout();
    stdout.write_all(&render_response(&response)?)?;
    stdout.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    endpoint: Endpoint,
    request_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    address: SocketAddr,
    authority: String,
    path: String,
}

/// One classified way a delivery attempt to the OpenCode server failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    EndpointUnreachable,
    RequestRejected,
    EndpointFailed,
    DeadlineExceeded,
    ResponseInvalid,
}

impl FailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::EndpointUnreachable => "opencode_endpoint_unreachable",
            Self::RequestRejected => "opencode_request_rejected",
            Self::EndpointFailed => "opencode_endpoint_failed",
            Self::DeadlineExceeded => "opencode_deadline_exceeded",
            Self::ResponseInvalid => "opencode_response_invalid",
        }
    }

    /// Whether a later attempt with byte-identical delivery bytes can succeed.
    /// See [`classify_response_status`] for the reasoning behind each answer.
    const fn retryable(self) -> bool {
        match self {
            Self::EndpointUnreachable | Self::EndpointFailed | Self::DeadlineExceeded => true,
            Self::RequestRejected | Self::ResponseInvalid => false,
        }
    }

    const fn exit_code(self) -> i32 {
        match self {
            Self::EndpointUnreachable => 10,
            Self::RequestRejected => 11,
            Self::EndpointFailed => 12,
            Self::DeadlineExceeded => 13,
            Self::ResponseInvalid => 14,
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

/// Map one HTTP answer from the OpenCode server onto the class the operator
/// sees, or `None` when the endpoint accepted the delivery.
///
/// The dispatcher records exactly one error code per failed attempt and
/// discards the adapter's stderr, so this classification is the operator's
/// only clue about what to do next. Every class is therefore a distinct code
/// and a distinct process exit status; a single generic failure would tell the
/// operator to go read the server's own logs, which is the thing the code
/// exists to avoid.
///
/// Retryable, because a later attempt with byte-identical bytes can succeed:
///
/// * `opencode_endpoint_unreachable` -- the TCP connect failed, raised in
///   [`connect`]. OpenCode's server is a separate local process: a refusal
///   means it is not listening yet, is restarting, or was started on another
///   port. Nothing about the delivery is wrong, and the operator's next move
///   is to start or re-point the server, not to inspect the event.
/// * `opencode_endpoint_failed` -- 5xx, 408, 429, or a body that never
///   completes; raised here, in [`write_request`], and in [`read_body`]. The
///   server accepted the bytes and then failed to finish the attempt, which is
///   what a transient fault is. 408 and 429 are numerically 4xx but RFC 9110
///   makes them explicit retry invitations, so they are classed with 5xx
///   instead of with a rejection -- classing them terminal would drop a
///   delivery the server itself asked us to re-send.
/// * `opencode_deadline_exceeded` -- the `--request-timeout-ms` deadline
///   elapsed, raised in [`remaining`] and [`read_more`]. A wedged server says
///   nothing about the delivery, and the ledger already stores a timeout
///   separately from other failures, so this must not look like a rejection.
///
/// Terminal, because retrying identical bytes reproduces the same answer and
/// would spend the subscription's retry budget on a delivery that can never
/// land:
///
/// * `opencode_request_rejected` -- every other 4xx. The server understood the
///   request and refused this exact route or payload: an unknown path, an
///   unsupported protocol version, a body it will not take. This adapter sends
///   byte-identical bytes on every attempt, so the answer cannot change.
/// * `opencode_response_invalid` -- 1xx, 3xx, or a 2xx whose body is not a
///   valid `AdapterResponse` for this delivery. That is a wrong or
///   misconfigured endpoint rather than a transient fault: a redirect means
///   the configured URL is wrong, and an acknowledgement that does not name
///   this subscription and event cannot be trusted to mean the delivery
///   arrived at all.
fn classify_response_status(status: u16) -> Option<FailureClass> {
    match status {
        200..=299 => None,
        408 | 429 | 500..=599 => Some(FailureClass::EndpointFailed),
        400..=499 => Some(FailureClass::RequestRejected),
        _ => Some(FailureClass::ResponseInvalid),
    }
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

    let mut endpoint = None;
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
            "--endpoint" => assign_once(&mut endpoint, parse_endpoint(value)?, flag)?,
            "--request-timeout-ms" => {
                assign_once(&mut request_timeout_ms, parse_timeout_ms(value)?, flag)?
            }
            _ => bail!("unknown argument: {flag}"),
        }
        index += 1;
    }

    Ok(Outcome::Args(Args {
        endpoint: endpoint.ok_or_else(|| anyhow::anyhow!("missing required flag: --endpoint"))?,
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

fn parse_timeout_ms(value: &str) -> Result<u64> {
    let timeout_ms: u64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--request-timeout-ms must be an integer"))?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("--request-timeout-ms must be in {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

/// Parse the one endpoint the host configuration pins.
///
/// The endpoint is an explicit argument and is never read from the
/// environment: the dispatcher clears the child environment before every
/// invocation, so an ambient variable would either be silently empty or, worse
/// on a host that leaks one through, aim a ledger event at an address nobody
/// wrote down in the subscription. The host must also be a loopback literal,
/// which keeps a typo from posting private ledger events off the machine and
/// removes name resolution from the delivery path entirely.
fn parse_endpoint(value: &str) -> Result<Endpoint> {
    if value.is_empty() || value.len() > MAX_ENDPOINT_BYTES {
        bail!("--endpoint must be 1..={MAX_ENDPOINT_BYTES} bytes");
    }
    if !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        bail!("--endpoint must be printable ASCII without spaces");
    }
    let rest = value
        .strip_prefix(SCHEME)
        .ok_or_else(|| anyhow::anyhow!("--endpoint must start with {SCHEME}"))?;
    let Some(separator) = rest.find('/') else {
        bail!("--endpoint must include an absolute path");
    };
    let (authority, path) = rest.split_at(separator);
    let address = parse_authority(authority)?;
    validate_endpoint_path(path)?;
    Ok(Endpoint {
        address,
        authority: authority.to_owned(),
        path: path.to_owned(),
    })
}

fn parse_authority(authority: &str) -> Result<SocketAddr> {
    if authority.contains('@') {
        bail!("--endpoint must not carry userinfo");
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            bail!("--endpoint IPv6 host must be bracketed");
        };
        let Some(port) = after.strip_prefix(':') else {
            bail!("--endpoint must name an explicit port");
        };
        let host: Ipv6Addr = host
            .parse()
            .map_err(|_| anyhow::anyhow!("--endpoint host must be an IP literal"))?;
        (IpAddr::V6(host), port)
    } else {
        let Some((host, port)) = authority.split_once(':') else {
            bail!("--endpoint must name an explicit port");
        };
        let host: Ipv4Addr = host
            .parse()
            .map_err(|_| anyhow::anyhow!("--endpoint host must be an IP literal"))?;
        (IpAddr::V4(host), port)
    };
    if !host.is_loopback() {
        bail!("--endpoint host must be a loopback address");
    }
    let parsed: u16 = port
        .parse()
        .map_err(|_| anyhow::anyhow!("--endpoint port must be 1..=65535"))?;
    if parsed == 0 || port != parsed.to_string() {
        bail!("--endpoint port must be 1..=65535 without padding");
    }
    Ok(SocketAddr::new(host, parsed))
}

fn validate_endpoint_path(path: &str) -> Result<()> {
    if path.len() > MAX_PATH_BYTES {
        bail!("--endpoint path must be at most {MAX_PATH_BYTES} bytes");
    }
    if path.contains('?') || path.contains('#') {
        bail!("--endpoint must not carry a query or fragment");
    }
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        bail!("--endpoint path must not contain relative segments");
    }
    Ok(())
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
    if request.target.consumer_id != OPENCODE_CONSUMER_ID {
        bail!("adapter target consumer ID must be {OPENCODE_CONSUMER_ID}");
    }
    if request.target.action_id != ENQUEUE_TURN_ACTION_ID {
        bail!("adapter target action ID must be {ENQUEUE_TURN_ACTION_ID}");
    }
    Ok(())
}

fn render_response(response: &AdapterResponse) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_BODY_BYTES {
        bail!("adapter response exceeds {MAX_BODY_BYTES} bytes");
    }
    Ok(bytes)
}

fn post_delivery(
    endpoint: &Endpoint,
    request: &AdapterRequest,
    deadline: Instant,
) -> Result<AdapterResponse> {
    let body = encode_request(request)?;
    let mut stream = connect(endpoint, deadline)?;
    write_request(&mut stream, endpoint, &body, deadline)?;
    let head = read_head(&mut stream, deadline)?;
    let status = parse_status_line(status_line(&head.text))?;
    if let Some(class) = classify_response_status(status) {
        return Err(failed(
            class,
            format!("the endpoint answered HTTP {status}"),
        ));
    }
    let length = content_length(&head.text)?;
    let body = read_body(&mut stream, head.body, length, deadline)?;
    decode_response(&body, request).map_err(|error| {
        failed(
            FailureClass::ResponseInvalid,
            format!("the endpoint accepted the delivery but acknowledged it invalidly: {error:#}"),
        )
    })
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

fn timed_out(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
}

fn connect(endpoint: &Endpoint, deadline: Instant) -> Result<TcpStream> {
    let left = remaining(deadline)?;
    TcpStream::connect_timeout(&endpoint.address, left).map_err(|error| {
        if timed_out(&error) {
            failed(
                FailureClass::DeadlineExceeded,
                format!(
                    "connecting to {} did not complete before the deadline: {error}",
                    endpoint.address
                ),
            )
        } else {
            failed(
                FailureClass::EndpointUnreachable,
                format!("connecting to {}: {error}", endpoint.address),
            )
        }
    })
}

fn request_bytes(endpoint: &Endpoint, body: &[u8]) -> Vec<u8> {
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: kanban-opencode-adapter/{}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.path,
        endpoint.authority,
        env!("CARGO_PKG_VERSION"),
        body.len()
    );
    let mut bytes = Vec::with_capacity(head.len() + body.len());
    bytes.extend_from_slice(head.as_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn write_request(
    stream: &mut TcpStream,
    endpoint: &Endpoint,
    body: &[u8],
    deadline: Instant,
) -> Result<()> {
    let bytes = request_bytes(endpoint, body);
    // A delivery runs up to a megabyte, which is larger than a loopback socket
    // buffer, so an endpoint that accepts the connection and then stops reading
    // would park this write forever without a deadline. A socket write timeout
    // bounds one syscall, not the attempt, so it is re-armed from the budget
    // that is actually left on every pass -- the same discipline as
    // [`read_more`] -- and the whole POST stays inside the deadline instead of
    // overrunning it once per partial write.
    let mut written = 0;
    while written < bytes.len() {
        let left = remaining(deadline)?;
        stream.set_write_timeout(Some(left)).map_err(|error| {
            failed(
                FailureClass::EndpointFailed,
                format!("arming the delivery write deadline: {error}"),
            )
        })?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(failed(
                    FailureClass::EndpointFailed,
                    format!(
                        "{} stopped accepting the delivery after {written} of {} bytes",
                        endpoint.address,
                        bytes.len()
                    ),
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if timed_out(&error) => {
                return Err(failed(
                    FailureClass::DeadlineExceeded,
                    format!(
                        "sending the delivery to {} did not complete before the deadline: {error}",
                        endpoint.address
                    ),
                ));
            }
            Err(error) => {
                return Err(failed(
                    FailureClass::EndpointFailed,
                    format!("sending the delivery to {}: {error}", endpoint.address),
                ));
            }
        }
    }
    Ok(())
}

struct Head {
    text: String,
    body: Vec<u8>,
}

fn read_head(stream: &mut TcpStream, deadline: Instant) -> Result<Head> {
    let mut buffer = Vec::new();
    let end = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return Err(failed(
                FailureClass::ResponseInvalid,
                format!("the endpoint's response head exceeds {MAX_HEAD_BYTES} bytes"),
            ));
        }
        if !read_more(stream, &mut buffer, deadline)? {
            return Err(failed(
                FailureClass::EndpointFailed,
                "the endpoint closed the connection before its response head completed",
            ));
        }
    };
    let body = buffer.split_off(end);
    let text = String::from_utf8(buffer).map_err(|_| {
        failed(
            FailureClass::ResponseInvalid,
            "the endpoint's response head is not UTF-8",
        )
    })?;
    Ok(Head { text, body })
}

fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn read_more(stream: &mut TcpStream, buffer: &mut Vec<u8>, deadline: Instant) -> Result<bool> {
    let left = remaining(deadline)?;
    stream.set_read_timeout(Some(left)).map_err(|error| {
        failed(
            FailureClass::EndpointFailed,
            format!("arming the response read deadline: {error}"),
        )
    })?;
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(false),
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                return Ok(true);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if timed_out(&error) => {
                return Err(failed(
                    FailureClass::DeadlineExceeded,
                    format!("the endpoint's response did not arrive before the deadline: {error}"),
                ));
            }
            Err(error) => {
                return Err(failed(
                    FailureClass::EndpointFailed,
                    format!("reading the endpoint's response: {error}"),
                ));
            }
        }
    }
}

fn status_line(head: &str) -> &str {
    head.split("\r\n").next().unwrap_or(head)
}

fn parse_status_line(line: &str) -> Result<u16> {
    let rest = line
        .strip_prefix("HTTP/1.1 ")
        .or_else(|| line.strip_prefix("HTTP/1.0 "))
        .ok_or_else(|| {
            failed(
                FailureClass::ResponseInvalid,
                "the endpoint did not answer HTTP/1.x",
            )
        })?;
    let code = rest.split(' ').next().unwrap_or(rest);
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(failed(
            FailureClass::ResponseInvalid,
            "the endpoint's status code is not three digits",
        ));
    }
    Ok(code
        .bytes()
        .fold(0_u16, |status, byte| status * 10 + u16::from(byte - b'0')))
}

/// Length of the accepted response body.
///
/// A declared length is required rather than optional: without one, a body
/// truncated by a crashing server is byte-for-byte indistinguishable from a
/// complete one, and this adapter must never report a delivery acknowledged
/// when it cannot prove it read the whole acknowledgement. That also refuses a
/// chunked answer, which is deliberate -- the endpoint is a single local
/// server posting a fixed-size acknowledgement, and the refusal names the
/// framing so a mismatch is a legible configuration error instead of a hang.
fn content_length(head: &str) -> Result<usize> {
    let mut declared = None;
    for line in head.split("\r\n").skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(failed(
                FailureClass::ResponseInvalid,
                "the endpoint declared a malformed Content-Length",
            ));
        }
        let length: usize = value.parse().map_err(|_| {
            failed(
                FailureClass::ResponseInvalid,
                "the endpoint declared an unrepresentable Content-Length",
            )
        })?;
        if declared.is_some_and(|first| first != length) {
            return Err(failed(
                FailureClass::ResponseInvalid,
                "the endpoint declared conflicting Content-Length headers",
            ));
        }
        declared = Some(length);
    }
    let length = declared.ok_or_else(|| {
        failed(
            FailureClass::ResponseInvalid,
            "the endpoint accepted the delivery without declaring a Content-Length, so a truncated acknowledgement could not be told apart from a complete one",
        )
    })?;
    if length > MAX_BODY_BYTES {
        return Err(failed(
            FailureClass::ResponseInvalid,
            format!(
                "the endpoint declared a {length}-byte body over the {MAX_BODY_BYTES}-byte limit"
            ),
        ));
    }
    Ok(length)
}

fn read_body(
    stream: &mut TcpStream,
    mut body: Vec<u8>,
    length: usize,
    deadline: Instant,
) -> Result<Vec<u8>> {
    while body.len() < length {
        if !read_more(stream, &mut body, deadline)? {
            return Err(failed(
                FailureClass::EndpointFailed,
                format!(
                    "the endpoint closed the connection after {} of {length} acknowledgement bytes",
                    body.len()
                ),
            ));
        }
    }
    if body.len() > length {
        return Err(failed(
            FailureClass::ResponseInvalid,
            format!(
                "the endpoint sent {} bytes for a {length}-byte acknowledgement",
                body.len()
            ),
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;

    fn endpoint() -> Endpoint {
        Endpoint {
            address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4096),
            authority: "127.0.0.1:4096".to_owned(),
            path: "/delivery".to_owned(),
        }
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
                consumer_id: OPENCODE_CONSUMER_ID.to_owned(),
                action_id: ENQUEUE_TURN_ACTION_ID.to_owned(),
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
            FailureClass::EndpointUnreachable,
            FailureClass::RequestRejected,
            FailureClass::EndpointFailed,
            FailureClass::DeadlineExceeded,
            FailureClass::ResponseInvalid,
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
    }

    #[test]
    fn http_status_classes_split_transient_faults_from_refusals() {
        assert_eq!(classify_response_status(200), None);
        assert_eq!(classify_response_status(202), None);
        for status in [400, 401, 403, 404, 409, 422] {
            assert_eq!(
                classify_response_status(status),
                Some(FailureClass::RequestRejected),
                "HTTP {status}"
            );
        }
        for status in [408, 429, 500, 502, 503] {
            assert_eq!(
                classify_response_status(status),
                Some(FailureClass::EndpointFailed),
                "HTTP {status}"
            );
        }
        for status in [100, 301, 302, 600] {
            assert_eq!(
                classify_response_status(status),
                Some(FailureClass::ResponseInvalid),
                "HTTP {status}"
            );
        }
    }

    #[test]
    fn retryable_classes_are_exactly_the_ones_a_resend_can_fix() {
        assert!(FailureClass::EndpointUnreachable.retryable());
        assert!(FailureClass::EndpointFailed.retryable());
        assert!(FailureClass::DeadlineExceeded.retryable());
        assert!(!FailureClass::RequestRejected.retryable());
        assert!(!FailureClass::ResponseInvalid.retryable());
        assert_eq!(
            FailureClass::RequestRejected.to_string(),
            "opencode_request_rejected (terminal)"
        );
        assert_eq!(
            FailureClass::EndpointUnreachable.to_string(),
            "opencode_endpoint_unreachable (retryable)"
        );
    }

    #[test]
    fn classified_failures_carry_their_exit_status_through_anyhow() {
        for class in [
            FailureClass::EndpointUnreachable,
            FailureClass::RequestRejected,
            FailureClass::EndpointFailed,
            FailureClass::DeadlineExceeded,
            FailureClass::ResponseInvalid,
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
    fn a_breached_deadline_is_its_own_class() {
        let error = remaining(Instant::now() - Duration::from_millis(1)).unwrap_err();
        assert_eq!(
            exit_code(&error),
            FailureClass::DeadlineExceeded.exit_code()
        );
        assert!(remaining(Instant::now() + Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn only_a_loopback_http_endpoint_with_an_absolute_path_is_accepted() {
        assert_eq!(
            parse_endpoint("http://127.0.0.1:4096/delivery").unwrap(),
            endpoint()
        );
        let parsed = parse_endpoint("http://[::1]:80/a/b").unwrap();
        assert_eq!(
            parsed.address,
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 80)
        );
        assert_eq!(parsed.authority, "[::1]:80");
        assert_eq!(parsed.path, "/a/b");

        for (value, expected) in [
            ("https://127.0.0.1:4096/x", "must start with"),
            ("http://127.0.0.1:4096", "absolute path"),
            ("http://10.0.0.1:4096/x", "loopback"),
            ("http://[2001:db8::1]:80/x", "loopback"),
            ("http://localhost:4096/x", "IP literal"),
            ("http://127.0.0.1/x", "explicit port"),
            ("http://127.0.0.1:0/x", "without padding"),
            ("http://127.0.0.1:04096/x", "without padding"),
            ("http://127.0.0.1:99999/x", "1..=65535"),
            ("http://user@127.0.0.1:80/x", "userinfo"),
            ("http://[::1:80/x", "bracketed"),
            ("http://[::1]80/x", "explicit port"),
            ("http://127.0.0.1:80/x?y=1", "query or fragment"),
            ("http://127.0.0.1:80/../x", "relative segments"),
            ("http://127.0.0.1:80/ x", "printable ASCII"),
            ("", "1..=256 bytes"),
        ] {
            let error = parse_endpoint(value).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "{value} reported {error}, expected {expected}"
            );
        }
        let long = format!("http://127.0.0.1:80/{}", "a".repeat(MAX_ENDPOINT_BYTES));
        assert!(
            parse_endpoint(&long)
                .unwrap_err()
                .to_string()
                .contains("1..=256 bytes")
        );
        let long_path = format!("http://127.0.0.1:80/{}", "a".repeat(MAX_PATH_BYTES));
        assert!(
            parse_endpoint(&long_path)
                .unwrap_err()
                .to_string()
                .contains("path must be at most")
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
                "--endpoint".into(),
                "http://127.0.0.1:4096/delivery".into(),
                "--request-timeout-ms".into(),
                "5000".into(),
            ])
            .unwrap(),
            Outcome::Args(Args {
                endpoint: endpoint(),
                request_timeout_ms: 5_000,
            })
        );

        for (tokens, expected) in [
            (vec!["bin".into(), "extra".into()], "positional"),
            (
                vec!["bin".into(), "--unknown".into(), "x".into()],
                "unknown argument",
            ),
            (vec!["bin".into(), "--endpoint".into()], "missing value"),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "--request-timeout-ms".into(),
                ],
                "missing value",
            ),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:1/a".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:2/a".into(),
                ],
                "argument repeated",
            ),
            (
                vec!["bin".into(), "--request-timeout-ms".into(), "5000".into()],
                "missing required flag: --endpoint",
            ),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:1/a".into(),
                ],
                "missing required flag: --request-timeout-ms",
            ),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:1/a".into(),
                    "--request-timeout-ms".into(),
                    "999".into(),
                ],
                "must be in 1000..=300000",
            ),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:1/a".into(),
                    "--request-timeout-ms".into(),
                    "300001".into(),
                ],
                "must be in 1000..=300000",
            ),
            (
                vec![
                    "bin".into(),
                    "--endpoint".into(),
                    "http://127.0.0.1:1/a".into(),
                    "--request-timeout-ms".into(),
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
    fn only_the_opencode_consumer_and_action_are_delivered() {
        assert!(validate_request_target(&request()).is_ok());
        let mut wrong = request();
        wrong.target.consumer_id = "codex.queue".to_owned();
        assert!(
            validate_request_target(&wrong)
                .unwrap_err()
                .to_string()
                .contains("consumer ID")
        );
        let mut wrong = request();
        wrong.target.action_id = "start-readonly-turn".to_owned();
        assert!(
            validate_request_target(&wrong)
                .unwrap_err()
                .to_string()
                .contains("action ID")
        );
    }

    #[test]
    fn the_posted_request_frames_the_delivery_for_a_local_server() {
        let body = encode_request(&request()).unwrap();
        let bytes = request_bytes(&endpoint(), &body);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("POST /delivery HTTP/1.1\r\n"));
        assert!(text.contains("\r\nHost: 127.0.0.1:4096\r\n"));
        assert!(text.contains("\r\nContent-Type: application/json\r\n"));
        assert!(text.contains(&format!("\r\nContent-Length: {}\r\n", body.len())));
        assert!(text.contains("\r\nConnection: close\r\n"));
        assert!(text.ends_with(&String::from_utf8(body).unwrap()));
    }

    #[test]
    fn a_response_head_must_be_framed_before_its_body_is_trusted() {
        assert_eq!(
            parse_status_line(status_line("HTTP/1.1 204 No Content\r\nA: b\r\n\r\n")).unwrap(),
            204
        );
        assert_eq!(parse_status_line("HTTP/1.0 500").unwrap(), 500);
        for line in ["HTTP/2 200 OK", "HTTP/1.1 20 OK", "HTTP/1.1 2x0 OK"] {
            let error = parse_status_line(line).unwrap_err();
            assert_eq!(
                exit_code(&error),
                FailureClass::ResponseInvalid.exit_code(),
                "{line}"
            );
        }

        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length: 12\r\n\r\n").unwrap(),
            12
        );
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\ncontent-length:  7 \r\nX: y\r\n\r\n").unwrap(),
            7
        );
        for head in [
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999999999\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 65537\r\n\r\n",
        ] {
            let error = content_length(head).unwrap_err();
            assert_eq!(
                exit_code(&error),
                FailureClass::ResponseInvalid.exit_code(),
                "{head}"
            );
        }
        assert_eq!(head_end(b"a\r\n\r\nb"), Some(5));
        assert_eq!(head_end(b"a\r\n"), None);
    }
}
