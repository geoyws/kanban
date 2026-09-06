//! Instructable fake of an ACP peer speaking NDJSON JSON-RPC 2.0 on stdio.
//!
//! The adapter spawns this with no argv, a cleared environment carrying only
//! `HOME` and `PATH`, and a private empty working directory. The scenario is
//! therefore read from `$HOME/scenario.txt`, and the observed invocation is
//! appended as one JSON line to `capture.ndjson` beside this executable.
//!
//! It reads exactly one newline-terminated frame from stdin -- never to end of
//! stream, because the adapter deliberately holds stdin open until it has its
//! answer -- and then answers according to the scenario:
//!
//! * `accept`    - one JSON-RPC result frame acknowledging the delivery it read.
//! * `reject`    - one JSON-RPC error frame for the id it read.
//! * `malformed` - `{` and a terminator: a complete frame that is not JSON.
//! * `oversized` - one byte past the adapter's response frame cap, then a
//!                 terminator that the adapter must never reach.
//! * `mismatch`  - an acknowledgement naming a different event.
//! * `alien-id`  - an acknowledgement for the right delivery under a JSON-RPC
//!                 id nobody asked.
//! * `truncate`  - half a frame, no terminator, then exit.
//! * `silent`    - exit without writing anything.
//! * `hang`      - read the frame and never answer.
//! * `linger`    - acknowledge, then stay alive ignoring stdin end of stream.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::Duration;

const HANG_SECONDS: u64 = 30;
const OVERSIZED_BYTES: usize = (1 << 16) + 1;

fn escaped(value: &str) -> String {
    let mut escaped = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other if other.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", other as u32));
            }
            other => escaped.push(other),
        }
    }
    escaped.push('"');
    escaped
}

fn json_string(input: &str, key: &str) -> String {
    let marker = format!("\"{key}\":\"");
    let start = input.find(&marker).expect("frame string field") + marker.len();
    let tail = &input[start..];
    tail[..tail.find('"').expect("frame closing quote")].to_owned()
}

fn json_i64(input: &str, key: &str) -> i64 {
    let marker = format!("\"{key}\":");
    let start = input.find(&marker).expect("frame integer field") + marker.len();
    input[start..]
        .chars()
        .take_while(|value| value.is_ascii_digit() || *value == '-')
        .collect::<String>()
        .parse()
        .expect("frame integer")
}

/// Read one newline-terminated frame without ever reading to end of stream.
fn read_frame() -> String {
    let mut stdin = std::io::stdin();
    let mut frame = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match stdin.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => frame.push(byte[0]),
            Err(error) => panic!("read the delivery frame: {error}"),
        }
    }
    String::from_utf8(frame).expect("the delivery frame is UTF-8")
}

/// Write bytes the adapter may already have stopped reading, then flush.
fn emit(bytes: &[u8]) {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(bytes);
    let _ = stdout.flush();
}

fn acknowledgement(frame: &str, event_id: &str) -> String {
    format!(
        "{{\"protocolVersion\":1,\"subscriptionID\":{},\"eventID\":{},\"createdAt\":{},\"replay\":{}}}",
        escaped(&json_string(frame, "subscriptionID")),
        escaped(event_id),
        json_i64(frame, "createdAt"),
        json_i64(frame, "attempt") > 1
    )
}

fn result_frame(id: i64, result: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}\n")
}

fn main() {
    let home = PathBuf::from(env::var_os("HOME").expect("the adapter passes HOME"));
    let capture = env::current_exe()
        .expect("locate this fake")
        .parent()
        .expect("the fake has a parent directory")
        .join("capture.ndjson");
    let scenario = fs::read_to_string(home.join("scenario.txt")).expect("read the scenario");
    let scenario = scenario.trim().to_owned();

    let frame = read_frame();
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut variables: Vec<(String, String)> = env::vars().collect();
    variables.sort();
    let record = format!(
        "{{\"argv\":[{}],\"env\":[{}],\"cwd\":{},\"frame\":{}}}\n",
        argv.iter()
            .map(|value| escaped(value))
            .collect::<Vec<_>>()
            .join(","),
        variables
            .iter()
            .map(|(name, value)| format!("[{},{}]", escaped(name), escaped(value)))
            .collect::<Vec<_>>()
            .join(","),
        escaped(
            &env::current_dir()
                .expect("read the working directory")
                .to_string_lossy()
        ),
        if frame.is_empty() {
            "null"
        } else {
            frame.as_str()
        }
    );
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&capture)
        .expect("open the capture file");
    file.write_all(record.as_bytes())
        .expect("capture the frame");
    file.flush().expect("flush the capture file");

    let id = json_i64(&frame, "id");
    match scenario.as_str() {
        "accept" => emit(
            result_frame(id, &acknowledgement(&frame, &json_string(&frame, "eventID"))).as_bytes(),
        ),
        "reject" => emit(
            format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"Method not found\",\"data\":{{\"method\":\"_kanban/deliverEvent\"}}}}}}\n"
            )
            .as_bytes(),
        ),
        "malformed" => emit(b"{\n"),
        "oversized" => {
            let mut answer = vec![b'x'; OVERSIZED_BYTES];
            answer.push(b'\n');
            emit(&answer);
        }
        "mismatch" => emit(result_frame(id, &acknowledgement(&frame, &"0".repeat(64))).as_bytes()),
        "alien-id" => emit(
            result_frame(
                id + 98,
                &acknowledgement(&frame, &json_string(&frame, "eventID")),
            )
            .as_bytes(),
        ),
        "truncate" => emit(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":").as_bytes()),
        "silent" => process::exit(7),
        "hang" => thread::sleep(Duration::from_secs(HANG_SECONDS)),
        "linger" => {
            emit(
                result_frame(id, &acknowledgement(&frame, &json_string(&frame, "eventID")))
                    .as_bytes(),
            );
            thread::sleep(Duration::from_secs(HANG_SECONDS));
        }
        other => panic!("unknown scenario: {other}"),
    }
}
