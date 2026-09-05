//! Instructable fake of the OpenCode local HTTP server.
//!
//! Usage: `fake <scenario> <port-file> <capture-file> <body-file>`
//!
//! The listener binds an ephemeral loopback port and publishes it by writing
//! `<port-file>.tmp` and renaming it, so a reader never observes half a port
//! number. The first connection's full request bytes are written to
//! `<capture-file>`, then the scenario decides the answer:
//!
//! * `accept`   - 200 with `<body-file>` and an exact `Content-Length`.
//! * `reject`   - 400 with a short body.
//! * `fail`     - 503 with a short body.
//! * `truncate` - 200 declaring 64 more body bytes than it sends, then closes.
//! * `hang`     - reads the request and never answers.

use std::env;
use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

const HANG_SECONDS: u64 = 30;

fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let [scenario, port_file, capture_file, body_file] = arguments.as_slice() else {
        panic!("usage: fake <scenario> <port-file> <capture-file> <body-file>");
    };

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
    let port = listener
        .local_addr()
        .expect("read the bound address")
        .port();
    let staged = format!("{port_file}.tmp");
    fs::write(&staged, port.to_string()).expect("stage the bound port");
    fs::rename(&staged, port_file).expect("publish the bound port");

    let (mut stream, _) = listener.accept().expect("accept one connection");
    let request = read_request(&mut stream);
    fs::write(capture_file, &request).expect("capture the request");

    match scenario.as_str() {
        "accept" => {
            let body = fs::read(body_file).expect("read the acknowledgement body");
            let length = body.len();
            respond(&mut stream, "200 OK", &body, length);
        }
        "reject" => respond(
            &mut stream,
            "400 Bad Request",
            br#"{"error":"rejected"}"#,
            20,
        ),
        "fail" => respond(
            &mut stream,
            "503 Service Unavailable",
            br#"{"error":"unavailable"}"#,
            23,
        ),
        "truncate" => {
            let body = fs::read(body_file).expect("read the acknowledgement body");
            let declared = body.len() + 64;
            respond(&mut stream, "200 OK", &body, declared);
        }
        "hang" => thread::sleep(Duration::from_secs(HANG_SECONDS)),
        other => panic!("unsupported scenario: {other}"),
    }
}

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(end) = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break end;
        }
        let count = stream.read(&mut chunk).expect("read the request head");
        assert!(count > 0, "the client closed before its head completed");
        bytes.extend_from_slice(&chunk[..count]);
    };
    let length = content_length(&bytes[..head_end]);
    while bytes.len() < head_end + length {
        let count = stream.read(&mut chunk).expect("read the request body");
        assert!(count > 0, "the client closed before its body completed");
        bytes.extend_from_slice(&chunk[..count]);
    }
    bytes
}

fn content_length(head: &[u8]) -> usize {
    let head = String::from_utf8(head.to_vec()).expect("the request head must be UTF-8");
    for line in head.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value.trim().parse().expect("a numeric Content-Length");
        }
    }
    panic!("the client sent no Content-Length");
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8], declared: usize) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(head.as_bytes())
        .expect("write the response head");
    stream.write_all(body).expect("write the response body");
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}
