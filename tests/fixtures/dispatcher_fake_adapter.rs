use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process;
use std::thread;
use std::time::Duration;

fn json_string(input: &str, key: &str) -> String {
    let marker = format!("\"{key}\":\"");
    let start = input.find(&marker).expect("request string field") + marker.len();
    let tail = &input[start..];
    tail[..tail.find('"').expect("request closing quote")].to_owned()
}

fn json_i64(input: &str, key: &str) -> i64 {
    let marker = format!("\"{key}\":");
    let start = input.find(&marker).expect("request integer field") + marker.len();
    let digits = input[start..]
        .chars()
        .take_while(|value| value.is_ascii_digit() || *value == '-')
        .collect::<String>();
    digits.parse().expect("request integer")
}

fn main() {
    let mut args = env::args().skip(1);
    let mode = args.next().expect("mode");
    let capture = args.next().expect("capture path");
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(capture)
        .unwrap();
    writeln!(
        file,
        "mode={mode} secret={} inherited={} request={input}",
        env::var_os("DISPATCH_TOKEN").is_some(),
        env::var_os("LEAK_ME").is_some()
    )
    .unwrap();
    file.flush().unwrap();

    match mode.as_str() {
        "success" => {
            let subscription = json_string(&input, "subscriptionID");
            let event = json_string(&input, "eventID");
            let created_at = json_i64(&input, "createdAt");
            print!(
                "{{\"protocolVersion\":1,\"subscriptionID\":\"{subscription}\",\"eventID\":\"{event}\",\"createdAt\":{created_at},\"replay\":false}}"
            );
        }
        "exit" => process::exit(7),
        "malformed" => print!("{{"),
        "mismatch" => {
            let subscription = json_string(&input, "subscriptionID");
            let created_at = json_i64(&input, "createdAt");
            print!(
                "{{\"protocolVersion\":1,\"subscriptionID\":\"{subscription}\",\"eventID\":\"{}\",\"createdAt\":{created_at},\"replay\":false}}",
                "0".repeat(64)
            );
        }
        "oversized" => {
            std::io::stdout().write_all(&vec![b'x'; (1 << 20) + 1]).unwrap();
        }
        "sleep" => thread::sleep(Duration::from_secs(30)),
        _ => process::exit(8),
    }
}
