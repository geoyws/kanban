//! Fixtures and helpers shared by the `e2e_*` integration targets
//! (t-2aeec40c). Cargo does not build `tests/<dir>/mod.rs` as a target; each
//! area declares `mod e2e_support;` and compiles its own copy. A helper only
//! one area uses stays in that area's file.
pub(crate) use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
pub(crate) use rusqlite::{Connection, params};
pub(crate) use serde_json::{Value, json};
pub(crate) use sha2::{Digest, Sha256};
pub(crate) use std::collections::BTreeMap;
pub(crate) use std::env;
pub(crate) use std::fs;
pub(crate) use std::io::{ErrorKind, Write};
pub(crate) use std::os::fd::AsRawFd;
pub(crate) use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::process::{Child, Command, Output, Stdio};
pub(crate) use std::sync::{Arc, Barrier, Mutex, OnceLock, mpsc};
pub(crate) use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
pub(crate) use syn::parse::Parser;
pub(crate) use syn::{
    Attribute, Expr, ExprLit, ExprMethodCall, ForeignItemFn, ImplItemFn, Item, Lit, Meta,
    Path as SynPath, Token, TraitItemFn, Variant,
    punctuated::Punctuated,
    visit::{self, Visit},
};
pub(crate) use uuid::Uuid;

pub(crate) struct Fixture {
    pub(crate) root: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) main: PathBuf,
    pub(crate) worktree: PathBuf,
}

impl Fixture {
    pub(crate) fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kanban-rust-e2e-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let main = root.join("main");
        let worktree = root.join("worktree");
        fs::create_dir_all(&main).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        // Both cwds are git repositories so provenance-bearing writes
        // (checkpoint, handoff create, sitrep post) capture a checkout instead
        // of refusing. Best-effort: without git the dirs stay plain, and only
        // tests that explicitly need provenance will fail.
        let _ = make_repo(&main);
        let _ = make_repo(&worktree);
        Self {
            root,
            data,
            main,
            worktree,
        }
    }

    pub(crate) fn command(&self, cwd: &Path) -> Command {
        self.command_with_data_dir(cwd, &self.data)
    }

    pub(crate) fn command_with_data_dir(&self, cwd: &Path, data_dir: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(cwd)
            .env("KANBAN_DATA_DIR", data_dir)
            .env_remove("KANBAN_DB")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    pub(crate) fn run(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd).args(args).output().unwrap()
    }

    pub(crate) fn ok_json(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command failed: {:?}\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

/// What a `--json` refusal leaves on stdout: an object holding only `error`,
/// so a parser meets the refusal rather than an empty result, and no answer
/// rides along with it. Returns the message.
pub(crate) fn refusal_object(output: &Output) -> String {
    assert!(
        !output.status.success(),
        "expected a refusal, got exit {:?}\nstdout: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "refusal stdout is not JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("refusal is not an object: {value}"));
    assert_eq!(
        object.keys().collect::<Vec<_>>(),
        ["error"],
        "a refusal carried more than its message: {value}"
    );
    object["error"]
        .as_str()
        .unwrap_or_else(|| panic!("refusal message is not a string: {value}"))
        .to_owned()
}

pub(crate) fn board_path_for_project(fixture: &Fixture, cwd: &Path, project_name: &str) -> PathBuf {
    fixture
        .ok_json(cwd, &["workspace", "list", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"].as_str() == Some(project_name))
        .unwrap_or_else(|| panic!("missing project named {project_name}"))["boardPath"]
        .as_str()
        .unwrap()
        .into()
}

pub(crate) static DB_LOCK_CONTENTION_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn db_lock_contention_test_guard() -> std::sync::MutexGuard<'static, ()> {
    DB_LOCK_CONTENTION_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Restore the pre-restriction shape before any older fixture is built.
///
/// First in the chain for the same reason [`remove_v29_tag_rename_schema`]
/// used to be: every "behind" board here is a CURRENT board stripped
/// downward, so the newest migration is undone first or the ladder meets its
/// own output. `task_claims.model` drops as a column because nothing but the
/// claim readers name it, and `task_models` drops outright.
pub(crate) fn remove_v30_model_restriction_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TABLE task_models; \
             ALTER TABLE task_claims DROP COLUMN model;",
        )
        .unwrap();
}

/// Restore the pre-rename `tags` shape before any older fixture is built.
///
/// Every "behind" board here is a CURRENT board stripped downward, so the
/// newest migration has to be undone first or the ladder's
/// `ALTER TABLE tags ADD COLUMN` meets a column that is already there. No view
/// or trigger reads these two, so dropping them is the whole of it.
pub(crate) fn remove_v29_tag_rename_schema(connection: &Connection) {
    remove_v30_model_restriction_schema(connection);
    connection
        .execute_batch(
            "ALTER TABLE tags DROP COLUMN renamed_from; \
             ALTER TABLE tags DROP COLUMN renamed_at;",
        )
        .unwrap();
}

/// Restore the exact v27 search corpus before removing v27's sprint tables.
/// Historical fixtures must exercise the real ladder input, not a current
/// schema with only user_version lowered.
pub(crate) fn remove_v28_sprint_search_schema(connection: &Connection) {
    remove_v29_tag_rename_schema(connection);
    let view_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='view' AND name='search_source_rows'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let sprint_arm = "\nUNION ALL\nSELECT\n 'sprint', sp.id, NULL,\n sp.title,\n COALESCE(sp.body,'') || char(10) || sp.target_version,\n sp.status, NULL, '',\n sp.created_at, sp.updated_at, sp.archived\nFROM sprints sp";
    let base_migration_sql = format!(
        "{};",
        view_sql
            .strip_suffix(sprint_arm)
            .expect("current search_source_rows must end with the v28 sprint arm")
    );
    connection.execute_batch(
        r#"
        DROP TRIGGER search_sprints_ai;
        DROP TRIGGER search_sprints_au;
        DROP TRIGGER search_sprints_ad;
        DROP TRIGGER search_documents_ai;
        DROP TRIGGER search_documents_ad;
        DROP TRIGGER search_documents_au;
        DROP TABLE search_fts;
        DROP VIEW search_source_rows;
        PRAGMA legacy_alter_table=ON;
        ALTER TABLE search_documents RENAME TO search_documents_v28;
        CREATE TABLE search_documents (
         seq INTEGER PRIMARY KEY,
         source_kind TEXT NOT NULL CHECK(source_kind IN ('task','note','checkpoint','handoff','attention','sitrep','rule','event')),
         source_id TEXT NOT NULL,
         task_id TEXT,
         title TEXT NOT NULL,
         body TEXT NOT NULL,
         status TEXT,
         lane TEXT,
         tags TEXT NOT NULL DEFAULT '',
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL,
         archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
         source_hash TEXT,
         embedding_model TEXT,
         embedding BLOB,
         UNIQUE(source_kind,source_id)
        ) STRICT;
        INSERT INTO search_documents(
         seq,source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,
         archived,source_hash,embedding_model,embedding
        )
        SELECT
         seq,source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,
         archived,source_hash,embedding_model,embedding
        FROM search_documents_v28 WHERE source_kind<>'sprint';
        DROP TABLE search_documents_v28;
        PRAGMA legacy_alter_table=OFF;
        CREATE INDEX idx_search_documents_source ON search_documents(source_kind,source_id);
        CREATE INDEX idx_search_documents_task ON search_documents(task_id);
        CREATE INDEX idx_search_documents_active ON search_documents(updated_at DESC) WHERE archived=0;
        CREATE VIRTUAL TABLE search_fts USING fts5(
         title, body, tags,
         content='search_documents', content_rowid='seq',
         tokenize='porter unicode61 remove_diacritics 2', prefix='2 3'
        );
        CREATE TRIGGER search_documents_ai AFTER INSERT ON search_documents BEGIN
         INSERT INTO search_fts(rowid,title,body,tags) VALUES(new.seq,new.title,new.body,new.tags);
        END;
        CREATE TRIGGER search_documents_ad AFTER DELETE ON search_documents BEGIN
         INSERT INTO search_fts(search_fts,rowid,title,body,tags)
         VALUES('delete',old.seq,old.title,old.body,old.tags);
        END;
        CREATE TRIGGER search_documents_au AFTER UPDATE OF title,body,tags ON search_documents BEGIN
         INSERT INTO search_fts(search_fts,rowid,title,body,tags)
         VALUES('delete',old.seq,old.title,old.body,old.tags);
         INSERT INTO search_fts(rowid,title,body,tags) VALUES(new.seq,new.title,new.body,new.tags);
        END;
        "#,
    ).unwrap();
    connection.execute_batch(&base_migration_sql).unwrap();
    connection
        .execute("INSERT INTO search_fts(search_fts) VALUES('rebuild')", [])
        .unwrap();
}

/// Restore the actual pre-sprint v26 shape before lowering a historical fixture.
pub(crate) fn remove_v27_sprint_schema(connection: &Connection) {
    remove_v28_sprint_search_schema(connection);
    connection.execute_batch(
        "ALTER TABLE deployments DROP COLUMN served_version; ALTER TABLE deployments DROP COLUMN target_version; ALTER TABLE deployments DROP COLUMN sprint_id; DROP TABLE task_sprints; DROP TABLE sprints;"
    ).unwrap();
}

pub(crate) fn remove_v13_search_schema(connection: &Connection) {
    let trigger_names = {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type='trigger' AND name LIKE 'search_%'")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    for trigger in trigger_names {
        connection
            .execute_batch(&format!("DROP TRIGGER \"{trigger}\""))
            .unwrap();
    }
    connection
        .execute_batch(
            "DROP VIEW search_source_rows;\
             DROP TABLE search_fts;\
             DROP TABLE search_documents;",
        )
        .unwrap();
}

pub(crate) fn remove_v18_board_audit_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DELETE FROM board_meta WHERE key LIKE 'audit_chain_%';\
             ALTER TABLE events DROP COLUMN event_hash;\
             ALTER TABLE events DROP COLUMN prev_hash;",
        )
        .unwrap();
}

/// Return a current fixture to the exact pre-subscription schema shape before
/// a historical migration test lowers `user_version` below V21.
pub(crate) fn remove_v21_subscription_schema(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TABLE subscription_delivery_attempts;\
             DROP TABLE subscription_deliveries;\
             DROP TABLE board_materialization_cursor;\
             DROP INDEX idx_subscriptions_consumer;\
             DROP INDEX idx_subscriptions_status;\
             DROP TABLE subscriptions;",
        )
        .unwrap();
}

/// A live MCP session, spoken over real pipes to the real binary.
pub(crate) struct Session {
    pub(crate) child: Option<std::process::Child>,
    pub(crate) outgoing: Option<std::process::ChildStdin>,
    pub(crate) incoming: std::sync::mpsc::Receiver<String>,
    pub(crate) reader: Option<std::thread::JoinHandle<()>>,
}

pub(crate) enum ShutdownResult {
    Clean(std::process::ExitStatus),
    TimedOut,
}

impl Session {
    pub(crate) fn start(binary: &Path, cwd: &Path, data: &Path) -> Self {
        Self::start_with_env(binary, cwd, data, &[])
    }

    /// A session whose server, and therefore every process it spawns to answer
    /// a call, inherits `env` on top of the usual fixture environment.
    pub(crate) fn start_with_env(
        binary: &Path,
        cwd: &Path,
        data: &Path,
        env: &[(&str, &str)],
    ) -> Self {
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut backoff = Duration::from_millis(10);
        let mut child = loop {
            match Command::new(binary)
                .arg("mcp")
                .current_dir(cwd)
                .env("KANBAN_DATA_DIR", data)
                .env_remove("KANBAN_DB")
                .env_remove("KANBAN_PROJECT")
                .envs(env.iter().copied())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => break child,
                Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                    if Instant::now() >= deadline {
                        panic!(
                            "spawn kanban mcp kept failing with ETXTBSY past the 750ms deadline: {error}"
                        );
                    }
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(50));
                }
                Err(error) => panic!("spawn kanban mcp: {error}"),
            }
        };
        let outgoing = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, incoming) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child: Some(child),
            outgoing: Some(outgoing),
            incoming,
            reader: Some(reader),
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    pub(crate) fn writer(&mut self) -> &mut std::process::ChildStdin {
        self.outgoing.as_mut().unwrap()
    }

    /// Write several requests in one syscall, so the server can pull more than
    /// one into a single read and genuinely hold an unparsed one in memory.
    /// Two separate writes usually arrive as two reads and never produce the
    /// state this is here to create.
    pub(crate) fn send_batch(&mut self, requests: &[Value]) {
        let mut frame = String::new();
        for request in requests {
            frame.push_str(&request.to_string());
            frame.push('\n');
        }
        self.writer().write_all(frame.as_bytes()).unwrap();
        self.writer().flush().unwrap();
    }

    /// The next reply, whichever request it belongs to.
    pub(crate) fn recv(&mut self) -> Value {
        let line = self
            .incoming
            .recv_timeout(Duration::from_secs(20))
            .expect("the server owed a reply and did not send one");
        serde_json::from_str(&line).unwrap()
    }

    /// Send one request and wait for its reply. Never blocks forever: a hung
    /// server is a failure, not a test that runs until someone kills it.
    pub(crate) fn ask(&mut self, request: Value) -> Value {
        writeln!(self.writer(), "{request}").unwrap();
        self.writer().flush().unwrap();
        let line = self
            .incoming
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no reply to {request}"));
        serde_json::from_str(&line).unwrap()
    }

    pub(crate) fn finish(mut self) {
        match self.shutdown(Duration::from_secs(5)) {
            ShutdownResult::Clean(status) => {
                assert!(
                    status.success(),
                    "session exited nonzero after a clean EOF: {status}"
                );
            }
            ShutdownResult::TimedOut => {
                panic!("session did not exit cleanly before the 5-second timeout");
            }
        }
    }

    pub(crate) fn shutdown(&mut self, timeout: Duration) -> ShutdownResult {
        let Some(mut child) = self.child.take() else {
            return ShutdownResult::TimedOut;
        };
        let _ = self.outgoing.take();
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if let Some(reader) = self.reader.take() {
                        let _ = reader.join();
                    }
                    return ShutdownResult::Clean(status);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let _ = child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        ShutdownResult::TimedOut
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown(Duration::ZERO);
    }
}

pub(crate) fn write_executable(path: &Path, body: &str) {
    // Written beside the target and renamed over it, because that is how a
    // binary is actually replaced -- and the rename is what leaves the running
    // process holding an unlinked inode.
    let staging = path.with_extension("staging");
    fs::write(&staging, body).unwrap();
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&staging, path).unwrap();
}

pub(crate) fn file_sha256(path: &Path) -> String {
    let mut file = fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer).unwrap();
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    format!("{:x}", hasher.finalize())
}

/// Every executable the crate declares, in `Cargo.toml` order.
///
/// Read out of the manifest instead of typed out again: the release package
/// has to carry exactly what `cargo build --release --locked --bins`
/// produces, and a second hand-written list is precisely what let the package
/// fall four binaries behind the crate.
pub(crate) fn declared_bin_names() -> Vec<String> {
    let manifest =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    let mut names = Vec::new();
    let mut in_bin_section = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_bin_section = line == "[[bin]]";
            continue;
        }
        if !in_bin_section {
            continue;
        }
        if let Some(value) = line.strip_prefix("name")
            && let Some(value) = value.trim_start().strip_prefix('=')
        {
            names.push(value.trim().trim_matches('"').to_string());
        }
    }
    assert!(
        !names.is_empty(),
        "Cargo.toml declares no [[bin]] targets, so the parse is wrong"
    );
    names
}

pub(crate) fn clone_release_package(source: &Path, target: &Path, source_commit: &str) {
    fs::create_dir_all(target).unwrap();
    for name in declared_bin_names() {
        fs::copy(source.join(&name), target.join(&name)).unwrap();
    }
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(source.join("manifest.json")).unwrap()).unwrap();
    manifest["sourceCommit"] = json!(source_commit);
    fs::write(
        target.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut receipt: Value =
        serde_json::from_slice(&fs::read(source.with_extension("receipt.json")).unwrap()).unwrap();
    receipt["sourceCommit"] = json!(source_commit);
    receipt["manifestSha256"] = json!(file_sha256(&target.join("manifest.json")));
    fs::write(
        target.with_extension("receipt.json"),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
}

pub(crate) struct HaxInstallContext<'a> {
    pub(crate) fixture: &'a Fixture,
    pub(crate) script: &'a Path,
    pub(crate) path: &'a str,
    pub(crate) hostname_bin: &'a Path,
    pub(crate) fake_repo_root: &'a Path,
    pub(crate) remote_root: &'a Path,
}

pub(crate) fn install_matching_hax_package(
    ctx: &HaxInstallContext<'_>,
    package_dir: &Path,
    commit: &str,
    label: &str,
) -> PathBuf {
    let hax_install_root = ctx.fixture.root.join(format!("{label}-install-hax"));
    let hax_bin_dir = ctx.fixture.root.join(format!("{label}-bin-hax"));
    let installed = Command::new("bash")
        .current_dir(&ctx.fixture.main)
        .env("PATH", ctx.path)
        .env("HOSTNAME_BIN", ctx.hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(ctx.hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", ctx.fake_repo_root)
        .env("FAKE_GIT_HEAD", commit)
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", ctx.remote_root)
        .arg(ctx.script)
        .args([
            "install",
            "hax",
            "--package",
            package_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "HAX install for {label} failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(
        PathBuf::from(installed_json["installRoot"].as_str().unwrap()),
        hax_install_root
    );
    hax_install_root
}

pub(crate) fn release_id_from_package(package_dir: &Path) -> String {
    let receipt: Value =
        serde_json::from_slice(&fs::read(package_dir.with_extension("receipt.json")).unwrap())
            .unwrap();
    format!(
        "{}-{}",
        receipt["sourceCommit"].as_str().unwrap(),
        receipt["manifestSha256"].as_str().unwrap()
    )
}

/// Byte-level picture of a tree that never follows symlinks: a planted link is
/// recorded as its target text, so a write that went THROUGH it shows up in
/// the snapshot of the directory it pointed at, not here.
pub(crate) fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, (&'static str, Vec<u8>)> {
    fn walk(root: &Path, path: &Path, out: &mut BTreeMap<PathBuf, (&'static str, Vec<u8>)>) {
        let relative = path.strip_prefix(root).unwrap().to_path_buf();
        let meta = fs::symlink_metadata(path).unwrap();
        if meta.file_type().is_symlink() {
            let target = fs::read_link(path).unwrap();
            out.insert(
                relative,
                ("link", target.to_string_lossy().into_owned().into_bytes()),
            );
        } else if meta.is_dir() {
            out.insert(relative, ("dir", Vec::new()));
            for entry in fs::read_dir(path).unwrap() {
                walk(root, &entry.unwrap().path(), out);
            }
        } else {
            out.insert(relative, ("file", fs::read(path).unwrap()));
        }
    }
    let mut out = BTreeMap::new();
    match fs::symlink_metadata(root) {
        Ok(_) => walk(root, root, &mut out),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => panic!("snapshot {}: {error}", root.display()),
    }
    out
}

pub(crate) fn capture_release_links(
    install_root: &Path,
    bin_dir: &Path,
) -> BTreeMap<String, PathBuf> {
    let mut links = BTreeMap::new();
    links.insert(
        "current".to_string(),
        fs::read_link(install_root.join("current")).unwrap(),
    );
    for name in declared_bin_names() {
        links.insert(name.clone(), fs::read_link(bin_dir.join(&name)).unwrap());
    }
    links
}

pub(crate) fn assert_release_view(install_root: &Path, bin_dir: &Path, release_dir: &Path) {
    let current_link = install_root.join("current");
    assert!(current_link.is_symlink(), "current symlink missing");
    assert_eq!(fs::read_link(&current_link).unwrap(), release_dir);
    for name in declared_bin_names() {
        let symlink = bin_dir.join(&name);
        assert!(symlink.is_symlink(), "missing bin symlink {name}");
        assert_eq!(fs::read_link(&symlink).unwrap(), current_link.join(&name));
    }
}

/// The twenty header bytes that make a file the platform a Kanban release
/// targets: ELF magic, 64-bit class, little-endian data, `ET_DYN` in
/// `e_type` - a release-profile Rust binary on linux is PIE - and
/// `EM_X86_64` in `e_machine`. The one place in this suite that writes that
/// header, so the containerised build that will eventually produce the real
/// thing has one place to meet.
pub(crate) fn linux_x86_64_elf_header() -> [u8; 20] {
    elf_header(2, 1, 0x3e, 3)
}

/// An ELF header with the class, data encoding, machine and object type a
/// case asks for, so a fixture can be an image no host could run - a
/// relocatable object, a core dump - while the gate still reads a true
/// header off it.
pub(crate) fn elf_header(class: u8, data: u8, machine: u16, etype: u16) -> [u8; 20] {
    let mut header = [0_u8; 20];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = class;
    header[5] = data;
    header[6] = 1; // EV_CURRENT
    header[16..18].copy_from_slice(&etype.to_le_bytes());
    header[18..20].copy_from_slice(&machine.to_le_bytes());
    header
}

/// A build output that IS the release platform, carrying `payload` under the
/// header: what the fake build produces, and what a case needs when it ships
/// a rebuilt binary that has to report something of its own. The runner runs
/// the payload, so the payload is what answers `version`.
pub(crate) fn write_release_platform_image(path: &Path, payload: &[u8]) {
    let mut image = linux_x86_64_elf_header().to_vec();
    image.extend_from_slice(payload);
    fs::write(path, &image).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The program the suite hands the release script as
/// `HIG_RELEASE_TARGET_RUNNER`: this host cannot execute a linux x86-64
/// image, so it names what runs one. Written beside the other release stubs.
pub(crate) fn release_target_runner(hostname_bin: &Path) -> PathBuf {
    hostname_bin.parent().unwrap().join("target-runner")
}

/// The program the suite hands the release script as
/// `KANBAN_RELEASE_CONTAINER_RUNTIME`: every machine this suite runs the
/// packaging path on is measured as one that is not linux x86-64, so the
/// capability gate takes the container leg, and it takes it against a stub
/// rather than a daemon. Written beside the other release stubs.
pub(crate) fn release_container_runtime(hostname_bin: &Path) -> PathBuf {
    hostname_bin.parent().unwrap().join("container-runtime")
}

/// The digest-pinned builder image every container case names. It is the
/// reference the real container build on `@@mbp` ran against on 2026-09-10,
/// which is why it is a true digest rather than a made-up one - but nothing
/// here pulls it: the runtime that receives it is the stub above, and what
/// the gate checks about this string is that it is digest-pinned and that the
/// runtime handed it reports x86_64.
pub(crate) const RELEASE_BUILDER_IMAGE: &str =
    "rust@sha256:3914072ca0c3b8aad871db9169a651ccfce30cf58303e5d6f2db16d1d8a7e58f";

pub(crate) fn write_release_tool_stubs(
    fixture: &Fixture,
    fake_repo_root: &Path,
    fake_git_head: &str,
    fake_release_binary: &str,
    fake_host: &str,
) -> PathBuf {
    let stubs = fixture.root.join("release-stubs");
    fs::create_dir_all(&stubs).unwrap();
    let kb_skill = fake_repo_root.join("skills/kb/SKILL.md");
    fs::create_dir_all(kb_skill.parent().unwrap()).unwrap();
    fs::write(&kb_skill, "# kb skill fixture\n").unwrap();
    write_executable(
        &stubs.join("hostname"),
        r#"#!/bin/sh
set -eu
printf '%s\n' "${FAKE_HOST:?}"
"#,
    );
    write_executable(
        &stubs.join("git"),
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "-C" ]; then
  shift 2
fi
case "${1:-}" in
  status)
    exit 0
    ;;
  rev-parse)
    case "${2:-}" in
      --show-toplevel)
        printf '%s\n' "${FAKE_REPO_ROOT:?}"
        ;;
      HEAD)
        printf '%s\n' "${FAKE_GIT_HEAD:?}"
        ;;
      *)
        printf 'unexpected git rev-parse %s\n' "$*" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    printf 'unexpected git %s\n' "$*" >&2
    exit 1
    ;;
esac
"#,
    );
    let platform_header = stubs.join("release-platform-header");
    fs::write(&platform_header, linux_x86_64_elf_header()).unwrap();
    write_executable(
        &stubs.join("uname"),
        // The capability gate measures the MACHINE, so the fixture is what
        // declares which machine the script is running on - exactly as the
        // hostname stub declares what it calls itself, and the two are now
        // different powers: the name records, the platform decides.
        //
        // The default is a machine that is NOT the release platform, which is
        // this Mac and is the case the whole container path exists for, so
        // every packaging case in this suite drives the same branch wherever
        // the suite itself runs. A case that wants the native path says so.
        r#"#!/bin/sh
set -eu
case "${1:-}" in
  -s)
    printf '%s\n' "${FAKE_UNAME_S:-Darwin}"
    ;;
  -m)
    printf '%s\n' "${FAKE_UNAME_M:-arm64}"
    ;;
  *)
    command -p uname "$@"
    ;;
esac
"#,
    );
    write_executable(
        &stubs.join("rustc"),
        // The other half of the recorded toolchain on the native path.
        r#"#!/bin/sh
set -eu
case "${1:-}" in
  --version)
    printf '%s\n' "${FAKE_RUSTC_VERSION:-rustc 1.90.0 (fake host toolchain)}"
    ;;
  *)
    printf 'unexpected rustc %s\n' "$*" >&2
    exit 1
    ;;
esac
"#,
    );
    write_executable(
        &stubs.join("container-runtime"),
        // The OCI runtime the capability gate finds and the container build
        // runs through, so no case in this suite needs a daemon. It holds the
        // runtime contract the gate depends on and refuses anything outside
        // it: the verb is `run`, the platform asked for is linux/amd64, and
        // the image is digest-pinned - a stub that accepted a tag would let a
        // regression in the gate pass unnoticed.
        //
        // Inside the "image" it answers the three things a containerised
        // release build asks: what machine it is, what toolchain it carries,
        // and - because a Mac cannot execute a linux x86-64 image at all -
        // what a packaged binary says when run. FAKE_CONTAINER_UNAME is how a
        // case makes a pinned image that does not run the release platform,
        // FAKE_CONTAINER_HANGS a runtime that has wedged, and
        // FAKE_CONTAINER_FAILS one whose daemon is not there to answer.
        &[
            r#"#!/bin/sh
set -eu
if [ -n "${FAKE_CONTAINER_LOG:-}" ]; then
  printf '%s %s\n' "${0##*/}" "$*" >> "$FAKE_CONTAINER_LOG"
fi
if [ -n "${FAKE_CONTAINER_HANGS:-}" ]; then
  # Wedged: it accepted the call and will never answer it. `exec` so the
  # process a deadline kills is this one and no sleep outlives the case.
  exec sleep 600
fi
if [ -n "${FAKE_CONTAINER_FAILS:-}" ]; then
  printf 'cannot connect to the container daemon: is it running?\n' >&2
  exit 3
fi
[ "${1:-}" = "run" ] || {
  printf 'unexpected container runtime verb %s\n' "${1:-}" >&2
  exit 1
}
shift
platform=""
workdir=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --rm)
      shift
      ;;
    --platform)
      platform="$2"
      shift 2
      ;;
    --volume | -v)
      shift 2
      ;;
    --workdir | -w)
      workdir="$2"
      shift 2
      ;;
    --env | -e)
      export "${2:?--env needs KEY=VALUE}"
      shift 2
      ;;
    -*)
      printf 'unexpected container runtime flag %s\n' "$1" >&2
      exit 1
      ;;
    *)
      break
      ;;
  esac
done
[ "$platform" = "linux/amd64" ] || {
  printf 'container runtime was asked for platform "%s", not linux/amd64\n' "$platform" >&2
  exit 1
}
image="${1:?container runtime needs an image}"
shift
case "$image" in
  *@sha256:*)
    ;;
  *)
    printf 'container runtime was handed the unpinned image %s\n' "$image" >&2
    exit 1
    ;;
esac
[ -z "$workdir" ] || cd "$workdir"
# How a tool inside the image answers `--version`: the pinned answer, or -
# for a case whose image cannot report a toolchain at all - nothing, or a
# failure carrying the image's own complaint. FAKE_CONTAINER_TOOLCHAIN_MUTE
# and FAKE_CONTAINER_TOOLCHAIN_FAILS each name the tool they apply to, or
# `all` for both, and FAKE_CONTAINER_TOOLCHAIN_COMPLAINT is what a failure
# says on stderr - unset for one that fails without saying anything.
toolchain_answer() {
  case "${FAKE_CONTAINER_TOOLCHAIN_MUTE:-}" in
    "$1" | all)
      exit 0
      ;;
  esac
  case "${FAKE_CONTAINER_TOOLCHAIN_FAILS:-}" in
    "$1" | all)
      [ -z "${FAKE_CONTAINER_TOOLCHAIN_COMPLAINT:-}" ] ||
        printf '%s\n' "$FAKE_CONTAINER_TOOLCHAIN_COMPLAINT" >&2
      exit 3
      ;;
  esac
  printf '%s\n' "$2"
}
case "${1:-}" in
  uname)
    printf '%s\n' "${FAKE_CONTAINER_UNAME:-x86_64}"
    ;;
  rustc)
    [ -z "${FAKE_CONTAINER_HANGS_TOOLCHAIN:-}" ] || exec sleep 600
    toolchain_answer rustc "${FAKE_CONTAINER_RUSTC:-rustc 1.90.0 (fake pinned image)}"
    ;;
  cargo)
    if [ "${2:-}" = "--version" ]; then
      [ -z "${FAKE_CONTAINER_HANGS_TOOLCHAIN:-}" ] || exec sleep 600
      toolchain_answer cargo "${FAKE_CONTAINER_CARGO:-cargo 1.90.0 (fake pinned image)}"
    else
      "$@"
    fi
    ;;
  *)
    # A release binary, run inside the image because this host cannot run one:
    # the header the fake build stamped on comes off and what is underneath
    # answers, so the version recorded is the packaged bytes' own answer.
    binary="${1:?container runtime needs a command}"
    shift
    [ -x "$binary" ] || {
      printf '%s: cannot execute\n' "$binary" >&2
      exit 126
    }
    payload_cache=""#,
            stubs.join("container-payloads").to_str().unwrap(),
            r#""
    mkdir -p "$payload_cache"
    payload="$payload_cache/$(sha256sum "$binary" | awk '{print $1}')"
    if [ ! -x "$payload" ]; then
      staged="$payload.$$"
      tail -c +"#,
            (linux_x86_64_elf_header().len() + 1).to_string().as_str(),
            r#" "$binary" > "$staged"
      chmod 0755 "$staged"
      mv "$staged" "$payload"
    fi
    "$payload" "$@"
    ;;
esac
"#,
        ]
        .concat(),
    );
    write_executable(
        &stubs.join("cargo"),
        // `cargo build --bins` produces every declared executable, so the stub
        // stands in for all of them: a release script that enumerates fewer
        // must still be caught by what it packages, not by a short fake build.
        //
        // What it produces is a linux x86-64 image, because that is what a
        // release is. This host builds Mach-O, so the stub stamps the header a
        // real linux build would carry onto the executable it copies: the
        // script's platform gate then reads a true ELF header off the build
        // output, and HIG_RELEASE_TARGET_RUNNER is what runs the executable
        // underneath it. FAKE_RELEASE_IMAGE hands the whole image over
        // instead, for a case whose build must produce something else.
        &[
            r#"#!/bin/sh
set -eu
case "${1:-}" in
  build)
    ;;
  --version)
    # The toolchain the receipt records is asked for its version in the
    # environment that compiled the binaries, and on the native path that is
    # this host's own cargo.
    printf '%s\n' "${FAKE_CARGO_VERSION:-cargo 1.90.0 (fake host toolchain)}"
    exit 0
    ;;
  *)
    printf 'unexpected cargo %s\n' "$*" >&2
    exit 1
    ;;
esac
target_root="${CARGO_TARGET_DIR:?}/release"
mkdir -p "$target_root"
for binary in "#,
            declared_bin_names().join(" ").as_str(),
            r#"; do
  source="${FAKE_RELEASE_BINARY:?}"
  if [ -n "${FAKE_RELEASE_BINARY_DIR:-}" ]; then
    source="$FAKE_RELEASE_BINARY_DIR/$binary"
  fi
  if [ -n "${FAKE_RELEASE_FIFO:-}" ]; then
    # A build output nothing can read to the end: reading it to judge it is
    # what hangs, so the script has to refuse it on its type instead.
    mkfifo "$target_root/$binary"
  elif [ -n "${FAKE_RELEASE_SYMLINK:-}" ]; then
    ln -s "$FAKE_RELEASE_SYMLINK" "$target_root/$binary"
    continue
  elif [ -n "${FAKE_RELEASE_IMAGE:-}" ]; then
    cp "$FAKE_RELEASE_IMAGE" "$target_root/$binary"
  else
    cat ""#,
            platform_header.to_str().unwrap(),
            r#"" "$source" > "$target_root/$binary"
  fi
  chmod 0755 "$target_root/$binary"
done
"#,
        ]
        .concat(),
    );
    write_executable(
        &stubs.join("target-runner"),
        // Stands in for executing a release binary, which this host cannot do:
        // a linux x86-64 ELF is not loadable here, and bash refuses to fall
        // back to running an ELF-magic file as a script. So it runs what is
        // inside the image it was handed - the header off, the executable
        // underneath run - which is why the version the script records is the
        // packaged bytes' own answer and not this stub's opinion. A case that
        // ships a rebuilt binary reporting a different version is answered
        // correctly without telling the runner anything.
        //
        // Mode bits are honoured: a package binary the installer could not
        // have executed must fail here too. FAKE_TARGET_RUNNER_FAILS and
        // FAKE_TARGET_RUNNER_SILENT are the two ways a runner can let the
        // script down - it cannot run the image, or it runs it and says
        // nothing - and the attempt is logged before either, so a case
        // asserting what was attempted asserts something that could be false.
        &[
            r#"#!/bin/sh
set -eu
binary="$1"
shift
[ -x "$binary" ] || {
  printf '%s: cannot execute\n' "$binary" >&2
  exit 126
}
if [ -n "${FAKE_TARGET_RUNNER_LOG:-}" ]; then
  printf '%s %s\n' "${binary##*/}" "$*" >> "$FAKE_TARGET_RUNNER_LOG"
fi
if [ -n "${FAKE_TARGET_RUNNER_FAILS:-}" ]; then
  printf 'cannot run %s on this host\n' "$binary" >&2
  exit 1
fi
if [ -n "${FAKE_TARGET_RUNNER_SILENT:-}" ]; then
  exit 0
fi
# Content-addressed, because an install probes the same image several times
# and copying it out again each time is the slowest thing in these cases. The
# key is the image's own hash, so a rebuilt binary is never answered by the
# payload of the one it replaced.
payload_cache=""#,
            stubs.join("target-payloads").to_str().unwrap(),
            r#""
mkdir -p "$payload_cache"
payload="$payload_cache/$(sha256sum "$binary" | awk '{print $1}')"
if [ ! -x "$payload" ]; then
  staged="$payload.$$"
  tail -c +"#,
            (linux_x86_64_elf_header().len() + 1).to_string().as_str(),
            r#" "$binary" > "$staged"
  chmod 0755 "$staged"
  mv "$staged" "$payload"
fi
status=0
"$payload" "$@" || status=$?
exit "$status"
"#,
        ]
        .concat(),
    );
    write_executable(
        &stubs.join("date"),
        r#"#!/bin/sh
set -eu
if [ "${1:-}" = "+%s" ] && [ -n "${FAKE_RELEASE_DATE_SECONDS:-}" ]; then
  printf '%s\n' "$FAKE_RELEASE_DATE_SECONDS"
  exit 0
fi
command -p date "$@"
"#,
    );
    write_executable(
        &stubs.join("install"),
        r#"#!/bin/sh
set -eu
mode=0755
while [ "$#" -gt 0 ]; do
  case "$1" in
    -m)
      mode="$2"
      shift 2
      ;;
    -*)
      printf 'unexpected install flag %s\n' "$1" >&2
      exit 1
      ;;
    *)
      break
      ;;
  esac
done
src="$1"
dest="$2"
mkdir -p "$(dirname "$dest")"
cp "$src" "$dest"
chmod "$mode" "$dest"
"#,
    );
    write_executable(
        &stubs.join("systemctl"),
        // A `kanban-serve` unit exists only for a test that asks for one, and
        // the stub decides that, never the host: this suite runs on hax and
        // hig, where a real kanban-serve is serving the authoritative boards,
        // and a test that reached the real systemctl would restart it.
        //
        // `show` is the whole classifier surface, so it answers the way
        // systemd does: `KEY=value` lines, bare values under `--value`, exit
        // 0 whenever the MANAGER answers -- an absent unit is
        // `LoadState=not-found`, not a failed call. A manager that cannot be
        // reached at all (FAKE_SERVE_MANAGER_FAILS) fails every call instead,
        // which is the case an installer must never read as "no unit here".
        r#"#!/bin/sh
set -eu
if [ -n "${FAKE_SERVE_MANAGER_FAILS:-}" ]; then
  # The ATTEMPT is recorded before the failure, so a test asserting that no
  # restart was attempted is asserting something that could have been false.
  case "${1:-}" in
    restart | stop)
      if [ -n "${FAKE_SERVE_RESTART_LOG:-}" ]; then
        printf '%s %s\n' "$1" "${2:-}" >> "$FAKE_SERVE_RESTART_LOG"
      fi
      ;;
  esac
  printf 'Failed to connect to bus: No such file or directory\n' >&2
  exit 1
fi
present="${FAKE_SERVE_UNIT_PRESENT:-}"
# Set once the HTTP probe has been answered, so a test can make the unit
# change under the installer at exactly the moment a 200 has been collected.
after=""
if [ -n "${FAKE_SERVE_CURL_TRIGGER:-}" ] && [ -e "${FAKE_SERVE_CURL_TRIGGER}" ]; then
  after=1
fi
case "${1:-}" in
  restart)
    [ -n "$present" ] || {
      printf 'Failed to restart %s.service: Unit not found.\n' "${2:-}" >&2
      exit 5
    }
    if [ -n "${FAKE_SERVE_RESTART_LOG:-}" ]; then
      printf 'restart %s\n' "${2:-}" >> "$FAKE_SERVE_RESTART_LOG"
    fi
    [ -z "${FAKE_SERVE_RESTART_FAILS:-}" ] || {
      printf 'Job for %s.service failed.\n' "${2:-}" >&2
      exit 1
    }
    # systemd writes job progress to stdout. An installer that folds stdout
    # into its own JSON channel is broken by exactly this line.
    printf 'Job for %s.service finished.\n' "${2:-}"
    ;;
  stop)
    [ -n "$present" ] || {
      printf 'Failed to stop %s.service: Unit not found.\n' "${2:-}" >&2
      exit 5
    }
    if [ -n "${FAKE_SERVE_RESTART_LOG:-}" ]; then
      printf 'stop %s\n' "${2:-}" >> "$FAKE_SERVE_RESTART_LOG"
    fi
    [ -z "${FAKE_SERVE_STOP_FAILS:-}" ] || {
      printf 'Job for %s.service failed.\n' "${2:-}" >&2
      exit 1
    }
    printf 'Stopped %s.service.\n' "${2:-}"
    ;;
  show)
    shift
    bare=0
    properties=""
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --value)
          bare=1
          shift
          ;;
        -p)
          properties="$properties $2"
          shift 2
          ;;
        *)
          shift
          ;;
      esac
    done
    for property in $properties; do
      case "$property" in
        LoadState)
          if [ -n "$present" ]; then value=loaded; else value=not-found; fi
          ;;
        UnitFileState)
          if [ -n "$present" ]; then value="${FAKE_SERVE_UNIT_FILE_STATE:-enabled}"; else value=""; fi
          ;;
        ActiveState)
          if [ -n "$present" ]; then value="${FAKE_SERVE_ACTIVE_STATE:-active}"; else value=inactive; fi
          if [ -n "$after" ] && [ -n "${FAKE_SERVE_ACTIVE_STATE_AFTER:-}" ]; then
            value="$FAKE_SERVE_ACTIVE_STATE_AFTER"
          fi
          ;;
        MainPID)
          value="${FAKE_SERVE_MAIN_PID:-0}"
          if [ -n "$after" ] && [ -n "${FAKE_SERVE_MAIN_PID_AFTER:-}" ]; then
            value="$FAKE_SERVE_MAIN_PID_AFTER"
          fi
          ;;
        ExecStart)
          if [ -n "${FAKE_SERVE_EXEC_START:-}" ]; then
            value="$FAKE_SERVE_EXEC_START"
          else
            argv="/root/.local/bin/kanban serve"
            if [ -n "${FAKE_SERVE_SOCKET:-}" ]; then
              argv="$argv --socket \"$FAKE_SERVE_SOCKET\""
            else
              argv="$argv --port ${FAKE_SERVE_PORT:-14200}"
            fi
            value="{ path=/root/.local/bin/kanban ; argv[]=$argv --actor-header X-Auth-Request-Email ; ignore_errors=no }"
          fi
          ;;
        *)
          printf 'unexpected systemctl show -p %s\n' "$property" >&2
          exit 1
          ;;
      esac
      if [ "$bare" -eq 1 ]; then
        printf '%s\n' "$value"
      else
        printf '%s=%s\n' "$property" "$value"
      fi
    done
    ;;
  *)
    printf 'unexpected systemctl %s\n' "$*" >&2
    exit 1
    ;;
esac
"#,
    );
    write_executable(
        &stubs.join("curl"),
        // Answers the release script's status probe, and records the listener
        // it was asked for so a test can prove the probe followed the unit's
        // own ExecStart. FAKE_SERVE_HTTP_BAD_RELEASE makes the answer depend
        // on which release `current` points at, which is how a test models one
        // release that serves and one that does not. FAKE_SERVE_CURL_REAL
        // hands the request to the real curl, for a case that must measure a
        // real stalled socket rather than a stub's opinion of one.
        r#"#!/bin/sh
set -eu
if [ -n "${FAKE_SERVE_CURL_REAL:-}" ]; then
  [ -z "${FAKE_SERVE_CURL_STARTED:-}" ] || : > "$FAKE_SERVE_CURL_STARTED"
  [ -z "${FAKE_SERVE_STAGE_LOG:-}" ] || printf 'curl-real-start\n' >> "$FAKE_SERVE_STAGE_LOG"
  command -p curl "$@"
  exit "$?"
fi
url=""
socket=""
previous=""
for arg in "$@"; do
  case "$previous" in
    --unix-socket)
      socket="$arg"
      ;;
  esac
  case "$arg" in
    http://*)
      url="$arg"
      ;;
  esac
  previous="$arg"
done
if [ -n "${FAKE_SERVE_CURL_LOG:-}" ]; then
  if [ -n "$socket" ]; then
    printf '%s %s\n' "$socket" "$url" >> "$FAKE_SERVE_CURL_LOG"
  else
    printf '%s\n' "$url" >> "$FAKE_SERVE_CURL_LOG"
  fi
fi
status="${FAKE_SERVE_HTTP:-200}"
if [ -n "${FAKE_SERVE_HTTP_BAD_RELEASE:-}" ] && [ -n "${FAKE_SERVE_CURRENT_LINK:-}" ]; then
  current="$(readlink "$FAKE_SERVE_CURRENT_LINK" 2>/dev/null || true)"
  if [ "${current##*/}" = "$FAKE_SERVE_HTTP_BAD_RELEASE" ]; then
    status="${FAKE_SERVE_HTTP_BAD_STATUS:-503}"
  fi
fi
if [ -n "${FAKE_SERVE_CURL_TRIGGER:-}" ]; then
  # A 200 has now been collected. Anything the test wants to change about the
  # unit from here happens between the probe and the revalidation that follows
  # it, which is the window a crashing service actually uses.
  : > "$FAKE_SERVE_CURL_TRIGGER"
fi
printf '%s' "$status"
"#,
    );
    write_executable(
        &stubs.join("serve-exe-of-pid"),
        // Stands in for `readlink /proc/<pid>/exe`, which does not exist on
        // macOS. It refuses any pid but the one the unit reported: an exe
        // proof taken against some other process proves nothing. Every call
        // is appended to FAKE_SERVE_EXE_CALL_LOG when a test sets one, and
        // the first FAKE_SERVE_EXE_EARLY_CALLS of them answer
        // FAKE_SERVE_EXE_EARLY instead, which is how a test reproduces the
        // window where the unit is already active with a MainPID whose exe is
        // still systemd's pre-exec helper rather than the service binary.
        // FAKE_SERVE_EXE_FROM_CURRENT answers whatever `current` points at
        // when it is asked, which is how a unit whose ExecStart runs the
        // public bin link behaves across a rollback and the restart after it.
        r#"#!/bin/sh
set -eu
expected_pid="${FAKE_SERVE_MAIN_PID:?}"
[ -z "${FAKE_SERVE_EXE_STARTED:-}" ] || : > "$FAKE_SERVE_EXE_STARTED"
[ -z "${FAKE_SERVE_STAGE_LOG:-}" ] || printf 'exe-start\n' >> "$FAKE_SERVE_STAGE_LOG"
[ -z "${FAKE_SERVE_EXE_DELAY_SECONDS:-}" ] || sleep "$FAKE_SERVE_EXE_DELAY_SECONDS"
if [ -n "${FAKE_SERVE_CURL_TRIGGER:-}" ] && [ -e "${FAKE_SERVE_CURL_TRIGGER}" ]; then
  expected_pid="${FAKE_SERVE_MAIN_PID_AFTER:-$expected_pid}"
fi
[ "${1:-}" = "$expected_pid" ] || {
  printf 'exe probe asked about pid %s, not the unit MainPID %s\n' "${1:-}" "$expected_pid" >&2
  exit 1
}
calls=1
if [ -n "${FAKE_SERVE_EXE_CALL_LOG:-}" ]; then
  printf '%s\n' "$1" >> "$FAKE_SERVE_EXE_CALL_LOG"
  calls=$(( $(wc -l < "$FAKE_SERVE_EXE_CALL_LOG") ))
fi
if [ "$calls" -le "${FAKE_SERVE_EXE_EARLY_CALLS:-0}" ]; then
  printf '%s\n' "${FAKE_SERVE_EXE_EARLY:?}"
  exit 0
fi
if [ -n "${FAKE_SERVE_EXE_FROM_CURRENT:-}" ]; then
  current="$(readlink "$FAKE_SERVE_EXE_FROM_CURRENT" 2>/dev/null || true)"
  [ -n "$current" ] || exit 0
  printf '%s/kanban\n' "$current"
  exit 0
fi
if [ -n "${FAKE_SERVE_CURL_TRIGGER:-}" ] && [ -e "${FAKE_SERVE_CURL_TRIGGER}" ] && [ -n "${FAKE_SERVE_EXE_AFTER:-}" ]; then
  printf '%s\n' "$FAKE_SERVE_EXE_AFTER"
  exit 0
fi
printf '%s\n' "${FAKE_SERVE_EXE:?}"
"#,
    );
    write_executable(
        &stubs.join("ssh"),
        r#"#!/bin/sh
set -eu
host="$1"
shift
if [ -n "${FAKE_SSH_INVOCATION_LOG:-}" ]; then
  printf '%s\n' "$host $*" >> "$FAKE_SSH_INVOCATION_LOG"
fi
case "${1:-}" in
  hostname)
    printf '%s\n' "$host"
    ;;
  mktemp\ -d*)
    mktemp -d "${FAKE_REMOTE_ROOT:?}/$host.XXXXXX"
    ;;
  bash)
    shift 3
    hidden_path=""
    restore_hidden_path() {
      if [ -n "$hidden_path" ]; then
        mv "$hidden_path" "${FAKE_SSH_HIDE_PATH:?}"
        hidden_path=""
      fi
    }
    if [ -n "${FAKE_SSH_HIDE_PATH:-}" ]; then
      hidden_path="${FAKE_SSH_HIDE_PATH}.fake-ssh-hidden"
      [ ! -e "$hidden_path" ]
      mv "$FAKE_SSH_HIDE_PATH" "$hidden_path"
      [ ! -e "$FAKE_SSH_HIDE_PATH" ]
      trap restore_hidden_path EXIT HUP INT TERM
    fi
    if [ -n "${FAKE_SSH_SWAP_STAGED:-}" ]; then
      # What arrived on the remote host is not what hax validated: a transfer
      # that garbled a binary, or a package staged by hand. The remote leg has
      # to judge the bytes it actually holds, so the staged copy is replaced
      # after the transfer and before the remote script runs. "$1" is the
      # stage root and "$2" the staged package, the argv install_remote uses.
      cp "$FAKE_SSH_SWAP_STAGED" "$2/${FAKE_SSH_SWAP_NAME:-kanban}"
    fi
    FAKE_HOST="$host" bash -s -- "$@"
    ;;
  *)
    FAKE_HOST="$host" bash -lc "$*"
    ;;
esac
"#,
    );
    let _ = (
        fake_repo_root,
        fake_git_head,
        fake_release_binary,
        fake_host,
    );
    stubs
}

/// The text a `tools/call` result carries, whichever way it went.
pub(crate) fn tool_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .expect("a tool result carries content")
        .iter()
        .map(|part| part["text"].as_str().unwrap_or_default())
        .collect()
}

/// One `tools/call batch` on an open session, answered as `results`.
pub(crate) fn batch_results(session: &mut Session, id: i64, calls: Vec<Value>) -> Value {
    let answered = session.ask(json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": { "name": "batch", "arguments": { "calls": calls } }
    }));
    assert_eq!(
        answered["result"]["isError"],
        false,
        "the batch itself was refused: {}",
        tool_text(&answered["result"])
    );
    let parsed: Value =
        serde_json::from_str(&tool_text(&answered["result"])).expect("a batch answers with JSON");
    parsed["results"].clone()
}

/// One `kanban transact`, answered as its envelope.
///
/// Modelled on `batch_results`, and it hands back the whole envelope rather
/// than only `results` because for `transact` the envelope IS the answer:
/// `ok`, `failedIndex` and `rolledBack` are what a caller acts on. The exit
/// status is checked against the envelope's own verdict here, so no case has
/// to remember to.
pub(crate) fn transact_results(fixture: &Fixture, cwd: &Path, items: &[Value]) -> Value {
    let list = serde_json::to_string(items).unwrap();
    let output = fixture.run(cwd, &["transact", "--items", &list, "--json"]);
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "transact stdout is not JSON: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(
        output.status.success(),
        envelope["ok"] == json!(true),
        "the exit status and the envelope disagree: {envelope}"
    );
    assert!(
        envelope["batchId"]
            .as_str()
            .is_some_and(|id| id.len() == 36),
        "the envelope carries no batch id: {envelope}"
    );
    envelope
}

/// The board journal's report inside `audit verify --json`.
pub(crate) fn board_audit(fixture: &Fixture) -> Value {
    let report = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(report["healthy"], true, "the ledger is unhealthy: {report}");
    report["boards"][0]["audit"].clone()
}

pub(crate) fn deduped(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

/// One `attention …` command that must be refused, and the message it gave.
pub(crate) fn attention_refusal(fixture: &Fixture, args: &[&str]) -> String {
    let mut full = vec!["attention"];
    full.extend_from_slice(args);
    full.push("--json");
    refusal_object(&fixture.run(&fixture.main, &full))
}

/// Every attention row plus the board's audit head: what a refusal must leave
/// exactly as it found it. The chain head is a stronger statement than the
/// file's bytes, which a WAL checkpoint can move without any write.
pub(crate) fn attention_and_chain(fixture: &Fixture) -> (Value, Value) {
    (
        fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--all", "--limit", "500", "--json"],
        ),
        board_audit(fixture),
    )
}

/// The one refusal every `--limit` surface answers an over-ceiling value
/// with. Assembled rather than substring-matched so a surface that grows its
/// own private wording fails here instead of quietly diverging.
pub(crate) fn over_ceiling_refusal(value: &str) -> String {
    format!(
        "--limit must be between 0 and 1000000, got {value}; the ceiling exists so a mistyped \
         value is refused, not to imply a page this size is wise"
    )
}

/// Make `dir` a git repository with one commit, so provenance has something to
/// resolve. Returns false when git is unavailable, which is not a test failure.
/// Idempotent: a directory already carrying a commit is left untouched, because
/// a second `commit` over a clean tree would fail and be misread as no repo.
pub(crate) fn make_repo(dir: &Path) -> bool {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if run(&["rev-parse", "--show-toplevel"]) {
        return run(&["rev-parse", "HEAD"]);
    }
    if !run(&["init", "-q", "-b", "work"]) {
        return false;
    }
    fs::write(dir.join("seed.txt"), "seed").unwrap();
    run(&["add", "-A"]) && run(&["commit", "-qm", "seed"])
}

pub(crate) fn commit_all(dir: &Path, message: &str) -> bool {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    };
    run(&["add", "-A"]) && run(&["commit", "-qm", message])
}

/// A packaged release plus a HAX activation of it, so both install paths can
/// be driven: `install hax` runs `install_release_tree` in this process's
/// bash, `install hig` ships the embedded remote script through the ssh stub.
pub(crate) struct ReleaseGuardHarness {
    pub(crate) fixture: Fixture,
    pub(crate) script: PathBuf,
    pub(crate) path: String,
    pub(crate) hostname_bin: PathBuf,
    pub(crate) fake_repo_root: PathBuf,
    pub(crate) remote_root: PathBuf,
    pub(crate) package_dir: PathBuf,
    pub(crate) hax_install_root: PathBuf,
}

impl ReleaseGuardHarness {
    pub(crate) fn new(label: &str) -> Self {
        let fixture = Fixture::new(label);
        let fake_repo_root = fixture.root.join("fake-repo");
        fs::create_dir_all(&fake_repo_root).unwrap();
        let remote_root = fixture.root.join("remote-root");
        fs::create_dir_all(&remote_root).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
        let stubs = write_release_tool_stubs(
            &fixture,
            &fake_repo_root,
            "0123456789abcdef0123456789abcdef01234567",
            env!("CARGO_BIN_EXE_kanban"),
            "hax",
        );
        let hostname_bin = stubs.join("hostname");
        let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
        let package_dir = fixture.root.join("package");
        let hax_install_root = fixture.root.join("install-hax");
        let harness = Self {
            fixture,
            script,
            path,
            hostname_bin,
            fake_repo_root,
            remote_root,
            package_dir,
            hax_install_root,
        };
        let packaged = harness
            .command()
            .args([
                "package",
                "hax",
                "--output",
                harness.package_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            packaged.status.success(),
            "{}",
            String::from_utf8_lossy(&packaged.stderr)
        );
        let hax_bin_dir = harness.fixture.root.join("bin-hax");
        let hax_installed = harness.install("hax", &harness.hax_install_root, &hax_bin_dir);
        assert!(
            hax_installed.status.success(),
            "{}",
            String::from_utf8_lossy(&hax_installed.stderr)
        );
        harness
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new("bash");
        command
            .current_dir(&self.fixture.main)
            .env("PATH", &self.path)
            .env("HOSTNAME_BIN", &self.hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&self.hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &self.fake_repo_root)
            .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &self.remote_root)
            .env(
                "KANBAN_RELEASE_CONTAINER_RUNTIME",
                release_container_runtime(&self.hostname_bin),
            )
            .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
            .arg(&self.script);
        command
    }

    /// `target` is `hax` for the local install path or `hig` for the embedded
    /// remote script.
    pub(crate) fn install(&self, target: &str, install_root: &Path, bin_dir: &Path) -> Output {
        self.install_from(
            target,
            &self.package_dir,
            &self.hax_install_root,
            install_root,
            bin_dir,
        )
    }

    /// `install`, for a package other than the one the harness built: a test
    /// that needs two distinct builds of one commit, or a second release to
    /// install over the first, packages them itself.
    pub(crate) fn install_from(
        &self,
        target: &str,
        package_dir: &Path,
        hax_install_root: &Path,
        install_root: &Path,
        bin_dir: &Path,
    ) -> Output {
        self.install_command(target, package_dir, hax_install_root, install_root, bin_dir)
            .output()
            .unwrap()
    }

    /// The invocation itself, so a test that must prove the installer never
    /// blocks can spawn it against a deadline instead of waiting on it.
    pub(crate) fn install_command(
        &self,
        target: &str,
        package_dir: &Path,
        hax_install_root: &Path,
        install_root: &Path,
        bin_dir: &Path,
    ) -> Command {
        let mut command = self.command();
        command.args([
            "install",
            target,
            "--package",
            package_dir.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ]);
        if target == "hig" {
            command.args(["--hax-install-root", hax_install_root.to_str().unwrap()]);
        }
        command
    }

    /// The stub tree `install_matching_hax_package` needs, so a test that must
    /// activate its own package on HAX before installing it on HIG reuses this
    /// harness instead of building a second set of fakes.
    pub(crate) fn hax_context(&self) -> HaxInstallContext<'_> {
        HaxInstallContext {
            fixture: &self.fixture,
            script: &self.script,
            path: &self.path,
            hostname_bin: &self.hostname_bin,
            fake_repo_root: &self.fake_repo_root,
            remote_root: &self.remote_root,
        }
    }

    /// Runs an install that must be refused, and proves every watched tree is
    /// byte-identical afterwards: the refusal happened before any mutation.
    pub(crate) fn assert_refused_without_mutation(
        &self,
        target: &str,
        install_root: &Path,
        bin_dir: &Path,
        refusal: &str,
        watched: &[&Path],
    ) {
        self.assert_package_refused_without_mutation(
            target,
            &self.package_dir,
            &self.hax_install_root,
            install_root,
            bin_dir,
            refusal,
            watched,
        );
    }

    /// The same proof for a package the test forged itself.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn assert_package_refused_without_mutation(
        &self,
        target: &str,
        package_dir: &Path,
        hax_install_root: &Path,
        install_root: &Path,
        bin_dir: &Path,
        refusal: &str,
        watched: &[&Path],
    ) {
        let before: Vec<_> = watched.iter().map(|path| snapshot_tree(path)).collect();
        let refused =
            self.install_from(target, package_dir, hax_install_root, install_root, bin_dir);
        assert!(
            !refused.status.success(),
            "{target}: install succeeded through an unsafe view\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(refusal),
            "{target}: expected {refusal:?} in:\n{stderr}"
        );
        for (path, before) in watched.iter().zip(before) {
            assert_eq!(
                snapshot_tree(path),
                before,
                "{target}: refused install changed {}",
                path.display()
            );
        }
    }
}
