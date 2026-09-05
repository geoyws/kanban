//! The registry-owning Unix-domain-socket broker (ADR-038, ADR-033).
//!
//! This module is the authentication boundary. It owns the [`Broker`]'s
//! `UnixListener`, resolves the connecting process's kernel peer credentials
//! (`SO_PEERCRED` on Linux, `getpeereid` on macOS/BSD), applies ADR-033's
//! two-way passwd check, and mints the sealed [`PrincipalContext`] that policy
//! and store code receive by reference. It also owns the file-ownership
//! boundary (clause 9), the named service-principal [`ClientKind`]s, and the
//! explicit offline-maintenance mode.
//!
//! Deliberately absent here: the `access` command grammar and generated
//! schemas (t-86eb4fb3), the CLI/MCP routing hop (t-f2aa39aa), and the
//! compiled-binary refusal matrix. Those slices consume this module; nothing
//! in this module consumes them.

use crate::policy::{PolicyActor, PolicyContext};
use crate::registry::Registry;
use anyhow::{Context, Result, bail};
use std::fs;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Kernel peer credentials (ADR-038 clause 2).
// ---------------------------------------------------------------------------

/// The kernel's answer to "which process connected to this socket". `uid` is
/// the principal evidence; `pid` and `gid` are recorded, never authorized on.
///
/// The fields and constructor are private: a [`PeerCredentials`] can only be
/// produced by the kernel ([`KernelPeerCredentials`]) or by a test fake in
/// this module, never from request data. That is what makes peer-credential
/// authentication structurally non-forgeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pid: Option<i32>,
    uid: u32,
    gid: u32,
}

impl PeerCredentials {
    fn new(pid: Option<i32>, uid: u32, gid: u32) -> Self {
        Self { pid, uid, gid }
    }

    pub fn pid(&self) -> Option<i32> {
        self.pid
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }
}

/// A source of kernel peer credentials, abstracted so the syscall is
/// unit-testable. Only this module (and its test child) can implement it,
/// because a [`PeerCredentials`] value cannot be constructed elsewhere.
pub trait PeerCredentialsSource {
    fn peer_credentials(&self, fd: RawFd) -> Result<PeerCredentials>;
}

/// The real kernel peer-credential source.
pub struct KernelPeerCredentials;

impl PeerCredentialsSource for KernelPeerCredentials {
    fn peer_credentials(&self, fd: RawFd) -> Result<PeerCredentials> {
        kernel_peer_credentials(fd)
    }
}

/// Linux: `getsockopt(fd, SOL_SOCKET, SO_PEERCRED)` reads `struct ucred`.
/// The kernel recorded the connecting process's credentials at `connect(2)`;
/// an unprivileged process cannot forge or relay them.
#[cfg(target_os = "linux")]
fn kernel_peer_credentials(fd: RawFd) -> Result<PeerCredentials> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(PeerCredentials::new(Some(cred.pid), cred.uid, cred.gid))
}

/// macOS/BSD: `getpeereid` returns the peer's effective UID and GID, without a
/// PID. This is the devbox/test primitive only: ADR-033's audit matrix admits
/// only `kernel_so_peercred*` and `kernel_peercred_unavailable`, so getpeereid
/// is not a managed-mode evidence source.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn kernel_peer_credentials(fd: RawFd) -> Result<PeerCredentials> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("getpeereid");
    }
    Ok(PeerCredentials::new(None, uid, gid))
}

/// Any other platform: deny under `kernel_peercred_unavailable` rather than
/// inventing an unauthenticated evidence source (ADR-033).
#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn kernel_peer_credentials(_fd: RawFd) -> Result<PeerCredentials> {
    bail!("kernel peer credentials are unavailable on this platform")
}

/// How the connecting process proved its identity. The evidence source is
/// decided by the host kernel, never by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    /// Linux `SO_PEERCRED`: managed-mode evidence (ADR-033).
    KernelSoPeercred,
    /// macOS/BSD `getpeereid`: compiles and tests on the devbox; not managed-mode evidence.
    KernelPeercred,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceSource::KernelSoPeercred => "kernel_so_peercred",
            EvidenceSource::KernelPeercred => "kernel_peercred",
        }
    }

    /// Whether this evidence source is admitted as managed-mode evidence
    /// (ADR-033: only `kernel_so_peercred*`).
    pub fn is_managed_mode_evidence(self) -> bool {
        matches!(self, EvidenceSource::KernelSoPeercred)
    }
}

fn kernel_evidence_source() -> EvidenceSource {
    #[cfg(target_os = "linux")]
    {
        EvidenceSource::KernelSoPeercred
    }
    #[cfg(not(target_os = "linux"))]
    {
        EvidenceSource::KernelPeercred
    }
}

// ---------------------------------------------------------------------------
// Passwd database and the two-way check (ADR-033, ADR-038 clause 1).
// ---------------------------------------------------------------------------

/// A host passwd database, abstracted so the two-way check is unit-testable
/// without mutating the real passwd file.
pub trait PasswdDatabase {
    /// `getpwuid(uid).pw_name`, or `None` when the UID is unresolved.
    fn name_for_uid(&self, uid: u32) -> Result<Option<String>>;
    /// `getpwnam(name).pw_uid`, or `None` when the name is unresolved.
    fn uid_for_name(&self, name: &str) -> Result<Option<u32>>;
}

/// The real host passwd database, via `getpwuid_r` / `getpwnam_r`.
pub struct SystemPasswd;

impl PasswdDatabase for SystemPasswd {
    fn name_for_uid(&self, uid: u32) -> Result<Option<String>> {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0_u8; 16384];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc)).context("getpwuid_r");
        }
        if result.is_null() {
            return Ok(None);
        }
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }
            .to_string_lossy()
            .into_owned();
        Ok(Some(name))
    }

    fn uid_for_name(&self, name: &str) -> Result<Option<u32>> {
        let cname = std::ffi::CString::new(name).context("username contains NUL")?;
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut buf = vec![0_u8; 16384];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast::<libc::c_char>(),
                buf.len(),
                &mut result,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc)).context("getpwnam_r");
        }
        if result.is_null() {
            return Ok(None);
        }
        Ok(Some(pwd.pw_uid))
    }
}

/// ADR-033's two-way passwd check: `getpwuid(uid).pw_name == username` AND
/// `getpwnam(username).pw_uid == uid`.
///
/// A one-way check lets a renamed or reused account impersonate: a UID can be
/// handed to a new person while the name still resolves, or a name can be
/// handed to a new person while the UID still resolves. Both directions must
/// agree on the *same* frozen pair, or the authenticator denies.
pub fn two_way_passwd_check(db: &dyn PasswdDatabase, username: &str, uid: u32) -> Result<bool> {
    let forward = matches!(db.name_for_uid(uid)?, Some(name) if name == username);
    let reverse = matches!(db.uid_for_name(username)?, Some(resolved) if resolved == uid);
    Ok(forward && reverse)
}

// ---------------------------------------------------------------------------
// Client kinds and named service principals (ADR-038 clause 3, ADR-033).
// ---------------------------------------------------------------------------

/// The kind of client a minted context was created for. Non-human callers
/// (dispatchers, adapters, backup/restore/archive/search, MCP command
/// processes) are service principals: they resolve through exactly the same
/// kernel-credential path as human callers and receive the grants of the
/// principal bound to their UID — no exemption, no bypass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    Cli,
    McpCommand,
    Web,
    Dispatcher,
    Adapter,
    Backup,
    Restore,
    Archive,
    Search,
}

impl ClientKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClientKind::Cli => "cli",
            ClientKind::McpCommand => "mcp_command",
            ClientKind::Web => "web",
            ClientKind::Dispatcher => "dispatcher",
            ClientKind::Adapter => "adapter",
            ClientKind::Backup => "backup",
            ClientKind::Restore => "restore",
            ClientKind::Archive => "archive",
            ClientKind::Search => "search",
        }
    }

    /// Whether this is a non-human (service) caller.
    pub fn is_service(self) -> bool {
        !matches!(self, ClientKind::Cli | ClientKind::Web)
    }
}

/// How the peer authenticated. This slice mints only `SocketPeer`; the
/// `sso_proxy`, `root_bootstrap`, and `root_breakglass` kinds are the
/// grammar slice's (t-86eb4fb3) concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthnKind {
    SocketPeer,
}

impl AuthnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthnKind::SocketPeer => "socket_peer",
        }
    }
}

// ---------------------------------------------------------------------------
// Explicit offline maintenance mode (ADR-033).
// ---------------------------------------------------------------------------

/// The broker's operating mode. Offline maintenance is a deliberate, named
/// broker operation under an exclusive registry-admin lock — never an implicit
/// fallback, and never what a missing socket silently produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerMode {
    /// Serving clients over the Unix socket.
    Online,
    /// Deliberately offline for maintenance.
    OfflineMaintenance,
}

impl BrokerMode {
    /// Resolve the mode from the operator's explicit, named instruction. An
    /// absent instruction is `Online`; the socket's existence is deliberately
    /// not an input.
    pub fn resolve(instruction: Option<&str>) -> Result<BrokerMode> {
        match instruction {
            None => Ok(BrokerMode::Online),
            Some("online") => Ok(BrokerMode::Online),
            Some("offline-maintenance") => Ok(BrokerMode::OfflineMaintenance),
            Some(other) => bail!(
                "unknown broker mode {other:?}; expected \"online\" or \"offline-maintenance\""
            ),
        }
    }
}

/// The decision for a direct (non-brokered) database open, given the explicit
/// mode and whether the broker socket is currently serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectOpen {
    /// Explicit offline maintenance: permitted, as a broker operation under
    /// the exclusive registry-admin lock.
    Permitted,
    /// Online and the socket is up: route through the broker.
    RefusedOnline,
    /// Online and the socket is down: a hard failure, never a fallback to
    /// direct access.
    RefusedSocketDown,
}

/// ADR-033: "Offline maintenance is a broker operation under an exclusive
/// registry-admin lock, not a direct-SQL compatibility path." A socket that is
/// down while the broker is `Online` is reported, never read as permission to
/// open directly; only the explicit `OfflineMaintenance` mode permits direct
/// access.
pub fn direct_open_decision(mode: BrokerMode, socket_serving: bool) -> DirectOpen {
    match mode {
        BrokerMode::OfflineMaintenance => DirectOpen::Permitted,
        BrokerMode::Online if socket_serving => DirectOpen::RefusedOnline,
        BrokerMode::Online => DirectOpen::RefusedSocketDown,
    }
}

// ---------------------------------------------------------------------------
// The file-ownership boundary (ADR-038 clause 9).
// ---------------------------------------------------------------------------

/// The ownership rule the broker enforces before serving. Derived from
/// ADR-038 clause 9 ("the file and its directory are owned by the broker's UID
/// and unreadable to the caller") and ADR-033 ("unreadable and unwritable by
/// ... clients"): every path the broker serves must be owned by the broker's
/// effective UID and carry no group/other permission bits (directory `0700`,
/// file `0600`).
#[derive(Debug, Clone, Copy)]
pub struct OwnershipRule {
    expected_uid: u32,
}

impl OwnershipRule {
    /// The rule for the current process: the broker's effective UID.
    pub fn current() -> Self {
        Self {
            expected_uid: unsafe { libc::geteuid() },
        }
    }

    /// A rule expecting a specific owner. Used by tests to simulate a
    /// foreign-owned path without requiring privilege to chown.
    pub fn expecting(uid: u32) -> Self {
        Self { expected_uid: uid }
    }

    fn verify(path: &Path, expected_uid: u32, label: &str) -> Result<()> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("inspect {label} {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!(
                "{label} {} is a symlink; refusing to serve through it",
                path.display()
            );
        }
        if metadata.uid() != expected_uid {
            bail!(
                "{label} {} is owned by uid {}, not the broker's uid {}; refusing to serve",
                path.display(),
                metadata.uid(),
                expected_uid
            );
        }
        let mode = metadata.mode() & 0o7777;
        if mode & 0o077 != 0 {
            bail!(
                "{label} {} has mode {mode:o}, which grants group/other access; refusing to serve",
                path.display()
            );
        }
        Ok(())
    }

    /// The socket's parent directory: owned by the broker UID, mode 0700.
    pub fn verify_socket_dir(&self, dir: &Path) -> Result<()> {
        Self::verify(dir, self.expected_uid, "broker socket directory")
    }

    /// The registry database file: owned by the broker UID, mode 0600.
    pub fn verify_registry_file(&self, file: &Path) -> Result<()> {
        Self::verify(file, self.expected_uid, "registry database file")
    }

    /// The socket file itself: owned by the broker UID, no group/other bits.
    pub fn verify_socket(&self, socket: &Path) -> Result<()> {
        Self::verify(socket, self.expected_uid, "broker socket")
    }
}

// ---------------------------------------------------------------------------
// The sealed PrincipalContext (ADR-038 clause 3).
// ---------------------------------------------------------------------------

/// The authenticated identity minted once, at the boundary, from kernel peer
/// credentials plus the resolved principal. Sealed: every field is private, the
/// constructor is private to this module, there is no serialization, no
/// deserialization, and no test-only production constructor. It is immutable
/// and cannot be reconstructed from request data — no client-controlled actor
/// string, environment variable, JSON field, or board/project selector can
/// contribute to it.
pub struct PrincipalContext {
    principal_id: String,
    username: String,
    uid: u32,
    authn_kind: AuthnKind,
    evidence_source: EvidenceSource,
    peer_pid: Option<i32>,
    peer_gid: u32,
    /// The live policy epoch at mint, compared for equality at commit
    /// (clause 8).
    epoch: i64,
    /// The resulting policy state hash at mint, compared with the epoch at
    /// commit (clause 8).
    state_hash: String,
    request_id: String,
    client_kind: ClientKind,
    /// The audit context (claimed actor, reason, ...) — a claim beside the
    /// identity, never part of it.
    context: PolicyContext,
}

impl PrincipalContext {
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn authn_kind(&self) -> &str {
        self.authn_kind.as_str()
    }

    pub fn evidence_source(&self) -> &str {
        self.evidence_source.as_str()
    }

    pub fn peer_pid(&self) -> Option<i32> {
        self.peer_pid
    }

    pub fn peer_gid(&self) -> u32 {
        self.peer_gid
    }

    pub fn epoch(&self) -> i64 {
        self.epoch
    }

    pub fn state_hash(&self) -> &str {
        &self.state_hash
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub fn client_kind(&self) -> ClientKind {
        self.client_kind
    }

    pub fn context(&self) -> &PolicyContext {
        &self.context
    }

    /// The read-only projection into the registry-side actor data that policy
    /// and store code consume. This is the clause-8 bridge: the actor carries
    /// the minted epoch and state hash, which `require_context` re-checks for
    /// equality immediately before commit.
    pub fn as_policy_actor(&self) -> PolicyActor {
        PolicyActor {
            principal_id: Some(self.principal_id.clone()),
            username: self.username.clone(),
            uid: self.uid,
            epoch: self.epoch,
            state_hash: self.state_hash.clone(),
            context: self.context.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Minting, reachable only from the boundary.
// ---------------------------------------------------------------------------

/// Mint one sealed [`PrincipalContext`] from kernel credentials, the host
/// passwd database, and the registry. Fails closed (`denied or not found`) on
/// an unresolved UID, a two-way passwd divergence, or a UID that resolves to
/// no enabled principal.
///
/// Private: the only callers are [`Broker::accept_principal`] (this module)
/// and the unit tests (a child module). Neither `request_id` nor
/// `claimed_actor` nor `client_kind` is identity; only the kernel credentials,
/// the passwd database, and the registry's frozen principals determine
/// `username`, `uid`, and `principal_id`.
fn mint_principal_context(
    creds: &PeerCredentials,
    passwd: &dyn PasswdDatabase,
    registry: &Registry,
    client_kind: ClientKind,
    request_id: String,
    claimed_actor: Option<String>,
) -> Result<PrincipalContext> {
    // 1. Resolve the kernel UID to a username (getpwuid). Unresolved → deny.
    let username = match passwd.name_for_uid(creds.uid())? {
        Some(name) => name,
        None => bail!("denied or not found"),
    };
    // 2. Two-way passwd check. Divergence (renamed/reused account) → deny.
    if !two_way_passwd_check(passwd, &username, creds.uid())? {
        bail!("denied or not found");
    }
    // 3. Resolve the frozen principal in the registry. No enabled principal
    //    for this pair → deny (clause 6).
    let principal = match registry.resolve_principal(&username, creds.uid())? {
        Some(principal) => principal,
        None => bail!("denied or not found"),
    };
    // 4. Capture the live epoch and state hash at mint (clause 8).
    let (epoch, state_hash) = registry.live_policy_state()?;
    let authn_kind = AuthnKind::SocketPeer;
    let context = PolicyContext {
        authn_kind: authn_kind.as_str().to_owned(),
        peer_uid: creds.uid(),
        real_uid: None,
        effective_uid: None,
        client_kind: client_kind.as_str().to_owned(),
        request_id: request_id.clone(),
        claimed_actor,
        reason: None,
        provider: None,
        subject: None,
    };
    Ok(PrincipalContext {
        principal_id: principal.row.id,
        username,
        uid: creds.uid(),
        authn_kind,
        evidence_source: kernel_evidence_source(),
        peer_pid: creds.pid(),
        peer_gid: creds.gid(),
        epoch,
        state_hash,
        request_id,
        client_kind,
        context,
    })
}

// ---------------------------------------------------------------------------
// The broker.
// ---------------------------------------------------------------------------

/// The registry-owning Unix-domain-socket broker. It binds a `UnixListener`
/// inside the registry's data root — behind the [`OwnershipRule`] boundary —
/// and mints a sealed [`PrincipalContext`] per accepted connection from the
/// kernel peer credentials.
pub struct Broker {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl Broker {
    /// Bind the broker socket inside the registry's data root, after verifying
    /// the file-ownership boundary. Refuses to serve from a directory, socket,
    /// or registry file it does not own (ADR-038 clause 9).
    pub fn bind(registry: &Registry, socket_name: &str) -> Result<Broker> {
        let root = registry.data_root_path();
        let rule = OwnershipRule::current();
        rule.verify_socket_dir(root)?;
        rule.verify_registry_file(&root.join("registry.db"))?;
        let socket_path = root.join(socket_name);
        match fs::symlink_metadata(&socket_path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!(
                        "broker socket {} is a symlink; refusing to serve",
                        socket_path.display()
                    );
                }
                rule.verify_socket(&socket_path)?;
                fs::remove_file(&socket_path).context("remove stale broker socket")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("inspect {}", socket_path.display()));
            }
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        // Narrow the freshly bound socket to 0600 and re-verify ownership.
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("secure {}", socket_path.display()))?;
        rule.verify_socket(&socket_path)?;
        Ok(Broker {
            listener,
            socket_path,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept one connection.
    pub fn accept(&self) -> Result<UnixStream> {
        let (stream, _addr) = self.listener.accept().context("accept broker connection")?;
        Ok(stream)
    }

    /// Authenticate an accepted connection by kernel peer credential and mint
    /// its sealed [`PrincipalContext`]. This is the only public route to a
    /// context; `--db` and every other direct-open path cannot reach it.
    pub fn accept_principal(
        &self,
        stream: &UnixStream,
        registry: &Registry,
        client_kind: ClientKind,
        request_id: String,
        claimed_actor: Option<String>,
    ) -> Result<PrincipalContext> {
        let creds = KernelPeerCredentials.peer_credentials(stream.as_raw_fd())?;
        mint_principal_context(
            &creds,
            &SystemPasswd,
            registry,
            client_kind,
            request_id,
            claimed_actor,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AddTask;
    use crate::store::Store;
    use serde_json::Value;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use uuid::Uuid;

    // -- helpers ------------------------------------------------------------

    struct FakePasswd {
        uid_to_name: HashMap<u32, String>,
        name_to_uid: HashMap<String, u32>,
    }

    impl FakePasswd {
        fn pair(name: &str, uid: u32) -> Self {
            let mut uid_to_name = HashMap::new();
            let mut name_to_uid = HashMap::new();
            uid_to_name.insert(uid, name.to_owned());
            name_to_uid.insert(name.to_owned(), uid);
            Self {
                uid_to_name,
                name_to_uid,
            }
        }

        /// Forward matches (`getpwuid(uid).pw_name == name`) but the reverse
        /// diverges (`getpwnam(name).pw_uid == now_at_uid != uid`): the name
        /// was reassigned to a different person.
        fn renamed(name: &str, uid: u32, now_at_uid: u32) -> Self {
            let mut uid_to_name = HashMap::new();
            let mut name_to_uid = HashMap::new();
            uid_to_name.insert(uid, name.to_owned());
            name_to_uid.insert(name.to_owned(), now_at_uid);
            Self {
                uid_to_name,
                name_to_uid,
            }
        }

        /// Reverse matches (`getpwnam(name).pw_uid == uid`) but the forward
        /// diverges (`getpwuid(uid).pw_name == now_name != name`): the UID was
        /// reused for a different person.
        fn reused_uid(name: &str, uid: u32, now_name: &str) -> Self {
            let mut uid_to_name = HashMap::new();
            let mut name_to_uid = HashMap::new();
            uid_to_name.insert(uid, now_name.to_owned());
            name_to_uid.insert(name.to_owned(), uid);
            Self {
                uid_to_name,
                name_to_uid,
            }
        }
    }

    impl PasswdDatabase for FakePasswd {
        fn name_for_uid(&self, uid: u32) -> Result<Option<String>> {
            Ok(self.uid_to_name.get(&uid).cloned())
        }

        fn uid_for_name(&self, name: &str) -> Result<Option<u32>> {
            Ok(self.name_to_uid.get(name).copied())
        }
    }

    fn test_registry(name: &str) -> Registry {
        let root = std::env::temp_dir().join(format!(
            "kanban-broker-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create temp broker dir");
        Registry::open_test_at(&root).expect("open test registry")
    }

    fn actor(
        principal_id: Option<&str>,
        username: &str,
        uid: u32,
        epoch: i64,
        hash: &str,
    ) -> PolicyActor {
        PolicyActor {
            principal_id: principal_id.map(str::to_owned),
            username: username.to_owned(),
            uid,
            epoch,
            state_hash: hash.to_owned(),
            context: PolicyContext {
                authn_kind: "socket_peer".to_owned(),
                peer_uid: uid,
                real_uid: None,
                effective_uid: None,
                client_kind: "cli".to_owned(),
                request_id: crate::policy::short_id("rq"),
                claimed_actor: Some(username.to_owned()),
                reason: Some("test".to_owned()),
                provider: None,
                subject: None,
            },
        }
    }

    /// A registry bootstrapped with one registered board and an admin
    /// principal; returns `(registry, admin_id)`.
    fn bootstrapped(name: &str) -> (Registry, String) {
        let mut registry = test_registry(name);
        registry
            .connection
            .execute(
                "INSERT INTO boards(board_path,name,created_at,last_used_at,archived) \
                 VALUES('/root/boards/b1.db','b1',1,1,0)",
                [],
            )
            .unwrap();
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let root_actor = actor(None, "root", 0, epoch, &hash);
        let admin = registry
            .policy_bootstrap("geoyws", 1000, &root_actor)
            .unwrap();
        (registry, admin.row.id.clone())
    }

    struct EnvGuard(&'static str, Option<std::ffi::OsString>);

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> EnvGuard {
            let previous = std::env::var_os(key);
            // SAFETY: serialized by ENV_LOCK; no other thread reads this key
            // while the guard is alive.
            unsafe { std::env::set_var(key, value) };
            EnvGuard(key, previous)
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => unsafe { std::env::set_var(self.0, value) },
                None => unsafe { std::env::remove_var(self.0) },
            }
        }
    }

    // -- peer credentials (item 2) -----------------------------------------

    #[test]
    fn kernel_peer_credentials_reads_the_connected_peer_on_this_host() {
        let (a, b) = UnixStream::pair().unwrap();
        let creds = KernelPeerCredentials
            .peer_credentials(a.as_raw_fd())
            .unwrap();
        // The peer of a socketpair is this process, so the UID/GID must match
        // the caller's own. This exercises the real primitive on the host
        // (getpeereid on macOS), but is NOT Linux peer-credential acceptance.
        assert_eq!(creds.uid(), unsafe { libc::geteuid() });
        assert_eq!(creds.gid(), unsafe { libc::getegid() });
        #[cfg(target_os = "linux")]
        assert!(creds.pid().is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(creds.pid().is_none());
        drop((a, b));
    }

    // -- two-way passwd check (item 2) -------------------------------------

    #[test]
    fn two_way_passwd_check_refuses_a_one_way_match() {
        // Forward matches, reverse diverges: the name was reassigned.
        let renamed = FakePasswd::renamed("alice", 1001, 2000);
        assert!(!two_way_passwd_check(&renamed, "alice", 1001).unwrap());
        // Reverse matches, forward diverges: the UID was reused.
        let reused = FakePasswd::reused_uid("alice", 1001, "mallory");
        assert!(!two_way_passwd_check(&reused, "alice", 1001).unwrap());
        // Both directions agree on the same pair: a live, self-consistent pair.
        let consistent = FakePasswd::pair("alice", 1001);
        assert!(two_way_passwd_check(&consistent, "alice", 1001).unwrap());
    }

    #[test]
    fn mint_fails_closed_when_the_passwd_pair_diverges() {
        let (mut registry, admin_id) = bootstrapped("mint-diverges");
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let admin = actor(Some(&admin_id), "geoyws", 1000, epoch, &hash);
        registry
            .bind_principal("alice", 1001, &[], &admin)
            .expect("bind alice");

        // The forward direction resolves (getpwuid says "alice") but the
        // reverse diverges (getpwnam says "alice" is now uid 2000). Without
        // the two-way check, resolve_principal would find the bound "alice"
        // and the mint would succeed; the check must deny it.
        let creds = PeerCredentials::new(Some(48213), 1001, 1001);
        let passwd = FakePasswd::renamed("alice", 1001, 2000);
        let result = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Cli,
            "rq-diverges".to_owned(),
            Some("geoyws".to_owned()),
        );
        assert!(result.is_err(), "a divergent passwd pair must fail closed");
    }

    // -- sealed context (item 3) -------------------------------------------

    #[test]
    fn forged_actor_selector_or_env_value_cannot_change_the_resolved_principal() {
        // Serialize with every other environment-mutating test (the canonical
        // dispatch env guard).
        let _guard = crate::dispatch::tests::env_guard();
        // Forge the classic sudo-spoof vector and a DB selector environment
        // default. Neither may reach the minted identity.
        let _sudo = EnvGuard::set("SUDO_USER", "mallory");
        let _db = EnvGuard::set("KANBAN_DB", "/tmp/forged/board.db");

        let (mut registry, admin_id) = bootstrapped("forged-actor");
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let admin = actor(Some(&admin_id), "geoyws", 1000, epoch, &hash);
        let alice = registry
            .bind_principal("alice", 1001, &[], &admin)
            .expect("bind alice");

        let creds = PeerCredentials::new(Some(48213), 1001, 1001);
        let passwd = FakePasswd::pair("alice", 1001);
        let ctx = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Cli,
            "rq-forged".to_owned(),
            Some("mallory".to_owned()),
        )
        .expect("mint alice");

        // Identity comes from kernel peer creds + passwd + the frozen
        // principal, never from the forged actor string or environment.
        assert_eq!(ctx.username(), "alice");
        assert_eq!(ctx.uid(), 1001);
        assert_eq!(ctx.principal_id(), alice.row.id);
        // The forged actor is recorded only as a claim, beside the identity.
        assert_eq!(ctx.context().claimed_actor.as_deref(), Some("mallory"));

        // Minting again with a different forged actor yields the identical
        // identity: the claim is not the decision.
        let ctx2 = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Cli,
            "rq-forged-2".to_owned(),
            Some("root".to_owned()),
        )
        .expect("mint alice again");
        assert_eq!(ctx2.principal_id(), alice.row.id);
        assert_eq!(ctx2.username(), "alice");
        assert_eq!(ctx2.uid(), 1001);
    }

    // -- ownership boundary (item 1) ---------------------------------------

    #[test]
    fn ownership_boundary_refuses_a_foreign_owned_socket_directory() {
        let root = std::env::temp_dir().join(format!(
            "kanban-broker-owned-dir-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let rule = OwnershipRule::expecting(unsafe { libc::geteuid() }.wrapping_add(1));
        let err = rule
            .verify_socket_dir(&root)
            .expect_err("a directory owned by a different UID must be refused");
        assert!(err.to_string().contains("refusing to serve"), "{err}");
    }

    #[test]
    fn ownership_boundary_refuses_a_foreign_owned_registry_file() {
        let root = std::env::temp_dir().join(format!(
            "kanban-broker-owned-file-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        let registry_file = root.join("registry.db");
        fs::write(&registry_file, b"").unwrap();
        let rule = OwnershipRule::expecting(unsafe { libc::geteuid() }.wrapping_add(1));
        let err = rule
            .verify_registry_file(&registry_file)
            .expect_err("a file owned by a different UID must be refused");
        assert!(err.to_string().contains("refusing to serve"), "{err}");
    }

    #[test]
    fn ownership_boundary_refuses_group_or_other_permission_bits() {
        let root = std::env::temp_dir().join(format!(
            "kanban-broker-mode-dir-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        // Owned by us, but world-readable/writable bits are set.
        let rule = OwnershipRule::current();
        let err = rule
            .verify_socket_dir(&root)
            .expect_err("a 0755 directory must be refused");
        assert!(err.to_string().contains("group/other access"), "{err}");
    }

    #[test]
    fn ownership_boundary_accepts_an_owned_private_path() {
        let root = std::env::temp_dir().join(format!(
            "kanban-broker-ok-dir-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let registry_file = root.join("registry.db");
        fs::write(&registry_file, b"").unwrap();
        fs::set_permissions(&registry_file, fs::Permissions::from_mode(0o600)).unwrap();
        let rule = OwnershipRule::current();
        rule.verify_socket_dir(&root).unwrap();
        rule.verify_registry_file(&registry_file).unwrap();
    }

    // -- service principals (item 4) ---------------------------------------

    #[test]
    fn a_service_principal_resolves_to_a_real_principal_identity() {
        let (mut registry, admin_id) = bootstrapped("svc-principal");
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let admin = actor(Some(&admin_id), "geoyws", 1000, epoch, &hash);
        let svc = registry
            .bind_principal("kanban-dispatcher", 2000, &[], &admin)
            .expect("bind service principal");

        let creds = PeerCredentials::new(Some(48214), 2000, 2000);
        let passwd = FakePasswd::pair("kanban-dispatcher", 2000);
        let ctx = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Dispatcher,
            "rq-svc".to_owned(),
            Some("claude@driver".to_owned()),
        )
        .expect("mint service principal");

        // The service caller resolves to the real bound principal — not a
        // synthetic "service" identity and not a bypass.
        assert_eq!(ctx.principal_id(), svc.row.id);
        assert_eq!(ctx.username(), "kanban-dispatcher");
        assert_eq!(ctx.uid(), 2000);
        assert!(ctx.client_kind().is_service());
        assert_eq!(ctx.client_kind(), ClientKind::Dispatcher);
        // It carries the same epoch/state-hash commitment as a human principal.
        let (live_epoch, live_hash) = registry.live_policy_state().unwrap();
        assert_eq!(ctx.epoch(), live_epoch);
        assert_eq!(ctx.state_hash(), live_hash);
    }

    // -- MCP command process (item 7) --------------------------------------

    #[test]
    fn an_mcp_command_process_is_authenticated_by_its_peer_uid_not_any_client_value() {
        // Serialize with every other environment-mutating test.
        let _guard = crate::dispatch::tests::env_guard();
        // Every client-supplied identity vector, forged. None may reach the
        // minted identity: the MCP hop (ADR-038 clause 11) carries no
        // authority, so the broker reads only the socket peer UID.
        let _sudo = EnvGuard::set("SUDO_USER", "mallory");
        let _db = EnvGuard::set("KANBAN_DB", "/tmp/forged/mcp.db");

        let (mut registry, admin_id) = bootstrapped("mcp-peer-uid");
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let admin = actor(Some(&admin_id), "geoyws", 1000, epoch, &hash);
        let mcp = registry
            .bind_principal("kanban-mcp", 1001, &[], &admin)
            .expect("bind the mcp service principal");

        // The MCP command process connects with kernel peer UID 1001; the
        // claimed actor and environment are forged and must not decide.
        let creds = PeerCredentials::new(Some(48215), 1001, 1001);
        let passwd = FakePasswd::pair("kanban-mcp", 1001);
        let ctx = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::McpCommand,
            "rq-mcp".to_owned(),
            Some("geoyws".to_owned()),
        )
        .expect("mint the mcp command principal");

        // Identity is the frozen principal resolved from the peer UID, never
        // the claimed actor, the forged env, or a caller-chosen value.
        assert_eq!(ctx.principal_id(), mcp.row.id);
        assert_eq!(ctx.username(), "kanban-mcp");
        assert_eq!(ctx.uid(), 1001);
        assert_eq!(ctx.client_kind(), ClientKind::McpCommand);
        assert!(ctx.client_kind().is_service());
    }

    // -- one boundary (item 8) ---------------------------------------------

    #[test]
    fn a_service_operation_and_a_cli_one_cross_the_same_boundary() {
        let (mut registry, admin_id) = bootstrapped("same-boundary");
        let (epoch, hash) = registry.live_policy_state().unwrap();
        let admin = actor(Some(&admin_id), "geoyws", 1000, epoch, &hash);
        let backup = registry
            .bind_principal("kanban-backup", 2000, &[], &admin)
            .expect("bind the backup service principal");

        // The same peer UID, minted once as a CLI caller and once as a backup
        // service caller, must resolve to the same principal: there is ONE
        // minting route (`mint_principal_context`), reached only through
        // `Broker::accept_principal`, whatever the client kind.
        let creds = PeerCredentials::new(Some(48216), 2000, 2000);
        let passwd = FakePasswd::pair("kanban-backup", 2000);

        let cli = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Cli,
            "rq-cli".to_owned(),
            Some("geoyws".to_owned()),
        )
        .expect("mint as cli");
        let service = mint_principal_context(
            &creds,
            &passwd,
            &registry,
            ClientKind::Backup,
            "rq-svc".to_owned(),
            Some("geoyws".to_owned()),
        )
        .expect("mint as service");

        // Same identity; the client kind is the only difference, and it is a
        // label beside the identity, not a second authority.
        assert_eq!(cli.principal_id(), service.principal_id());
        assert_eq!(cli.principal_id(), backup.row.id);
        assert_eq!(cli.username(), service.username());
        assert_eq!(cli.uid(), service.uid());
        assert_eq!(cli.client_kind(), ClientKind::Cli);
        assert_eq!(service.client_kind(), ClientKind::Backup);
    }

    // -- offline maintenance (item 5) --------------------------------------

    #[test]
    fn offline_maintenance_is_explicit_not_an_absent_socket_fallback() {
        // Online with the socket down is a hard refusal, never direct access.
        assert_eq!(
            direct_open_decision(BrokerMode::Online, false),
            DirectOpen::RefusedSocketDown
        );
        // Online with the socket up routes through the broker.
        assert_eq!(
            direct_open_decision(BrokerMode::Online, true),
            DirectOpen::RefusedOnline
        );
        // Only the explicit named mode permits direct maintenance.
        assert_eq!(
            direct_open_decision(BrokerMode::OfflineMaintenance, false),
            DirectOpen::Permitted
        );
        // And the mode itself is entered only by the explicit instruction.
        assert_eq!(BrokerMode::resolve(None).unwrap(), BrokerMode::Online);
        assert_eq!(
            BrokerMode::resolve(Some("offline-maintenance")).unwrap(),
            BrokerMode::OfflineMaintenance
        );
        assert!(BrokerMode::resolve(Some("auto")).is_err());
    }

    // -- direct-db isolation (item 6) --------------------------------------

    #[test]
    fn a_direct_db_open_yields_no_principal_context_and_writes_no_policy_decision() {
        let registry = test_registry("direct-db");

        // The exact `--db` path: `Store::open` on a raw board file path, with
        // no broker, no socket, and no peer credential involved.
        let board_dir = std::env::temp_dir().join(format!(
            "kanban-broker-direct-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&board_dir).unwrap();
        let board_path = board_dir.join("board.db");
        {
            let mut store = Store::open(&board_path).expect("open board directly");
            store
                .add_task(AddTask {
                    id: None,
                    task_type: "task".to_owned(),
                    parent_id: None,
                    title: "direct db probe".to_owned(),
                    body: None,
                    assignee: None,
                    lane: None,
                    deliverable: None,
                    stale_minutes: None,
                    driver_only: false,
                    status: "todo".to_owned(),
                    priority: 0,
                    dependencies: vec![],
                    metadata: Value::Null,
                    actor: Some("geoyws".to_owned()),
                    tags: vec![],
                })
                .expect("write through the direct path");
        }

        // A direct open writes no policy decision: the policy journals stay
        // empty. (Structurally, there is also no constructor from a `--db`
        // path to a PrincipalContext — the only mint is Broker::accept_principal.)
        let audit_rows: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM access_audit", [], |row| row.get(0))
            .unwrap();
        let policy_rows: i64 = registry
            .connection
            .query_row("SELECT count(*) FROM policy_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            audit_rows, 0,
            "a direct open must not write an access-audit row"
        );
        assert_eq!(
            policy_rows, 0,
            "a direct open must not write a policy event"
        );

        fs::remove_dir_all(&board_dir).ok();
    }

    /// The claim "macOS coverage is not Linux peer-credential acceptance" is
    /// carried entirely by this predicate, so it needs a test of its own:
    /// widening the `matches!` would otherwise let `getpeereid` evidence pass
    /// as managed-mode with every other test still green.
    #[test]
    fn only_linux_so_peercred_counts_as_managed_mode_evidence() {
        assert!(EvidenceSource::KernelSoPeercred.is_managed_mode_evidence());
        assert!(!EvidenceSource::KernelPeercred.is_managed_mode_evidence());

        // What THIS host actually mints, so a devbox run can never be mistaken
        // for acceptance evidence.
        let here = kernel_evidence_source();
        if cfg!(target_os = "linux") {
            assert_eq!(here, EvidenceSource::KernelSoPeercred);
            assert!(here.is_managed_mode_evidence());
        } else {
            assert_eq!(here, EvidenceSource::KernelPeercred);
            assert!(
                !here.is_managed_mode_evidence(),
                "a non-Linux host must not produce managed-mode evidence"
            );
        }
    }
}
