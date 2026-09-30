use std::env;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;

fn escaped(value: &str) -> String {
    let mut s = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if c.is_control() => write!(s, "\\u{:04x}", c as u32).unwrap(),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}
fn array(v: &[String]) -> String {
    format!(
        "[{}]",
        v.iter().map(|x| escaped(x)).collect::<Vec<_>>().join(",")
    )
}
fn capture_path() -> PathBuf {
    env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .join("capture.ndjson")
}
fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let mode = if args == ["--version"] {
        "version"
    } else if args == ["--help"] {
        "help"
    } else if args.first().map(String::as_str) == Some("--safe-mode") {
        "print"
    } else {
        "unknown"
    };
    let scenario =
        fs::read_to_string(PathBuf::from(env::var_os("HOME").unwrap()).join("scenario.txt"))
            .unwrap_or_else(|_| "object".into());
    let prompt = args
        .iter()
        .position(|x| x == "--print")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_default();
    let ack = prompt
        .strip_prefix("Reply with exactly this acknowledgement and nothing else: ")
        .unwrap_or("");
    let stdout=match mode{"version"=>"2.1.236 (Claude Code)\n".into(),"help"=>"--safe-mode\n--print\n--output-format\n--tools\n--disallowedTools\n--no-session-persistence\n--permission-mode\n".into(),"print"=>match scenario.trim(){"array"=>format!("[{{\"type\":\"assistant\"}},{{\"type\":\"result\",\"is_error\":false,\"result\":{}}}]",escaped(ack)),"api-error"=>"{\"type\":\"result\",\"is_error\":true,\"result\":\"OAuth token revoked\"}".into(),"mismatch"=>"{\"type\":\"result\",\"result\":\"wrong\"}".into(),"tool"=>format!("[{{\"type\":\"assistant\",\"tool_use\":{{}}}},{{\"type\":\"result\",\"result\":{}}}]",escaped(ack)),"overflow"=>"x".repeat((1<<16)+1),"trailing"=>format!("{{\"type\":\"result\",\"result\":{}}} trailing",escaped(ack)),_=>format!("{{\"type\":\"result\",\"is_error\":false,\"result\":{}}}",escaped(ack))},_=>String::new()};
    let mut vars = env::vars().collect::<Vec<_>>();
    vars.sort();
    let env_json = format!(
        "[{}]",
        vars.iter()
            .map(|(k, v)| format!("[{},{}]", escaped(k), escaped(v)))
            .collect::<Vec<_>>()
            .join(",")
    );
    let cwd = env::current_dir().unwrap();
    let record = format!(
        "{{\"mode\":{},\"argv\":{},\"env\":{},\"cwd\":{},\"stdin\":{}}}",
        escaped(mode),
        array(&args),
        env_json,
        escaped(&cwd.to_string_lossy()),
        escaped(&input)
    );
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture_path())
        .unwrap();
    writeln!(f, "{record}").unwrap();
    match scenario.trim() {
        "stderr" if mode == "print" => eprint!("unexpected stderr"),
        "nonzero" if mode == "print" => process::exit(23),
        _ => print!("{stdout}"),
    };
    if mode == "unknown" {
        process::exit(8)
    }
}
