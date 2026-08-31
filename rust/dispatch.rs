use crate::model::Subscription;
use crate::registry::data_root;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const CONFIG_FILE: &str = "dispatchers.json";
const CONFIG_MAX_BYTES: usize = 1 << 20;
const LOCK_DIR: &str = "dispatcher-locks";
const LOCK_DIR_MODE: u32 = 0o700;
const LOCK_FILE_MODE: u32 = 0o600;
const CONFIG_ROOT_MODE_MASK: u32 = 0o077;
const EXEC_MODE_GROUP_OTHER_WRITE: u32 = 0o022;
const EXEC_MODE_ANY_EXEC: u32 = 0o111;
const IDENTIFIER_MAX: usize = 64;
const SECRET_REF_MAX: usize = 128;
const ENV_NAME_MAX: usize = 128;
const SUPPORTED_VERSION: i64 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DispatcherFile {
    version: i64,
    consumers: BTreeMap<String, ConsumerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsumerConfig {
    capabilities: Vec<String>,
    actions: BTreeMap<String, ActionConfig>,
    secrets: BTreeMap<String, SecretConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionConfig {
    capability: String,
    executable: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretConfig {
    #[serde(rename = "sourceEnv")]
    source_env: String,
    #[serde(rename = "targetEnv")]
    target_env: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedDispatch {
    pub(crate) consumer_id: String,
    pub(crate) action_id: String,
    pub(crate) executable: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) secret: Option<ResolvedSecret>,
}

#[derive(Clone)]
pub(crate) struct ResolvedSecret {
    pub(crate) target_env: String,
    secret_value: OsString,
}

impl ResolvedSecret {
    pub(crate) fn secret_value(&self) -> &OsStr {
        &self.secret_value
    }
}

impl fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("target_env", &self.target_env)
            .field("secret_value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct ConsumerLock {
    _file: File,
}

#[derive(Debug)]
pub(crate) struct DispatcherConfig {
    consumers: BTreeMap<String, ConsumerConfig>,
}

#[derive(Clone, Copy)]
struct FileSnapshot {
    dev: u64,
    ino: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileSnapshot {
    fn capture(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
            is_dir: metadata.is_dir(),
            is_file: metadata.is_file(),
            is_symlink: metadata.file_type().is_symlink(),
        }
    }

    fn matches(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.uid == other.uid
            && self.gid == other.gid
            && self.mode == other.mode
            && self.is_dir == other.is_dir
            && self.is_file == other.is_file
            && self.is_symlink == other.is_symlink
    }
}

fn nonempty<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{label} is required");
    }
    Ok(trimmed)
}

fn exact_identifier(value: &str, label: &str, max: usize) -> Result<String> {
    let value = nonempty(value, label)?;
    if value.len() > max
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!(
            "{label} must be at most {max} ASCII characters, start with a letter or digit, and contain only letters, digits, dot, underscore, or hyphen"
        );
    }
    Ok(value.to_owned())
}

pub(crate) fn validate_consumer_id(value: &str) -> Result<String> {
    exact_identifier(value, "consumer id", IDENTIFIER_MAX)
}

fn exact_env_name(value: &str, label: &str) -> Result<String> {
    let value = nonempty(value, label)?;
    if value.len() > ENV_NAME_MAX
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        || !value
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
    {
        bail!(
            "{label} must be at most {ENV_NAME_MAX} ASCII characters, start with a letter or underscore, and contain only letters, digits, or underscore"
        );
    }
    Ok(value.to_owned())
}

fn unsafe_target_env_name(value: &str) -> bool {
    matches!(
        value,
        "PATH"
            | "LD_PRELOAD"
            | "BASH_ENV"
            | "ENV"
            | "SHELLOPTS"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PERL5LIB"
            | "PERLLIB"
            | "RUBYOPT"
            | "RUBYLIB"
            | "NODE_OPTIONS"
    ) || value.starts_with("LD_")
        || value.starts_with("DYLD_")
}

fn exact_target_env_name(value: &str, label: &str) -> Result<String> {
    let value = exact_env_name(value, label)?;
    if unsafe_target_env_name(&value) {
        bail!("{label} must not use execution-sensitive environment names");
    }
    Ok(value)
}

fn reject_nul(value: &str, label: &str) -> Result<()> {
    if value.as_bytes().contains(&0) {
        bail!("{label} must not contain NUL");
    }
    Ok(())
}

fn config_root() -> Result<PathBuf> {
    data_root()
}

fn ensure_private_root(root: &Path) -> Result<()> {
    let metadata =
        fs::symlink_metadata(root).with_context(|| format!("read data root {}", root.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        bail!("data root must be a regular directory");
    }
    if metadata.permissions().mode() & CONFIG_ROOT_MODE_MASK != 0 {
        bail!("data root must not be group- or other-accessible");
    }
    Ok(())
}

fn verify_dir_snapshot(
    path: &Path,
    snapshot: &FileSnapshot,
    exact_mode: u32,
    label: &str,
) -> Result<()> {
    let current = inspect_path(path, label)?;
    if !snapshot.matches(&current) {
        bail!("{label} changed while opening");
    }
    if current.is_symlink || !current.is_dir {
        bail!("{label} must be a regular directory");
    }
    if current.mode & 0o777 != exact_mode {
        bail!("{label} must have mode {:o}", exact_mode);
    }
    Ok(())
}

fn ensure_private_dir(path: &Path, exact_mode: u32, label: &str) -> Result<FileSnapshot> {
    match fs::DirBuilder::new().mode(exact_mode).create(path) {
        Ok(()) => {
            let snapshot = inspect_path(path, label)?;
            if snapshot.is_symlink || !snapshot.is_dir {
                bail!("{label} must be a regular directory");
            }
            if snapshot.mode & 0o777 != exact_mode {
                bail!("{label} must have mode {:o}", exact_mode);
            }
            Ok(snapshot)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let snapshot = inspect_path(path, label)?;
            if snapshot.is_symlink || !snapshot.is_dir {
                bail!("{label} must be a regular directory");
            }
            if snapshot.mode & 0o777 != exact_mode {
                bail!("{label} must have mode {:o}", exact_mode);
            }
            Ok(snapshot)
        }
        Err(error) => Err(error).with_context(|| format!("create directory {}", path.display())),
    }
}

fn verified_regular_file(path: &Path, label: &str) -> Result<(File, FileSnapshot)> {
    let pre = inspect_path(path, label)?;
    if pre.is_symlink || !pre.is_file {
        bail!("{label} must be a regular file");
    }
    if pre.mode & CONFIG_ROOT_MODE_MASK != 0 {
        bail!("{label} must not be group- or other-accessible");
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    let post = inspect_path(path, label)?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspect opened {label} {}", path.display()))?;
    let opened_snapshot = FileSnapshot::capture(&opened);
    if !pre.matches(&opened_snapshot) || !post.matches(&opened_snapshot) {
        bail!("{label} changed while opening");
    }
    Ok((file, opened_snapshot))
}

fn inspect_path(path: &Path, label: &str) -> Result<FileSnapshot> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    Ok(FileSnapshot::capture(&metadata))
}

fn open_verified_file(path: &Path, label: &str) -> Result<File> {
    let (file, _) = verified_regular_file(path, label)?;
    Ok(file)
}

fn read_verified_config(path: &Path) -> Result<DispatcherFile> {
    let mut file = open_verified_file(path, "dispatcher config")?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((CONFIG_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read dispatcher config {}", path.display()))?;
    if bytes.len() > CONFIG_MAX_BYTES {
        bail!("dispatcher config exceeds {CONFIG_MAX_BYTES} bytes");
    }
    let config: DispatcherFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse dispatcher config {}", path.display()))?;
    Ok(config)
}

fn validate_capabilities(capabilities: &[String], consumer_id: &str) -> Result<()> {
    let mut seen = HashSet::new();
    if capabilities.is_empty() {
        bail!("consumer {consumer_id} must declare at least one capability");
    }
    for capability in capabilities {
        let capability = exact_identifier(capability, "capability", IDENTIFIER_MAX)?;
        if !seen.insert(capability) {
            bail!("consumer {consumer_id} declares capability more than once");
        }
    }
    Ok(())
}

fn validate_action_config(action: &ActionConfig, action_id: &str) -> Result<()> {
    exact_identifier(&action.capability, "capability", IDENTIFIER_MAX)?;
    let executable = action.executable.trim();
    if executable.is_empty() {
        bail!("action {action_id} executable is required");
    }
    if !Path::new(executable).is_absolute() {
        bail!("action {action_id} executable must be absolute");
    }
    reject_nul(&action.executable, "action executable")?;
    for arg in &action.args {
        reject_nul(arg, "action arg")?;
    }
    Ok(())
}

fn validate_secret_config(secret: &SecretConfig, secret_id: &str) -> Result<()> {
    exact_env_name(&secret.source_env, "source env")?;
    exact_target_env_name(&secret.target_env, "target env")?;
    exact_identifier(secret_id, "secret reference", SECRET_REF_MAX)?;
    Ok(())
}

fn verify_executable(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect executable {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!("action executable must be a regular file");
    }
    let mode = metadata.mode();
    if mode & EXEC_MODE_ANY_EXEC == 0 {
        bail!("action executable must have at least one execute bit");
    }
    if mode & EXEC_MODE_GROUP_OTHER_WRITE != 0 {
        bail!("action executable must not be group- or other-writable");
    }
    Ok(())
}

pub(crate) fn consumer_lock_path(consumer_id: &str) -> Result<PathBuf> {
    let consumer_id = validate_consumer_id(consumer_id)?;
    let digest = Sha256::digest(consumer_id.as_bytes());
    Ok(config_root()?
        .join(LOCK_DIR)
        .join(format!("{digest:x}.lock")))
}

fn verify_lock_file_snapshot(
    path: &Path,
    file: &File,
    pre: Option<FileSnapshot>,
    label: &str,
) -> Result<()> {
    let post = inspect_path(path, label)?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspect opened {label} {}", path.display()))?;
    let opened_snapshot = FileSnapshot::capture(&opened);
    if let Some(pre) = pre
        && !pre.matches(&opened_snapshot)
    {
        bail!("{label} changed while opening");
    }
    if !post.matches(&opened_snapshot) {
        bail!("{label} changed while opening");
    }
    if post.is_symlink || !post.is_file {
        bail!("{label} must be a regular file");
    }
    if post.mode & 0o777 != LOCK_FILE_MODE {
        bail!("{label} must have mode {:o}", LOCK_FILE_MODE);
    }
    Ok(())
}

fn open_verified_existing_lock_file(path: &Path, label: &str) -> Result<File> {
    let pre = inspect_path(path, label)?;
    if pre.is_symlink || !pre.is_file {
        bail!("{label} must be a regular file");
    }
    if pre.mode & 0o777 != LOCK_FILE_MODE {
        bail!("{label} must have mode {:o}", LOCK_FILE_MODE);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {label} {}", path.display()))?;
    verify_lock_file_snapshot(path, &file, Some(pre), label)?;
    Ok(file)
}

fn open_verified_lock_file(path: &Path, label: &str) -> Result<File> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .truncate(false)
        .mode(LOCK_FILE_MODE)
        .open(path)
    {
        Ok(file) => {
            verify_lock_file_snapshot(path, &file, None, label)?;
            Ok(file)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            open_verified_existing_lock_file(path, label)
        }
        Err(error) => Err(error).with_context(|| format!("create {label} {}", path.display())),
    }
}

pub(crate) fn try_consumer_lock(consumer_id: &str) -> Result<Option<ConsumerLock>> {
    let root = config_root()?;
    ensure_private_root(&root)?;
    let lock_dir = root.join(LOCK_DIR);
    let lock_dir_snapshot =
        ensure_private_dir(&lock_dir, LOCK_DIR_MODE, "dispatcher lock directory")?;
    let path = consumer_lock_path(consumer_id)?;
    let file = open_verified_lock_file(&path, "dispatcher lock file")?;
    verify_dir_snapshot(
        &lock_dir,
        &lock_dir_snapshot,
        LOCK_DIR_MODE,
        "dispatcher lock directory",
    )?;
    match file.try_lock() {
        Ok(()) => {
            verify_lock_file_snapshot(&path, &file, None, "dispatcher lock file")?;
            verify_dir_snapshot(
                &lock_dir,
                &lock_dir_snapshot,
                LOCK_DIR_MODE,
                "dispatcher lock directory",
            )?;
            Ok(Some(ConsumerLock { _file: file }))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => {
            Err(anyhow::Error::new(error).context(format!("lock file {}", path.display())))
        }
    }
}

pub(crate) struct DispatcherConfigLoader;

impl DispatcherConfigLoader {
    pub(crate) fn load() -> Result<DispatcherConfig> {
        let root = config_root()?;
        ensure_private_root(&root)?;
        let path = root.join(CONFIG_FILE);
        let config = read_verified_config(&path)?;
        if config.version != SUPPORTED_VERSION {
            bail!("unsupported dispatcher config version {}", config.version);
        }
        if config.consumers.is_empty() {
            bail!("dispatcher config must declare at least one consumer");
        }
        for (consumer_id, consumer) in &config.consumers {
            exact_identifier(consumer_id, "consumer id", IDENTIFIER_MAX)?;
            validate_capabilities(&consumer.capabilities, consumer_id)?;
            if consumer.actions.is_empty() {
                bail!("consumer {consumer_id} must declare at least one action");
            }
            for (action_id, action) in &consumer.actions {
                exact_identifier(action_id, "action id", IDENTIFIER_MAX)?;
                validate_action_config(action, action_id)?;
            }
            for (secret_id, secret) in &consumer.secrets {
                exact_identifier(secret_id, "secret reference", SECRET_REF_MAX)?;
                validate_secret_config(secret, secret_id)?;
            }
        }
        Ok(DispatcherConfig {
            consumers: config.consumers,
        })
    }
}

impl DispatcherConfig {
    pub(crate) fn require_consumer(&self, consumer_id: &str) -> Result<()> {
        let consumer_id = validate_consumer_id(consumer_id)?;
        if !self.consumers.contains_key(&consumer_id) {
            bail!("unknown consumer {consumer_id}");
        }
        Ok(())
    }

    pub(crate) fn resolve(&self, subscription: &Subscription) -> Result<ResolvedDispatch> {
        let consumer_id = validate_consumer_id(&subscription.consumer_id)?;
        let action_id = exact_identifier(&subscription.action_id, "action id", IDENTIFIER_MAX)?;
        let consumer = self
            .consumers
            .get(&consumer_id)
            .with_context(|| format!("unknown consumer {consumer_id}"))?;
        let action = consumer
            .actions
            .get(&action_id)
            .with_context(|| format!("unknown action {action_id} for consumer {consumer_id}"))?;
        let capability = exact_identifier(&action.capability, "capability", IDENTIFIER_MAX)?;
        if !consumer
            .capabilities
            .iter()
            .any(|value| value == &capability)
        {
            bail!("consumer {consumer_id} does not declare capability {capability}");
        }
        let executable = PathBuf::from(action.executable.trim());
        verify_executable(&executable)?;
        let secret = match subscription.secret_ref.as_ref() {
            Some(secret_ref) => {
                let secret_ref = exact_identifier(secret_ref, "secret reference", SECRET_REF_MAX)?;
                let secret = consumer.secrets.get(&secret_ref).with_context(|| {
                    format!("unknown secret {secret_ref} for consumer {consumer_id}")
                })?;
                let source_env = exact_env_name(&secret.source_env, "source env")?;
                let target_env = exact_target_env_name(&secret.target_env, "target env")?;
                let Some(value) = env::var_os(&source_env) else {
                    bail!("missing source env for secret {secret_ref}");
                };
                Some(ResolvedSecret {
                    target_env,
                    secret_value: value,
                })
            }
            None => None,
        };
        let resolved = ResolvedDispatch {
            consumer_id,
            action_id,
            executable,
            args: action.args.clone(),
            secret,
        };
        resolved.validate()?;
        Ok(resolved)
    }
}

impl ResolvedDispatch {
    pub(crate) fn validate(&self) -> Result<()> {
        verify_executable(&self.executable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Subscription;
    use serde_json::json;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use uuid::Uuid;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    struct EnvRestore {
        key: String,
        original: Option<std::ffi::OsString>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var(&self.key, value),
                    None => env::remove_var(&self.key),
                }
            }
        }
    }

    fn set_env_os(key: &str, value: Option<&OsStr>) -> Option<EnvRestore> {
        let original = env::var_os(key);
        unsafe {
            match value {
                Some(value) => env::set_var(key, value),
                None => env::remove_var(key),
            }
        }
        Some(EnvRestore {
            key: key.to_owned(),
            original,
        })
    }

    fn set_env(key: &str, value: Option<&str>) -> Option<EnvRestore> {
        set_env_os(key, value.map(OsStr::new))
    }

    fn temp_root() -> PathBuf {
        let root = env::temp_dir().join(format!(
            "kanban-dispatch-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    struct TestRoot {
        path: PathBuf,
        executable: PathBuf,
        original: Option<std::ffi::OsString>,
        _env: Option<EnvRestore>,
    }

    impl TestRoot {
        fn new() -> Self {
            let path = temp_root();
            let executable = path.join("dispatch-ok.sh");
            fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
            let original = env::var_os("KANBAN_DATA_DIR");
            let _env = set_env("KANBAN_DATA_DIR", path.to_str());
            Self {
                path,
                executable,
                original,
                _env,
            }
        }

        fn executable_path(&self) -> &Path {
            self.executable.as_path()
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            unsafe {
                match &self.original {
                    Some(value) => env::set_var("KANBAN_DATA_DIR", value),
                    None => env::remove_var("KANBAN_DATA_DIR"),
                }
            }
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_config(root: &Path, content: &serde_json::Value, mode: u32) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join(CONFIG_FILE);
        fs::write(&path, serde_json::to_vec_pretty(content).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn base_config(executable: &str, secret_source: &str) -> serde_json::Value {
        json!({
            "version": 1,
            "consumers": {
                "consumer-a": {
                    "capabilities": ["cap-a"],
                    "actions": {
                        "action-a": {
                            "capability": "cap-a",
                            "executable": executable,
                            "args": ["--mode", "dispatch"]
                        }
                    },
                    "secrets": {
                        "secret-a": {
                            "sourceEnv": secret_source,
                            "targetEnv": "DISPATCH_TOKEN"
                        }
                    }
                }
            }
        })
    }

    fn config_with_secret(
        secret_id: &str,
        source_env: &str,
        executable: &str,
    ) -> serde_json::Value {
        json!({
            "version": 1,
            "consumers": {
                "consumer-a": {
                    "capabilities": ["cap-a"],
                    "actions": {
                        "action-a": {
                            "capability": "cap-a",
                            "executable": executable,
                            "args": []
                        }
                    },
                    "secrets": {
                        secret_id: {
                            "sourceEnv": source_env,
                            "targetEnv": "DISPATCH_TOKEN"
                        }
                    }
                }
            }
        })
    }

    fn repeated_ascii(ch: char, len: usize) -> String {
        std::iter::repeat_n(ch, len).collect()
    }

    fn subscription(secret_ref: Option<&str>) -> Subscription {
        Subscription {
            id: "sub-1".to_owned(),
            protocol_version: 1,
            subject_task_id: None,
            relations: Vec::new(),
            kinds: Vec::new(),
            prior_statuses: Vec::new(),
            current_statuses: Vec::new(),
            tags: Vec::new(),
            consumer_id: "consumer-a".to_owned(),
            action_id: "action-a".to_owned(),
            timeout_ms: 1,
            max_retries: 0,
            rate_per_minute: 1,
            max_concurrency: 1,
            start_event_seq: 0,
            secret_ref: secret_ref.map(str::to_owned),
            status: "active".to_owned(),
            created_at: 1,
            created_by: "geo".to_owned(),
            updated_at: 1,
            updated_by: "geo".to_owned(),
            paused_at: None,
            paused_by: None,
        }
    }

    #[test]
    fn valid_exact_resolution_returns_a_redacted_secret_and_validates_the_executable() {
        let _env = env_guard();
        let root = TestRoot::new();
        let executable = root.executable_path();
        let secret_source = "KANBAN_DISPATCH_SECRET";
        let _secret = set_env(secret_source, Some("supersecret"));
        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );

        let config = DispatcherConfigLoader::load().unwrap();
        config.require_consumer("consumer-a").unwrap();
        assert!(config.require_consumer("consumer-missing").is_err());
        let resolved = config.resolve(&subscription(Some("secret-a"))).unwrap();
        assert_eq!(resolved.consumer_id, "consumer-a");
        assert_eq!(resolved.action_id, "action-a");
        assert_eq!(resolved.executable, PathBuf::from(executable));
        assert_eq!(resolved.args, vec!["--mode", "dispatch"]);
        let secret = resolved.secret.as_ref().unwrap();
        assert_eq!(secret.target_env, "DISPATCH_TOKEN");
        assert_eq!(secret.secret_value(), OsStr::new("supersecret"));
        assert!(!format!("{secret:?}").contains("supersecret"));
        resolved.validate().unwrap();
    }

    #[test]
    fn unknown_consumer_action_capability_secret_and_missing_source_env_fail_closed() {
        let _env = env_guard();
        let root = TestRoot::new();
        let executable = root.executable_path();
        let secret_source = "KANBAN_DISPATCH_SECRET";
        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );
        let config = DispatcherConfigLoader::load().unwrap();

        let consumer_error = config
            .resolve(&Subscription {
                consumer_id: "consumer-missing".to_owned(),
                ..subscription(None)
            })
            .unwrap_err()
            .to_string();
        assert!(
            consumer_error.contains("consumer-missing"),
            "{consumer_error}"
        );

        let action_error = config
            .resolve(&Subscription {
                action_id: "action-missing".to_owned(),
                ..subscription(None)
            })
            .unwrap_err()
            .to_string();
        assert!(action_error.contains("action-missing"), "{action_error}");

        let mut capability_config = base_config(executable.to_str().unwrap(), secret_source);
        capability_config["consumers"]["consumer-a"]["capabilities"] = json!(["cap-b"]);
        write_config(&root.path, &capability_config, 0o600);
        let config = DispatcherConfigLoader::load().unwrap();
        let capability_error = config.resolve(&subscription(None)).unwrap_err().to_string();
        assert!(capability_error.contains("cap-a"), "{capability_error}");

        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );
        let config = DispatcherConfigLoader::load().unwrap();
        let secret_error = config
            .resolve(&Subscription {
                secret_ref: Some("secret-missing".to_owned()),
                ..subscription(None)
            })
            .unwrap_err()
            .to_string();
        assert!(secret_error.contains("secret-missing"), "{secret_error}");

        unsafe { env::remove_var(secret_source) };
        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );
        let config = DispatcherConfigLoader::load().unwrap();
        let source_error = config
            .resolve(&subscription(Some("secret-a")))
            .unwrap_err()
            .to_string();
        assert!(
            source_error.contains("missing source env"),
            "{source_error}"
        );
        assert!(!source_error.contains(secret_source), "{source_error}");
    }

    #[test]
    fn unsafe_target_env_names_are_rejected_and_the_allowlisted_target_resolves() {
        let _env = env_guard();
        let root = TestRoot::new();
        let executable = root.executable_path();
        let secret_source = "KANBAN_DISPATCH_SECRET_TARGET";
        let _secret = set_env(secret_source, Some("supersecret"));
        for target_env in [
            "PATH",
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "BASH_ENV",
            "ENV",
            "SHELLOPTS",
            "PYTHONPATH",
            "PYTHONHOME",
            "PERL5LIB",
            "PERLLIB",
            "RUBYOPT",
            "RUBYLIB",
            "NODE_OPTIONS",
        ] {
            let mut config = base_config(executable.to_str().unwrap(), secret_source);
            config["consumers"]["consumer-a"]["secrets"]["secret-a"]["targetEnv"] =
                json!(target_env);
            write_config(&root.path, &config, 0o600);
            let error = DispatcherConfigLoader::load().unwrap_err().to_string();
            assert!(
                error.contains("execution-sensitive environment names"),
                "{target_env}: {error}"
            );
        }

        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );
        let config = DispatcherConfigLoader::load().unwrap();
        let secret = config
            .resolve(&subscription(Some("secret-a")))
            .unwrap()
            .secret
            .unwrap();
        assert_eq!(secret.target_env, "DISPATCH_TOKEN");
        assert_eq!(secret.secret_value(), OsStr::new("supersecret"));
    }

    #[test]
    fn unknown_json_and_duplicate_capabilities_are_rejected() {
        let _env = env_guard();
        let root = TestRoot::new();
        let path = root.path.join(CONFIG_FILE);
        fs::write(
            &path,
            r#"{"version":1,"consumers":{"consumer-a":{"capabilities":["cap-a"],"actions":{"action-a":{"capability":"cap-a","executable":"/bin/sh","args":[]}},"secrets":{},"unexpected":true}}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(DispatcherConfigLoader::load().is_err());

        write_config(
            &root.path,
            &json!({
                "version": 1,
                "consumers": {
                    "consumer-a": {
                        "capabilities": ["cap-a", "cap-a"],
                        "actions": {
                            "action-a": {
                                "capability": "cap-a",
                                "executable": "/bin/sh",
                                "args": []
                            }
                        },
                        "secrets": {}
                    }
                }
            }),
            0o600,
        );
        let duplicate_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(
            duplicate_error.contains("declares capability more than once"),
            "{duplicate_error}"
        );
    }

    #[test]
    fn unsafe_root_config_and_file_modes_are_rejected() {
        let _env = env_guard();
        let root = TestRoot::new();

        let file_root = root.path.join("not-a-dir");
        fs::write(&file_root, b"nope").unwrap();
        unsafe { env::set_var("KANBAN_DATA_DIR", &file_root) };
        let file_root_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(file_root_error.contains("data root"), "{file_root_error}");

        unsafe { env::set_var("KANBAN_DATA_DIR", &root.path) };
        fs::create_dir_all(&root.path).unwrap();
        fs::set_permissions(&root.path, fs::Permissions::from_mode(0o755)).unwrap();
        let dir_mode_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(dir_mode_error.contains("data root"), "{dir_mode_error}");
        fs::set_permissions(&root.path, fs::Permissions::from_mode(0o700)).unwrap();

        let config_path = write_config(
            &root.path,
            &base_config("/bin/sh", "KANBAN_DISPATCH_SECRET"),
            0o644,
        );
        let mode_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(mode_error.contains("dispatcher config"), "{mode_error}");

        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
        let target = root.path.join("dispatchers-real.json");
        fs::write(
            &target,
            serde_json::to_vec(&base_config("/bin/sh", "KANBAN_DISPATCH_SECRET")).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::remove_file(&config_path).unwrap();
        std::os::unix::fs::symlink(&target, &config_path).unwrap();
        let symlink_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(
            symlink_error.contains("dispatcher config"),
            "{symlink_error}"
        );
    }

    #[test]
    fn consumer_lock_creation_verifies_new_file_and_created_directory_mode() {
        let _env = env_guard();
        let root = TestRoot::new();
        let lock = try_consumer_lock("consumer-new").unwrap().unwrap();
        let lock_dir = root.path.join(LOCK_DIR);
        let lock_path = consumer_lock_path("consumer-new").unwrap();
        let dir_meta = fs::symlink_metadata(&lock_dir).unwrap();
        assert!(dir_meta.is_dir());
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
        let file_meta = fs::symlink_metadata(&lock_path).unwrap();
        assert!(file_meta.is_file());
        assert_eq!(file_meta.permissions().mode() & 0o777, 0o600);
        drop(lock);
    }

    #[test]
    fn unsafe_preexisting_lock_path_and_mode_are_refused() {
        let _env = env_guard();
        let root = TestRoot::new();
        let lock_dir = root.path.join(LOCK_DIR);
        fs::create_dir_all(&lock_dir).unwrap();
        fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let lock_path = consumer_lock_path("consumer-preexisting").unwrap();
        fs::write(&lock_path, b"locked").unwrap();
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o644)).unwrap();
        let mode_error = try_consumer_lock("consumer-preexisting")
            .unwrap_err()
            .to_string();
        assert!(mode_error.contains("mode 600"), "{mode_error}");

        fs::remove_file(&lock_path).unwrap();
        let target = root.path.join("dispatch-target");
        fs::write(&target, b"target").unwrap();
        std::os::unix::fs::symlink(&target, &lock_path).unwrap();
        let symlink_error = try_consumer_lock("consumer-preexisting")
            .unwrap_err()
            .to_string();
        assert!(symlink_error.contains("regular file"), "{symlink_error}");
    }

    #[test]
    fn secret_reference_128_bytes_is_accepted_and_129_is_rejected() {
        let _env = env_guard();
        let root = TestRoot::new();
        let secret_source = "KANBAN_DISPATCH_SECRET_128";
        let secret_value = "boundary-secret";
        let _secret = set_env(secret_source, Some(secret_value));
        let accepted_ref = repeated_ascii('s', SECRET_REF_MAX);
        write_config(
            &root.path,
            &config_with_secret(
                &accepted_ref,
                secret_source,
                root.executable_path().to_str().unwrap(),
            ),
            0o600,
        );

        let config = DispatcherConfigLoader::load().unwrap();
        let resolved = config
            .resolve(&Subscription {
                secret_ref: Some(accepted_ref.clone()),
                ..subscription(None)
            })
            .unwrap();
        let secret = resolved.secret.as_ref().unwrap();
        assert_eq!(secret.secret_value(), OsStr::new(secret_value));
        assert!(!format!("{secret:?}").contains(secret_value));

        let rejected_ref = repeated_ascii('t', SECRET_REF_MAX + 1);
        let error = config
            .resolve(&Subscription {
                secret_ref: Some(rejected_ref),
                ..subscription(None)
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("secret reference must be at most 128"),
            "{error}"
        );
    }

    #[test]
    fn executable_resolution_rejects_relative_symlink_non_executable_and_group_or_other_writable_paths()
     {
        let _env = env_guard();
        let root = TestRoot::new();
        let secret_source = "KANBAN_DISPATCH_SECRET";
        let good_config = |executable: &str| base_config(executable, secret_source);

        let relative_path = "bin/dispatch";
        write_config(&root.path, &good_config(relative_path), 0o600);
        let relative_error = DispatcherConfigLoader::load().unwrap_err().to_string();
        assert!(relative_error.contains("absolute"), "{relative_error}");

        let executable = root.path.join("dispatch.sh");
        fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        write_config(
            &root.path,
            &good_config(executable.to_str().unwrap()),
            0o600,
        );
        let non_exec_error = DispatcherConfigLoader::load()
            .unwrap()
            .resolve(&subscription(None))
            .unwrap_err()
            .to_string();
        assert!(non_exec_error.contains("execute bit"), "{non_exec_error}");

        fs::set_permissions(&executable, fs::Permissions::from_mode(0o777)).unwrap();
        write_config(
            &root.path,
            &good_config(executable.to_str().unwrap()),
            0o600,
        );
        let writable_error = DispatcherConfigLoader::load()
            .unwrap()
            .resolve(&subscription(None))
            .unwrap_err()
            .to_string();
        assert!(writable_error.contains("writable"), "{writable_error}");

        let real_executable = root.path.join("dispatch-real.sh");
        fs::write(&real_executable, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&real_executable, fs::Permissions::from_mode(0o755)).unwrap();
        let symlink = root.path.join("dispatch-link.sh");
        std::os::unix::fs::symlink(&real_executable, &symlink).unwrap();
        write_config(&root.path, &good_config(symlink.to_str().unwrap()), 0o600);
        let symlink_error = DispatcherConfigLoader::load()
            .unwrap()
            .resolve(&subscription(None))
            .unwrap_err()
            .to_string();
        assert!(symlink_error.contains("regular file"), "{symlink_error}");
    }

    #[test]
    fn secret_value_is_redacted_from_debug_and_error_messages() {
        let _env = env_guard();
        let root = TestRoot::new();
        let secret_source = "KANBAN_DISPATCH_SECRET";
        let secret_value = "supersecret";
        let _secret = set_env(secret_source, Some(secret_value));
        write_config(
            &root.path,
            &base_config(root.executable_path().to_str().unwrap(), secret_source),
            0o600,
        );
        let config = DispatcherConfigLoader::load().unwrap();
        let secret = config
            .resolve(&subscription(Some("secret-a")))
            .unwrap()
            .secret
            .unwrap();
        assert_eq!(secret.secret_value(), OsStr::new(secret_value));
        assert!(!format!("{secret:?}").contains(secret_value));

        let error = config
            .resolve(&Subscription {
                secret_ref: Some("secret-missing".to_owned()),
                ..subscription(None)
            })
            .unwrap_err()
            .to_string();
        assert!(!error.contains(secret_value), "{error}");
    }

    #[test]
    fn non_unicode_secret_values_round_trip_without_unicode_conversion_leaks() {
        let _env = env_guard();
        let root = TestRoot::new();
        let executable = root.executable_path();
        let secret_source = "KANBAN_DISPATCH_SECRET_NONUNICODE";
        let secret_value = OsString::from_vec(vec![b's', b'e', b'c', b'r', b'e', b't', 0x80]);
        let _secret = set_env_os(secret_source, Some(secret_value.as_os_str()));
        write_config(
            &root.path,
            &base_config(executable.to_str().unwrap(), secret_source),
            0o600,
        );

        let config = DispatcherConfigLoader::load().unwrap();
        let secret = config
            .resolve(&subscription(Some("secret-a")))
            .unwrap()
            .secret
            .unwrap();
        let debug = format!("{secret:?}");
        assert_eq!(secret.secret_value(), secret_value.as_os_str());
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains("\\x80"), "{debug}");

        unsafe { env::remove_var(secret_source) };
        let error = DispatcherConfigLoader::load()
            .unwrap()
            .resolve(&subscription(Some("secret-a")))
            .unwrap_err();
        let error_text = error.to_string();
        assert!(error_text.contains("missing source env"), "{error_text}");
        assert_eq!(error.chain().count(), 1, "{error_text}");
        assert!(!format!("{error:?}").contains("NotUnicode"));
    }

    #[test]
    fn same_consumer_locks_exclude_and_different_consumers_do_not() {
        let _env = env_guard();
        let _root = TestRoot::new();
        let first = try_consumer_lock("consumer-a").unwrap().unwrap();
        assert!(try_consumer_lock("consumer-a").unwrap().is_none());
        assert!(try_consumer_lock("consumer-b").unwrap().is_some());
        drop(first);
    }

    #[test]
    fn lock_path_uses_only_the_consumer_hash() {
        let _env = env_guard();
        let path = consumer_lock_path("consumer-a").unwrap();
        let digest = Sha256::digest(b"consumer-a");
        let expected = format!("{digest:x}");
        let path_str = path.to_string_lossy();
        assert!(path_str.contains(LOCK_DIR), "{path_str}");
        assert!(path_str.contains(&expected), "{path_str}");
        assert!(!path_str.contains("consumer-a"), "{path_str}");
        assert!(path_str.ends_with(".lock"), "{path_str}");
    }
}
