//! Instructable, dependency-free fake of a Cursor worker.
//!
//! The adapter clears the child environment, so the fake is told what to do
//! through files inside its `HOME` (which is the worker's `--state-dir`):
//!
//! * `scenario-<eventID>.txt`, else `scenario.txt`, else `ok` -- the scenario.
//! * `turns.ndjson` -- an append-only log of one `start` record per turn and
//!   one `end` record per turn that was allowed to finish. Because the log is
//!   opened `O_APPEND` and each record is one short line, the *order of lines
//!   is the order the turns actually ran*, which is what the overlap test
//!   asserts on rather than comparing wall-clock readings across processes.
//!
//! Scenarios:
//!
//! * `ok`             - acknowledge the delivery at once.
//! * `hold`           - occupy the worker for `HOLD_MS`, then acknowledge.
//! * `hang`           - never finish; the adapter's deadline must kill it.
//! * `nonzero`        - write nothing to stdout and exit 23.
//! * `malformed`      - print a truncated JSON document and exit 0.
//! * `wrong-delivery` - print a well-formed acknowledgement for a *different*
//!                      subscription and event, which is what two turns
//!                      racing inside one worker looks like from outside.

use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::process;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HOLD_MS: u64 = 400;
const HANG_SECONDS: u64 = 30;
const OTHER_SUBSCRIPTION: &str = "sub-other";
const OTHER_EVENT: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").expect("the adapter must set HOME"))
}

fn escaped(value: &str) -> String {
    let mut out = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other.is_control() => write!(out, "\\u{:04x}", other as u32).unwrap(),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| escaped(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Pull one string field out of the delivery without a JSON dependency.
fn string_field(body: &str, name: &str) -> String {
    let needle = format!("\"{name}\":\"");
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("the delivery has no {name}"))
        + needle.len();
    let rest = &body[start..];
    let end = rest.find('"').expect("an unterminated string field");
    rest[..end].to_owned()
}

fn number_field(body: &str, name: &str) -> String {
    let needle = format!("\"{name}\":");
    let start = body
        .find(&needle)
        .unwrap_or_else(|| panic!("the delivery has no {name}"))
        + needle.len();
    let rest = &body[start..];
    let end = rest
        .find(|character: char| !character.is_ascii_digit() && character != '-')
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock moved backwards")
        .as_nanos()
}

fn record(fields: &str) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(home().join("turns.ndjson"))
        .expect("open the turn log");
    writeln!(log, "{{{fields}}}").expect("append a turn record");
}

fn scenario(event_id: &str) -> String {
    let home = home();
    fs::read_to_string(home.join(format!("scenario-{event_id}.txt")))
        .or_else(|_| fs::read_to_string(home.join("scenario.txt")))
        .unwrap_or_else(|_| "ok".into())
        .trim()
        .to_owned()
}

fn acknowledgement(subscription: &str, event: &str, created_at: &str, replay: bool) -> String {
    format!(
        "{{\"protocolVersion\":1,\"subscriptionID\":{},\"eventID\":{},\"createdAt\":{created_at},\"replay\":{replay}}}",
        escaped(subscription),
        escaped(event)
    )
}

fn main() {
    let argv = env::args().skip(1).collect::<Vec<_>>();
    let mut delivery = String::new();
    std::io::stdin()
        .read_to_string(&mut delivery)
        .expect("read the delivery from stdin");

    let event_id = string_field(&delivery, "eventID");
    let subscription_id = string_field(&delivery, "subscriptionID");
    let created_at = number_field(&delivery, "createdAt");
    let attempt: i64 = number_field(&delivery, "attempt")
        .parse()
        .expect("a numeric attempt");
    let scenario = scenario(&event_id);

    let mut variables = env::vars().collect::<Vec<_>>();
    variables.sort();
    let environment = format!(
        "[{}]",
        variables
            .iter()
            .map(|(name, value)| format!("[{},{}]", escaped(name), escaped(value)))
            .collect::<Vec<_>>()
            .join(",")
    );
    record(&format!(
        "\"phase\":\"start\",\"eventID\":{},\"scenario\":{},\"atNanos\":{},\"argv\":{},\"cwd\":{},\"env\":{},\"stdin\":{}",
        escaped(&event_id),
        escaped(&scenario),
        nanos(),
        array(&argv),
        escaped(&env::current_dir().unwrap().to_string_lossy()),
        environment,
        escaped(&delivery)
    ));

    match scenario.as_str() {
        "hold" => thread::sleep(Duration::from_millis(HOLD_MS)),
        "hang" => {
            thread::sleep(Duration::from_secs(HANG_SECONDS));
            // Only reached if the adapter failed to enforce its deadline; the
            // record makes that visible instead of silently passing.
            record(&format!(
                "\"phase\":\"outlived-the-deadline\",\"eventID\":{},\"atNanos\":{}",
                escaped(&event_id),
                nanos()
            ));
        }
        _ => {}
    }

    let stdout = match scenario.as_str() {
        "nonzero" => String::new(),
        "malformed" => "{\"protocolVersion\":1,".to_owned(),
        "wrong-delivery" => acknowledgement(OTHER_SUBSCRIPTION, OTHER_EVENT, &created_at, false),
        _ => acknowledgement(&subscription_id, &event_id, &created_at, attempt > 1),
    };

    record(&format!(
        "\"phase\":\"end\",\"eventID\":{},\"atNanos\":{}",
        escaped(&event_id),
        nanos()
    ));

    if scenario == "nonzero" {
        eprint!("the fake worker refused this turn");
        process::exit(23);
    }
    print!("{stdout}");
}
