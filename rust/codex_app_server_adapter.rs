use crate::adapter_process::{child_exited_unreaped, terminate_and_reap};
use crate::adapter_protocol::{AdapterRequest, AdapterResponse, decode_request};
use crate::audit;
use crate::codex_app_server_messages::{
    initialize_line, initialized_line, thread_start_line, turn_start_line,
};
use crate::codex_app_server_state::{StateMachine, Transition};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const HELP: &str = "kanban-codex-app-server-adapter --codex PATH --codex-home PATH --cwd PATH --required-version VER --client-request-sha256 HEX --protocol-schema-sha256 HEX --protocol-timeout-ms N";
const MAX_STDIN_BYTES: usize = 1 << 16;
const MAX_STREAM_BYTES: usize = 1 << 16;
const CLIENT_NAME: &str = "kanban-codex-app-server-adapter";
const CODEX_APP_SERVER_CONSUMER_ID: &str = "codex.app-server";
const START_READONLY_TURN_ACTION_ID: &str = "start-readonly-turn";
const APP_SERVER_HELP_USAGE: &str = "Usage: codex app-server";
const APP_SERVER_HELP_LISTEN: &str = "--listen <URL>";
const APP_SERVER_HELP_SCHEMA: &str = "generate-json-schema";
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 300_000;
const CLEANUP_WINDOW: Duration = Duration::from_secs(2);

static CANCELLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_CODEX_PGID: AtomicI32 = AtomicI32::new(0);
#[cfg(test)]
static SPAWN_REGISTRATION_MASK_OBSERVED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static CLEANUP_TRANSITION_SEAM_ENABLED: AtomicBool = AtomicBool::new(false);
#[cfg(test)]
static CLEANUP_SIGNAL_BEFORE_CLEAR_TARGET: AtomicI32 = AtomicI32::new(0);
#[cfg(test)]
static CLEANUP_SIGNAL_AFTER_CLEAR_TARGET: AtomicI32 = AtomicI32::new(-1);
#[cfg(test)]
static CLEANUP_RESERVED_AFTER_CLEAR_OBSERVED: AtomicBool = AtomicBool::new(false);

fn relay_signal_to_active() -> i32 {
    let pgid = ACTIVE_CODEX_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        // SAFETY: `kill` is async-signal-safe. The active id remains reserved
        // through process-group termination. Its exact owner clears it before
        // final reap, so the numeric group cannot be recycled while visible.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    pgid
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
    relay_signal_to_active();
}

fn install_signal_handlers() -> Result<()> {
    // SAFETY: sigaction is fully initialized before installation. The handler
    // performs only lock-free atomic operations and async-signal-safe kill(2).
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(io::Error::last_os_error()).context("initialize adapter signals");
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("install adapter signal {signal}"));
            }
        }
    }
    Ok(())
}

fn check_cancelled() -> Result<()> {
    if CANCELLED.load(Ordering::SeqCst) {
        bail!("adapter cancelled");
    }
    Ok(())
}

fn termination_signal_set() -> io::Result<libc::sigset_t> {
    // SAFETY: the set is initialized before it is returned, and sigaddset
    // receives only the supported SIGINT/SIGTERM constants.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0
            || libc::sigaddset(&mut set, libc::SIGINT) != 0
            || libc::sigaddset(&mut set, libc::SIGTERM) != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(set)
    }
}

struct TerminationSignalMask {
    previous: libc::sigset_t,
    restored: bool,
}

impl TerminationSignalMask {
    fn block() -> io::Result<Self> {
        let set = termination_signal_set()?;
        // SAFETY: pthread_sigmask receives initialized sets owned by this
        // thread. The previous mask is captured for exact restoration.
        unsafe {
            let mut previous: libc::sigset_t = std::mem::zeroed();
            let result = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut previous);
            if result != 0 {
                return Err(io::Error::from_raw_os_error(result));
            }
            Ok(Self {
                previous,
                restored: false,
            })
        }
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.restored {
            return Ok(());
        }
        // SAFETY: previous is the exact mask captured by block() for this
        // thread and remains initialized for the guard's lifetime.
        let result = unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut())
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminationSignalMask {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn termination_signals_blocked() -> io::Result<bool> {
    // Supplying a null set queries this thread's current mask without changing
    // it on both macOS and Linux.
    unsafe {
        let mut current: libc::sigset_t = std::mem::zeroed();
        let result = libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut current);
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        Ok(libc::sigismember(&current, libc::SIGINT) == 1
            && libc::sigismember(&current, libc::SIGTERM) == 1)
    }
}

fn reset_child_termination_signals() -> io::Result<()> {
    let set = termination_signal_set()?;
    // SAFETY: this runs after fork and before exec. sigaction and
    // pthread_sigmask are async-signal-safe and receive initialized values.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0
            || libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0
        {
            return Err(io::Error::last_os_error());
        }
        if libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn signal_codex_group(pid: i32, signal: i32) -> io::Result<bool> {
    loop {
        // SAFETY: pid is the positive process-group leader owned by the
        // ActiveCodexChild, and no pointer crosses the FFI boundary.
        let result = unsafe { libc::kill(-pid, signal) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ESRCH) => return Ok(false),
            _ => return Err(error),
        }
    }
}

fn child_exited_reserved(pid: u32) -> io::Result<bool> {
    loop {
        match child_exited_unreaped(pid) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn wait_child_retry(child: &mut Child) -> io::Result<std::process::ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

struct ActiveCodexChild {
    child: Child,
    pid: i32,
    reaped: bool,
}

impl ActiveCodexChild {
    fn new(mut child: Child) -> Result<Self> {
        let pid = match i32::try_from(child.id()) {
            Ok(pid) if pid > 0 => pid,
            _ => {
                let _ = terminate_and_reap(&mut child);
                bail!("codex child pid exceeds i32");
            }
        };
        if ACTIVE_CODEX_PGID
            .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            let _ = terminate_and_reap(&mut child);
            bail!("another codex process group is already active");
        }
        let mut active = Self {
            child,
            pid,
            reaped: false,
        };
        if let Err(error) = check_cancelled() {
            active.terminate_and_reap();
            return Err(error);
        }
        Ok(active)
    }

    fn child(&self) -> &Child {
        &self.child
    }

    fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn deactivate(&self) {
        let _ = ACTIVE_CODEX_PGID.compare_exchange(self.pid, 0, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn force_kill_clear_reap(&mut self) -> Option<std::process::ExitStatus> {
        // Observation failed, so do not risk a wait that reaps before the
        // active owner is cleared. First make continued execution impossible,
        // then clear the exact owner, then reap.
        let _ = signal_codex_group(self.pid, libc::SIGKILL);
        let _ = self.child.kill();
        self.deactivate();
        let _ = wait_child_retry(&mut self.child);
        self.reaped = true;
        None
    }

    fn finish_reserved_exit(
        &mut self,
        signal_owned_outcome: bool,
    ) -> Option<std::process::ExitStatus> {
        // The WNOWAIT observation keeps the leader PID reserved. Kill the
        // complete group one final time before ending global ownership so no
        // descendant can escape between the ownership clear and final reap.
        let _ = signal_codex_group(self.pid, libc::SIGKILL);

        #[cfg(test)]
        if CLEANUP_TRANSITION_SEAM_ENABLED.load(Ordering::SeqCst) {
            CLEANUP_SIGNAL_BEFORE_CLEAR_TARGET.store(relay_signal_to_active(), Ordering::SeqCst);
        }

        self.deactivate();

        #[cfg(test)]
        if CLEANUP_TRANSITION_SEAM_ENABLED.load(Ordering::SeqCst) {
            CLEANUP_SIGNAL_AFTER_CLEAR_TARGET.store(relay_signal_to_active(), Ordering::SeqCst);
            CLEANUP_RESERVED_AFTER_CLEAR_OBSERVED.store(
                child_exited_reserved(self.child.id()).unwrap_or(false),
                Ordering::SeqCst,
            );
        }

        let status = wait_child_retry(&mut self.child).ok();
        self.reaped = true;
        if signal_owned_outcome { None } else { status }
    }

    fn terminate_and_reap(&mut self) -> Option<std::process::ExitStatus> {
        if self.reaped {
            return None;
        }
        match child_exited_reserved(self.child.id()) {
            Ok(true) => return self.finish_reserved_exit(false),
            Ok(false) => {}
            Err(_) => return self.force_kill_clear_reap(),
        }

        let term_sent = match signal_codex_group(self.pid, libc::SIGTERM) {
            Ok(sent) => sent,
            Err(_) => return self.force_kill_clear_reap(),
        };
        if !term_sent {
            match child_exited_reserved(self.child.id()) {
                Ok(true) => return self.finish_reserved_exit(false),
                Ok(false) => {}
                Err(_) => return self.force_kill_clear_reap(),
            }
        }

        if term_sent {
            let deadline = Instant::now() + Duration::from_millis(200);
            while Instant::now() < deadline {
                match child_exited_reserved(self.child.id()) {
                    Ok(true) => return self.finish_reserved_exit(true),
                    Ok(false) => {}
                    Err(_) => return self.force_kill_clear_reap(),
                }
                thread::sleep(Duration::from_millis(10));
            }
        }

        // Escalation owns the outcome. Keep observing with WNOWAIT after
        // SIGKILL so the leader remains reserved until final group kill and
        // ownership clear are complete.
        let _ = signal_codex_group(self.pid, libc::SIGKILL);
        let _ = self.child.kill();
        loop {
            match child_exited_reserved(self.child.id()) {
                Ok(true) => return self.finish_reserved_exit(true),
                Ok(false) => {}
                Err(_) => return self.force_kill_clear_reap(),
            }
            let _ = signal_codex_group(self.pid, libc::SIGKILL);
            let _ = self.child.kill();
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ActiveCodexChild {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.terminate_and_reap();
        } else {
            self.deactivate();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Args {
    codex: PathBuf,
    codex_home: PathBuf,
    cwd: PathBuf,
    required_version: String,
    client_request_sha256: String,
    protocol_schema_sha256: String,
    protocol_timeout_ms: u64,
}

#[derive(Debug)]
pub(crate) struct Validated {
    canonical_codex: PathBuf,
    canonical_codex_file: fs::File,
    canonical_codex_identity: FileIdentity,
    canonical_codex_home: PathBuf,
    canonical_codex_home_file: fs::File,
    canonical_codex_home_identity: FileIdentity,
    canonical_cwd: PathBuf,
    canonical_cwd_file: fs::File,
    canonical_cwd_identity: FileIdentity,
    required_version: String,
    client_request_sha256: String,
    protocol_schema_sha256: String,
    protocol_timeout_ms: u64,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
    mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Help,
    Version,
    Args(Args),
}

#[derive(Debug)]
enum StreamEvent {
    Line(Vec<u8>),
    Eof,
}

#[derive(Debug)]
struct TempSchemaDir {
    path: PathBuf,
    identity: FileIdentity,
    closed: bool,
}

impl TempSchemaDir {
    fn new() -> Result<Self> {
        let parent = std::env::temp_dir();
        let pid = std::process::id();
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        for _ in 0..256 {
            let candidate = parent.join(format!(
                "kanban-codex-app-server-schema-{pid}-{}",
                Uuid::new_v4()
            ));
            match builder.create(&candidate) {
                Ok(()) => {
                    validate_private_dir(&candidate)?;
                    let identity = file_identity(&fs::symlink_metadata(&candidate)?);
                    return Ok(Self {
                        path: candidate,
                        identity,
                        closed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("failed to allocate a private schema output directory")
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let metadata = fs::symlink_metadata(&self.path)?;
        if file_identity(&metadata) != self.identity {
            bail!("schema directory identity changed");
        }
        if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
            bail!("schema directory is no longer the private directory we created");
        }
        fs::remove_dir_all(&self.path)?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for TempSchemaDir {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

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
                "kanban-codex-app-server-adapter {}",
                env!("CARGO_PKG_VERSION")
            )?;
            Ok(())
        }
        Outcome::Args(args) => run(args),
    }
}

fn run(args: Args) -> Result<()> {
    install_signal_handlers()?;
    check_cancelled()?;
    let validated = validate_paths(&args)?;
    let deadline = Instant::now() + Duration::from_millis(validated.protocol_timeout_ms);
    probe_codex_version(&validated, deadline)?;
    probe_codex_app_server_help(&validated, deadline)?;
    verify_generated_schema(&validated, deadline)?;
    let request = decode_request_from_stdin()?;
    validate_request_target(&request)?;
    let response = drive_app_server(&validated, &request, deadline)?;
    let mut stdout = io::stdout();
    stdout.write_all(&render_response(&response)?)?;
    stdout.flush()?;
    Ok(())
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

    let mut codex = None;
    let mut codex_home = None;
    let mut cwd = None;
    let mut required_version = None;
    let mut client_request_sha256 = None;
    let mut protocol_schema_sha256 = None;
    let mut protocol_timeout_ms = None;

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
            "--codex" => assign_once(&mut codex, parse_absolute_path(value, "--codex")?, flag)?,
            "--codex-home" => assign_once(
                &mut codex_home,
                parse_absolute_path(value, "--codex-home")?,
                flag,
            )?,
            "--cwd" => assign_once(&mut cwd, parse_absolute_path(value, "--cwd")?, flag)?,
            "--required-version" => {
                assign_once(&mut required_version, parse_required_version(value)?, flag)?
            }
            "--client-request-sha256" => assign_once(
                &mut client_request_sha256,
                parse_lower_hex_64(value, "--client-request-sha256")?,
                flag,
            )?,
            "--protocol-schema-sha256" => assign_once(
                &mut protocol_schema_sha256,
                parse_lower_hex_64(value, "--protocol-schema-sha256")?,
                flag,
            )?,
            "--protocol-timeout-ms" => {
                assign_once(&mut protocol_timeout_ms, parse_timeout_ms(value)?, flag)?
            }
            _ => bail!("unknown argument: {flag}"),
        }
        index += 1;
    }

    Ok(Outcome::Args(Args {
        codex: codex.ok_or_else(|| anyhow::anyhow!("missing required flag: --codex"))?,
        codex_home: codex_home
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --codex-home"))?,
        cwd: cwd.ok_or_else(|| anyhow::anyhow!("missing required flag: --cwd"))?,
        required_version: required_version
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --required-version"))?,
        client_request_sha256: client_request_sha256
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --client-request-sha256"))?,
        protocol_schema_sha256: protocol_schema_sha256
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --protocol-schema-sha256"))?,
        protocol_timeout_ms: protocol_timeout_ms
            .ok_or_else(|| anyhow::anyhow!("missing required flag: --protocol-timeout-ms"))?,
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

fn parse_absolute_path(value: &str, flag: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if !path.is_absolute() {
        bail!("{flag} must be an absolute path");
    }
    Ok(path.to_path_buf())
}

fn parse_required_version(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > 32 {
        bail!("--required-version must be 1..=32 ASCII digits and dots");
    }
    if value.starts_with('.') || value.ends_with('.') || value.contains("..") {
        bail!("--required-version must not start, end, or repeat dots");
    }
    if !value.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        bail!("--required-version must contain only ASCII digits and dots");
    }
    Ok(value.to_owned())
}

fn parse_lower_hex_64(value: &str, flag: &str) -> Result<String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{flag} must be a lowercase 64-hex string");
    }
    Ok(value.to_owned())
}

fn parse_timeout_ms(value: &str) -> Result<u64> {
    let timeout_ms: u64 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("--protocol-timeout-ms must be an integer"))?;
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        bail!("--protocol-timeout-ms must be in {MIN_TIMEOUT_MS}..={MAX_TIMEOUT_MS}");
    }
    Ok(timeout_ms)
}

fn validate_private_dir(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!("private directory must not be a symlink");
    }
    if !metadata.file_type().is_dir() {
        bail!("private directory must be a directory");
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        bail!("private directory must not be accessible by group or others");
    }
    let uid = metadata.uid();
    let euid = unsafe { libc::geteuid() };
    if uid != euid && uid != 0 {
        bail!("private directory must be owned by the effective user or root");
    }
    Ok(())
}

pub(crate) fn validate_paths(args: &Args) -> Result<Validated> {
    let codex_lstat = fs::symlink_metadata(&args.codex)?;
    if !(codex_lstat.file_type().is_file() || codex_lstat.file_type().is_symlink()) {
        bail!("--codex must be a regular file or a symlink to one");
    }

    let canonical_codex = fs::canonicalize(&args.codex)?;
    let codex_stat = fs::metadata(&canonical_codex)?;
    if !codex_stat.file_type().is_file() {
        bail!("--codex must resolve to a regular file");
    }
    let codex_mode = codex_stat.permissions().mode();
    if codex_mode & 0o111 == 0 {
        bail!("--codex target must have at least one execute bit");
    }
    if codex_mode & 0o022 != 0 {
        bail!("--codex target must not be group- or other-writable");
    }
    let euid = unsafe { libc::geteuid() };
    if codex_stat.uid() != euid && codex_stat.uid() != 0 {
        bail!("--codex target must be owned by the effective user or root");
    }

    let codex_home_lstat = fs::symlink_metadata(&args.codex_home)?;
    if codex_home_lstat.file_type().is_symlink() {
        bail!("--codex-home must not be a symlink");
    }
    if !codex_home_lstat.file_type().is_dir() {
        bail!("--codex-home must be an existing directory");
    }
    let canonical_codex_home = fs::canonicalize(&args.codex_home)?;
    let codex_home_stat = fs::metadata(&canonical_codex_home)?;
    if !codex_home_stat.file_type().is_dir() {
        bail!("--codex-home must resolve to a directory");
    }
    if codex_home_stat.permissions().mode() & 0o077 != 0 {
        bail!("--codex-home must not be accessible by group or others");
    }
    if codex_home_stat.uid() != euid && codex_home_stat.uid() != 0 {
        bail!("--codex-home must be owned by the effective user or root");
    }

    let cwd_lstat = fs::symlink_metadata(&args.cwd)?;
    if cwd_lstat.file_type().is_symlink() {
        bail!("--cwd must not be a symlink");
    }
    if !cwd_lstat.file_type().is_dir() {
        bail!("--cwd must be an existing directory");
    }
    let canonical_cwd = fs::canonicalize(&args.cwd)?;
    let cwd_stat = fs::metadata(&canonical_cwd)?;
    if !cwd_stat.file_type().is_dir() {
        bail!("--cwd must resolve to a directory");
    }
    if cwd_stat.permissions().mode() & 0o077 != 0 {
        bail!("--cwd must not be accessible by group or others");
    }
    if cwd_stat.uid() != euid && cwd_stat.uid() != 0 {
        bail!("--cwd must be owned by the effective user or root");
    }

    let canonical_codex_file = fs::File::open(&canonical_codex)?;
    let canonical_codex_identity = file_identity(&canonical_codex_file.metadata()?);
    if canonical_codex_identity != file_identity(&codex_stat) {
        bail!("--codex must resolve to a regular file");
    }

    let canonical_codex_home_file = fs::File::open(&canonical_codex_home)?;
    let canonical_codex_home_identity = file_identity(&canonical_codex_home_file.metadata()?);
    if canonical_codex_home_identity != file_identity(&codex_home_stat) {
        bail!("--codex-home must resolve to a directory");
    }

    let canonical_cwd_file = fs::File::open(&canonical_cwd)?;
    let canonical_cwd_identity = file_identity(&canonical_cwd_file.metadata()?);
    if canonical_cwd_identity != file_identity(&cwd_stat) {
        bail!("--cwd must resolve to a directory");
    }

    let validated = Validated {
        canonical_codex,
        canonical_codex_file,
        canonical_codex_identity,
        canonical_codex_home,
        canonical_codex_home_file,
        canonical_codex_home_identity,
        canonical_cwd,
        canonical_cwd_file,
        canonical_cwd_identity,
        required_version: args.required_version.clone(),
        client_request_sha256: args.client_request_sha256.clone(),
        protocol_schema_sha256: args.protocol_schema_sha256.clone(),
        protocol_timeout_ms: args.protocol_timeout_ms,
    };

    validate_canonical_path_trust(&validated)?;
    Ok(validated)
}

fn validate_canonical_ancestor_chain(path: &Path, label: &str) -> Result<()> {
    let euid = unsafe { libc::geteuid() };
    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::metadata(ancestor)?;
        if !metadata.file_type().is_dir() {
            bail!(
                "{label} ancestor must be a directory: {}",
                ancestor.display()
            );
        }
        let uid = metadata.uid();
        if uid != euid && uid != 0 {
            bail!(
                "{label} ancestor must be owned by the effective user or root: {}",
                ancestor.display()
            );
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0 {
            let sticky_owned = mode & 0o1000 != 0 && (uid == euid || uid == 0);
            if !sticky_owned {
                bail!(
                    "{label} ancestor must not be group- or other-writable unless sticky and owned by the effective user or root: {}",
                    ancestor.display()
                );
            }
        }
    }
    Ok(())
}

fn validate_canonical_path_trust(validated: &Validated) -> Result<()> {
    validate_canonical_ancestor_chain(&validated.canonical_codex, "--codex")?;
    validate_canonical_ancestor_chain(&validated.canonical_codex_home, "--codex-home")?;
    validate_canonical_ancestor_chain(&validated.canonical_cwd, "--cwd")?;
    Ok(())
}

fn file_identity(metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        mode: metadata.permissions().mode(),
    }
}

fn validate_codex_identity(validated: &Validated) -> Result<()> {
    let codex_stat = fs::metadata(&validated.canonical_codex)?;
    if !codex_stat.file_type().is_file() {
        bail!("codex target is no longer trusted");
    }
    let mode = codex_stat.permissions().mode();
    if mode & 0o111 == 0 || mode & 0o022 != 0 {
        bail!("codex target is no longer trusted");
    }
    let euid = unsafe { libc::geteuid() };
    if codex_stat.uid() != euid && codex_stat.uid() != 0 {
        bail!("codex target is no longer trusted");
    }
    let pinned_identity = file_identity(&validated.canonical_codex_file.metadata()?);
    if pinned_identity != validated.canonical_codex_identity
        || file_identity(&codex_stat) != pinned_identity
    {
        bail!("codex target is no longer trusted");
    }
    Ok(())
}

fn validate_codex_home_identity(validated: &Validated) -> Result<()> {
    let codex_home_stat = fs::metadata(&validated.canonical_codex_home)?;
    if !codex_home_stat.file_type().is_dir() {
        bail!("codex home is no longer trusted");
    }
    if codex_home_stat.permissions().mode() & 0o077 != 0 {
        bail!("codex home is no longer trusted");
    }
    let euid = unsafe { libc::geteuid() };
    if codex_home_stat.uid() != euid && codex_home_stat.uid() != 0 {
        bail!("codex home is no longer trusted");
    }
    let pinned_identity = file_identity(&validated.canonical_codex_home_file.metadata()?);
    if pinned_identity != validated.canonical_codex_home_identity
        || file_identity(&codex_home_stat) != pinned_identity
    {
        bail!("codex home is no longer trusted");
    }
    Ok(())
}

fn validate_cwd_identity(validated: &Validated) -> Result<()> {
    let cwd_stat = fs::metadata(&validated.canonical_cwd)?;
    if !cwd_stat.file_type().is_dir() {
        bail!("cwd is no longer trusted");
    }
    if cwd_stat.permissions().mode() & 0o077 != 0 {
        bail!("cwd is no longer trusted");
    }
    let euid = unsafe { libc::geteuid() };
    if cwd_stat.uid() != euid && cwd_stat.uid() != 0 {
        bail!("cwd is no longer trusted");
    }
    let pinned_identity = file_identity(&validated.canonical_cwd_file.metadata()?);
    if pinned_identity != validated.canonical_cwd_identity
        || file_identity(&cwd_stat) != pinned_identity
    {
        bail!("cwd is no longer trusted");
    }
    Ok(())
}

fn validate_identities_before_spawn(validated: &Validated) -> Result<()> {
    validate_canonical_path_trust(validated)?;
    validate_codex_identity(validated)?;
    validate_codex_home_identity(validated)?;
    validate_cwd_identity(validated)?;
    Ok(())
}

fn codex_command(validated: &Validated, args: &[OsString]) -> Command {
    let mut command = Command::new(&validated.canonical_codex);
    command
        .current_dir(&validated.canonical_cwd)
        .env_clear()
        .env("CODEX_HOME", &validated.canonical_codex_home)
        .args(args)
        .process_group(0);
    // SAFETY: the hook invokes only async-signal-safe libc operations. It
    // resets inherited handlers and unblocks termination signals before exec.
    unsafe {
        command.pre_exec(reset_child_termination_signals);
    }
    command
}

fn spawn_active_codex(command: &mut Command) -> Result<ActiveCodexChild> {
    let mut mask = TerminationSignalMask::block().context("block adapter termination signals")?;
    let child = command.spawn()?;
    let mut child = ActiveCodexChild::new(child)?;
    let registration_masked = termination_signals_blocked()?;
    #[cfg(test)]
    SPAWN_REGISTRATION_MASK_OBSERVED.store(registration_masked, Ordering::SeqCst);
    if !registration_masked {
        child.terminate_and_reap();
        bail!("termination signals were not blocked during child registration");
    }
    mask.restore()
        .context("restore adapter termination signals")?;
    if let Err(error) = check_cancelled() {
        child.terminate_and_reap();
        return Err(error);
    }
    Ok(child)
}

fn capture_output(mut child: ActiveCodexChild, label: &str, deadline: Instant) -> Result<Output> {
    let stdout = child
        .child_mut()
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stdout pipe for {label}"))?;
    let stderr = child
        .child_mut()
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing stderr pipe for {label}"))?;

    let stdout_thread = thread::spawn(move || read_bounded_stream(stdout, MAX_STREAM_BYTES));
    let stderr_thread = thread::spawn(move || read_bounded_stream(stderr, MAX_STREAM_BYTES));

    enum Outcome {
        Exited(std::process::ExitStatus),
        TimedOut,
        Cancelled,
        WaitFailed,
    }

    let outcome = loop {
        match child_exited_unreaped(child.child().id()) {
            Ok(true) => {
                let status = child
                    .terminate_and_reap()
                    .ok_or_else(|| anyhow::anyhow!("{label} exited without a reaped status"))?;
                break Outcome::Exited(status);
            }
            Ok(false) => {}
            Err(_) => {
                let _ = child.terminate_and_reap();
                break Outcome::WaitFailed;
            }
        }
        if CANCELLED.load(Ordering::SeqCst) {
            let _ = child.terminate_and_reap();
            break Outcome::Cancelled;
        }
        if Instant::now() >= deadline {
            break match child.terminate_and_reap() {
                Some(status) => Outcome::Exited(status),
                None => Outcome::TimedOut,
            };
        }
        thread::sleep(Duration::from_millis(10));
    };

    let (stdout, stdout_overflowed) = stdout_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stdout capture panicked"))??;
    let (stderr, stderr_overflowed) = stderr_thread
        .join()
        .map_err(|_| anyhow::anyhow!("{label} stderr capture panicked"))??;

    if stdout_overflowed {
        bail!("{label} stdout exceeds {MAX_STREAM_BYTES} bytes");
    }
    if stderr_overflowed {
        bail!("{label} stderr exceeds {MAX_STREAM_BYTES} bytes");
    }

    match outcome {
        Outcome::Exited(status) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        Outcome::TimedOut => bail!("{label} timed out before the deadline"),
        Outcome::Cancelled => bail!("adapter cancelled"),
        Outcome::WaitFailed => bail!("{label} wait failed"),
    }
}

fn read_bounded_stream<R: io::Read>(mut reader: R, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(limit.min(8 * 1024));
    let mut overflowed = false;
    let mut scratch = [0u8; 8192];
    loop {
        let read = reader.read(&mut scratch)?;
        if read == 0 {
            break;
        }
        if bytes.len() < limit {
            let remaining = limit - bytes.len();
            let take = remaining.min(read);
            bytes.extend_from_slice(&scratch[..take]);
            if take < read {
                overflowed = true;
            }
        } else {
            overflowed = true;
        }
    }
    Ok((bytes, overflowed))
}

fn run_codex_command(
    validated: &Validated,
    args: &[OsString],
    label: &str,
    deadline: Instant,
) -> Result<Output> {
    check_cancelled()?;
    validate_identities_before_spawn(validated)?;
    let mut command = codex_command(validated, args);
    let child = spawn_active_codex(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    capture_output(child, label, deadline)
}

fn version_args() -> Vec<OsString> {
    vec![OsString::from("--version")]
}

fn app_server_help_args() -> Vec<OsString> {
    vec![OsString::from("app-server"), OsString::from("--help")]
}

fn schema_generation_args(out_dir: &Path) -> Vec<OsString> {
    vec![
        OsString::from("app-server"),
        OsString::from("generate-json-schema"),
        OsString::from("--out"),
        out_dir.as_os_str().to_os_string(),
    ]
}

fn app_server_args() -> Vec<OsString> {
    vec![
        OsString::from("app-server"),
        OsString::from("--listen"),
        OsString::from("stdio://"),
    ]
}

fn validate_version_probe(output: &Output, required_version: &str) -> Result<()> {
    if !output.status.success() {
        bail!("codex version probe failed");
    }
    if !output.stderr.is_empty() {
        bail!("codex version probe wrote to stderr");
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("codex version probe stdout must be UTF-8"))?;
    if stdout.trim() != format!("codex-cli {required_version}") {
        bail!("codex version probe returned an unexpected version");
    }
    Ok(())
}

fn validate_app_server_help_probe(output: &Output) -> Result<()> {
    if !output.status.success() {
        bail!("codex app-server help probe failed");
    }
    if !output.stderr.is_empty() {
        bail!("codex app-server help probe wrote to stderr");
    }
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| anyhow::anyhow!("codex app-server help probe stdout must be UTF-8"))?;
    if !stdout.contains(APP_SERVER_HELP_USAGE) {
        bail!("codex app-server help probe did not expose the app-server usage");
    }
    if count_occurrences(stdout, APP_SERVER_HELP_LISTEN) != 1 {
        bail!("codex app-server help probe must mention --listen <URL> exactly once");
    }
    if count_occurrences(stdout, APP_SERVER_HELP_SCHEMA) != 1 {
        bail!("codex app-server help probe must mention generate-json-schema exactly once");
    }
    Ok(())
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.match_indices(needle).count()
}

fn probe_codex_version(validated: &Validated, deadline: Instant) -> Result<()> {
    let output = run_codex_command(validated, &version_args(), "codex version probe", deadline)?;
    validate_version_probe(&output, &validated.required_version)
}

fn probe_codex_app_server_help(validated: &Validated, deadline: Instant) -> Result<()> {
    let output = run_codex_command(
        validated,
        &app_server_help_args(),
        "codex app-server help probe",
        deadline,
    )?;
    validate_app_server_help_probe(&output)
}

fn verify_generated_schema(validated: &Validated, deadline: Instant) -> Result<()> {
    let mut temp = TempSchemaDir::new()?;
    let out_dir = temp.path().to_path_buf();
    let output = run_codex_command(
        validated,
        &schema_generation_args(&out_dir),
        "codex app-server schema generation",
        deadline,
    )?;
    if !output.status.success() {
        bail!("codex app-server schema generation failed");
    }
    if !output.stderr.is_empty() {
        bail!("codex app-server schema generation wrote to stderr");
    }
    let client = find_schema_file(temp.path(), "ClientRequest.json")?;
    let protocol = find_schema_file(temp.path(), "codex_app_server_protocol.v2.schemas.json")?;
    let client_sha = audit::file_sha256(&client)?;
    if client_sha != validated.client_request_sha256 {
        bail!("ClientRequest.json sha256 does not match");
    }
    let protocol_sha = audit::file_sha256(&protocol)?;
    if protocol_sha != validated.protocol_schema_sha256 {
        bail!("codex_app_server_protocol.v2.schemas.json sha256 does not match");
    }
    temp.cleanup()?;
    Ok(())
}

fn find_schema_file(root: &Path, name: &str) -> Result<PathBuf> {
    let mut matches = Vec::new();
    collect_schema_files(root, name, &mut matches)?;
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("missing schema file: {name}"),
        _ => bail!("duplicate schema file: {name}"),
    }
}

fn collect_schema_files(root: &Path, name: &str, matches: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            bail!("schema output must not contain symlinks");
        }
        if file_type.is_dir() {
            collect_schema_files(&path, name, matches)?;
            continue;
        }
        if !file_type.is_file() {
            bail!("schema output must contain regular files only");
        }
        if entry.file_name() == OsStr::new(name) {
            validate_schema_file(&path)?;
            matches.push(path);
        }
    }
    Ok(())
}

fn validate_schema_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("schema file must be a regular file");
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        bail!("schema file must not be group- or other-writable");
    }
    let euid = unsafe { libc::geteuid() };
    if metadata.uid() != euid && metadata.uid() != 0 {
        bail!("schema file must be owned by the effective user or root");
    }
    Ok(())
}

fn decode_request_bytes(bytes: &[u8]) -> Result<AdapterRequest> {
    if bytes.len() > MAX_STDIN_BYTES {
        bail!("adapter request exceeds {MAX_STDIN_BYTES} bytes");
    }
    let request = decode_request(bytes)?;
    validate_request_target(&request)?;
    Ok(request)
}

fn read_bounded_stdin(limit: usize) -> Result<Vec<u8>> {
    let mut stdin = io::stdin().lock().take((limit + 1) as u64);
    let mut bytes = Vec::new();
    stdin.read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("adapter request exceeds {limit} bytes");
    }
    Ok(bytes)
}

fn decode_request_from_stdin() -> Result<AdapterRequest> {
    let bytes = read_bounded_stdin(MAX_STDIN_BYTES)?;
    decode_request_bytes(&bytes)
}

fn validate_request_target(request: &AdapterRequest) -> Result<()> {
    if request.target.consumer_id != CODEX_APP_SERVER_CONSUMER_ID {
        bail!(
            "adapter target consumer ID must be {}",
            CODEX_APP_SERVER_CONSUMER_ID
        );
    }
    if request.target.action_id != START_READONLY_TURN_ACTION_ID {
        bail!(
            "adapter target action ID must be {}",
            START_READONLY_TURN_ACTION_ID
        );
    }
    Ok(())
}

fn response_for(request: &AdapterRequest) -> AdapterResponse {
    AdapterResponse {
        protocol_version: request.protocol_version,
        subscription_id: request.delivery.subscription_id.clone(),
        event_id: request.delivery.event_id.clone(),
        created_at: request.delivery.created_at,
        replay: request.delivery.attempt > 1,
    }
}

fn render_response(response: &AdapterResponse) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(response)?;
    if bytes.len() > MAX_STREAM_BYTES {
        bail!("adapter response exceeds {MAX_STREAM_BYTES} bytes");
    }
    Ok(bytes)
}

fn app_server_command(validated: &Validated) -> Command {
    let mut command = codex_command(validated, &app_server_args());
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn spawn_stdout_reader(
    stdout: impl io::Read + Send + 'static,
) -> (
    thread::JoinHandle<Result<()>>,
    mpsc::Receiver<Result<StreamEvent>>,
) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || read_stdout_events(stdout, tx));
    (handle, rx)
}

fn read_stdout_events<R: io::Read>(
    mut reader: R,
    tx: mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let mut total = 0usize;
    let mut line = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        let read = reader.read(&mut scratch)?;
        if read == 0 {
            if !line.is_empty() {
                bail!("app-server stdout line was not newline-terminated");
            }
            tx.send(Ok(StreamEvent::Eof))
                .map_err(|_| anyhow::anyhow!("app-server stdout receiver dropped"))?;
            return Ok(());
        }
        for &byte in &scratch[..read] {
            total += 1;
            if total > MAX_STREAM_BYTES {
                let error = anyhow::anyhow!("app-server stdout exceeds {MAX_STREAM_BYTES} bytes");
                let _ = tx.send(Err(anyhow::anyhow!(
                    "app-server stdout exceeds {MAX_STREAM_BYTES} bytes"
                )));
                return Err(error);
            }
            if line.len() == MAX_STREAM_BYTES {
                let error =
                    anyhow::anyhow!("app-server stdout line exceeds {MAX_STREAM_BYTES} bytes");
                let _ = tx.send(Err(anyhow::anyhow!(
                    "app-server stdout line exceeds {MAX_STREAM_BYTES} bytes"
                )));
                return Err(error);
            }
            line.push(byte);
            if byte == b'\n' {
                let emitted = std::mem::take(&mut line);
                tx.send(Ok(StreamEvent::Line(emitted)))
                    .map_err(|_| anyhow::anyhow!("app-server stdout receiver dropped"))?;
            }
        }
    }
}

fn spawn_stderr_reader(
    stderr: impl io::Read + Send + 'static,
) -> thread::JoinHandle<Result<Vec<u8>>> {
    thread::spawn(move || {
        let (bytes, overflowed) = read_bounded_stream(stderr, MAX_STREAM_BYTES)?;
        if overflowed {
            bail!("app-server stderr exceeds {MAX_STREAM_BYTES} bytes");
        }
        Ok(bytes)
    })
}

struct WriteRequest {
    bytes: Vec<u8>,
    result: mpsc::SyncSender<io::Result<()>>,
}

struct StdinWriter {
    sender: Option<mpsc::Sender<WriteRequest>>,
    handle: Option<thread::JoinHandle<io::Result<()>>>,
}

impl StdinWriter {
    fn new(mut stdin: ChildStdin) -> Self {
        let (sender, receiver) = mpsc::channel::<WriteRequest>();
        let handle = thread::spawn(move || {
            for request in receiver {
                let result = stdin.write_all(&request.bytes).and_then(|()| stdin.flush());
                match result {
                    Ok(()) => {
                        let _ = request.result.send(Ok(()));
                    }
                    Err(error) => {
                        let kind = error.kind();
                        let message = error.to_string();
                        let _ = request
                            .result
                            .send(Err(io::Error::new(kind, message.clone())));
                        return Err(io::Error::new(kind, message));
                    }
                }
            }
            Ok(())
        });
        Self {
            sender: Some(sender),
            handle: Some(handle),
        }
    }

    fn write_line(&self, line: &str, deadline: Instant) -> Result<()> {
        check_cancelled()?;
        let now = Instant::now();
        if now >= deadline {
            bail!("protocol timed out");
        }
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.sender
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("app-server stdin is closed"))?
            .send(WriteRequest {
                bytes: line.as_bytes().to_vec(),
                result: result_tx,
            })
            .map_err(|_| anyhow::anyhow!("app-server stdin writer disconnected"))?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("protocol timed out");
        }
        match result_rx.recv_timeout(remaining) {
            Ok(Ok(())) => {
                check_cancelled()?;
                Ok(())
            }
            Ok(Err(error)) => {
                check_cancelled()?;
                Err(error).context("write app-server protocol line")
            }
            Err(RecvTimeoutError::Timeout) => bail!("protocol timed out during stdin write"),
            Err(RecvTimeoutError::Disconnected) => {
                check_cancelled()?;
                bail!("app-server stdin writer disconnected")
            }
        }
    }

    fn close(&mut self) {
        self.sender.take();
    }

    fn join(&mut self) -> Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("app-server stdin writer panicked"))??;
        Ok(())
    }
}

fn wait_for_clean_exit(child: &mut ActiveCodexChild) -> Result<Output> {
    let deadline = Instant::now() + CLEANUP_WINDOW;
    loop {
        if child_exited_unreaped(child.child().id())? {
            let status = child
                .terminate_and_reap()
                .ok_or_else(|| anyhow::anyhow!("app-server exited without a reaped status"))?;
            return Ok(Output {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.terminate_and_reap();
    bail!("app-server did not exit cleanly within the cleanup window");
}

fn abort_child(child: &mut ActiveCodexChild) {
    let _ = child.terminate_and_reap();
}

struct AppServerCleanup {
    child: ActiveCodexChild,
    stdin_writer: StdinWriter,
    stdout_handle: Option<thread::JoinHandle<Result<()>>>,
    stderr_handle: Option<thread::JoinHandle<Result<Vec<u8>>>>,
    completed: bool,
    cleaned: bool,
}

impl AppServerCleanup {
    fn new(
        child: ActiveCodexChild,
        stdin: ChildStdin,
        stdout_handle: thread::JoinHandle<Result<()>>,
        stderr_handle: thread::JoinHandle<Result<Vec<u8>>>,
    ) -> Self {
        Self {
            child,
            stdin_writer: StdinWriter::new(stdin),
            stdout_handle: Some(stdout_handle),
            stderr_handle: Some(stderr_handle),
            completed: false,
            cleaned: false,
        }
    }

    fn write_line(&self, line: &str, deadline: Instant) -> Result<()> {
        self.stdin_writer.write_line(line, deadline)
    }

    fn close_stdin(&mut self) {
        self.stdin_writer.close();
    }

    fn join_stdout(&mut self) -> Result<()> {
        let handle = self
            .stdout_handle
            .take()
            .ok_or_else(|| anyhow::anyhow!("stdout reader already joined"))?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("app-server stdout reader panicked"))?
    }

    fn join_stderr(&mut self) -> Result<Vec<u8>> {
        let handle = self
            .stderr_handle
            .take()
            .ok_or_else(|| anyhow::anyhow!("stderr reader already joined"))?;
        handle
            .join()
            .map_err(|_| anyhow::anyhow!("app-server stderr reader panicked"))?
    }

    fn finish(&mut self) -> Result<()> {
        self.close_stdin();
        let status = wait_for_clean_exit(&mut self.child)?.status;
        self.stdin_writer.join()?;
        self.join_stdout()?;
        let stderr_bytes = self.join_stderr()?;
        if !stderr_bytes.is_empty() {
            bail!("app-server wrote to stderr");
        }
        if !status.success() {
            bail!("app-server exited unsuccessfully");
        }
        if !self.completed {
            bail!("app-server exited before protocol completion");
        }
        self.cleaned = true;
        Ok(())
    }

    fn abort(&mut self) {
        if self.cleaned {
            return;
        }
        self.close_stdin();
        abort_child(&mut self.child);
        let _ = self.stdin_writer.join();
        let _ = self.join_stdout();
        let _ = self.join_stderr();
        self.cleaned = true;
    }
}

impl Drop for AppServerCleanup {
    fn drop(&mut self) {
        self.abort();
    }
}

fn drive_app_server(
    validated: &Validated,
    request: &AdapterRequest,
    deadline: Instant,
) -> Result<AdapterResponse> {
    check_cancelled()?;
    validate_identities_before_spawn(validated)?;
    let cwd = validated
        .canonical_cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("cwd must be valid UTF-8"))?;
    let codex_home = validated
        .canonical_codex_home
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("codex home must be valid UTF-8"))?;
    let idempotency_key = format!(
        "{}:{}",
        request.delivery.subscription_id, request.delivery.event_id
    );
    let mut state = StateMachine::new(cwd, codex_home, idempotency_key.clone())?;

    let mut command = app_server_command(validated);
    let mut child = spawn_active_codex(&mut command).context("spawn codex app-server")?;
    let pipes = (|| -> Result<(ChildStdin, std::process::ChildStdout, std::process::ChildStderr)> {
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdin pipe for app-server"))?;
        let stdout = child
            .child_mut()
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stdout pipe for app-server"))?;
        let stderr = child
            .child_mut()
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("missing stderr pipe for app-server"))?;
        Ok((stdin, stdout, stderr))
    })();
    let (stdin, stdout, stderr) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            abort_child(&mut child);
            return Err(error);
        }
    };

    let (stdout_handle, stdout_rx) = spawn_stdout_reader(stdout);
    let stderr_handle = spawn_stderr_reader(stderr);
    let mut cleanup = AppServerCleanup::new(child, stdin, stdout_handle, stderr_handle);

    let result = (|| -> Result<AdapterResponse> {
        cleanup.write_line(&initialize_line()?, deadline)?;

        loop {
            let now = Instant::now();
            if now >= deadline {
                bail!("protocol timed out");
            }
            let remaining = deadline.saturating_duration_since(now);
            let event = match stdout_rx.recv_timeout(remaining) {
                Ok(event) => event?,
                Err(RecvTimeoutError::Timeout) => bail!("protocol timed out"),
                Err(RecvTimeoutError::Disconnected) => {
                    bail!("app-server stdout channel disconnected")
                }
            };
            match event {
                StreamEvent::Line(line) => {
                    if cleanup.completed {
                        bail!("post-completion stdout is not allowed");
                    }
                    let transition = state.feed(&line)?;
                    match transition {
                        Transition::Continue => {}
                        Transition::SendThreadStart => {
                            cleanup.write_line(&initialized_line()?, deadline)?;
                            cleanup.write_line(&thread_start_line(cwd)?, deadline)?;
                        }
                        Transition::SendTurnStart { thread_id } => {
                            cleanup.write_line(
                                &turn_start_line(&thread_id, &idempotency_key, &request.event)?,
                                deadline,
                            )?;
                        }
                        Transition::Completed => {
                            cleanup.completed = true;
                            cleanup.close_stdin();
                        }
                    }
                }
                StreamEvent::Eof => break,
            }
        }

        Ok(response_for(request))
    })();

    match result {
        Ok(response) => {
            if let Err(error) = cleanup.finish() {
                cleanup.abort();
                Err(error)
            } else {
                Ok(response)
            }
        }
        Err(error) => {
            cleanup.abort();
            Err(error)
        }
    }
}

fn render_json<T: Serialize>(value: &T) -> Result<String> {
    let rendered = serde_json::to_string(value)?;
    if rendered.len() > MAX_STREAM_BYTES {
        bail!("rendered JSON exceeds {MAX_STREAM_BYTES} bytes");
    }
    Ok(rendered)
}

fn render_response_line(response: &AdapterResponse) -> Result<String> {
    Ok(format!("{}\n", render_json(response)?))
}

pub(crate) fn render_response_bytes(response: &AdapterResponse) -> Result<Vec<u8>> {
    Ok(render_response_line(response)?.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter_protocol::{AdapterDelivery, AdapterTarget};
    use serde_json::json;
    use std::fs;
    use std::io::Cursor;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir(prefix: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kanban-codex-app-server-adapter-{prefix}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_file(path: &Path, mode: u32, content: &[u8]) {
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_dir(path: &Path, mode: u32) {
        fs::create_dir(path).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn request() -> AdapterRequest {
        AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: "sub-1".into(),
                event_id: "a".repeat(64),
                attempt: 2,
                created_at: 123,
            },
            target: AdapterTarget {
                consumer_id: CODEX_APP_SERVER_CONSUMER_ID.into(),
                action_id: START_READONLY_TURN_ACTION_ID.into(),
            },
            event: json!({
                "eventHash": "a".repeat(64),
                "eventID": "a".repeat(64),
                "timestamp": 123,
            }),
        }
    }

    fn output(status_success: bool, stdout: &str, stderr: &str) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(if status_success { 0 } else { 1 }),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn spawn_descendant_fixture(marker: &Path) -> Child {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "adapter_process::tests::child_fixture",
                "--nocapture",
            ])
            .env_clear()
            .env(
                "KANBAN_TEST_ADAPTER",
                format!("descendant:{}", marker.display()),
            )
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        command.spawn().unwrap()
    }

    fn pid_exists(pid: i32) -> bool {
        // SAFETY: signal 0 performs existence/permission checking only.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    fn validated_for_script(prefix: &str, script: &str) -> Validated {
        let root = temp_dir(prefix);
        let codex = root.join("codex.sh");
        write_file(&codex, 0o755, script.as_bytes());
        let codex_home = root.join("codex-home");
        write_dir(&codex_home, 0o700);
        let cwd = root.join("cwd");
        write_dir(&cwd, 0o700);

        let canonical_codex = fs::canonicalize(&codex).unwrap();
        let canonical_codex_file = fs::File::open(&canonical_codex).unwrap();
        let canonical_codex_identity = file_identity(&canonical_codex_file.metadata().unwrap());

        let canonical_codex_home = fs::canonicalize(&codex_home).unwrap();
        let canonical_codex_home_file = fs::File::open(&canonical_codex_home).unwrap();
        let canonical_codex_home_identity =
            file_identity(&canonical_codex_home_file.metadata().unwrap());

        let canonical_cwd = fs::canonicalize(&cwd).unwrap();
        let canonical_cwd_file = fs::File::open(&canonical_cwd).unwrap();
        let canonical_cwd_identity = file_identity(&canonical_cwd_file.metadata().unwrap());

        Validated {
            canonical_codex,
            canonical_codex_file,
            canonical_codex_identity,
            canonical_codex_home,
            canonical_codex_home_file,
            canonical_codex_home_identity,
            canonical_cwd,
            canonical_cwd_file,
            canonical_cwd_identity,
            required_version: "0.150.1".to_owned(),
            client_request_sha256: "a".repeat(64),
            protocol_schema_sha256: "b".repeat(64),
            protocol_timeout_ms: 1_000,
        }
    }

    fn spawnable_validated() -> Validated {
        validated_for_script("spawn-group", "#!/bin/sh\n/bin/sleep 30\n")
    }

    fn descendant_timeout_validated() -> Validated {
        validated_for_script(
            "probe-timeout",
            "#!/bin/sh\n/bin/sleep 30 &\n/bin/sleep 30\n",
        )
    }

    #[test]
    fn parse_outcome_accepts_help_and_version_only() {
        assert!(matches!(
            parse_outcome(vec!["bin".into(), "--help".into()]).unwrap(),
            Outcome::Help
        ));
        assert!(matches!(
            parse_outcome(vec!["bin".into(), "--version".into()]).unwrap(),
            Outcome::Version
        ));
    }

    #[test]
    fn parse_outcome_rejects_unknowns_repeats_positionals_and_bad_values() {
        for args in [
            vec!["bin".into(), "positional".into()],
            vec!["bin".into(), "--unknown".into(), "x".into()],
            vec![
                "bin".into(),
                "--codex".into(),
                "a".into(),
                "--codex".into(),
                "b".into(),
            ],
            vec!["bin".into(), "--protocol-timeout-ms".into(), "999".into()],
            vec!["bin".into(), "--client-request-sha256".into(), "zz".into()],
            vec!["bin".into(), "--codex".into(), "relative".into()],
        ] {
            assert!(parse_outcome(args).is_err());
        }
    }

    #[test]
    fn version_and_help_probe_parsers_are_exact() {
        validate_version_probe(&output(true, "codex-cli 1.2.3\n", ""), "1.2.3").unwrap();
        validate_app_server_help_probe(&output(
            true,
            "Usage: codex app-server\n--listen <URL>\ngenerate-json-schema\n",
            "",
        ))
        .unwrap();
    }

    #[test]
    fn request_target_is_exact_and_idempotency_key_is_composed_from_delivery_ids() {
        let request = request();
        validate_request_target(&request).unwrap();
        let response = response_for(&request);
        assert_eq!(response.subscription_id, "sub-1");
        assert_eq!(
            response.event_id,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(response.replay);
    }

    #[test]
    fn render_response_and_bounded_stream_seams_work() {
        let response = response_for(&request());
        let rendered = render_response(&response).unwrap();
        assert!(rendered.len() <= MAX_STREAM_BYTES);
        let (bytes, overflowed) = read_bounded_stream(
            Cursor::new(vec![b'x'; MAX_STREAM_BYTES + 1]),
            MAX_STREAM_BYTES,
        )
        .unwrap();
        assert_eq!(bytes.len(), MAX_STREAM_BYTES);
        assert!(overflowed);
    }

    #[test]
    fn shared_process_group_cleanup_removes_app_server_descendants() {
        let marker = std::env::temp_dir().join(format!(
            "kanban-app-server-descendant-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mut child = spawn_descendant_fixture(&marker);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let pid = loop {
            if let Ok(text) = fs::read_to_string(&marker)
                && let Ok(pid) = text.trim().parse::<i32>()
                && pid_exists(pid)
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "descendant pid marker was never created"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };

        let _ = terminate_and_reap(&mut child);

        let cleanup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pid_exists(pid) && std::time::Instant::now() < cleanup_deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !pid_exists(pid),
            "descendant process {pid} survived cleanup"
        );
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn app_server_spawn_uses_a_private_process_group() {
        let validated = spawnable_validated();
        let mut child = app_server_command(&validated).spawn().unwrap();
        let pid = child.id() as i32;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        let _ = terminate_and_reap(&mut child);
    }

    #[test]
    fn run_codex_command_times_out_and_reaps_descendants() {
        SPAWN_REGISTRATION_MASK_OBSERVED.store(false, Ordering::SeqCst);
        CLEANUP_SIGNAL_BEFORE_CLEAR_TARGET.store(0, Ordering::SeqCst);
        CLEANUP_SIGNAL_AFTER_CLEAR_TARGET.store(-1, Ordering::SeqCst);
        CLEANUP_RESERVED_AFTER_CLEAR_OBSERVED.store(false, Ordering::SeqCst);
        CLEANUP_TRANSITION_SEAM_ENABLED.store(true, Ordering::SeqCst);
        let validated = descendant_timeout_validated();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let error = run_codex_command(&validated, &version_args(), "codex version probe", deadline)
            .unwrap_err();
        CLEANUP_TRANSITION_SEAM_ENABLED.store(false, Ordering::SeqCst);
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(
            SPAWN_REGISTRATION_MASK_OBSERVED.load(Ordering::SeqCst),
            "SIGINT/SIGTERM were not masked across spawn and PGID registration"
        );
        assert!(
            CLEANUP_SIGNAL_BEFORE_CLEAR_TARGET.load(Ordering::SeqCst) > 0,
            "the signal relay did not target the still-reserved owned group"
        );
        assert_eq!(
            CLEANUP_SIGNAL_AFTER_CLEAR_TARGET.load(Ordering::SeqCst),
            0,
            "the signal relay retained a target after ownership clear"
        );
        assert!(
            CLEANUP_RESERVED_AFTER_CLEAR_OBSERVED.load(Ordering::SeqCst),
            "the leader was reaped before exact PGID ownership was cleared"
        );
    }

    #[test]
    fn bounded_stdout_reader_rejects_overflow_without_exceeding_the_limit() {
        let (tx, rx) = mpsc::channel();
        let error =
            read_stdout_events(Cursor::new(vec![b'x'; MAX_STREAM_BYTES + 1]), tx).unwrap_err();
        assert!(error.to_string().contains("app-server stdout exceeds"));
        assert!(rx.recv().unwrap().is_err());
    }

    #[test]
    fn temp_schema_dir_cleanup_rejects_replaced_identity() {
        let mut dir = TempSchemaDir::new().unwrap();
        let path = dir.path().to_path_buf();
        let moved = path.with_extension("moved");
        fs::rename(&path, &moved).unwrap();
        write_file(&path, 0o600, b"{}");
        assert!(dir.cleanup().is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_dir_all(&moved).unwrap();
    }

    #[test]
    fn schema_file_validation_rejects_symlink_and_writable_files() {
        let root = temp_dir("schema");
        let file = root.join("ClientRequest.json");
        write_file(&file, 0o600, b"{}");
        validate_schema_file(&file).unwrap();

        let link = root.join("link.json");
        symlink(&file, &link).unwrap();
        assert!(validate_schema_file(&link).is_err());

        let writable = root.join("writable.json");
        write_file(&writable, 0o666, b"{}");
        assert!(validate_schema_file(&writable).is_err());
    }

    #[test]
    fn temp_schema_dir_is_private_and_removed_on_drop() {
        let path = {
            let dir = TempSchemaDir::new().unwrap();
            let path = dir.path().to_path_buf();
            assert!(path.exists());
            path
        };
        assert!(!path.exists());
    }
}
