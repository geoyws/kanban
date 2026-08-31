use crate::adapter_process::{ProcessSpec, run_process};
use crate::adapter_protocol::{
    AdapterDelivery, AdapterRequest, AdapterTarget, decode_response, encode_request,
};
use crate::dispatch::{
    ConsumerLock, DispatcherConfigLoader, ResolvedDispatch, try_consumer_lock, validate_consumer_id,
};
use crate::lock::{self, DataRootLock};
use crate::model::{SubscriptionDeliveryCandidate, SubscriptionDeliveryClaim};
use crate::registry::Registry;
use crate::registry::now_ms;
use crate::store::Store;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static CANCELLED: AtomicBool = AtomicBool::new(false);
static SIGNALS_INSTALLED: AtomicBool = AtomicBool::new(false);
const LEASE_CLEANUP_HEADROOM_MS: i64 = 30_000;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(debug_assertions)]
const TEST_CRASH_EVENT_ENV: &str = "KANBAN_DISPATCHER_TEST_CRASH_AFTER_EVENT_ID";
const HELP: &str = r#"kanban-dispatcher — durable subscription delivery worker

Usage:
  kanban-dispatcher (--db PATH | --project NAME | --workspace PATH)
                    [--consumer NAME] [--once] [--json]
  kanban-dispatcher --help
  kanban-dispatcher --version

The board selector is always explicit. KANBAN_DB and KANBAN_PROJECT may only
repeat the matching explicit selector; conflicting implicit selection fails.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardSelector {
    Db(PathBuf),
    Project(String),
    Workspace(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatcherArgs {
    pub(crate) selector: Option<BoardSelector>,
    pub(crate) consumer: Option<String>,
    pub(crate) once: bool,
    pub(crate) json: bool,
    pub(crate) help: bool,
    pub(crate) version: bool,
}

pub(crate) struct DispatcherContext {
    pub(crate) board_path: PathBuf,
    pub(crate) board_name: Option<String>,
    pub(crate) consumer: Option<String>,
    pub(crate) once: bool,
    _data_root_lock: Option<DataRootLock>,
}

fn os_is_empty(value: &OsStr) -> bool {
    value.as_bytes().is_empty()
}

fn flag_name(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).context("dispatcher flag name must be valid UTF-8")
}

fn text_value(value: OsString, flag: &str) -> Result<String> {
    if os_is_empty(&value) {
        bail!("--{flag} requires a nonempty value");
    }
    value
        .into_string()
        .map_err(|_| anyhow::anyhow!("--{flag} must be valid UTF-8"))
}

fn path_value(value: OsString, flag: &str) -> Result<PathBuf> {
    if os_is_empty(&value) {
        bail!("--{flag} requires a nonempty value");
    }
    Ok(PathBuf::from(value))
}

fn nonempty_env(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !os_is_empty(value))
}

fn require_value(
    values: &[OsString],
    index: &mut usize,
    name: &str,
    inline: Option<OsString>,
) -> Result<OsString> {
    if let Some(value) = inline {
        if os_is_empty(&value) {
            bail!("--{name} requires a nonempty value");
        }
        return Ok(value);
    }
    *index += 1;
    let value = values
        .get(*index)
        .with_context(|| format!("--{name} requires a value"))?
        .clone();
    if value.as_bytes().starts_with(b"--") {
        bail!("--{name} requires a value before the next flag");
    }
    if os_is_empty(&value) {
        bail!("--{name} requires a nonempty value");
    }
    Ok(value)
}

pub(crate) fn parse_args(values: Vec<OsString>) -> Result<DispatcherArgs> {
    parse_args_with_env(
        values,
        env::var_os("KANBAN_DB"),
        env::var_os("KANBAN_PROJECT"),
    )
}

fn parse_args_with_env(
    values: Vec<OsString>,
    kanban_db: Option<OsString>,
    kanban_project: Option<OsString>,
) -> Result<DispatcherArgs> {
    let mut selector = None;
    let mut consumer = None;
    let mut once = false;
    let mut json = false;
    let mut help = false;
    let mut version = false;
    let mut seen = HashSet::new();
    let mut index = 0_usize;

    while index < values.len() {
        let token = &values[index];
        let bytes = token.as_bytes();
        if !bytes.starts_with(b"--") || bytes.len() == 2 {
            bail!("unexpected dispatcher positional argument {:?}", token);
        }
        let raw = &bytes[2..];
        let (name_bytes, inline) = match raw.iter().position(|byte| *byte == b'=') {
            Some(position) => (
                &raw[..position],
                Some(OsString::from_vec(raw[position + 1..].to_vec())),
            ),
            None => (raw, None),
        };
        let name = flag_name(name_bytes)?;
        if !matches!(
            name,
            "db" | "project" | "workspace" | "consumer" | "once" | "json" | "help" | "version"
        ) {
            bail!("unknown dispatcher flag --{name}");
        }
        if !seen.insert(name.to_owned()) {
            bail!("--{name} given more than once");
        }

        match name {
            "once" | "json" | "help" | "version" => {
                if inline.is_some() {
                    bail!("--{name} is a boolean flag and takes no value");
                }
                match name {
                    "once" => once = true,
                    "json" => json = true,
                    "help" => help = true,
                    "version" => version = true,
                    _ => unreachable!(),
                }
            }
            "db" | "project" | "workspace" => {
                if selector.is_some() {
                    bail!(
                        "dispatcher board selectors conflict; give exactly one of --db, --project, or --workspace"
                    );
                }
                let value = require_value(&values, &mut index, name, inline)?;
                selector = Some(match name {
                    "db" => BoardSelector::Db(path_value(value, name)?),
                    "project" => BoardSelector::Project(text_value(value, name)?),
                    "workspace" => BoardSelector::Workspace(path_value(value, name)?),
                    _ => unreachable!(),
                });
            }
            "consumer" => {
                let value = require_value(&values, &mut index, name, inline)?;
                consumer = Some(validate_consumer_id(&text_value(value, name)?)?);
            }
            _ => unreachable!(),
        }
        index += 1;
    }

    if help && version {
        bail!("--help and --version cannot be combined");
    }
    if !help && !version && selector.is_none() {
        bail!("dispatcher requires exactly one explicit selector: --db, --project, or --workspace");
    }

    let kanban_db = nonempty_env(kanban_db);
    let kanban_project = nonempty_env(kanban_project);
    if !help && !version {
        if kanban_db.is_some() && kanban_project.is_some() {
            bail!("KANBAN_DB and KANBAN_PROJECT both select a board; unset one");
        }
        match selector
            .as_ref()
            .expect("normal execution requires a selector")
        {
            BoardSelector::Db(path) => {
                if kanban_project.is_some() {
                    bail!("--db conflicts with KANBAN_PROJECT");
                }
                if let Some(env_path) = kanban_db
                    && env_path != path.as_os_str()
                {
                    bail!("--db conflicts with KANBAN_DB");
                }
            }
            BoardSelector::Project(name) => {
                if kanban_db.is_some() {
                    bail!("--project conflicts with KANBAN_DB");
                }
                if let Some(env_name) = kanban_project
                    && env_name != OsStr::new(name)
                {
                    bail!("--project conflicts with KANBAN_PROJECT");
                }
            }
            BoardSelector::Workspace(_) => {
                if kanban_db.is_some() || kanban_project.is_some() {
                    bail!("--workspace conflicts with KANBAN_DB or KANBAN_PROJECT");
                }
            }
        }
    }

    Ok(DispatcherArgs {
        selector,
        consumer,
        once,
        json,
        help,
        version,
    })
}

fn existing_board(path: &Path) -> Result<()> {
    let metadata = path
        .metadata()
        .with_context(|| format!("dispatcher board {} is not present", path.display()))?;
    if !metadata.is_file() {
        bail!("dispatcher board {} is not a regular file", path.display());
    }
    Ok(())
}

pub(crate) fn resolve_context(args: DispatcherArgs) -> Result<DispatcherContext> {
    if args.help || args.version {
        bail!("help and version do not resolve a dispatcher board");
    }
    let selector = args
        .selector
        .as_ref()
        .context("dispatcher selector is required")?;
    let direct_canonical = match selector {
        BoardSelector::Db(path) => Some(
            path.canonicalize()
                .with_context(|| format!("dispatcher board {} is not present", path.display()))?,
        ),
        BoardSelector::Project(_) | BoardSelector::Workspace(_) => None,
    };
    let needs_data_root_lock = match selector {
        BoardSelector::Db(path) => {
            lock::touches_data_root(Some(path))
                || lock::touches_data_root(direct_canonical.as_deref())
        }
        BoardSelector::Project(_) | BoardSelector::Workspace(_) => true,
    };
    let data_root_lock = needs_data_root_lock.then(lock::shared).transpose()?;

    let (board_path, board_name) = match selector {
        BoardSelector::Db(path) => {
            let canonical = path
                .canonicalize()
                .with_context(|| format!("dispatcher board {} is not present", path.display()))?;
            if direct_canonical.as_ref() != Some(&canonical) {
                bail!(
                    "dispatcher board {} changed while resolving",
                    path.display()
                );
            }
            existing_board(&canonical)?;
            let store = Store::open_readonly(&canonical)?;
            (canonical, store.board_name()?)
        }
        BoardSelector::Project(name) => {
            let registry = Registry::open_readonly()?;
            let projects = registry.by_name(name)?;
            let project = match projects.as_slice() {
                [project] => project,
                [] => bail!("no Kanban project named {name}"),
                many => bail!(
                    "{} Kanban projects are named {name}; select one with --workspace",
                    many.len()
                ),
            };
            let path = PathBuf::from(&project.board_path);
            existing_board(&path)?;
            (path, Some(project.name.clone()))
        }
        BoardSelector::Workspace(workspace) => {
            let registry = Registry::open_readonly()?;
            let project = registry
                .resolve_readonly(workspace)?
                .with_context(|| format!("no Kanban project contains {}", workspace.display()))?;
            let path = PathBuf::from(&project.board_path);
            existing_board(&path)?;
            (path, Some(project.name))
        }
    };

    Ok(DispatcherContext {
        board_path,
        board_name,
        consumer: args.consumer,
        once: args.once,
        _data_root_lock: data_root_lock,
    })
}

extern "C" fn handle_signal(_signal: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}

pub(crate) fn install_signal_handlers() -> Result<()> {
    if SIGNALS_INSTALLED.load(Ordering::SeqCst) {
        return Ok(());
    }
    // SAFETY: sigaction is fully initialized before the calls, the handler
    // performs only a lock-free atomic store, and installing the same handler
    // more than once is deterministic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_signal as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        if libc::sigemptyset(&mut action.sa_mask) != 0 {
            return Err(std::io::Error::last_os_error()).context("initialize dispatcher signals");
        }
        for signal in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("install dispatcher signal {signal}"));
            }
        }
    }
    SIGNALS_INSTALLED.store(true, Ordering::SeqCst);
    Ok(())
}

pub(crate) fn cancellation_flag() -> &'static AtomicBool {
    &CANCELLED
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DispatcherReport {
    pub(crate) attempted: u64,
    pub(crate) succeeded: u64,
    pub(crate) failed: u64,
    pub(crate) idle: bool,
    pub(crate) busy: bool,
    pub(crate) cancelled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeliveryFailure {
    code: &'static str,
    timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepOutcome {
    Succeeded,
    Failed,
    Idle,
    Busy,
    LostEligibility,
    Cancelled,
}

trait SchedulerBackend {
    type Candidate: Clone;
    type Resolved;
    type ConsumerLock;
    type Claim;

    fn cancelled(&self) -> bool;
    fn prepare(&mut self) -> Result<Option<Self::Candidate>>;
    fn resolve(&mut self, candidate: &Self::Candidate) -> Result<Self::Resolved>;
    fn consumer_id<'a>(&self, resolved: &'a Self::Resolved) -> &'a str;
    fn timeout_ms(&self, candidate: &Self::Candidate) -> i64;
    fn try_lock(&mut self, consumer_id: &str) -> Result<Option<Self::ConsumerLock>>;
    fn claim(
        &mut self,
        candidate: &Self::Candidate,
        lease_duration_ms: i64,
    ) -> Result<Option<Self::Claim>>;
    fn invoke(
        &mut self,
        claim: &Self::Claim,
        resolved: &Self::Resolved,
    ) -> std::result::Result<(), DeliveryFailure>;
    fn after_adapter_success(&mut self, claim: &Self::Claim);
    fn finalize_success(&mut self, claim: &Self::Claim) -> Result<bool>;
    fn finalize_failure(&mut self, claim: &Self::Claim, failure: DeliveryFailure) -> Result<bool>;
    fn wait(&mut self) {
        thread::sleep(POLL_INTERVAL);
    }
}

fn scheduler_step<B: SchedulerBackend>(backend: &mut B) -> Result<StepOutcome> {
    if backend.cancelled() {
        return Ok(StepOutcome::Cancelled);
    }
    let Some(candidate) = backend.prepare()? else {
        return Ok(StepOutcome::Idle);
    };

    // Validate the target before contending for the consumer lock. A missing
    // capability or secret must never turn a pending delivery into a lease.
    let initial = backend.resolve(&candidate)?;
    let consumer_id = backend.consumer_id(&initial).to_owned();
    let Some(_consumer_lock) = backend.try_lock(&consumer_id)? else {
        return Ok(StepOutcome::Busy);
    };

    // The config may have changed while this process waited for the lock.
    // Discard the initial resolution and re-open/re-validate under the lock.
    let resolved = backend.resolve(&candidate)?;
    if backend.consumer_id(&resolved) != consumer_id {
        bail!("dispatcher consumer identity changed while acquiring its lock");
    }
    if backend.cancelled() {
        return Ok(StepOutcome::Cancelled);
    }

    let lease_duration_ms = backend
        .timeout_ms(&candidate)
        .checked_add(LEASE_CLEANUP_HEADROOM_MS)
        .context("dispatcher lease duration overflowed")?;
    let Some(claim) = backend.claim(&candidate, lease_duration_ms)? else {
        return Ok(StepOutcome::LostEligibility);
    };

    match backend.invoke(&claim, &resolved) {
        Ok(()) => {
            // Compiled-process recovery tests replace this hook with an exact
            // event-id crash. Release builds compile the hook out entirely;
            // debug builds reach it only through the test-named environment.
            backend.after_adapter_success(&claim);
            if !backend.finalize_success(&claim)? {
                bail!("dispatcher success acknowledgement lost its exact lease");
            }
            Ok(StepOutcome::Succeeded)
        }
        Err(failure) => {
            if !backend.finalize_failure(&claim, failure)? {
                bail!("dispatcher failure acknowledgement lost its exact lease");
            }
            Ok(StepOutcome::Failed)
        }
    }
}

fn drive<B: SchedulerBackend>(backend: &mut B, once: bool) -> Result<DispatcherReport> {
    let mut report = DispatcherReport::default();
    loop {
        let outcome = scheduler_step(backend)?;
        match outcome {
            StepOutcome::Succeeded => {
                report.attempted += 1;
                report.succeeded += 1;
            }
            StepOutcome::Failed => {
                report.attempted += 1;
                report.failed += 1;
            }
            StepOutcome::Idle => report.idle = true,
            StepOutcome::Busy => report.busy = true,
            StepOutcome::LostEligibility => {}
            StepOutcome::Cancelled => {
                report.cancelled = true;
                return Ok(report);
            }
        }
        if once {
            return Ok(report);
        }
        if backend.cancelled() {
            report.cancelled = true;
            return Ok(report);
        }
        backend.wait();
    }
}

struct SystemBackend {
    context: DispatcherContext,
    store: Store,
}

impl SystemBackend {
    fn open(context: DispatcherContext) -> Result<Self> {
        let config = DispatcherConfigLoader::load()?;
        if let Some(consumer_id) = context.consumer.as_deref() {
            config.require_consumer(consumer_id)?;
        }
        let store = Store::open_for_dispatcher(&context.board_path)?;
        Ok(Self { context, store })
    }

    fn request(
        &self,
        claim: &SubscriptionDeliveryClaim,
        resolved: &ResolvedDispatch,
    ) -> std::result::Result<(AdapterRequest, Vec<u8>), DeliveryFailure> {
        let event = crate::watch::project_board_event(
            claim.event.clone(),
            &self.context.board_path,
            self.context.board_name.as_deref(),
        )
        .map_err(|_| DeliveryFailure {
            code: "adapter_request_invalid",
            timed_out: false,
        })?;
        let request = AdapterRequest {
            protocol_version: 1,
            delivery: AdapterDelivery {
                subscription_id: claim.subscription.id.clone(),
                event_id: claim.event_id.clone(),
                attempt: claim.attempt_number,
                created_at: claim.event_created_at,
            },
            target: AdapterTarget {
                consumer_id: resolved.consumer_id.clone(),
                action_id: resolved.action_id.clone(),
            },
            event,
        };
        let input = encode_request(&request).map_err(|_| DeliveryFailure {
            code: "adapter_request_invalid",
            timed_out: false,
        })?;
        Ok((request, input))
    }
}

impl SchedulerBackend for SystemBackend {
    type Candidate = SubscriptionDeliveryCandidate;
    type Resolved = ResolvedDispatch;
    type ConsumerLock = ConsumerLock;
    type Claim = SubscriptionDeliveryClaim;

    fn cancelled(&self) -> bool {
        cancellation_flag().load(Ordering::SeqCst)
    }

    fn prepare(&mut self) -> Result<Option<Self::Candidate>> {
        let now = now_ms();
        self.store.materialize_subscriptions()?;
        self.store.recover_expired_subscription_deliveries(now)?;
        match self.context.consumer.as_deref() {
            Some(consumer_id) => self
                .store
                .next_due_subscription_delivery_for_consumer(now, Some(consumer_id)),
            None => self.store.next_due_subscription_delivery(now),
        }
    }

    fn resolve(&mut self, candidate: &Self::Candidate) -> Result<Self::Resolved> {
        DispatcherConfigLoader::load()?.resolve(&candidate.subscription)
    }

    fn consumer_id<'a>(&self, resolved: &'a Self::Resolved) -> &'a str {
        &resolved.consumer_id
    }

    fn timeout_ms(&self, candidate: &Self::Candidate) -> i64 {
        candidate.subscription.timeout_ms
    }

    fn try_lock(&mut self, consumer_id: &str) -> Result<Option<Self::ConsumerLock>> {
        try_consumer_lock(consumer_id)
    }

    fn claim(
        &mut self,
        candidate: &Self::Candidate,
        lease_duration_ms: i64,
    ) -> Result<Option<Self::Claim>> {
        self.store.claim_subscription_delivery(
            &candidate.subscription.id,
            &candidate.event_id,
            now_ms(),
            lease_duration_ms,
        )
    }

    fn invoke(
        &mut self,
        claim: &Self::Claim,
        resolved: &Self::Resolved,
    ) -> std::result::Result<(), DeliveryFailure> {
        let (request, input) = self.request(claim, resolved)?;
        let spec = ProcessSpec {
            executable: resolved.executable.clone(),
            args: resolved.args.iter().map(OsString::from).collect(),
            secret: resolved.secret.as_ref().map(|secret| {
                (
                    OsString::from(&secret.target_env),
                    secret.secret_value().to_os_string(),
                )
            }),
        };
        let output = run_process(
            &spec,
            &input,
            claim.subscription.timeout_ms,
            cancellation_flag(),
        )
        .map_err(|failure| DeliveryFailure {
            code: failure.code,
            timed_out: failure.timed_out,
        })?;
        decode_response(&output.bytes, &request).map_err(|_| DeliveryFailure {
            code: "adapter_response_invalid",
            timed_out: false,
        })?;
        Ok(())
    }

    fn after_adapter_success(&mut self, _claim: &Self::Claim) {
        #[cfg(debug_assertions)]
        if env::var_os(TEST_CRASH_EVENT_ENV).as_deref() == Some(OsStr::new(&_claim.event_id)) {
            std::process::exit(86);
        }
    }

    fn finalize_success(&mut self, claim: &Self::Claim) -> Result<bool> {
        self.store.finalize_subscription_delivery_success(
            &claim.subscription.id,
            &claim.event_id,
            &claim.lease_token,
            now_ms(),
        )
    }

    fn finalize_failure(&mut self, claim: &Self::Claim, failure: DeliveryFailure) -> Result<bool> {
        self.store.finalize_subscription_delivery_failure(
            &claim.subscription.id,
            &claim.event_id,
            &claim.lease_token,
            now_ms(),
            failure.timed_out,
            failure.code,
        )
    }
}

pub(crate) fn run(context: DispatcherContext) -> Result<DispatcherReport> {
    install_signal_handlers()?;
    let once = context.once;
    let mut backend = SystemBackend::open(context)?;
    drive(&mut backend, once)
}

pub(crate) fn command(values: Vec<OsString>) -> Result<()> {
    let args = parse_args(values)?;
    if args.help {
        io::stdout().write_all(HELP.as_bytes())?;
        return Ok(());
    }
    if args.version {
        writeln!(
            io::stdout(),
            "kanban-dispatcher {}",
            env!("CARGO_PKG_VERSION")
        )?;
        return Ok(());
    }
    let json = args.json;
    let report = run(resolve_context(args)?)?;
    if json {
        serde_json::to_writer(io::stdout().lock(), &report)?;
        writeln!(io::stdout())?;
    } else {
        writeln!(
            io::stdout(),
            "attempted={} succeeded={} failed={} idle={} busy={} cancelled={}",
            report.attempted,
            report.succeeded,
            report.failed,
            report.idle,
            report.busy,
            report.cancelled
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn reset_cancellation() {
    CANCELLED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::symlink;
    use uuid::Uuid;

    #[derive(Clone)]
    struct FakeCandidate {
        timeout_ms: i64,
    }

    struct FakeBackend {
        trace: Vec<String>,
        candidate: Option<FakeCandidate>,
        lock_available: bool,
        claim_available: bool,
        invoke_failure: Option<DeliveryFailure>,
        cancellation_checks: Cell<usize>,
        cancel_at_check: Option<usize>,
        resolve_count: usize,
    }

    impl FakeBackend {
        fn success() -> Self {
            Self {
                trace: Vec::new(),
                candidate: Some(FakeCandidate { timeout_ms: 7_000 }),
                lock_available: true,
                claim_available: true,
                invoke_failure: None,
                cancellation_checks: Cell::new(0),
                cancel_at_check: None,
                resolve_count: 0,
            }
        }
    }

    impl SchedulerBackend for FakeBackend {
        type Candidate = FakeCandidate;
        type Resolved = String;
        type ConsumerLock = ();
        type Claim = String;

        fn cancelled(&self) -> bool {
            let check = self.cancellation_checks.get() + 1;
            self.cancellation_checks.set(check);
            self.cancel_at_check == Some(check)
        }

        fn prepare(&mut self) -> Result<Option<Self::Candidate>> {
            self.trace.push("prepare".into());
            Ok(self.candidate.take())
        }

        fn resolve(&mut self, _candidate: &Self::Candidate) -> Result<Self::Resolved> {
            self.resolve_count += 1;
            self.trace.push(format!("resolve-{}", self.resolve_count));
            Ok("consumer.test".into())
        }

        fn consumer_id<'a>(&self, resolved: &'a Self::Resolved) -> &'a str {
            resolved
        }

        fn timeout_ms(&self, candidate: &Self::Candidate) -> i64 {
            candidate.timeout_ms
        }

        fn try_lock(&mut self, consumer_id: &str) -> Result<Option<Self::ConsumerLock>> {
            self.trace.push(format!("lock:{consumer_id}"));
            Ok(self.lock_available.then_some(()))
        }

        fn claim(
            &mut self,
            _candidate: &Self::Candidate,
            lease_duration_ms: i64,
        ) -> Result<Option<Self::Claim>> {
            self.trace.push(format!("claim:{lease_duration_ms}"));
            Ok(self.claim_available.then(|| "lease-token".into()))
        }

        fn invoke(
            &mut self,
            claim: &Self::Claim,
            _resolved: &Self::Resolved,
        ) -> std::result::Result<(), DeliveryFailure> {
            self.trace.push(format!("invoke:{claim}"));
            match self.invoke_failure {
                Some(failure) => Err(failure),
                None => Ok(()),
            }
        }

        fn after_adapter_success(&mut self, claim: &Self::Claim) {
            self.trace.push(format!("after-success:{claim}"));
        }

        fn finalize_success(&mut self, claim: &Self::Claim) -> Result<bool> {
            self.trace.push(format!("success:{claim}"));
            Ok(true)
        }

        fn finalize_failure(
            &mut self,
            claim: &Self::Claim,
            failure: DeliveryFailure,
        ) -> Result<bool> {
            self.trace.push(format!(
                "failure:{claim}:{}:{}",
                failure.code, failure.timed_out
            ));
            Ok(true)
        }

        fn wait(&mut self) {
            self.trace.push("wait".into());
        }
    }

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse(values: &[&str]) -> Result<DispatcherArgs> {
        parse_args_with_env(strings(values), None, None)
    }

    #[test]
    fn parser_accepts_the_exact_surface_and_inline_values() {
        assert_eq!(
            parse(&[
                "--db=/tmp/board.db",
                "--consumer",
                "worker",
                "--once",
                "--json"
            ])
            .unwrap(),
            DispatcherArgs {
                selector: Some(BoardSelector::Db("/tmp/board.db".into())),
                consumer: Some("worker".into()),
                once: true,
                json: true,
                help: false,
                version: false,
            }
        );
        assert!(matches!(
            parse(&["--project", "demo"]).unwrap().selector,
            Some(BoardSelector::Project(name)) if name == "demo"
        ));
        assert!(matches!(
            parse(&["--workspace", "/tmp"]).unwrap().selector,
            Some(BoardSelector::Workspace(path)) if path == Path::new("/tmp")
        ));
    }

    #[test]
    fn parser_rejects_unknown_positionals_missing_empty_repeated_and_conflicting_values() {
        for values in [
            vec!["--db", "/tmp/a", "extra"],
            vec!["--unknown"],
            vec!["--db"],
            vec!["--db="],
            vec!["--db", "/tmp/a", "--db", "/tmp/a"],
            vec!["--db", "/tmp/a", "--project", "demo"],
            vec!["--once", "--once", "--db", "/tmp/a"],
            vec!["--once=true", "--db", "/tmp/a"],
            vec!["--consumer", "", "--db", "/tmp/a"],
            vec!["--consumer", "../worker", "--db", "/tmp/a"],
            vec!["--consumer", "--once", "--db", "/tmp/a"],
            vec![],
            vec!["--help", "--version"],
        ] {
            assert!(parse(&values).is_err(), "accepted {values:?}");
        }
    }

    #[test]
    fn help_and_version_do_not_require_a_selector() {
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["--version"]).unwrap().version);
    }

    #[test]
    fn matching_selector_environment_is_accepted() {
        let db =
            parse_args_with_env(strings(&["--db", "/tmp/a"]), Some("/tmp/a".into()), None).unwrap();
        assert!(matches!(db.selector, Some(BoardSelector::Db(_))));
        let project =
            parse_args_with_env(strings(&["--project", "demo"]), None, Some("demo".into()))
                .unwrap();
        assert!(matches!(project.selector, Some(BoardSelector::Project(_))));
    }

    #[test]
    fn every_selector_environment_conflict_fails_closed() {
        let cases = [
            (vec!["--db", "/tmp/a"], Some("/tmp/b"), None),
            (vec!["--db", "/tmp/a"], None, Some("demo")),
            (vec!["--project", "demo"], Some("/tmp/a"), None),
            (vec!["--project", "demo"], None, Some("other")),
            (vec!["--workspace", "/tmp"], Some("/tmp/a"), None),
            (vec!["--workspace", "/tmp"], None, Some("demo")),
            (vec!["--db", "/tmp/a"], Some("/tmp/a"), Some("demo")),
        ];
        for (values, db, project) in cases {
            assert!(
                parse_args_with_env(
                    strings(&values),
                    db.map(OsString::from),
                    project.map(OsString::from),
                )
                .is_err(),
                "accepted {values:?} with db={db:?} project={project:?}"
            );
        }
    }

    #[test]
    fn a_missing_direct_database_is_rejected_without_creation() {
        let path = env::temp_dir().join(format!(
            "kanban-dispatcher-missing-{}-{}.db",
            std::process::id(),
            Uuid::new_v4()
        ));
        let args =
            parse_args_with_env(vec!["--db".into(), path.as_os_str().to_owned()], None, None)
                .unwrap();
        let error = match resolve_context(args) {
            Ok(_) => panic!("missing database resolved"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not present"), "{error:#}");
        assert!(!path.exists());
    }

    #[test]
    fn signal_handler_only_marks_the_cancellation_flag() {
        reset_cancellation();
        assert!(!cancellation_flag().load(Ordering::SeqCst));
        handle_signal(libc::SIGTERM);
        assert!(cancellation_flag().load(Ordering::SeqCst));
        reset_cancellation();
    }

    #[test]
    fn direct_external_database_resolution_holds_no_data_root_lock() {
        let dir = env::temp_dir().join(format!(
            "kanban-dispatcher-board-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("board.db");
        let mut store = Store::open(&path).unwrap();
        store
            .initialize("dispatcher-test", "test@dispatcher")
            .unwrap();
        drop(store);
        let args =
            parse_args_with_env(vec!["--db".into(), path.as_os_str().to_owned()], None, None)
                .unwrap();
        let context = resolve_context(args).unwrap();
        assert_eq!(context.board_path, path.canonicalize().unwrap());
        assert_eq!(context.board_name.as_deref(), Some("dispatcher-test"));
        assert!(context._data_root_lock.is_none());
        drop(context);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn direct_symlink_resolution_uses_the_stable_canonical_board() {
        let dir = env::temp_dir().join(format!(
            "kanban-dispatcher-symlink-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target.db");
        let link = dir.join("link.db");
        let mut store = Store::open(&target).unwrap();
        store
            .initialize("dispatcher-link", "test@dispatcher")
            .unwrap();
        drop(store);
        symlink(&target, &link).unwrap();
        let args =
            parse_args_with_env(vec!["--db".into(), link.as_os_str().to_owned()], None, None)
                .unwrap();
        let context = resolve_context(args).unwrap();
        assert_eq!(context.board_path, target.canonicalize().unwrap());
        assert_eq!(context.board_name.as_deref(), Some("dispatcher-link"));
        drop(context);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn scheduler_reloads_config_under_lock_then_claims_with_exact_headroom() {
        let mut backend = FakeBackend::success();
        assert_eq!(
            scheduler_step(&mut backend).unwrap(),
            StepOutcome::Succeeded
        );
        assert_eq!(
            backend.trace,
            [
                "prepare",
                "resolve-1",
                "lock:consumer.test",
                "resolve-2",
                "claim:37000",
                "invoke:lease-token",
                "after-success:lease-token",
                "success:lease-token",
            ]
        );
    }

    #[test]
    fn scheduler_cancellation_after_config_reload_never_claims() {
        let mut backend = FakeBackend::success();
        backend.cancel_at_check = Some(2);
        assert_eq!(
            scheduler_step(&mut backend).unwrap(),
            StepOutcome::Cancelled
        );
        assert_eq!(
            backend.trace,
            ["prepare", "resolve-1", "lock:consumer.test", "resolve-2"]
        );
    }

    #[test]
    fn scheduler_failure_finalizes_the_exact_claim_and_safe_code() {
        let mut backend = FakeBackend::success();
        backend.invoke_failure = Some(DeliveryFailure {
            code: "adapter_timeout",
            timed_out: true,
        });
        assert_eq!(scheduler_step(&mut backend).unwrap(), StepOutcome::Failed);
        assert_eq!(
            backend.trace.last().map(String::as_str),
            Some("failure:lease-token:adapter_timeout:true")
        );
        assert!(
            !backend
                .trace
                .iter()
                .any(|entry| entry.starts_with("after-success:"))
        );
    }

    #[test]
    fn once_processes_at_most_one_delivery_without_waiting() {
        let mut backend = FakeBackend::success();
        let report = drive(&mut backend, true).unwrap();
        assert_eq!(
            report,
            DispatcherReport {
                attempted: 1,
                succeeded: 1,
                failed: 0,
                idle: false,
                busy: false,
                cancelled: false,
            }
        );
        assert_eq!(
            backend
                .trace
                .iter()
                .filter(|entry| entry.as_str() == "prepare")
                .count(),
            1
        );
        assert!(!backend.trace.iter().any(|entry| entry == "wait"));
    }

    #[test]
    fn long_poll_exits_cleanly_when_cancelled_during_idle() {
        let mut backend = FakeBackend::success();
        backend.candidate = None;
        backend.cancel_at_check = Some(2);
        let report = drive(&mut backend, false).unwrap();
        assert!(report.idle);
        assert!(report.cancelled);
        assert_eq!(backend.trace, ["prepare"]);
    }

    #[test]
    fn busy_consumer_never_claims() {
        let mut backend = FakeBackend::success();
        backend.lock_available = false;
        assert_eq!(scheduler_step(&mut backend).unwrap(), StepOutcome::Busy);
        assert_eq!(
            backend.trace,
            ["prepare", "resolve-1", "lock:consumer.test"]
        );
    }
}
