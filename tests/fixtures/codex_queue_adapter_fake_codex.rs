use std::env;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;

fn capture_path() -> PathBuf {
    env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("capture.ndjson")
}

fn json_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for ch in value.chars() {
        match ch {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            c if c.is_control() => {
                write!(encoded, "\\u{:04x}", c as u32).unwrap();
            }
            c => encoded.push(c),
        }
    }
    encoded.push('"');
    encoded
}

fn json_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|value| json_string(value))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn json_env(values: &[(String, String)]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|(key, value)| format!("[{},{}]", json_string(key), json_string(value)))
            .collect::<Vec<_>>()
            .join(",")
    )
}

fn write_capture(mode: &str, argv: &[String], input: &str, stdout: &str) {
    let mut env_vars = env::vars().collect::<Vec<_>>();
    env_vars.sort_by(|left, right| left.0.cmp(&right.0));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture_path())
        .unwrap();
    let record = format!(
        "{{\"mode\":{},\"argv\":{},\"env\":{},\"stdin\":{},\"stdout\":{}}}",
        json_string(mode),
        json_array(argv),
        json_env(&env_vars),
        json_string(input),
        json_string(stdout),
    );
    writeln!(file, "{record}").unwrap();
    file.flush().unwrap();
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let mode = match args.as_slice() {
        [flag] if flag == "--version" => "version",
        [command, flag] if command == "queue" && flag == "--help" => "queue-help",
        [command, ..] if command == "queue" => "queue",
        _ => "unknown",
    };
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();

    let stdout = match mode {
        "version" => "codex-cli 1.2.3\n",
        "queue-help" => {
            "Queue a message for an existing session\n\nUsage: codex queue [OPTIONS] --thread <THREAD> --message <TEXT>\n\nOptions:\n      --config <PATH>    Use a named config file\n      --thread <THREAD>\n      --message <TEXT>\n"
        }
        "queue" => "Queued message\n",
        _ => "",
    };

    write_capture(&mode, &args, &input, stdout);

    match mode {
        "version" => {
            print!("{stdout}");
        }
        "queue-help" => {
            print!("{stdout}");
        }
        "queue" => {
            print!("{stdout}");
        }
        _ => process::exit(8),
    }
}
