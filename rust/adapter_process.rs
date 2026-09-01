use std::ffi::OsString;
use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) const STREAM_LIMIT: usize = 1 << 20;

#[derive(Debug, PartialEq)]
pub(crate) struct BoundedOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) overflowed: bool,
}

#[derive(Clone)]
/// One trusted host-configured adapter invocation.
///
/// Process-group supervision contains ordinary descendants but is not an OS
/// sandbox. An authorized adapter must not change session/process group or
/// daemonize; code trusted to receive the configured secret is already inside
/// the host trust boundary.
pub(crate) struct ProcessSpec {
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) secret: Option<(OsString, OsString)>,
}

impl fmt::Debug for ProcessSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessSpec")
            .field("executable", &self.executable)
            .field("args", &self.args)
            .field(
                "secret",
                &self.secret.as_ref().map(|(name, _)| (name, "<redacted>")),
            )
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessFailure {
    pub(crate) code: &'static str,
    pub(crate) timed_out: bool,
}

impl fmt::Debug for ProcessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessFailure")
            .field("code", &self.code)
            .field("timed_out", &self.timed_out)
            .finish()
    }
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for ProcessFailure {}

fn failure(code: &'static str, timed_out: bool) -> ProcessFailure {
    ProcessFailure { code, timed_out }
}

#[cfg(coverage)]
fn inherited_llvm_profile_file() -> Option<OsString> {
    std::env::var_os("LLVM_PROFILE_FILE")
}

pub(crate) fn drain_bounded<R: Read>(mut reader: R) -> io::Result<BoundedOutput> {
    let mut bytes = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = STREAM_LIMIT.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&chunk[..retained]);
        overflowed |= retained < count;
    }
    Ok(BoundedOutput { bytes, overflowed })
}

enum ChildState {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
    WaitFailed,
}

fn signal_group(pid: u32, signal: i32) -> io::Result<bool> {
    let pid = i32::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child pid exceeds i32"))?;
    // SAFETY: `kill` is called with the negated process-group id created for
    // the child. No pointer crosses the FFI boundary.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error)
    }
}

fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    #[cfg(target_vendor = "apple")]
    {
        info.si_pid
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        // SAFETY: waitid initialized this siginfo_t for a SIGCHLD result.
        unsafe { info.si_pid() }
    }
}

pub(crate) fn child_exited_unreaped(pid: u32) -> io::Result<bool> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "child pid exceeds pid_t"))?;
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: info points to writable siginfo_t storage. WNOWAIT observes the
    // exact child without reaping it, so its PID cannot be recycled before
    // process-group cleanup completes.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid returned success, and the storage was zero-initialized
    // for the WNOHANG case where no child state was available.
    let info = unsafe { info.assume_init() };
    Ok(siginfo_pid(&info) == pid)
}

fn reap_if_exited(child: &mut std::process::Child) -> io::Result<Option<std::process::ExitStatus>> {
    if !child_exited_unreaped(child.id())? {
        return Ok(None);
    }
    let pid = child.id();
    // The adapter leader is still an unreaped zombie, reserving this PID.
    // Kill any descendants in its private process group before reaping it.
    let _ = signal_group(pid, libc::SIGKILL);
    child.wait().map(Some)
}

/// Terminate the process group and reap its leader.
///
/// Returns the real exit status when the leader had already exited before the
/// first signal. Once this function sends a signal, the caller's timeout or
/// cancellation reason owns the outcome.
pub(crate) fn terminate_and_reap(
    child: &mut std::process::Child,
) -> Option<std::process::ExitStatus> {
    if let Ok(Some(status)) = reap_if_exited(child) {
        return Some(status);
    }
    let pid = child.id();
    let signalled = match signal_group(pid, libc::SIGTERM) {
        Ok(true) => true,
        Ok(false) | Err(_) => {
            if let Ok(Some(status)) = reap_if_exited(child) {
                return Some(status);
            }
            match child.kill() {
                Ok(()) => true,
                Err(_) => {
                    if let Ok(Some(status)) = reap_if_exited(child) {
                        return Some(status);
                    }
                    false
                }
            }
        }
    };
    if signalled {
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            match child_exited_unreaped(pid) {
                Ok(true) => {
                    let _ = signal_group(pid, libc::SIGKILL);
                    let _ = child.wait();
                    return None;
                }
                Ok(false) => thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        // Signal the group while the leader is still unreaped and its PID is
        // unavailable for reuse.
        let _ = signal_group(pid, libc::SIGKILL);
        let _ = child.kill();
    }
    match child.wait() {
        Ok(status) if !signalled => Some(status),
        _ => None,
    }
}

fn join_io<T>(handle: JoinHandle<io::Result<T>>) -> Result<T, ProcessFailure> {
    handle
        .join()
        .map_err(|_| failure("adapter_io_thread", false))?
        .map_err(|_| failure("adapter_io", false))
}

pub(crate) fn run_process(
    spec: &ProcessSpec,
    input: &[u8],
    timeout_ms: i64,
    cancelled: &AtomicBool,
) -> Result<BoundedOutput, ProcessFailure> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(failure("adapter_cancelled", false));
    }
    if !spec.executable.is_absolute() || !(1..=300_000).contains(&timeout_ms) {
        return Err(failure("adapter_target_invalid", false));
    }

    #[cfg(coverage)]
    let llvm_profile_file = inherited_llvm_profile_file();
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.args)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    #[cfg(coverage)]
    if let Some(value) = llvm_profile_file {
        command.env("LLVM_PROFILE_FILE", value);
    }
    if let Some((name, value)) = &spec.secret {
        command.env(name, value);
    }

    let mut child = command
        .spawn()
        .map_err(|_| failure("adapter_spawn", false))?;
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        let _ = terminate_and_reap(&mut child);
        return Err(failure("adapter_io_setup", false));
    };

    let owned_input = input.to_vec();
    let stdin_thread = match thread::Builder::new()
        .name("kanban-adapter-stdin".into())
        .spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&owned_input)?;
            stdin.flush()
        }) {
        Ok(handle) => handle,
        Err(_) => {
            let _ = terminate_and_reap(&mut child);
            return Err(failure("adapter_stdin_thread", false));
        }
    };
    let stdout_thread = match thread::Builder::new()
        .name("kanban-adapter-stdout".into())
        .spawn(move || drain_bounded(stdout))
    {
        Ok(handle) => handle,
        Err(_) => {
            let _ = terminate_and_reap(&mut child);
            let _ = stdin_thread.join();
            return Err(failure("adapter_stdout_thread", false));
        }
    };
    let stderr_thread = match thread::Builder::new()
        .name("kanban-adapter-stderr".into())
        .spawn(move || drain_bounded(stderr))
    {
        Ok(handle) => handle,
        Err(_) => {
            let _ = terminate_and_reap(&mut child);
            let _ = stdin_thread.join();
            let _ = stdout_thread.join();
            return Err(failure("adapter_stderr_thread", false));
        }
    };

    let started = Instant::now();
    let timeout = Duration::from_millis(timeout_ms as u64);
    let state = loop {
        match reap_if_exited(&mut child) {
            Ok(Some(status)) => break ChildState::Exited(status),
            Ok(None) => {}
            Err(_) => {
                let _ = terminate_and_reap(&mut child);
                break ChildState::WaitFailed;
            }
        }
        if cancelled.load(Ordering::SeqCst) {
            match reap_if_exited(&mut child) {
                Ok(Some(status)) => break ChildState::Exited(status),
                Ok(None) => {
                    break match terminate_and_reap(&mut child) {
                        Some(status) => ChildState::Exited(status),
                        None => ChildState::Cancelled,
                    };
                }
                Err(_) => {
                    let _ = terminate_and_reap(&mut child);
                    break ChildState::WaitFailed;
                }
            }
        }
        if started.elapsed() >= timeout {
            match reap_if_exited(&mut child) {
                Ok(Some(status)) => break ChildState::Exited(status),
                Ok(None) => {
                    break match terminate_and_reap(&mut child) {
                        Some(status) => ChildState::Exited(status),
                        None => ChildState::TimedOut,
                    };
                }
                Err(_) => {
                    let _ = terminate_and_reap(&mut child);
                    break ChildState::WaitFailed;
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    };

    let stdin_result = stdin_thread
        .join()
        .map_err(|_| failure("adapter_stdin_thread", false));
    let stdout_result = join_io(stdout_thread).map_err(|error| ProcessFailure {
        code: if error.code == "adapter_io_thread" {
            "adapter_stdout_thread"
        } else {
            "adapter_stdout_read"
        },
        timed_out: false,
    });
    let stderr_result = join_io(stderr_thread).map_err(|error| ProcessFailure {
        code: if error.code == "adapter_io_thread" {
            "adapter_stderr_thread"
        } else {
            "adapter_stderr_read"
        },
        timed_out: false,
    });

    match state {
        ChildState::TimedOut => return Err(failure("adapter_timeout", true)),
        ChildState::Cancelled => return Err(failure("adapter_cancelled", false)),
        ChildState::WaitFailed => return Err(failure("adapter_wait", false)),
        ChildState::Exited(status) if !status.success() => {
            return Err(failure("adapter_exit", false));
        }
        ChildState::Exited(_) => {}
    }
    let stdin_result = stdin_result?;
    stdin_result.map_err(|_| failure("adapter_stdin", false))?;
    let stdout = stdout_result?;
    let stderr = stderr_result?;
    if stdout.overflowed {
        return Err(failure("adapter_stdout_overflow", false));
    }
    if stderr.overflowed {
        return Err(failure("adapter_stderr_overflow", false));
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::io::{Cursor, Error, ErrorKind};
    use std::process::Command;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn fixture_spec(mode: OsString) -> ProcessSpec {
        ProcessSpec {
            executable: env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "adapter_process::tests::child_fixture".into(),
                "--nocapture".into(),
            ],
            secret: Some(("KANBAN_TEST_ADAPTER".into(), mode)),
        }
    }

    fn run_fixture(
        mode: impl Into<OsString>,
        input: &[u8],
        timeout_ms: i64,
        cancelled: &AtomicBool,
    ) -> Result<BoundedOutput, ProcessFailure> {
        run_process(&fixture_spec(mode.into()), input, timeout_ms, cancelled)
    }

    fn spawn_child_fixture(mode: &str) -> std::process::Child {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "adapter_process::tests::child_fixture",
                "--nocapture",
            ])
            .env_clear()
            .env("KANBAN_TEST_ADAPTER", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        #[cfg(coverage)]
        if let Some(value) = inherited_llvm_profile_file() {
            command.env("LLVM_PROFILE_FILE", value);
        }
        command.spawn().unwrap()
    }

    struct ChildReaper(Option<std::process::Child>);

    impl ChildReaper {
        fn new(child: std::process::Child) -> Self {
            Self(Some(child))
        }

        fn child(&self) -> &std::process::Child {
            self.0.as_ref().unwrap()
        }
    }

    impl Drop for ChildReaper {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn child_fixture() {
        let Some(mode) = env::var_os("KANBAN_TEST_ADAPTER") else {
            return;
        };
        let mode = mode.to_string_lossy();
        match mode.as_ref() {
            "echo" => {
                let mut input = Vec::new();
                io::stdin().read_to_end(&mut input).unwrap();
                io::stdout().write_all(&input).unwrap();
            }
            "env" => {
                if env::var_os("KANBAN_PROCESS_LEAK").is_some() {
                    std::process::exit(9);
                }
                io::stdout().write_all(b"isolated").unwrap();
            }
            #[cfg(coverage)]
            "coverage-profile" => {
                use std::os::unix::ffi::OsStrExt;

                let value = env::var_os("LLVM_PROFILE_FILE")
                    .expect("coverage builds must preserve LLVM_PROFILE_FILE");
                io::stdout()
                    .write_all(value.as_os_str().as_bytes())
                    .unwrap();
            }
            "exit" => std::process::exit(7),
            "exit-ok" => {}
            "stdout-overflow" => {
                io::stdout()
                    .write_all(&vec![b'o'; STREAM_LIMIT + 1])
                    .unwrap();
            }
            "stderr-overflow" => {
                io::stderr()
                    .write_all(&vec![b'e'; STREAM_LIMIT + 1])
                    .unwrap();
            }
            "sleep" => thread::sleep(Duration::from_secs(30)),
            value if value.starts_with("descendant:") => {
                let path = value.strip_prefix("descendant:").unwrap();
                let child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
                fs::write(path, child.id().to_string()).unwrap();
                drop(child);
                thread::sleep(Duration::from_secs(30));
            }
            _ => std::process::exit(8),
        }
    }

    #[test]
    fn bounded_drain_handles_empty_and_short_streams() {
        assert_eq!(
            drain_bounded(Cursor::new(Vec::<u8>::new())).unwrap(),
            BoundedOutput {
                bytes: Vec::new(),
                overflowed: false,
            }
        );
        assert_eq!(
            drain_bounded(Cursor::new(b"hello")).unwrap(),
            BoundedOutput {
                bytes: b"hello".to_vec(),
                overflowed: false,
            }
        );
    }

    #[test]
    fn bounded_drain_accepts_exactly_the_limit() {
        let input = vec![b'a'; STREAM_LIMIT];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes, input);
        assert!(!output.overflowed);
    }

    #[test]
    fn bounded_drain_marks_one_byte_over_and_keeps_only_the_limit() {
        let input = vec![b'b'; STREAM_LIMIT + 1];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes, input[..STREAM_LIMIT]);
        assert!(output.overflowed);
    }

    #[test]
    fn bounded_drain_continues_through_a_much_larger_stream() {
        let input = vec![b'c'; STREAM_LIMIT * 3 + 17];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes.len(), STREAM_LIMIT);
        assert!(output.bytes.iter().all(|byte| *byte == b'c'));
        assert!(output.overflowed);
    }

    struct FailingReader {
        first: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                buffer[..3].copy_from_slice(b"abc");
                Ok(3)
            } else {
                Err(Error::other("fixture failure"))
            }
        }
    }

    #[test]
    fn bounded_drain_propagates_reader_errors() {
        let error = drain_bounded(FailingReader { first: true }).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other);
    }

    #[test]
    fn process_failure_formats_the_public_fields() {
        let failure = ProcessFailure {
            code: "adapter_example",
            timed_out: true,
        };
        assert_eq!(
            format!("{failure:?}"),
            "ProcessFailure { code: \"adapter_example\", timed_out: true }"
        );
        assert_eq!(failure.to_string(), "adapter_example");
    }

    #[test]
    fn signal_group_distinguishes_missing_process_groups_from_other_errors() {
        let missing = signal_group(i32::MAX as u32, libc::SIGTERM).unwrap();
        assert!(!missing);

        let child = ChildReaper::new(spawn_child_fixture("sleep"));
        let error = signal_group(child.child().id(), 9999).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn run_process_rejects_relative_targets_and_bad_timeouts() {
        let spec = ProcessSpec {
            executable: PathBuf::from("relative/path"),
            args: Vec::new(),
            secret: None,
        };
        let error = run_process(&spec, b"", 5_000, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(error.code, "adapter_target_invalid");

        let spec = fixture_spec("echo".into());
        let error = run_process(&spec, b"", 0, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(error.code, "adapter_target_invalid");
    }

    #[test]
    fn process_runner_writes_stdin_and_captures_stdout() {
        let output = run_fixture("echo", b"request-body", 5_000, &AtomicBool::new(false)).unwrap();
        assert!(
            output
                .bytes
                .windows(b"request-body".len())
                .any(|window| window == b"request-body")
        );
    }

    #[test]
    fn process_runner_clears_the_environment_and_sets_only_the_target_secret() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        unsafe { env::set_var("KANBAN_PROCESS_LEAK", "must-not-leak") };
        let result = run_fixture("env", b"", 5_000, &AtomicBool::new(false));
        unsafe { env::remove_var("KANBAN_PROCESS_LEAK") };
        assert!(result.is_ok());
    }

    #[cfg(coverage)]
    #[test]
    fn process_runner_preserves_llvm_profile_file_under_coverage() {
        use std::os::unix::ffi::OsStrExt;

        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let expected = env::var_os("LLVM_PROFILE_FILE")
            .expect("llvm-cov must set LLVM_PROFILE_FILE for coverage builds");
        let output = run_fixture("coverage-profile", b"", 5_000, &AtomicBool::new(false)).unwrap();
        assert!(
            output
                .bytes
                .windows(expected.as_os_str().as_bytes().len())
                .any(|window| window == expected.as_os_str().as_bytes())
        );
    }

    #[test]
    fn process_runner_classifies_exit_and_stream_overflow() {
        for (mode, code) in [
            ("exit", "adapter_exit"),
            ("stdout-overflow", "adapter_stdout_overflow"),
            ("stderr-overflow", "adapter_stderr_overflow"),
        ] {
            let error = run_fixture(mode, b"", 5_000, &AtomicBool::new(false)).unwrap_err();
            assert_eq!(error.code, code);
            assert!(!error.timed_out);
        }
    }

    #[test]
    fn process_runner_times_out_and_honors_cancellation() {
        let timeout = run_fixture("sleep", b"", 50, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(timeout.code, "adapter_timeout");
        assert!(timeout.timed_out);

        let cancelled = AtomicBool::new(true);
        let error = run_fixture("echo", b"", 5_000, &cancelled).unwrap_err();
        assert_eq!(error.code, "adapter_cancelled");
        assert!(!error.timed_out);

        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let setter = std::sync::Arc::clone(&cancelled);
        let cancellation_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });
        let error = run_fixture("sleep", b"", 5_000, &cancelled).unwrap_err();
        cancellation_thread.join().unwrap();
        assert_eq!(error.code, "adapter_cancelled");
        assert!(!error.timed_out);
    }

    #[test]
    fn termination_preserves_an_exit_observed_before_any_signal() {
        let mut child = spawn_child_fixture("exit-ok");
        let deadline = Instant::now() + Duration::from_secs(3);
        while !child_exited_unreaped(child.id()).unwrap() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let status = terminate_and_reap(&mut child).expect("exit must win before any signal");
        assert!(status.success());
    }

    fn pid_exists(pid: i32) -> bool {
        // SAFETY: signal 0 performs existence/permission checking only.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[test]
    fn timeout_terminates_same_group_descendants() {
        let marker = env::temp_dir().join(format!(
            "kanban-adapter-descendant-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        let mode = OsString::from(format!("descendant:{}", marker.display()));
        let error = run_fixture(mode, b"", 500, &AtomicBool::new(false)).unwrap_err();
        assert_eq!(error.code, "adapter_timeout");
        let pid: i32 = fs::read_to_string(&marker).unwrap().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while pid_exists(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !pid_exists(pid),
            "descendant process {pid} survived timeout"
        );
        let _ = fs::remove_file(marker);
    }

    #[test]
    fn process_debug_output_redacts_secret_values() {
        let spec = fixture_spec("top-secret-value".into());
        let rendered = format!("{spec:?}");
        assert!(!rendered.contains("top-secret-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
