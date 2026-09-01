use std::collections::HashMap;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

#[derive(Clone, Debug)]
enum Emit {
    None,
    Hex(Vec<u8>),
    Repeat { byte: u8, count: usize },
    Sleep(u64),
}

impl Emit {
    fn parse(value: &str) -> Self {
        if value == "none" {
            return Self::None;
        }
        if let Some(rest) = value.strip_prefix("hex:") {
            return Self::Hex(hex_decode(rest));
        }
        if let Some(rest) = value.strip_prefix("repeat:") {
            let (count, byte) = rest
                .split_once(':')
                .expect("repeat specification must include count and byte");
            let count = count.parse::<usize>().expect("repeat count");
            let byte = byte.as_bytes().first().copied().expect("repeat byte");
            return Self::Repeat { byte, count };
        }
        if let Some(rest) = value.strip_prefix("sleep:") {
            return Self::Sleep(rest.parse::<u64>().expect("sleep duration"));
        }
        panic!("unsupported emit spec: {value}");
    }
}

#[allow(dead_code)]
fn hex_encode(bytes: &[u8]) -> String {
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(rendered, "{byte:02x}").unwrap();
    }
    rendered
}

fn hex_decode(value: &str) -> Vec<u8> {
    assert_eq!(value.len() % 2, 0, "hex value must have an even length");
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let raw = value.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        let hi = (raw[index] as char).to_digit(16).expect("hex high nibble");
        let lo = (raw[index + 1] as char)
            .to_digit(16)
            .expect("hex low nibble");
        bytes.push(((hi << 4) | lo) as u8);
        index += 2;
    }
    bytes
}

fn capture_path() -> PathBuf {
    env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("capture.ndjson")
}

fn scenario_path() -> PathBuf {
    env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("scenario.txt")
}

fn json_string(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len() + 2);
    encoded.push('"');
    for ch in input.chars() {
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

fn load_scenario() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let text = fs::read_to_string(scenario_path()).expect("read scenario");
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').expect("scenario line");
        map.insert(key.to_owned(), value.to_owned());
    }
    map
}

fn emit<W: io::Write>(writer: &mut W, emit: &Emit, captured: &mut String) {
    match emit {
        Emit::None => {}
        Emit::Hex(bytes) => {
            writer.write_all(bytes).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
            captured.push_str(std::str::from_utf8(bytes).unwrap());
            captured.push('\n');
        }
        Emit::Repeat { byte, count } => {
            let bytes = vec![*byte; *count];
            writer.write_all(&bytes).unwrap();
            writer.write_all(b"\n").unwrap();
            writer.flush().unwrap();
            captured.push_str(&String::from_utf8(bytes).unwrap());
            captured.push('\n');
        }
        Emit::Sleep(ms) => thread::sleep(Duration::from_millis(*ms)),
    }
}

fn capture_record(
    mode: &str,
    argv: &[String],
    stdin: &str,
    stdout: &str,
    stderr: &str,
    extra: &[(&str, String)],
) {
    let mut env_vars = env::vars().collect::<Vec<_>>();
    env_vars.sort_by(|left, right| left.0.cmp(&right.0));
    let mut record = format!(
        "{{\"mode\":{},\"argv\":{},\"cwd\":{},\"env\":{},\"stdin\":{},\"stdout\":{},\"stderr\":{}",
        json_string(mode),
        json_array(argv),
        json_string(&env::current_dir().unwrap().to_string_lossy()),
        json_env(&env_vars),
        json_string(stdin),
        json_string(stdout),
        json_string(stderr),
    );
    for (key, value) in extra {
        write!(record, ",{}:{}", json_string(key), json_string(value)).unwrap();
    }
    record.push('}');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture_path())
        .unwrap();
    writeln!(file, "{record}").unwrap();
    file.flush().unwrap();
}

fn get_spec(map: &HashMap<String, String>, key: &str) -> Emit {
    Emit::parse(map.get(key).map(String::as_str).unwrap_or("none"))
}

fn command_exit(map: &HashMap<String, String>, prefix: &str) -> i32 {
    map.get(&format!("{prefix}.exit"))
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0)
}

fn command_stdout(map: &HashMap<String, String>, prefix: &str) -> Emit {
    get_spec(map, &format!("{prefix}.stdout"))
}

fn command_stderr(map: &HashMap<String, String>, prefix: &str) -> Emit {
    get_spec(map, &format!("{prefix}.stderr"))
}

fn schema_client_request(map: &HashMap<String, String>) -> Vec<u8> {
    match map.get("schema.client_request") {
        Some(value) if value.starts_with("hex:") => hex_decode(&value[4..]),
        Some(value) => value.as_bytes().to_vec(),
        None => b"{\"kind\":\"client-request\"}".to_vec(),
    }
}

fn schema_protocol(map: &HashMap<String, String>) -> Vec<u8> {
    match map.get("schema.protocol") {
        Some(value) if value.starts_with("hex:") => hex_decode(&value[4..]),
        Some(value) => value.as_bytes().to_vec(),
        None => b"{\"kind\":\"protocol-schema\"}".to_vec(),
    }
}

fn write_schema_files(out_dir: &Path, map: &HashMap<String, String>) {
    fs::create_dir_all(out_dir).unwrap();
    fs::write(out_dir.join("ClientRequest.json"), schema_client_request(map)).unwrap();
    fs::write(
        out_dir.join("codex_app_server_protocol.v2.schemas.json"),
        schema_protocol(map),
    )
    .unwrap();
}

fn run_version_or_help(mode: &str, map: &HashMap<String, String>, argv: &[String]) -> i32 {
    let mut stdin = String::new();
    io::stdin().read_to_string(&mut stdin).unwrap();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut captured_stdout = String::new();
    let mut captured_stderr = String::new();
    emit(&mut stdout, &command_stdout(map, mode), &mut captured_stdout);
    emit(&mut stderr, &command_stderr(map, mode), &mut captured_stderr);
    capture_record(mode, argv, &stdin, &captured_stdout, &captured_stderr, &[]);
    command_exit(map, mode)
}

fn run_schema(map: &HashMap<String, String>, argv: &[String], out_dir: &str) -> i32 {
    let mut stdin = String::new();
    io::stdin().read_to_string(&mut stdin).unwrap();
    let out_dir = PathBuf::from(out_dir);
    write_schema_files(&out_dir, map);
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut captured_stdout = String::new();
    let mut captured_stderr = String::new();
    emit(
        &mut stdout,
        &command_stdout(map, "schema"),
        &mut captured_stdout,
    );
    emit(
        &mut stderr,
        &command_stderr(map, "schema"),
        &mut captured_stderr,
    );
    capture_record(
        "schema",
        argv,
        &stdin,
        &captured_stdout,
        &captured_stderr,
        &[("out_dir", out_dir.to_string_lossy().into_owned())],
    );
    command_exit(map, "schema")
}

fn run_listen(map: &HashMap<String, String>, argv: &[String]) -> i32 {
    let mut stdin = io::stdin().lock();
    let mut line = String::new();
    let mut captured_stdin = String::new();
    let mut captured_stdout = String::new();
    let mut captured_stderr = String::new();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut stage = 0u8;
    let exit_code = map
        .get("listen.exit")
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(0);
    let exit_after_stage = map
        .get("listen.exit_after_stage")
        .and_then(|value| value.parse::<u8>().ok())
        .unwrap_or(0);
    let wait_for_eof = map
        .get("listen.wait_for_eof")
        .map(|value| value == "true")
        .unwrap_or(true);
    let post_count = map
        .get("listen.post_count")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    loop {
        line.clear();
        let read = stdin.read_line(&mut line).unwrap();
        if read == 0 {
            break;
        }
        captured_stdin.push_str(&line);
        if line.contains("\"method\":\"initialize\"") {
            stage = 1;
            emit(
                &mut stdout,
                &get_spec(map, "listen.response1"),
                &mut captured_stdout,
            );
            if exit_after_stage == stage {
                break;
            }
        } else if line.contains("\"method\":\"initialized\"") {
            continue;
        } else if line.contains("\"method\":\"thread/start\"") {
            stage = 2;
            emit(
                &mut stdout,
                &get_spec(map, "listen.response2"),
                &mut captured_stdout,
            );
            emit(
                &mut stdout,
                &get_spec(map, "listen.thread_started"),
                &mut captured_stdout,
            );
            emit(
                &mut stderr,
                &get_spec(map, "listen.stderr"),
                &mut captured_stderr,
            );
            if exit_after_stage == stage {
                break;
            }
        } else if line.contains("\"method\":\"turn/start\"") {
            stage = 3;
            emit(
                &mut stdout,
                &get_spec(map, "listen.response3"),
                &mut captured_stdout,
            );
            for index in 0..post_count {
                let key = format!("listen.post{index}");
                emit(&mut stdout, &get_spec(map, &key), &mut captured_stdout);
            }
            emit(
                &mut stderr,
                &get_spec(map, "listen.stderr"),
                &mut captured_stderr,
            );
            if exit_after_stage == stage {
                break;
            }
            if !wait_for_eof {
                break;
            }
        }
    }

    capture_record(
        "listen",
        argv,
        &captured_stdin,
        &captured_stdout,
        &captured_stderr,
        &[("stage", stage.to_string())],
    );
    exit_code
}

fn main() {
    let map = load_scenario();
    let args = env::args().skip(1).collect::<Vec<_>>();
    let exit_code = match args.as_slice() {
        [flag] if flag == "--version" => run_version_or_help("version", &map, &args),
        [command, flag] if command == "app-server" && flag == "--help" => {
            run_version_or_help("help", &map, &args)
        }
        [command, subcommand, flag, out]
            if command == "app-server" && subcommand == "generate-json-schema" && flag == "--out" =>
        {
            run_schema(&map, &args, out)
        }
        [command, flag, url] if command == "app-server" && flag == "--listen" && url == "stdio://" => {
            run_listen(&map, &args)
        }
        _ => {
            let mut stdin = String::new();
            io::stdin().read_to_string(&mut stdin).unwrap();
            capture_record("unknown", &args, &stdin, "", "", &[]);
            8
        }
    };
    process::exit(exit_code);
}
