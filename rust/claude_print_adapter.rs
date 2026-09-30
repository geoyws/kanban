use crate::adapter_protocol::{
    AdapterRequest, AdapterResponse, decode_request, validate_request as validate_protocol_request,
};
use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write as _};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;

const HELP: &str = "kanban-claude-print-adapter --claude ABSOLUTE_PATH --home ABSOLUTE_PATH --cwd ABSOLUTE_PATH --required-version VERSION";
const MAX_STDIN_BYTES: usize = 1 << 20;
const MAX_OUTPUT_BYTES: usize = 1 << 16;
const MAX_ID_BYTES: usize = 128;
const CHILD_PATH: &str = "/usr/bin:/bin";
const CONSUMER_ID: &str = "claude.print";
const ACTION_ID: &str = "start-readonly-turn";
const HELP_MARKERS: [&str; 7] = [
    "--print",
    "--output-format",
    "--tools",
    "--disallowedTools",
    "--no-session-persistence",
    "--permission-mode",
    "--safe-mode",
];

pub(crate) fn entrypoint() -> Result<()> {
    match parse_outcome(std::env::args_os())? {
        Outcome::Help => println!("{HELP}"),
        Outcome::Version => println!("kanban-claude-print-adapter {}", env!("CARGO_PKG_VERSION")),
        Outcome::Args(args) => {
            let validated = validate_paths(&args)?;
            validate_version_probe(
                &run(&validated, &version_args(), "claude version probe")?,
                &validated.required_version,
            )?;
            validate_help_probe(&run(&validated, &help_args(), "claude help probe")?)?;
            let request = decode_request_from_stdin()?;
            let (prompt, acknowledgement) = render_prompt(&request)?;
            let output = run(&validated, &print_args(&prompt), "claude print invocation")?;
            validate_print_result(&output, &acknowledgement)?;
            io::stdout().write_all(&serde_json::to_vec(&response_for(&request))?)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Args {
    claude: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    required_version: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}
#[derive(Debug)]
struct Pinned {
    path: PathBuf,
    file: fs::File,
    identity: Identity,
    directory: bool,
    label: &'static str,
}
#[derive(Debug)]
struct Validated {
    claude: Pinned,
    home: Pinned,
    cwd: Pinned,
    required_version: String,
}

fn parse_outcome<I: IntoIterator<Item = OsString>>(args: I) -> Result<Outcome> {
    let tokens: Vec<_> = args.into_iter().skip(1).collect();
    if matches!(tokens.as_slice(), [v] if v == "--help") {
        return Ok(Outcome::Help);
    }
    if matches!(tokens.as_slice(), [v] if v == "--version") {
        return Ok(Outcome::Version);
    }
    let (mut claude, mut home, mut cwd, mut version) = (None, None, None, None);
    let mut i = 0;
    while i < tokens.len() {
        let flag = text(&tokens[i], "argument")?;
        if !flag.starts_with("--") {
            bail!("positional argument is not allowed: {flag}");
        }
        i += 1;
        let raw = tokens
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("missing value for {flag}"))?;
        let value = text(raw, "value")?;
        if value.starts_with("--") {
            bail!("missing value for {flag}");
        }
        match flag {
            "--claude" => once(&mut claude, absolute(value, flag)?, flag)?,
            "--home" => once(&mut home, absolute(value, flag)?, flag)?,
            "--cwd" => once(&mut cwd, absolute(value, flag)?, flag)?,
            "--required-version" => once(&mut version, required_version(value)?, flag)?,
            _ => bail!("unknown argument: {flag}"),
        }
        i += 1;
    }
    Ok(Outcome::Args(Args {
        claude: claude.ok_or_else(|| anyhow::anyhow!("missing required flag: --claude"))?,
        home: home.ok_or_else(|| anyhow::anyhow!("missing required flag: --home"))?,
        cwd: cwd.ok_or_else(|| anyhow::anyhow!("missing required flag: --cwd"))?,
        required_version: version
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --required-version"))?,
    }))
}
fn text<'a>(v: &'a OsStr, label: &str) -> Result<&'a str> {
    v.to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 {label}"))
}
fn once<T>(slot: &mut Option<T>, value: T, flag: &str) -> Result<()> {
    if slot.is_some() {
        bail!("argument repeated: {flag}");
    }
    *slot = Some(value);
    Ok(())
}
fn absolute(value: &str, flag: &str) -> Result<PathBuf> {
    let p = Path::new(value);
    if !p.is_absolute() {
        bail!("{flag} must be an absolute path");
    }
    Ok(p.into())
}
fn required_version(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 32
        || value.starts_with('.')
        || value.ends_with('.')
        || value.contains("..")
        || !value.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    {
        bail!("--required-version must be 1..=32 dot-separated ASCII digits");
    }
    Ok(value.into())
}
fn identity(m: &fs::Metadata) -> Identity {
    Identity {
        dev: m.dev(),
        ino: m.ino(),
        uid: m.uid(),
        mode: m.permissions().mode(),
    }
}
fn validate_ancestors(path: &Path, label: &str) -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    for p in path.ancestors().skip(1) {
        let m = fs::metadata(p)?;
        let uid = m.uid();
        let mode = m.permissions().mode();
        if !m.is_dir() || (uid != euid && uid != 0) || (mode & 0o022 != 0 && mode & 0o1000 == 0) {
            bail!("{label} ancestor is not trusted: {}", p.display());
        }
    }
    Ok(())
}
fn pin(path: &Path, directory: bool, label: &'static str) -> Result<Pinned> {
    let l = fs::symlink_metadata(path)?;
    if l.file_type().is_symlink() {
        bail!("{label} must not be a symlink");
    }
    let path = fs::canonicalize(path)?;
    let m = fs::metadata(&path)?;
    let euid = unsafe { libc::geteuid() };
    if m.is_dir() != directory || (!directory && !m.is_file()) {
        bail!("{label} has the wrong file type");
    }
    let mode = m.permissions().mode();
    if (!directory && (mode & 0o111 == 0 || mode & 0o022 != 0)) || (directory && mode & 0o077 != 0)
    {
        bail!("{label} permissions are not trusted");
    }
    if m.uid() != euid && m.uid() != 0 {
        bail!("{label} owner is not trusted");
    }
    validate_ancestors(&path, label)?;
    let file = fs::File::open(&path)?;
    let id = identity(&file.metadata()?);
    if id != identity(&m) {
        bail!("{label} identity changed");
    }
    Ok(Pinned {
        path,
        file,
        identity: id,
        directory,
        label,
    })
}
fn validate_empty_cwd(path: &Path, label: &str) -> Result<()> {
    if fs::read_dir(path)?.next().transpose()?.is_some() {
        bail!("{label} must be empty");
    }
    Ok(())
}
fn validate_paths(a: &Args) -> Result<Validated> {
    let cwd = pin(&a.cwd, true, "--cwd")?;
    validate_empty_cwd(&cwd.path, "--cwd")?;
    Ok(Validated {
        claude: pin(&a.claude, false, "--claude")?,
        home: pin(&a.home, true, "--home")?,
        cwd,
        required_version: a.required_version.clone(),
    })
}
fn revalidate(p: &Pinned) -> Result<()> {
    validate_ancestors(&p.path, p.label)?;
    let m = fs::metadata(&p.path)?;
    if m.is_dir() != p.directory
        || (!p.directory && !m.is_file())
        || identity(&m) != p.identity
        || identity(&p.file.metadata()?) != p.identity
    {
        bail!("{} identity is no longer trusted", p.label);
    }
    Ok(())
}

fn command(v: &Validated, args: &[OsString]) -> Command {
    let mut c = Command::new(&v.claude.path);
    c.current_dir(&v.cwd.path)
        .env_clear()
        .env("HOME", &v.home.path)
        .env("PATH", CHILD_PATH)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}
fn bounded<R: Read>(mut r: R) -> Result<(Vec<u8>, bool)> {
    let mut out = Vec::new();
    let mut buf = [0; 8192];
    let mut overflow = false;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let take = n.min(MAX_OUTPUT_BYTES.saturating_sub(out.len()));
        out.extend_from_slice(&buf[..take]);
        overflow |= take < n;
    }
    Ok((out, overflow))
}
fn capture(mut child: std::process::Child, label: &str) -> Result<Output> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stderr"))?;
    let a = thread::spawn(move || bounded(stdout));
    let b = thread::spawn(move || bounded(stderr));
    let status = child.wait()?;
    let (stdout, ao) = a
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stdout capture panicked"))??;
    let (stderr, bo) = b
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stderr capture panicked"))??;
    if ao || bo {
        bail!("{label} output exceeds {MAX_OUTPUT_BYTES} bytes");
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
fn run(v: &Validated, args: &[OsString], label: &str) -> Result<Output> {
    revalidate(&v.claude)?;
    revalidate(&v.home)?;
    revalidate(&v.cwd)?;
    validate_empty_cwd(&v.cwd.path, "cwd")?;
    capture(command(v, args).spawn()?, label)
}
fn version_args() -> Vec<OsString> {
    vec!["--version".into()]
}
fn help_args() -> Vec<OsString> {
    vec!["--help".into()]
}
fn print_args(prompt: &str) -> Vec<OsString> {
    vec![
        "--safe-mode".into(),
        "--print".into(),
        prompt.into(),
        "--output-format".into(),
        "json".into(),
        "--tools".into(),
        "".into(),
        "--disallowedTools".into(),
        "mcp__*".into(),
        "--no-session-persistence".into(),
        "--permission-mode".into(),
        "dontAsk".into(),
    ]
}
fn clean<'a>(output: &'a Output, label: &str) -> Result<&'a str> {
    if !output.status.success() {
        bail!("{label} failed");
    }
    if !output.stderr.is_empty() {
        bail!("{label} wrote to stderr");
    }
    std::str::from_utf8(&output.stdout).map_err(|_| anyhow::anyhow!("{label} stdout must be UTF-8"))
}
fn validate_version_probe(o: &Output, v: &str) -> Result<()> {
    if clean(o, "claude version probe")?.trim() != format!("{v} (Claude Code)") {
        bail!("claude version probe returned an unexpected version");
    }
    Ok(())
}
fn validate_help_probe(o: &Output) -> Result<()> {
    let s = clean(o, "claude help probe")?;
    if HELP_MARKERS.iter().any(|m| s.matches(m).count() != 1) {
        bail!("claude help probe returned an unexpected help layout");
    }
    Ok(())
}
fn identifier(v: &str, label: &str) -> Result<()> {
    if v.is_empty()
        || v.len() > MAX_ID_BYTES
        || !v
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        bail!("{label} must be a bounded ASCII identifier");
    }
    Ok(())
}
fn render_prompt(r: &AdapterRequest) -> Result<(String, String)> {
    identifier(&r.delivery.subscription_id, "subscription ID")?;
    identifier(&r.delivery.event_id, "event ID")?;
    let ack = format!("ACK {}:{}", r.delivery.subscription_id, r.delivery.event_id);
    Ok((
        format!("Reply with exactly this acknowledgement and nothing else: {ack}"),
        ack,
    ))
}
fn has_tool_evidence(v: &Value) -> bool {
    match v {
        Value::Object(o) => {
            o.get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| matches!(t, "tool_use" | "tool_result" | "tool_call"))
                || o.iter().any(|(k, v)| {
                    matches!(
                        k.as_str(),
                        "tool_use"
                            | "tool_result"
                            | "tool_calls"
                            | "toolUse"
                            | "toolResult"
                            | "toolCalls"
                    ) || has_tool_evidence(v)
                })
        }
        Value::Array(a) => a.iter().any(has_tool_evidence),
        _ => false,
    }
}
fn has_failure_evidence(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.iter().any(|(k, v)| {
            (matches!(k.as_str(), "error" | "apiError") && !v.is_null())
                || (k == "is_error" && v.as_bool() == Some(true))
                || has_failure_evidence(v)
        }),
        Value::Array(a) => a.iter().any(has_failure_evidence),
        _ => false,
    }
}
fn validate_result_object(v: &Value, ack: &str) -> Result<()> {
    let o = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("claude result must be an object"))?;
    if o.get("type").and_then(Value::as_str) != Some("result")
        || has_failure_evidence(v)
        || has_tool_evidence(v)
        || o.get("result").and_then(Value::as_str) != Some(ack)
    {
        bail!("claude result did not contain the exact acknowledgement");
    }
    Ok(())
}
fn validate_print_result(o: &Output, ack: &str) -> Result<()> {
    let s = clean(o, "claude print invocation")?;
    let mut de = serde_json::Deserializer::from_str(s);
    let value = Value::deserialize(&mut de)?;
    de.end()?;
    match &value {
        Value::Object(_) => validate_result_object(&value, ack),
        Value::Array(a) => {
            if a.is_empty() || a.iter().any(has_tool_evidence) || a.iter().any(has_failure_evidence)
            {
                bail!("claude result array is invalid");
            }
            validate_result_object(a.last().unwrap(), ack)
        }
        _ => bail!("claude result must be an object or array"),
    }
}
fn decode_request_from_stdin() -> Result<AdapterRequest> {
    let mut b = Vec::new();
    io::stdin()
        .take((MAX_STDIN_BYTES + 1) as u64)
        .read_to_end(&mut b)?;
    if b.len() > MAX_STDIN_BYTES {
        bail!("adapter request exceeds {MAX_STDIN_BYTES} bytes");
    }
    let r = decode_request(&b)?;
    validate_protocol_request(&r)?;
    if r.target.consumer_id != CONSUMER_ID || r.target.action_id != ACTION_ID {
        bail!("adapter request target mismatch");
    }
    Ok(r)
}
fn response_for(r: &AdapterRequest) -> AdapterResponse {
    AdapterResponse {
        protocol_version: 1,
        subscription_id: r.delivery.subscription_id.clone(),
        event_id: r.delivery.event_id.clone(),
        created_at: r.delivery.created_at,
        replay: r.delivery.attempt > 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT_TEMP: AtomicUsize = AtomicUsize::new(0);
    fn trusted_paths() -> (PathBuf, Args) {
        let root = std::env::temp_dir().join(format!(
            "kanban-claude-path-test-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        let cwd = root.join("cwd");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir(&cwd).unwrap();
        for path in [&root, &home, &cwd] {
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        let claude = root.join("claude");
        fs::write(&claude, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&claude).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&claude, permissions).unwrap();
        (
            root,
            Args {
                claude,
                home,
                cwd,
                required_version: "2.1.236".into(),
            },
        )
    }
    fn output(s: &str, e: &str, ok: bool) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if ok { 0 } else { 256 }),
            stdout: s.as_bytes().into(),
            stderr: e.as_bytes().into(),
        }
    }
    #[test]
    fn parser_accepts_exact_flags() {
        let Outcome::Args(a) = parse_outcome(
            [
                "p",
                "--claude",
                "/bin/x",
                "--home",
                "/tmp/h",
                "--cwd",
                "/tmp/c",
                "--required-version",
                "2.1.236",
            ]
            .map(Into::into),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(a.required_version, "2.1.236");
    }
    #[test]
    fn parser_rejects_invalid_shapes_and_values() {
        for a in [
            vec!["p", "--claude", "x"],
            vec!["p", "--claude", "/x", "--claude", "/y"],
            vec!["p", "--required-version", "2.x"],
            vec!["p", "positional"],
            vec!["p", "--claude"],
            vec!["p", "--unknown", "value"],
            vec!["p", "--claude", "/x"],
        ] {
            assert!(parse_outcome(a.into_iter().map(Into::into)).is_err());
        }
    }
    #[test]
    fn trusted_paths_accept_private_inputs_and_reject_symlink_or_writable_home() {
        let (root, args) = trusted_paths();
        assert!(validate_paths(&args).is_ok());
        let link = root.join("claude-link");
        symlink(&args.claude, &link).unwrap();
        let mut linked = args.clone();
        linked.claude = link;
        assert!(validate_paths(&linked).is_err());
        let mut permissions = fs::metadata(&args.home).unwrap().permissions();
        permissions.set_mode(0o770);
        fs::set_permissions(&args.home, permissions).unwrap();
        assert!(validate_paths(&args).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn trusted_paths_reject_wrong_executable_type_mode_and_writable_ancestor() {
        let (root, args) = trusted_paths();
        fs::remove_file(&args.claude).unwrap();
        fs::create_dir(&args.claude).unwrap();
        assert!(validate_paths(&args).is_err());
        fs::remove_dir(&args.claude).unwrap();
        fs::write(&args.claude, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&args.claude, permissions).unwrap();
        assert!(validate_paths(&args).is_err());
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o720);
        fs::set_permissions(&args.claude, permissions).unwrap();
        assert!(validate_paths(&args).is_err());
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&args.claude, permissions).unwrap();
        let mut ancestor_permissions = fs::metadata(&root).unwrap().permissions();
        ancestor_permissions.set_mode(0o770);
        fs::set_permissions(&root, ancestor_permissions).unwrap();
        assert!(validate_paths(&args).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn pinned_identity_rejects_mode_and_inode_drift() {
        let (root, args) = trusted_paths();
        let validated = validate_paths(&args).unwrap();
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&args.claude, permissions).unwrap();
        assert!(revalidate(&validated.claude).is_err());
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&args.claude, permissions).unwrap();
        fs::remove_file(&args.claude).unwrap();
        fs::write(&args.claude, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&args.claude).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&args.claude, permissions).unwrap();
        assert!(revalidate(&validated.claude).is_err());
        drop(validated);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn cwd_must_stay_empty() {
        let (root, args) = trusted_paths();
        fs::write(args.cwd.join("instruction.md"), "untrusted instruction").unwrap();
        assert!(validate_paths(&args).is_err());
        fs::remove_file(args.cwd.join("instruction.md")).unwrap();
        let validated = validate_paths(&args).unwrap();
        fs::write(args.cwd.join("instruction.md"), "late instruction").unwrap();
        assert!(validate_empty_cwd(&validated.cwd.path, "cwd").is_err());
        drop(validated);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn probes_are_exact() {
        assert!(
            validate_version_probe(&output("2.1.236 (Claude Code)\n", "", true), "2.1.236").is_ok()
        );
        assert!(validate_version_probe(&output("2.1.236\n", "", true), "2.1.236").is_err());
        let h = HELP_MARKERS.join("\n");
        assert!(validate_help_probe(&output(&h, "", true)).is_ok());
        assert!(validate_help_probe(&output(&format!("{h}\n--print"), "", true)).is_err());
    }
    #[test]
    fn result_accepts_object_and_final_array_only() {
        let ack = "ACK s:e";
        assert!(
            validate_print_result(
                &output(
                    &format!(r#"{{"type":"result","result":"{ack}","is_error":false}}"#),
                    "",
                    true
                ),
                ack
            )
            .is_ok()
        );
        assert!(
            validate_print_result(
                &output(
                    &format!(r#"[{{"type":"assistant"}},{{"type":"result","result":"{ack}"}}]"#),
                    "",
                    true
                ),
                ack
            )
            .is_ok()
        );
    }
    #[test]
    fn result_rejects_errors_mismatch_tools_trailing_and_process_failures() {
        let ack = "ACK s:e";
        for s in [
            r#"{"type":"result","result":"wrong"}"#,
            r#"{"type":"result","result":"ACK s:e","is_error":true}"#,
            r#"[{"tool_use":{}},{"type":"result","result":"ACK s:e"}]"#,
            r#"{"type":"result","result":"ACK s:e"} trailing"#,
            r#"42"#,
            r#"[]"#,
            r#"[{"nested":{"apiError":"revoked"}},{"type":"result","result":"ACK s:e"}]"#,
            r#"[{"nested":{"toolCalls":[]}},{"type":"result","result":"ACK s:e"}]"#,
        ] {
            assert!(
                validate_print_result(&output(s, "", true), ack).is_err(),
                "{s}"
            );
        }
        assert!(validate_print_result(&output("{}", "revoked", true), ack).is_err());
        assert!(validate_print_result(&output("{}", "", false), ack).is_err());
    }
    #[test]
    fn bounded_ids_and_prompt_exclude_event_body() {
        assert!(identifier(&"a".repeat(128), "id").is_ok());
        assert!(identifier(&"a".repeat(129), "id").is_err());
    }
    #[test]
    fn response_marks_only_retries_as_replay() {
        let request = AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-test".into(),
                event_id: "a".repeat(64),
                attempt: 1,
                created_at: 123,
            },
            target: AdapterTarget {
                consumer_id: CONSUMER_ID.into(),
                action_id: ACTION_ID.into(),
            },
            event: json!({
                "eventID": "a".repeat(64),
                "eventHash": "a".repeat(64),
                "timestamp": 123
            }),
        };
        assert!(!response_for(&request).replay);
    }
    #[test]
    fn capture_limit_boundary() {
        assert!(
            !bounded(std::io::Cursor::new(vec![b'x'; MAX_OUTPUT_BYTES]))
                .unwrap()
                .1
        );
        assert!(
            bounded(std::io::Cursor::new(vec![b'x'; MAX_OUTPUT_BYTES + 1]))
                .unwrap()
                .1
        );
    }
}
