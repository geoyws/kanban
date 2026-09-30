//! Instructable fake of a ZCode notification sink.
//!
//! The adapter starts a sink with NO arguments and the notice on stdin, and
//! clears the environment first, so this fake takes its instructions from
//! files named after its own executable path -- the only channel the adapter
//! leaves open:
//!
//! * `<argv0>.scenario` - the behaviour to perform; `accept` when absent.
//! * `<argv0>.payload`  - the bytes the `answer` scenario tries to answer with.
//! * `<argv0>.notice`   - written: the bytes it read from stdin.
//! * `<argv0>.capture`  - written: JSON facts about how it was started.
//!
//! Scenarios:
//!
//! * `accept`      - drain stdin, exit 0.
//! * `answer`      - drain stdin, then try to answer with `<argv0>.payload` on
//!                   both stdout and stderr, exit 0.
//! * `refuse`      - drain stdin, exit 7.
//! * `hang`        - drain stdin, then sleep past any sane deadline.
//! * `signal`      - drain stdin, then have itself killed with SIGKILL, so it
//!                   produces no exit code at all.
//! * `close-stdin` - exit 0 without ever reading the notice.

use std::env;
use std::fs::{self, File};
use std::io::{Read as _, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::FileTypeExt as _;
use std::process::Command;
use std::thread;
use std::time::Duration;

const SECRET_ENV: &str = "KANBAN_ZCODE_TEST_SECRET";
const MAX_NOTICE_BYTES: usize = 4 << 20;
const HANG_SECONDS: u64 = 60;
const SIGNAL_GRACE_SECONDS: u64 = 30;

struct Facts {
    scenario: String,
    argument_count: usize,
    saw_secret: bool,
    path: String,
    stdout_kind: &'static str,
    stderr_kind: &'static str,
    read_stdin: bool,
    notice_bytes: usize,
    answered_stdout: bool,
    answered_stderr: bool,
    answer_bytes: usize,
}

fn main() {
    let arguments: Vec<String> = env::args().collect();
    let argv0 = arguments.first().expect("argv0").clone();
    let scenario = fs::read_to_string(format!("{argv0}.scenario"))
        .map(|text| text.trim().to_owned())
        .unwrap_or_else(|_| "accept".to_owned());

    let mut facts = Facts {
        scenario: scenario.clone(),
        argument_count: arguments.len(),
        saw_secret: env::var_os(SECRET_ENV).is_some(),
        path: env::var("PATH").unwrap_or_default(),
        // What the adapter attached to this process's own output streams. A
        // character device is /dev/null; a FIFO would mean the adapter kept
        // the other end of a pipe and could read whatever is written here.
        stdout_kind: fd_kind(std::io::stdout().as_fd()),
        stderr_kind: fd_kind(std::io::stderr().as_fd()),
        read_stdin: false,
        notice_bytes: 0,
        answered_stdout: false,
        answered_stderr: false,
        answer_bytes: 0,
    };

    if scenario == "close-stdin" {
        write_capture(&argv0, &facts);
        return;
    }

    let notice = drain_stdin();
    facts.read_stdin = true;
    facts.notice_bytes = notice.len();
    fs::write(format!("{argv0}.notice"), &notice).expect("record the notice");

    match scenario.as_str() {
        "accept" => write_capture(&argv0, &facts),
        "answer" => {
            let payload = fs::read(format!("{argv0}.payload")).expect("read the answer payload");
            facts.answer_bytes = payload.len();
            facts.answered_stdout = answer(std::io::stdout(), &payload);
            facts.answered_stderr = answer(std::io::stderr(), &payload);
            write_capture(&argv0, &facts);
        }
        "refuse" => {
            write_capture(&argv0, &facts);
            std::process::exit(7);
        }
        "hang" => {
            write_capture(&argv0, &facts);
            thread::sleep(Duration::from_secs(HANG_SECONDS));
        }
        "signal" => {
            write_capture(&argv0, &facts);
            // SIGKILL rather than abort(): the process must leave no exit
            // code, and a hard kill does not summon a crash reporter.
            Command::new("/bin/kill")
                .arg("-KILL")
                .arg(std::process::id().to_string())
                .status()
                .expect("kill this process");
            thread::sleep(Duration::from_secs(SIGNAL_GRACE_SECONDS));
            // Reached only if the kill silently failed, which must not look
            // like any scenario the adapter classifies.
            std::process::exit(99);
        }
        other => panic!("unsupported scenario: {other}"),
    }
}

fn fd_kind(fd: BorrowedFd<'_>) -> &'static str {
    // Duplicated so the returned handle owns a descriptor of its own and
    // dropping it cannot close the real stdout or stderr.
    let owned: OwnedFd = fd.try_clone_to_owned().expect("duplicate the descriptor");
    let file_type = File::from(owned)
        .metadata()
        .expect("stat the descriptor")
        .file_type();
    if file_type.is_char_device() {
        "char"
    } else if file_type.is_fifo() {
        "fifo"
    } else if file_type.is_file() {
        "file"
    } else if file_type.is_socket() {
        "socket"
    } else {
        "other"
    }
}

fn drain_stdin() -> Vec<u8> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((MAX_NOTICE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .expect("read the notice");
    assert!(
        bytes.len() <= MAX_NOTICE_BYTES,
        "the notice exceeds {MAX_NOTICE_BYTES} bytes"
    );
    bytes
}

/// Whether the sink managed to hand the payload to its own output stream.
/// True even when the adapter routed that stream to /dev/null: the write
/// succeeds, the bytes simply have nowhere to arrive.
fn answer(mut stream: impl Write, payload: &[u8]) -> bool {
    stream
        .write_all(payload)
        .and_then(|()| stream.flush())
        .is_ok()
}

fn write_capture(argv0: &str, facts: &Facts) {
    let capture = format!(
        concat!(
            "{{\"scenario\":\"{}\",\"pid\":{},\"argumentCount\":{},\"sawSecret\":{},",
            "\"path\":\"{}\",\"stdoutKind\":\"{}\",\"stderrKind\":\"{}\",\"readStdin\":{},",
            "\"noticeBytes\":{},\"answeredStdout\":{},\"answeredStderr\":{},\"answerBytes\":{}}}"
        ),
        escape(&facts.scenario),
        std::process::id(),
        facts.argument_count,
        facts.saw_secret,
        escape(&facts.path),
        facts.stdout_kind,
        facts.stderr_kind,
        facts.read_stdin,
        facts.notice_bytes,
        facts.answered_stdout,
        facts.answered_stderr,
        facts.answer_bytes,
    );
    // Staged and renamed so a reader never observes half a capture.
    let staged = format!("{argv0}.capture.tmp");
    fs::write(&staged, capture).expect("stage the capture");
    fs::rename(&staged, format!("{argv0}.capture")).expect("publish the capture");
}

fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
