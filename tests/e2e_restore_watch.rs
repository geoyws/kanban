//! Compiled-binary E2E: snapshots and restore, data-root locking, import,
//! rescue, the schema surface and `watch`.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

fn ndjson_values(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn decode_watch_cursor(cursor: &str) -> Value {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn insert_raw_board_event(
    board_path: &Path,
    task_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: Value,
) -> i64 {
    let connection = Connection::open(board_path).unwrap();
    let seq = connection
        .query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();
    connection
        .execute(
            "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) \
             VALUES(?,?,?,?,?,?,0,?,?)",
            params![
                seq,
                task_id,
                kind,
                actor,
                payload.to_string(),
                seq,
                format!("prev-{seq}"),
                format!("hash-{seq}")
            ],
        )
        .unwrap();
    seq
}

/// Mirrors `POLL_INTERVAL` in `rust/watch.rs`: how long `watch --follow` waits
/// between keep-alive heartbeats while it has no matching batch. Only used to
/// size a deliberate delay, so a drift between the two costs test time rather
/// than correctness.
const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);

struct WatchSession {
    child: Option<std::process::Child>,
    stdout_rx: mpsc::Receiver<String>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl WatchSession {
    fn start(fixture: &Fixture, cwd: &Path, data_dir: &Path, args: &[&str]) -> Self {
        let mut child = fixture
            .command_with_data_dir(cwd, data_dir)
            .args(["watch"])
            .args(args)
            .stdin(Stdio::null())
            .env_remove("KANBAN_PROJECT")
            .spawn()
            .unwrap_or_else(|error| panic!("spawn kanban watch: {error}"));
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let stdout_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if stdout_tx.send(line).is_err() {
                    return;
                }
            }
        });
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_sink = Arc::clone(&stderr_lines);
        let stderr_thread = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
            {
                stderr_sink.lock().unwrap().push(line);
            }
        });
        Self {
            child: Some(child),
            stdout_rx,
            stdout_thread: Some(stdout_thread),
            stderr_lines,
            stderr_thread: Some(stderr_thread),
        }
    }

    fn next_stdout_line(&self, timeout: Duration) -> String {
        self.stdout_rx
            .recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("watch stdout stalled: {error}"))
    }

    fn next_stdout_json(&self, timeout: Duration) -> Value {
        serde_json::from_str(&self.next_stdout_line(timeout)).unwrap()
    }

    fn try_next_stdout_json(&self, timeout: Duration) -> Option<Value> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => Some(serde_json::from_str(&line).unwrap()),
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                None
            }
        }
    }

    /// Returns the next `event` envelope, discarding keep-alive heartbeats.
    ///
    /// `watch --follow` emits a heartbeat every poll interval while it has no
    /// matching batch, so any number of them can land between a mutation and
    /// the event that mutation produces. `timeout` stays a hard deadline for
    /// the event itself: heartbeats never extend it, so an event that never
    /// arrives fails instead of looping forever.
    fn next_stdout_event_json(&self, timeout: Duration) -> Value {
        self.next_stdout_event_json_with_drain_count(timeout).0
    }

    /// [`Self::next_stdout_event_json`], plus how many heartbeats it discarded.
    ///
    /// The count is what proves the drain actually ran, so a test that means to
    /// exercise the interleaved-heartbeat path can assert on it instead of
    /// silently degrading to the pass-through case.
    ///
    /// On timeout the panic reports what was seen rather than naming a cause:
    /// an event can be missing because stdout stalled, because the child died,
    /// or because the event was never delivered at all. The drained count and
    /// the last heartbeat's `state` and `cursor` are the evidence that tells
    /// those apart -- in particular a cursor that has moved past the awaited
    /// event distinguishes a lost event from a quiet stream.
    fn next_stdout_event_json_with_drain_count(&self, timeout: Duration) -> (Value, usize) {
        let deadline = Instant::now() + timeout;
        let mut drained = 0_usize;
        let mut last_heartbeat: Option<Value> = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(envelope) = self.try_next_stdout_json(remaining) else {
                let seen = match &last_heartbeat {
                    Some(heartbeat) => {
                        let cursor = heartbeat["cursor"].as_str().unwrap_or_default();
                        // Decode leniently: this runs on the failure path, so a
                        // malformed cursor must still be reported, not panic.
                        let seq = URL_SAFE_NO_PAD
                            .decode(cursor)
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                            .map(|decoded| decoded["seq"].to_string())
                            .unwrap_or_else(|| format!("<undecodable {cursor}>"));
                        format!(
                            "drained {drained} heartbeat(s), last was state {} at cursor seq {seq}",
                            heartbeat["payload"]["state"]
                        )
                    }
                    None => "no heartbeat arrived either".to_owned(),
                };
                panic!("watch delivered no event within {timeout:?}: {seen}");
            };
            match envelope["type"].as_str().unwrap() {
                "event" => return (envelope, drained),
                "heartbeat" => {
                    drained += 1;
                    last_heartbeat = Some(envelope);
                }
                other => panic!("unexpected watch envelope type {other}"),
            }
        }
    }

    fn stderr_snapshot(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.stdout_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }

    fn finish(mut self) -> Vec<String> {
        self.shutdown();
        self.stderr_snapshot()
    }
}

impl Drop for WatchSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[test]
fn compiled_binary_restores_a_rootless_board_snapshot_and_keeps_name_addressing() {
    let fixture = Fixture::new("restore-rootless");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "ROOTLESS", "--rootless", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "rootless work",
            "--id",
            "t-rootless",
            "--project",
            "ROOTLESS",
            "--json",
        ],
    );

    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    let attach = fixture.root.join("attach-rootless");
    fs::create_dir_all(&attach).unwrap();
    fixture.ok_json(
        &attach,
        &["workspace", "attach", "--to", "ROOTLESS", "--json"],
    );

    let attached = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let attached_board = attached
        .as_array()
        .unwrap()
        .iter()
        .find(|board| board["name"] == "ROOTLESS")
        .expect("the attached board must be listed");
    assert_eq!(
        attached_board["workspaceRoots"].as_array().unwrap().len(),
        1
    );

    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );

    let restored = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let restored_board = restored
        .as_array()
        .unwrap()
        .iter()
        .find(|board| board["name"] == "ROOTLESS")
        .expect("the restored board must be listed");
    assert!(
        restored_board["workspaceRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "t-rootless",
                "--project",
                "ROOTLESS",
                "--json"
            ]
        )["id"],
        "t-rootless"
    );
}

#[test]
fn compiled_binary_refuses_a_snapshot_changed_after_its_manifest() {
    let fixture = Fixture::new("manifest-tamper");
    fixture.ok_json(&fixture.main, &["init", "--name", "Manifest", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "live work", "--id", "t-live", "--json"],
    );
    let backup = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    let manifest = Path::new(backup["manifest"].as_str().unwrap());
    assert!(manifest.is_file());
    assert_eq!(backup["manifestSha256"].as_str().unwrap().len(), 64);
    let manifested: Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    assert!(
        manifested["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| file["audit"]["head"].as_str().unwrap().len() == 64)
    );

    let copied_board = backup["boards"][0].as_str().unwrap();
    Connection::open(copied_board)
        .unwrap()
        .execute("UPDATE tasks SET title='substituted' WHERE id='t-live'", [])
        .unwrap();
    let restore = fixture.run(
        &fixture.main,
        &[
            "restore",
            "--from",
            backup["directory"].as_str().unwrap(),
            "--force",
            "--json",
        ],
    );
    assert!(
        !restore.status.success(),
        "a substituted snapshot was restored"
    );
    assert!(
        String::from_utf8_lossy(&restore.stderr).contains("SHA-256 differs"),
        "restore did not name the failed manifest check: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-live", "--json"])["title"],
        "live work",
        "a refused restore changed live state"
    );
}

#[test]
fn compiled_binary_detects_rollback_past_a_retained_manifest_anchor() {
    let fixture = Fixture::new("manifest-rollback");
    fixture.ok_json(&fixture.main, &["init", "--name", "Rollback", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "first", "--id", "t-first", "--json"],
    );
    let old = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "anchored", "--id", "t-anchor", "--json"],
    );
    let anchor = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    let live_board =
        fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();

    fs::copy(old["boards"][0].as_str().unwrap(), &live_board).unwrap();
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true,
        "an intact older chain should be internally valid"
    );
    let anchored = fixture.run(
        &fixture.main,
        &[
            "audit",
            "verify",
            "--against",
            anchor["manifest"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !anchored.status.success(),
        "rollback passed the retained anchor"
    );
    let receipt: Value = serde_json::from_slice(&anchored.stdout).unwrap();
    assert!(
        receipt["boards"][0]["audit"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error.as_str().unwrap().contains("before anchored sequence")),
        "rollback receipt did not name the missing anchored history: {receipt}"
    );
}

#[test]
fn compiled_binary_prunes_only_the_backups_directory_it_manages() {
    let fixture = Fixture::new("prune");
    fixture.ok_json(&fixture.main, &["init", "--name", "Prune", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "work", "--id", "t-1", "--json"],
    );
    for _ in 0..3 {
        fixture.ok_json(&fixture.main, &["backup", "--json"]);
    }
    let kept = fixture.ok_json(&fixture.main, &["backup", "--keep", "2", "--json"]);
    assert_eq!(
        kept["pruned"].as_array().unwrap().len(),
        2,
        "4 snapshots, keep 2"
    );
    let remaining = fs::read_dir(fixture.data.join("backups")).unwrap().count();
    assert_eq!(remaining, 2);

    // Deleting from a directory the operator chose is the same overreach as
    // re-permissioning one, so --keep refuses outside the managed root.
    let mine = fixture.root.join("mine/snap");
    let refused = fixture.run(
        &fixture.main,
        &[
            "backup",
            "--output",
            mine.to_str().unwrap(),
            "--keep",
            "1",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("only prunes the managed"));
    assert!(
        !fixture
            .run(&fixture.main, &["backup", "--keep", "0", "--json"])
            .status
            .success()
    );
}

#[test]
fn compiled_binary_locks_the_data_root_against_a_concurrent_restore() {
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("lock");
    fixture.ok_json(&fixture.main, &["init", "--name", "Locked", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "work", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();
    let lock_path = fixture.data.join(".lock");
    // Created here rather than assumed: the test must fail on the behaviour it
    // asserts, not on the absence of a file that is an implementation detail.
    let hold = || {
        fs::File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap()
    };

    // A live board command holds the data root shared. `restore --force` used
    // to document "stop every kanban process first" and enforce nothing, so it
    // would rename database files out from under an open SQLite connection.
    {
        let held = hold();
        held.lock_shared().unwrap();
        let refused = fixture.run(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );
        assert!(
            !refused.status.success(),
            "restore replaced the data root while another process held it open"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("another kanban process"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    // Released, the identical restore succeeds — so the refusal above was the
    // lock, not something else about the snapshot.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );

    // Shared holders never exclude each other: the lock must not serialize the
    // agents it exists to protect.
    {
        let held = hold();
        held.lock_shared().unwrap();
        let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
        assert_eq!(listed[0]["id"], "t-1");
    }

    // While a restore holds it exclusively, a board command waits out its
    // window and then says so, rather than reading a half-replaced root.
    {
        let held = hold();
        held.lock().unwrap();
        let refused = fixture.run(&fixture.main, &["task", "list", "--json"]);
        assert!(
            !refused.status.success(),
            "a board command read the data root mid-restore"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("restore is replacing"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }
}

#[test]
fn compiled_binary_locks_only_the_data_root_it_was_asked_to_touch() {
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("lock-scope");
    let outside = fixture.root.join("outside.db");

    // A board named straight by path, living elsewhere, is not data-root
    // state. Locking it anyway would create a private data root as a side
    // effect of a command that never wanted one — the same overreach as
    // re-permissioning a directory we do not own.
    fixture.ok_json(
        &fixture.main,
        &[
            "--db",
            outside.to_str().unwrap(),
            "task",
            "add",
            "standalone",
            "--json",
        ],
    );
    assert!(
        !fixture.data.exists(),
        "an external --db board created a data root it never needed"
    );

    // A board that does live under the data root is covered, even when the
    // path spells the root through a traversal.
    fixture.ok_json(&fixture.main, &["init", "--name", "Scoped", "--json"]);
    let inside = fixture.data.join("boards/../boards/inside.db");
    let held = fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(fixture.data.join(".lock"))
        .unwrap();
    held.lock().unwrap();
    let refused = fixture.run(
        &fixture.main,
        &["--db", inside.to_str().unwrap(), "task", "list", "--json"],
    );
    assert!(
        !refused.status.success(),
        "a board inside the data root escaped the lock through .."
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("restore is replacing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn compiled_binary_bounds_priority_without_rewriting_history() {
    let fixture = Fixture::new("priority");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Priority", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();

    // The band is the one the ledger already uses: 0 is the routing tier
    // driver-only work sorts on, 9 the least urgent.
    for good in ["0", "3", "9"] {
        let task = fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "in band",
                "--id",
                &format!("t-{good}"),
                "--priority",
                good,
                "--json",
            ],
        );
        let expected = match good {
            "0" => "P0",
            "3" => "P1",
            _ => "P2",
        };
        assert_eq!(task["priorityLevel"], expected);
    }

    let routine = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "routine default",
            "--id",
            "t-default",
            "--json",
        ],
    );
    assert_eq!(routine["priority"], 6);
    assert_eq!(routine["priorityLevel"], "P2");

    for (symbol, anchor) in [("P0", 0), ("p1", 3), ("P2", 6)] {
        let task = fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "symbolic",
                "--id",
                &format!("t-{}", symbol.to_lowercase()),
                "--priority",
                symbol,
                "--json",
            ],
        );
        assert_eq!(task["priority"], anchor);
    }

    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "interrupt",
            "--as",
            "agent",
            "--priority",
            "P0",
            "--json",
        ],
    );
    assert_eq!(attention["priority"], 0);
    assert_eq!(attention["priorityLevel"], "P0");

    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "agent",
            "--to",
            "@:team/project/driver-2",
            "--summary",
            "resume this",
            "--intent",
            "finish it",
            "--next-action",
            "claim work",
            "--priority",
            "P1",
            "--json",
        ],
    );
    assert_eq!(handoff["priority"], 3);
    assert_eq!(handoff["priorityLevel"], "P1");

    // `claim --next` hands work out in ascending priority, so an unbounded
    // field let a negative value hold the head of every queue permanently:
    // nothing can outrank the bottom of an i64.
    for bad in ["-1", "10", "-9223372036854775808", "9223372036854775807"] {
        let refused = fixture.run(
            &fixture.main,
            &["task", "add", "out of band", "--priority", bad, "--json"],
        );
        assert!(
            !refused.status.success(),
            "task add --priority {bad} was accepted"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr).contains("most urgent"),
            "stderr: {}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    // A value that is not a number at all names the flag it came from:
    // "invalid digit found in string" does not say which of --priority,
    // --stale-minutes or --lease-minutes was wrong, and an agent reading
    // stderr has nothing to act on.
    for (flag, value) in [("--priority", "abc"), ("--stale-minutes", "soon")] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "task", "update", "t-3", "--as", "geoyws", flag, value, "--json",
            ],
        );
        assert!(!refused.status.success(), "{flag} {value} was accepted");
        let message = String::from_utf8_lossy(&refused.stderr);
        assert!(message.contains(flag), "stderr must name {flag}: {message}");
        assert!(
            message.contains(value),
            "stderr must quote the value: {message}"
        );
    }

    // The same band applies on update, and a refused update changes nothing.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-3",
            "--as",
            "geoyws",
            "--priority",
            "-1",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priority"],
        3,
        "a refused update must leave the recorded priority alone"
    );

    // A row that already holds an out-of-band priority — an atmux import, or a
    // board written before this rule — keeps it. Validating what a caller
    // types is not a licence to rewrite recorded history to match.
    let database = Connection::open(&board).unwrap();
    database
        .execute("UPDATE tasks SET priority=99 WHERE id='t-3'", [])
        .unwrap();
    drop(database);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priority"],
        99,
        "an existing out-of-band priority must still be readable"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-3", "--json"])["priorityLevel"],
        Value::Null
    );
    let updated = fixture.ok_json(
        &fixture.main,
        &[
            "task", "update", "t-3", "--as", "geoyws", "--title", "renamed", "--json",
        ],
    );
    assert_eq!(updated["title"], "renamed");
    assert_eq!(
        updated["priority"], 99,
        "an update that never mentioned priority silently normalized it"
    );
}

#[test]
fn compiled_binary_waits_out_a_long_write_lock_instead_of_dropping_the_write() {
    let _db_lock_contention_test_guard = db_lock_contention_test_guard();
    let fixture = Fixture::new("busy");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Busy", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();

    // Hold the write lock past the ceiling the binary used to give up at. A
    // swarm write that loses the race has to queue, not fail: an agent reads
    // an exit status and moves on, so a dropped write is lost work that
    // nothing downstream will notice is missing.
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let connection = Connection::open(&board).unwrap();
        connection
            .busy_handler(Some(|_| {
                std::thread::sleep(Duration::from_millis(50));
                true
            }))
            .unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        started_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(7_500));
        connection.execute_batch("COMMIT").unwrap();
    });
    started_rx.recv().unwrap();

    let started = Instant::now();
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "queued behind a long writer",
            "--id",
            "t-queued",
            "--json",
        ],
    );
    let waited = started.elapsed();
    holder.join().unwrap();

    assert!(
        waited >= Duration::from_secs(5),
        "the write never queued behind the lock, so this proves nothing ({waited:?})"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-queued", "--json"])["title"],
        "queued behind a long writer"
    );
}

#[test]
fn compiled_binary_previews_an_import_and_will_not_void_a_live_lease_quietly() {
    let fixture = Fixture::new("import-safety");
    fixture.ok_json(&fixture.main, &["init", "--name", "Reconcile", "--json"]);
    let export = fixture.root.join("export.json");
    let write_export = |id: &str, title: &str| {
        fs::write(
            &export,
            serde_json::to_vec(&json!({
                "epics": [],
                "stories": [],
                "tasks": [{"id":id,"subject":title,"status":"todo"}]
            }))
            .unwrap(),
        )
        .unwrap();
    };
    let import = |extra: &[&str]| {
        let mut args = vec![
            "import",
            "atmux-json",
            export.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ];
        args.extend_from_slice(extra);
        fixture.run(&fixture.main, &args)
    };
    let title = || {
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let seizures = || {
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--kind", "lease_seized", "--json"],
            )
            .as_array()
            .unwrap()
            .len()
    };

    write_export("t-1", "original");
    assert!(import(&[]).status.success());
    assert_eq!(title(), "original");

    // A dry run reports what it would create and leaves the board alone.
    write_export("t-2", "previewed creation");
    let preview: Value =
        serde_json::from_slice(&import(&["--dry-run"]).stdout).expect("dry run must still report");
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["created"], 1);
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-2", "--json"])
            .status
            .success(),
        "a dry run wrote to the board"
    );

    write_export("t-1", "previewed");

    // Claimed by a live agent, `--reconcile` used to delete the claim row on
    // its way past — the same silent lease void that task move/remove refuse.
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let refused = import(&["--reconcile"]);
    assert!(
        !refused.status.success(),
        "reconcile voided a live lease without being asked twice"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(message.contains("live lease"), "stderr: {message}");
    assert!(
        message.contains("held by worker"),
        "the refusal must name the holder: {message}"
    );
    assert_eq!(title(), "original", "a refused import wrote anyway");

    // Forced, but previewed: it says which leases it would seize and still
    // takes none of them.
    let forecast: Value =
        serde_json::from_slice(&import(&["--reconcile", "--force", "--dry-run"]).stdout)
            .expect("forced dry run must report");
    assert_eq!(forecast["seizedLeases"], json!(["t-1"]));
    assert_eq!(title(), "original");
    assert_eq!(seizures(), 0, "a dry run recorded a seizure it never made");

    // Forced for real: the overwrite lands and the seizure is on the record.
    let applied: Value =
        serde_json::from_slice(&import(&["--reconcile", "--force"]).stdout).unwrap();
    assert_eq!(applied["dryRun"], false);
    assert_eq!(applied["seizedLeases"], json!(["t-1"]));
    assert_eq!(title(), "previewed");
    assert_eq!(seizures(), 1, "a forced seizure left no audit trail");
}

#[test]
fn compiled_binary_reports_a_missing_board_instead_of_replacing_it() {
    let fixture = Fixture::new("missing-board");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Gone", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real work", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // A partial restore, a stray rm, a half-copied data root.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", board.display()));
    }

    // Opening a board creates it, so `doctor` used to recreate the very file
    // it was asked to inspect and then certify the empty result healthy — the
    // health check destroying the evidence that anything was wrong.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor called a board with no file healthy"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["projects"][0]["present"], false);
    assert!(!board.is_file(), "doctor recreated the board it inspected");

    // A command that does work on that board refuses, and names both ways out.
    let refused = fixture.run(&fixture.main, &["task", "list", "--json"]);
    assert!(
        !refused.status.success(),
        "a work command silently stood an empty board up in place of the lost one"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("registered but missing"),
        "stderr: {message}"
    );
    assert!(message.contains("kanban restore"), "stderr: {message}");
    assert!(
        !board.is_file(),
        "a refused command still created the board"
    );

    // A survey command snapshots what remains and says what it could not take.
    let partial = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(partial["boards"].as_array().unwrap().len(), 0);
    assert_eq!(
        partial["missingBoards"][0],
        board.to_string_lossy().as_ref()
    );

    // And the documented recovery actually recovers.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
        "real work"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
        true
    );
}

/// Take a board's permissions away, and report whether that actually denied
/// this process.
///
/// `chmod 000` only stops a process the mode bits apply to. Root bypasses them
/// entirely, so under a privileged runner the unreadable case cannot be staged
/// at all. Skipping there would report green while measuring nothing, so the
/// callers branch instead and assert the true statement for the situation they
/// are really in: if this harness can still read the file, so can the binary,
/// and the board must come back readable.
fn deny_board_reads(board: &Path) -> bool {
    fs::set_permissions(board, fs::Permissions::from_mode(0o000)).unwrap();
    fs::read(board).is_err()
}

fn count_pre_restore_snapshots(data: &Path) -> usize {
    let backups = data.join("backups");
    if !backups.is_dir() {
        return 0;
    }
    fs::read_dir(backups)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("pre-restore-")
        })
        .count()
}

/// Every survey tells a board it could not open from one that is gone.
///
/// Before this, both answered `present: false` and nothing else — byte for
/// byte the same receipt for intact data behind a permission bit and for data
/// that had been deleted. The move an operator makes on `missing` is to restore
/// a snapshot over the path, so the receipt was the instruction to destroy the
/// board it was describing. Boards are created `0600`, which is all it takes:
/// one written by root, or living on a shared path, reads as gone.
#[test]
fn compiled_binary_tells_an_unreadable_board_from_a_missing_one() {
    let fixture = Fixture::new("unreadable-board");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Locked", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "real work", "--id", "t-1", "--json"],
    );

    // A healthy board first, so the new field is measured against all three
    // answers and not just the one this is about.
    let healthy = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(healthy["healthy"], true);
    assert_eq!(healthy["projects"][0]["present"], true);
    assert_eq!(healthy["projects"][0]["boardState"], "readable");

    if !deny_board_reads(&board) {
        // Privileged runner: the file stayed readable, so the binary reads it
        // too and the only honest assertion is that nothing changed.
        let still_fine = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
        assert_eq!(still_fine["projects"][0]["boardState"], "readable");
        fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    // doctor: unhealthy, because nothing about this board was checked at all —
    // and unreadable rather than absent, with the reason that stopped it.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor certified a board it never opened"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["projects"][0]["boardState"], "unreadable");
    assert_eq!(
        report["projects"][0]["unreadableReason"],
        "Permission denied (os error 13)"
    );
    assert!(board.is_file(), "doctor removed the board it inspected");

    // dashboard: never `boardMissing`, which is the flag a reader acts on.
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(dashboard[0]["boardState"], "unreadable");
    assert_eq!(
        dashboard[0]["boardUnreadableReason"],
        "Permission denied (os error 13)"
    );
    assert!(
        dashboard[0].get("boardMissing").is_none(),
        "dashboard called a board that is right there missing: {}",
        dashboard[0]
    );

    // backup: still snapshots what it can, and names what it left out as
    // unreadable rather than filing it under boards that no longer exist.
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(snapshot["boards"].as_array().unwrap().len(), 0);
    assert_eq!(snapshot["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(snapshot["unreadableBoards"][0]["name"], "Locked");
    assert_eq!(
        snapshot["unreadableBoards"][0]["boardPath"],
        board.to_string_lossy().as_ref()
    );
    assert_eq!(
        snapshot["unreadableBoards"][0]["reason"],
        "Permission denied (os error 13)"
    );
    // The manifest carries it too, so a snapshot's own record says it is
    // incomplete rather than looking whole.
    let manifest: Value =
        serde_json::from_slice(&fs::read(snapshot["manifest"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(manifest["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(manifest["unreadableBoards"][0]["name"], "Locked");

    // audit verify: not healthy, because an unopened ledger has had nothing
    // verified about it and the whole receipt is a claim about ledgers checked.
    let audited = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    assert!(!audited.status.success());
    let audit: Value = serde_json::from_slice(&audited.stdout).unwrap();
    assert_eq!(audit["healthy"], false);
    assert_eq!(audit["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(audit["unreadableBoards"][0]["name"], "Locked");

    // search --all-boards
    let searched = fixture.ok_json(&fixture.main, &["search", "real", "--all-boards", "--json"]);
    assert_eq!(searched["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(searched["unreadableBoards"][0]["name"], "Locked");

    // search-rebuild --all-boards
    let rebuilt = fixture.ok_json(
        &fixture.main,
        &[
            "search-rebuild",
            "--all-boards",
            "--as",
            "codex@cli",
            "--json",
        ],
    );
    assert_eq!(rebuilt["missingBoards"].as_array().unwrap().len(), 0);
    assert_eq!(rebuilt["unreadableBoards"][0]["name"], "Locked");

    // The same board deleted still reports missing, and reports nothing under
    // unreadable — the two answers stay apart in both directions.
    fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", board.display()));
    }

    let gone = fixture.run(&fixture.main, &["doctor", "--json"]);
    let gone_report: Value = serde_json::from_slice(&gone.stdout).unwrap();
    assert_eq!(gone_report["projects"][0]["boardState"], "missing");
    assert_eq!(gone_report["projects"][0]["present"], false);
    assert!(
        gone_report["projects"][0].get("unreadableReason").is_none(),
        "a deleted board carried a read failure"
    );

    let gone_dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    assert_eq!(gone_dashboard[0]["boardState"], "missing");
    assert_eq!(gone_dashboard[0]["boardMissing"], true);

    let gone_snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"]);
    assert_eq!(
        gone_snapshot["missingBoards"][0],
        board.to_string_lossy().as_ref()
    );
    assert_eq!(
        gone_snapshot["unreadableBoards"].as_array().unwrap().len(),
        0
    );

    let gone_audit = fixture.run(&fixture.main, &["audit", "verify", "--json"]);
    let gone_audit_report: Value = serde_json::from_slice(&gone_audit.stdout).unwrap();
    assert_eq!(gone_audit_report["missingBoards"][0], "Locked");
    assert_eq!(
        gone_audit_report["unreadableBoards"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let gone_search = fixture.ok_json(&fixture.main, &["search", "real", "--all-boards", "--json"]);
    assert_eq!(gone_search["missingBoards"][0], "Locked");
    assert_eq!(gone_search["unreadableBoards"].as_array().unwrap().len(), 0);

    let gone_rebuild = fixture.ok_json(
        &fixture.main,
        &[
            "search-rebuild",
            "--all-boards",
            "--as",
            "codex@cli",
            "--json",
        ],
    );
    assert_eq!(gone_rebuild["missingBoards"][0], "Locked");
    assert_eq!(
        gone_rebuild["unreadableBoards"].as_array().unwrap().len(),
        0
    );
}

/// `restore` stops rather than replacing a board it could not copy first.
///
/// Measured before the refusal existed: a live board at mode 000, holding a
/// task added after the snapshot was taken, was skipped by the pre-restore
/// rescue copy as "missing" and then replaced anyway — `replace_database`
/// renames over the path, which needs the directory's permissions and not the
/// file's. The command exited 0, the rescue snapshot had no `boards` directory
/// in it at all, and the post-snapshot task was gone with nothing to recover it
/// from. The rescue copy is the only thing that makes `--force` reversible.
#[test]
fn compiled_binary_refuses_to_restore_over_a_board_it_cannot_rescue() {
    let fixture = Fixture::new("unreadable-restore");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Rescue", "--json"]);
    let board = PathBuf::from(project["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "in the snapshot", "--id", "t-1", "--json"],
    );
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Work committed after the snapshot: this is what the rescue copy exists to
    // preserve, and what the measured bug destroyed.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "added after the backup",
            "--id",
            "t-2",
            "--json",
        ],
    );

    if !deny_board_reads(&board) {
        // Privileged runner: the board is readable, so it is rescued the
        // ordinary way and the restore goes through.
        let done = fixture.ok_json(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );
        let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
        assert!(
            rescue.join("boards").is_dir(),
            "the rescue snapshot kept no copy of the live board"
        );
        fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
        return;
    }

    let refused = fixture.run(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    assert!(
        !refused.status.success(),
        "restore replaced a board it could not copy first"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("cannot copy into the rescue snapshot first"),
        "stderr: {message}"
    );
    assert!(
        message.contains("Permission denied (os error 13)"),
        "the refusal did not say what stopped the read: {message}"
    );
    assert!(
        message.contains("very likely intact"),
        "the refusal read as data loss rather than a permission problem: {message}"
    );
    assert_eq!(
        count_pre_restore_snapshots(&fixture.data),
        0,
        "a refused restore left a half-built rescue snapshot behind"
    );

    // The board was not touched, and the work added after the snapshot is
    // still there once the permission bit is back.
    fs::set_permissions(&board, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-2", "--json"])["title"],
        "added after the backup"
    );

    // And with the board readable the restore runs, rescuing it on the way
    // through — so the refusal gated on the rescue copy, nothing else.
    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    assert!(
        rescue.join("boards").is_dir(),
        "the rescue snapshot kept no copy of the live board"
    );
    let rescue_manifest: Value =
        serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        rescue_manifest["unreadableBoards"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        rescue_manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|file| file["kind"] == "board")
            .count(),
        1,
        "the rescue manifest recorded no board copy"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
        "in the snapshot"
    );
}

/// `restore` rescues what it will overwrite, not what the registry happens to
/// list.
///
/// The destruction is keyed to the filesystem: the replacement loop renames a
/// snapshot file over `<root>/boards/<file name>` for every board in the
/// snapshot, registered or not. Restoring an older snapshot drops a project
/// from the registry while leaving its file on disk, so restoring a newer one
/// then renames over a file that nothing classifies. Measured before this was
/// keyed correctly: the work committed after that snapshot was destroyed with
/// no rescue copy, and the unreadable-board refusal could not fire either,
/// because it was keyed to the registry too.
#[test]
fn compiled_binary_rescues_a_board_file_the_registry_no_longer_lists() {
    let fixture = Fixture::new("unregistered-overwrite");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let first = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    let bee = fixture.ok_json(&fixture.worktree, &["init", "--name", "Bee", "--json"]);
    let bee_board = PathBuf::from(bee["boardPath"].as_str().unwrap());
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "in the second snapshot",
            "--id",
            "t-b1",
            "--json",
        ],
    );
    let second = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Committed after the second snapshot: exactly what a rescue copy is for.
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "after the second snapshot",
            "--id",
            "t-b2",
            "--json",
        ],
    );

    // Restoring the older snapshot drops Bee from the registry and leaves its
    // file where it is — the state that hides the next overwrite from anything
    // classifying by registry membership.
    fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &first, "--force", "--json"],
    );
    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Bee"),
        "the older snapshot did not drop Bee, so this never reaches the case: {listed}"
    );
    assert!(
        bee_board.is_file(),
        "restoring an older snapshot deleted a board it never mentioned"
    );

    // Restoring the newer snapshot renames over that unregistered file, so it
    // has to reach the rescue snapshot first.
    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &second, "--force", "--json"],
    );
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    let manifest: Value =
        serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
    let rescued = manifest["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|file| file["kind"] == "board")
        .collect::<Vec<_>>();
    assert_eq!(
        rescued.len(),
        2,
        "a board about to be overwritten was left out of the rescue snapshot: {manifest}"
    );
    let unregistered = rescued
        .iter()
        .find(|file| file["project"] == Value::Null)
        .unwrap_or_else(|| {
            panic!("the file the registry no longer lists was not rescued: {manifest}")
        });
    assert_eq!(
        unregistered["path"],
        format!(
            "boards/{}",
            bee_board.file_name().unwrap().to_string_lossy()
        ),
        "the unnamed rescue copy is not the file that was overwritten"
    );

    // The rescue copy holds the work the overwrite destroyed. This is the whole
    // guarantee: the live file is gone, and it is recoverable.
    let copy = rescue.join(unregistered["path"].as_str().unwrap());
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "t-b2",
                "--db",
                copy.to_str().unwrap(),
                "--json"
            ],
        )["title"],
        "after the second snapshot"
    );
    // And the live file really was replaced, as restore promises.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "show",
                "t-b1",
                "--db",
                bee_board.to_str().unwrap(),
                "--json",
            ],
        )["title"],
        "in the second snapshot"
    );
}

/// A file that is not a board is copied out of the way, not silently replaced.
///
/// `BoardFile::Foreign`'s contract is "Never opened, never migrated, never
/// overwritten", and it exists because `task list --db notes.txt` once left
/// 372736 bytes of SQLite where an operator's file had been. `restore` renames
/// over `<root>/boards/<file name>` for every board in the snapshot, so it can
/// destroy such a file just as completely.
///
/// Refusing was the wrong way to keep that promise: a board whose header is
/// damaged classifies as foreign too, so refusing would block recovery of
/// exactly the disaster `restore` exists for. Copying keeps the file
/// recoverable, which is what the promise protects, and the receipt names it so
/// the replacement is never silent.
#[test]
fn compiled_binary_copies_a_foreign_file_out_of_the_way_before_replacing_it() {
    let fixture = Fixture::new("foreign-overwrite");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let bee = fixture.ok_json(&fixture.worktree, &["init", "--name", "Bee", "--json"]);
    let bee_board = PathBuf::from(bee["boardPath"].as_str().unwrap());
    let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
        .as_str()
        .unwrap()
        .to_owned();

    // Something that is not a board, sitting exactly where the restore writes.
    for suffix in ["-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{suffix}", bee_board.display()));
    }
    fs::write(&bee_board, "operator notes, not a database\n").unwrap();

    let done = fixture.ok_json(
        &fixture.main,
        &["restore", "--from", &snapshot, "--force", "--json"],
    );
    let unparsed = done["rescuedUnparsed"].as_array().unwrap();
    assert_eq!(
        unparsed.len(),
        1,
        "a file that was never a board was replaced with no copy: {done}"
    );
    assert_eq!(
        unparsed[0]["originalPath"],
        bee_board.to_string_lossy().as_ref()
    );
    assert_eq!(unparsed[0]["reason"], "not a Kanban board");

    // The bytes survive verbatim in the rescue snapshot.
    let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(rescue.join(unparsed[0]["path"].as_str().unwrap())).unwrap(),
        "operator notes, not a database\n",
        "the operator's file was destroyed rather than copied"
    );

    // And the restore did its job: the path is a board again.
    assert_eq!(
        fixture.ok_json(&fixture.worktree, &["doctor", "--json"])["healthy"],
        true
    );
}

/// Corrupt the pages of a board while leaving the file readable.
///
/// `offset` selects how deep the damage goes, which decides which layer
/// notices: the 16-byte header is checked by the classifier, the schema lives
/// in the first page, and anything past that is invisible until every page is
/// read.
fn corrupt_board_bytes(board: &Path, offset: usize, fill: u8) {
    let mut bytes = fs::read(board).unwrap();
    let end = bytes.len().min(65536);
    assert!(
        end > offset,
        "board too small to corrupt at {offset}: {} bytes",
        bytes.len()
    );
    for byte in &mut bytes[offset..end] {
        *byte = fill;
    }
    fs::write(board, &bytes).unwrap();
}

/// A corrupt board is what `restore` is for, so it must not block on one.
///
/// Measured before this predicate was corrected: `restore` refused with
/// `database disk image is malformed` and told the operator to check
/// permissions that were fine — the one command that recovers from disk
/// corruption, refusing because of disk corruption. The rescue copy does not
/// need SQLite to parse a file, only to read it, so the question is whether the
/// bytes can be read, and every readable file is copied out of the way.
///
/// Three depths, because three different layers notice: a damaged header stops
/// the classifier, a damaged first page stops the online backup, and damage
/// past the first page is invisible until the copy's audit chain is read back.
#[test]
fn compiled_binary_restores_over_a_corrupt_board_and_keeps_a_copy_of_it() {
    for (label, offset, fill) in [
        ("damaged-header", 0usize, b'!'),
        ("schema-page", 100, 0x5A),
        ("past-the-first-page", 4096, 0xAA),
    ] {
        let fixture = Fixture::new(&format!("corrupt-restore-{label}"));
        let project = fixture.ok_json(&fixture.main, &["init", "--name", "Ord", "--json"]);
        let board = PathBuf::from(project["boardPath"].as_str().unwrap());
        fixture.ok_json(
            &fixture.main,
            &["task", "add", "good work", "--id", "t-1", "--json"],
        );
        let snapshot = fixture.ok_json(&fixture.main, &["backup", "--json"])["directory"]
            .as_str()
            .unwrap()
            .to_owned();

        corrupt_board_bytes(&board, offset, fill);
        let damaged = fs::read(&board).unwrap();
        assert!(
            fs::read(&board).is_ok(),
            "{label}: the harness cannot read the file, so this measures the wrong thing"
        );

        // The recovery must run, not refuse.
        let done = fixture.ok_json(
            &fixture.main,
            &["restore", "--from", &snapshot, "--force", "--json"],
        );

        // The corrupt file was copied out of the way before being replaced,
        // byte for byte, and the receipt says so rather than staying silent.
        let unparsed = done["rescuedUnparsed"].as_array().unwrap();
        assert_eq!(
            unparsed.len(),
            1,
            "{label}: the corrupt board was replaced without a copy: {done}"
        );
        assert_eq!(
            unparsed[0]["originalPath"],
            board.to_string_lossy().as_ref(),
            "{label}"
        );
        assert!(
            !unparsed[0]["reason"].as_str().unwrap().is_empty(),
            "{label}: no reason recorded"
        );

        let rescue = PathBuf::from(done["rescueSnapshot"].as_str().unwrap());
        let copy = rescue.join(unparsed[0]["path"].as_str().unwrap());
        assert_eq!(
            fs::read(&copy).unwrap(),
            damaged,
            "{label}: the rescue copy is not the file that was replaced"
        );

        // The rescue manifest records it, and the rescue snapshot as a whole
        // still verifies — an unparsed copy must not make it unrestorable.
        let manifest: Value =
            serde_json::from_slice(&fs::read(rescue.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(
            manifest["unparsedFiles"].as_array().unwrap().len(),
            1,
            "{label}"
        );
        fixture.ok_json(
            &fixture.main,
            &[
                "audit",
                "verify",
                "--against",
                rescue.join("manifest.json").to_str().unwrap(),
                "--json",
            ],
        );

        // And the restore actually recovered the board.
        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["title"],
            "good work",
            "{label}"
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
            true,
            "{label}"
        );
    }
}

#[test]
fn compiled_binary_doctor_looks_past_the_btree() {
    let fixture = Fixture::new("doctor-depth");
    let project = fixture.ok_json(&fixture.main, &["init", "--name", "Deep", "--json"]);
    let board = project["boardPath"].as_str().unwrap().to_owned();
    for id in ["t-ok", "t-future"] {
        fixture.ok_json(&fixture.main, &["task", "add", id, "--id", id, "--json"]);
    }
    assert_eq!(
        fixture.ok_json(&fixture.main, &["doctor", "--json"])["healthy"],
        true
    );

    // `integrity_check` validates the b-tree and says nothing about what the
    // rows mean, so both of these leave a structurally perfect board.
    let database = Connection::open(&board).unwrap();
    database
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO task_notes(task_id,author,kind,body,created_at)
               VALUES('t-vanished','ghost','progress','orphan',1);",
        )
        .unwrap();
    let horizon = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 86_400_000;
    database
        .execute(
            "UPDATE tasks SET created_at=? WHERE id='t-future'",
            [horizon],
        )
        .unwrap();
    drop(database);

    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(!checked.status.success());
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let board_report = &report["projects"][0];
    assert_eq!(
        board_report["integrity"],
        json!(["ok"]),
        "the b-tree really is intact; that is the point"
    );
    assert_eq!(report["healthy"], false);
    assert!(
        board_report["orphanedRows"][0]
            .as_str()
            .unwrap()
            .contains("task_notes"),
        "a note on a task that does not exist went unreported: {board_report}"
    );
    // A task stamped in the future sorts ahead of real work, and on a claim it
    // holds a lease no sweep will ever retire.
    assert_eq!(board_report["futureDatedTasks"], json!(["t-future"]));
}

#[test]
fn compiled_binary_refuses_arguments_it_would_have_dropped() {
    let fixture = Fixture::new("positionals");
    fixture.ok_json(&fixture.main, &["init", "--name", "Args", "--json"]);

    // Forgetting to quote is the likeliest slip at a shell, and it used to
    // produce a durable record that was wrong with nothing to notice it by:
    // this recorded the title `Fix` and reported success.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Fix", "the", "broken", "parser", "--id", "t-1", "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unquoted title was accepted and silently cut to its first word"
    );
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(
        message.contains("unexpected arguments"),
        "stderr: {message}"
    );
    assert!(
        message.contains("after `task add Fix`"),
        "the error must show what it thought the command was: {message}"
    );
    assert!(
        !fixture
            .run(&fixture.main, &["task", "show", "t-1", "--json"])
            .status
            .success(),
        "a refused add wrote a task anyway"
    );

    // Quoted, the whole title lands.
    let added = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Fix the broken parser",
            "--id",
            "t-1",
            "--json",
        ],
    );
    assert_eq!(added["title"], "Fix the broken parser");

    // The same slip on a note body recorded `the`.
    let refused = fixture.run(
        &fixture.main,
        &[
            "note", "t-1", "the", "build", "is", "red", "--as", "ci", "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unquoted note body was accepted"
    );
    fixture.ok_json(
        &fixture.main,
        &["note", "t-1", "the build is red", "--as", "ci", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["notes"][0]["body"],
        "the build is red"
    );

    // An extra id is refused too — it was never going to be read.
    let refused = fixture.run(&fixture.main, &["task", "show", "t-1", "t-2", "--json"]);
    assert!(!refused.status.success(), "a second task id was ignored");

    // And every arity the surface actually uses still parses: no positional,
    // one, and the two `task move` takes.
    fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-1", "todo", "--as", "geoyws", "--json"],
    );
}

#[test]
fn compiled_binary_refuses_two_requests_dressed_as_one() {
    let fixture = Fixture::new("ambiguous");
    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "geoyws/scheduler",
            "--description",
            "queue selection",
            "--as",
            "test",
            "--json",
        ],
    );
    let other = fixture.root.join("other");
    fs::create_dir_all(&other).unwrap();
    fixture.ok_json(&other, &["init", "--name", "Beta", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "head of the queue",
            "--id",
            "t-first",
            "--priority",
            "1",
            "--tag",
            "geoyws/scheduler",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "the one asked for",
            "--id",
            "t-named",
            "--priority",
            "9",
            "--json",
        ],
    );

    // `claim t-named --next` used to drop the id and hand back t-first, so an
    // agent that asked for a named task held a lease on a different one.
    let refused = fixture.run(
        &fixture.main,
        &["claim", "t-named", "--next", "--as", "worker", "--json"],
    );
    assert!(
        !refused.status.success(),
        "claim ignored the task id it was given and picked a different task"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("not both"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // Either request alone still means what it says.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-named", "--as", "worker", "--json"]
        )["taskID"],
        "t-named"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "other", "--json"]
        )["taskID"],
        "t-first"
    );

    // A repeated single-valued flag kept the last occurrence, so a wrapper
    // appending a default --project silently retargeted the board.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "whose board?",
            "--id",
            "t-stray",
            "--project",
            "Alpha",
            "--project",
            "Beta",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a repeated --project picked one board without saying which"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--project (Alpha, Beta)"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    for project in ["Alpha", "Beta"] {
        assert!(
            !fixture
                .run(
                    &fixture.main,
                    &["task", "show", "t-stray", "--project", project, "--json"]
                )
                .status
                .success(),
            "the refused task landed on {project} anyway"
        );
    }

    // List-valued flags are exactly what repeating is for, and still repeat.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "with deps",
            "--id",
            "t-deps",
            "--depends-on",
            "t-first",
            "--depends-on",
            "t-named",
            "--json",
        ],
    );
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--with-relations", "--json"],
    );
    let deps = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-deps")
        .unwrap();
    assert_eq!(deps["dependencies"], json!(["t-first", "t-named"]));
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-deps", "--json"]);
    let dependency = shown["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-first")
        .unwrap();
    assert_eq!(dependency["tags"], json!(["geoyws/scheduler"]));
}

#[test]
fn compiled_binary_never_shortens_context_without_saying_so() {
    let fixture = Fixture::new("context-budget");
    fixture.ok_json(&fixture.main, &["init", "--name", "Budget", "--json"]);
    let long = "x".repeat(600);
    let title = "T".repeat(300);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", &title, "--id", "t-1", "--json"],
    );
    let lease =
        fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"])["leaseToken"]
            .as_str()
            .unwrap()
            .to_owned();
    for command in ["checkpoint", "handoff"] {
        let mut args = vec![command];
        if command == "handoff" {
            args.push("create");
        }
        args.extend_from_slice(&[
            "t-1",
            "--lease",
            &lease,
            "--as",
            "worker",
            "--summary",
            &long,
            "--intent",
            &long,
            "--next-action",
            &long,
            "--json",
        ]);
        fixture.ok_json(&fixture.main, &args);
    }
    for index in 0..5 {
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-1",
                &format!("note {index} {long}"),
                "--as",
                "worker",
                "--json",
            ],
        );
    }

    // Every render is stamped, so two runs differ on that line alone.
    let render = |budget: &str| -> String {
        let output = fixture.run(&fixture.main, &["context", "t-1", "--max-chars", budget]);
        assert!(
            output.status.success(),
            "context --max-chars {budget} failed"
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| {
                if line.starts_with("Generated: ") {
                    "Generated: N"
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let complete = render("999999");

    // The compact rendering used to append its marker and hope: past the
    // smallest budgets the body already overran, `take_chars` cut from the
    // end, and the marker was the first thing lost — precisely when the
    // reader most needed telling that the ancestry, the dependencies, every
    // earlier checkpoint and every note had gone.
    for budget in [
        "1000", "1001", "1100", "1200", "1500", "3000", "5000", "8000", "9000", "20000",
    ] {
        let text = render(budget);
        let length = text.chars().count();
        assert!(
            length <= budget.parse::<usize>().unwrap(),
            "--max-chars {budget} produced {length} characters"
        );
        if text != complete {
            assert!(
                text.contains("[context compacted") || text.contains("[older history omitted]"),
                "--max-chars {budget} dropped history and said nothing (ends: {:?})",
                &text.chars().rev().take(60).collect::<String>()
            );
        }
    }

    // --max-chars bounds the rendered text and never did anything here, so
    // accepting it handed an unbounded packet to a caller asking for a bound.
    let refused = fixture.run(
        &fixture.main,
        &["context", "t-1", "--json", "--max-chars", "1000"],
    );
    assert!(
        !refused.status.success(),
        "--json accepted --max-chars and ignored it"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("returns the whole packet"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // Each on its own still works.
    fixture.ok_json(&fixture.main, &["context", "t-1", "--json"]);
    assert!(!render("2000").is_empty());
}

#[test]
fn a_lease_is_only_ever_granted_on_a_task() {
    let fixture = Fixture::new("claimable-type");
    fixture.ok_json(&fixture.main, &["init", "--name", "TYPES", "--json"]);

    // Both containers sort ahead of the real work on every tiebreak --next
    // uses: lower priority number first, then created_at.
    for (id, kind) in [("e-top", "epic"), ("s-top", "story")] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "Container",
                "--id",
                id,
                "--type",
                kind,
                "--priority",
                "0",
                "--json",
            ],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "The real work",
            "--id",
            "t-work",
            "--priority",
            "9",
            "--json",
        ],
    );

    // --next skips a container instead of failing on it: a row that was never
    // claimable must not stall the queue behind it.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker", "--json"]
        )["taskID"],
        "t-work"
    );

    // Naming one explicitly is refused, and the refusal says what to do instead.
    let epic = fixture.run(
        &fixture.main,
        &["claim", "e-top", "--as", "worker", "--json"],
    );
    assert!(!epic.status.success(), "an epic was handed out as work");
    let epic_error = String::from_utf8_lossy(&epic.stderr).to_string();
    assert!(
        epic_error.contains("only a task is claimable"),
        "{epic_error}"
    );
    assert!(epic_error.contains("children"), "{epic_error}");

    let story = fixture.run(
        &fixture.main,
        &["claim", "s-top", "--as", "worker", "--json"],
    );
    assert!(!story.status.success(), "a story was handed out as work");
    let story_error = String::from_utf8_lossy(&story.stderr).to_string();
    assert!(
        story_error.contains("story advance"),
        "a story refusal must point at its gate: {story_error}"
    );

    // The refusal left both rows exactly as they were — no assignee written,
    // no status flipped, which is what made the ledger contradict itself.
    for id in ["e-top", "s-top"] {
        let shown = fixture.ok_json(&fixture.main, &["task", "show", id, "--json"]);
        assert_eq!(shown["status"], "todo", "{id} was moved by a refused claim");
        assert!(shown["assignee"].is_null(), "{id} was assigned anyway");
        assert!(shown["claim"].is_null(), "{id} holds a lease");
    }
}

#[test]
fn a_handoff_addressed_to_a_container_cannot_be_accepted() {
    // A board written before this rule — or imported from atmux — can still
    // carry a pending handoff on a row that is not a task. Accepting it would
    // mint exactly the lease `claim` now refuses, so the guard sits on both
    // lease-minting paths rather than on the verb the operator happened to use.
    let fixture = Fixture::new("handoff-container");
    fixture.ok_json(&fixture.main, &["init", "--name", "LEGACY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Legacy row", "--id", "t-legacy", "--json"],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-legacy", "--as", "outgoing", "--json"],
    );
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-legacy",
            "--lease",
            &token,
            "--as",
            "outgoing",
            "--summary",
            "Ran out of context",
            "--intent",
            "Continue the work",
            "--next-action",
            "Pick up where I stopped",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap().to_owned();

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute("UPDATE tasks SET type='story' WHERE id='t-legacy'", [])
        .unwrap();

    let accepted = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert!(
        !accepted.status.success(),
        "a handoff on a container minted a lease"
    );
    assert!(
        String::from_utf8_lossy(&accepted.stderr).contains("only a task is claimable"),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
}

#[test]
fn a_story_status_is_not_writable_around_its_gate() {
    let fixture = Fixture::new("story-projection");
    fixture.ok_json(&fixture.main, &["init", "--name", "GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Story", "--id", "s-1", "--type", "story", "--json",
        ],
    );

    // Marking it done by hand would stamp completed_at while the gate never
    // took a signoff, dispatched a merge task, or flipped a parent epic.
    let direct = fixture.run(
        &fixture.main,
        &["task", "move", "s-1", "done", "--as", "geoyws", "--json"],
    );
    assert!(
        !direct.status.success(),
        "a story was completed around its gate"
    );
    let error = String::from_utf8_lossy(&direct.stderr).to_string();
    assert!(error.contains("story advance"), "{error}");
    assert!(
        error.contains("planning"),
        "the refusal must say where the gate actually is: {error}"
    );

    let untouched = fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"]);
    // `task add` defaults a story to todo regardless of type, so this is the
    // status the row already held — the point is that the refused move did not
    // change it, and did not stamp completedAt.
    assert_eq!(untouched["status"], "todo", "the refused move still landed");
    assert!(
        untouched["completedAt"].is_null(),
        "completedAt was stamped"
    );

    // The gate itself keeps writing the same column, and the projection holds.
    fixture.ok_json(
        &fixture.main,
        &["story", "advance", "s-1", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"])["status"],
        "todo",
        "ready must project to todo"
    );

    // blocked is outside the gate's vocabulary, so it stays directly writable —
    // refusing it would remove the only way to say it.
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "s-1", "blocked", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-1", "--json"])["status"],
        "blocked"
    );

    // --force overwrites the projection and says so in the ledger.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "s-1", "done", "--as", "geoyws", "--force", "--json",
        ],
    );
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "s-1", "--json"]);
    let bypassed = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["payload"]["gateBypassed"] == json!(true))
        .count();
    assert_eq!(bypassed, 1, "the forced override was not recorded once");

    // A plain task is untouched by any of this.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-1", "done", "--as", "geoyws", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["status"],
        "done"
    );
}

#[test]
fn a_task_cannot_be_made_to_contain_work() {
    let fixture = Fixture::new("nesting");
    fixture.ok_json(&fixture.main, &["init", "--name", "TREE", "--json"]);
    for (id, kind) in [("e-1", "epic"), ("s-1", "story"), ("t-1", "task")] {
        fixture.ok_json(
            &fixture.main,
            &["task", "add", "Row", "--id", id, "--type", kind, "--json"],
        );
    }

    // The three shapes the ledger is actually used in.
    for (id, kind, parent) in [
        ("s-ok", "story", "e-1"),
        ("t-ok-epic", "task", "e-1"),
        ("t-ok-story", "task", "s-1"),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "add", "Row", "--id", id, "--type", kind, "--parent", parent, "--json",
            ],
        );
    }

    // A story under a task is the costly one: `story advance` flips a parent
    // only when it is an epic, so the mis-nested story would silently never
    // flip anything and nothing would ever say so.
    let inverted = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Row", "--id", "s-bad", "--type", "story", "--parent", "t-1", "--json",
        ],
    );
    assert!(!inverted.status.success(), "a story nested under a task");
    let error = String::from_utf8_lossy(&inverted.stderr).to_string();
    assert!(error.contains("story") && error.contains("task"), "{error}");
    assert!(error.contains("contains nothing"), "{error}");

    // An epic nests under an epic: a plan is an epic, so a programme plan holds
    // its sub-plans. This was refused until plans had somewhere to live.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Sub-plan", "--id", "e-sub", "--type", "epic", "--parent", "e-1",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-sub", "--json"])["parentID"],
        "e-1"
    );
    // A story in a story still has no meaning, and neither does a container
    // inside something narrower than itself.
    for (id, kind, parent) in [("s-bad", "story", "s-1"), ("e-bad2", "epic", "s-1")] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "task", "add", "Row", "--id", id, "--type", kind, "--parent", parent, "--json",
            ],
        );
        assert!(!refused.status.success(), "{kind} nested under a story");
    }

    let epic_under_task = fixture.run(
        &fixture.main,
        &[
            "task", "add", "Row", "--id", "e-bad", "--type", "epic", "--parent", "t-1", "--json",
        ],
    );
    assert!(
        !epic_under_task.status.success(),
        "an epic nested under a task"
    );

    // Re-parenting is the same rule: it is the other way to write the field.
    let reparent = fixture.run(
        &fixture.main,
        &[
            "task", "update", "s-ok", "--as", "geoyws", "--parent", "t-1", "--json",
        ],
    );
    assert!(
        !reparent.status.success(),
        "a story was re-parented under a task"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "s-ok", "--json"])["parentID"],
        "e-1",
        "the refused re-parent still landed"
    );

    // Nothing the refusals touched was created.
    for ghost in ["s-bad", "e-bad"] {
        assert!(
            !fixture
                .run(&fixture.main, &["task", "show", ghost, "--json"])
                .status
                .success(),
            "{ghost} was written despite the refusal"
        );
    }
}

/// The bounded fresh-turn protocol of `docs/integrating-orch.md`, driven end to
/// end against the compiled binary, including the restart it exists to survive.
#[test]
fn the_long_horizon_turn_protocol_survives_a_runner_restart() {
    let fixture = Fixture::new("orch-protocol");
    fixture.ok_json(&fixture.main, &["init", "--name", "ORCH", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Long horizon work", "--id", "t-lh", "--json"],
    );

    // (1)-(2) The runner claims the task under its run identity.
    let first = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-lh",
            "--as",
            "orch/run-1",
            "--session",
            "session-1",
            "--json",
        ],
    );
    let stale_token = first["leaseToken"].as_str().unwrap().to_owned();

    // (3) Context is fetched before each model invocation, in both renderings.
    let packet = fixture.ok_json(&fixture.main, &["context", "t-lh", "--json"]);
    assert_eq!(packet["task"]["id"], "t-lh");
    let rendered = fixture.run(&fixture.main, &["context", "t-lh"]);
    assert!(rendered.status.success());

    // The lease token authorizes writes and must never reach a prompt. No read
    // surface may carry it, whichever rendering the runner feeds the model.
    for surface in [
        vec!["context", "t-lh"],
        vec!["context", "t-lh", "--json"],
        vec!["task", "show", "t-lh", "--json"],
        vec!["events", "--task", "t-lh", "--json"],
        vec!["dashboard", "--json"],
    ] {
        let out = fixture.run(&fixture.main, &surface);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !text.contains(&stale_token),
            "{surface:?} leaked the lease token"
        );
    }

    // (5)-(6) The runner writes the envelope itself. `continue` keeps the lease.
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &stale_token,
            "--as",
            "orch/run-1",
            "--state",
            "continue",
            "--summary",
            "turn one changed the parser",
            "--intent",
            "the next turn is the suite",
            "--next-action",
            "run cargo test",
            "--validation",
            "cargo build passed",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-lh", "--json"])["status"],
        "in_progress",
        "a continue checkpoint must keep the task running"
    );

    // The runner now dies. Its lease is still live, so nobody else may take the
    // task -- a crash must not hand work to a second runner mid-turn.
    let contested = fixture.run(
        &fixture.main,
        &["claim", "t-lh", "--as", "orch/run-2", "--json"],
    );
    assert!(!contested.status.success(), "a live lease was taken over");

    // Time passes and the lease lapses. Expiry is what makes the work
    // reclaimable, so drive it the way the sweep does.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE task_claims SET expires_at=1 WHERE task_id='t-lh'",
            [],
        )
        .unwrap();

    // A restarted runner reacquires, and resumes from the newest durable
    // checkpoint rather than from any memory of the old session.
    let second = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-lh",
            "--as",
            "orch/run-2",
            "--session",
            "session-2",
            "--json",
        ],
    );
    let live_token = second["leaseToken"].as_str().unwrap().to_owned();
    assert_ne!(live_token, stale_token, "a restart reused the dead lease");

    let resumed = fixture.ok_json(&fixture.main, &["context", "t-lh", "--json"]);
    let newest = resumed["checkpoints"].as_array().unwrap().last().unwrap();
    assert_eq!(newest["nextAction"], "run cargo test");
    assert_eq!(newest["state"], "continue");

    // The hazard the protocol names: the crashed runner wakes up holding a
    // token from before the handover. It must be refused, and told the truth --
    // "no active lease" would send it to claim a task somebody else is running.
    let zombie = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &stale_token,
            "--as",
            "orch/run-1",
            "--state",
            "done",
            "--summary",
            "zombie write",
            "--intent",
            "stale",
            "--next-action",
            "stale",
            "--json",
        ],
    );
    assert!(!zombie.status.success(), "a superseded lease still wrote");
    let refusal = String::from_utf8_lossy(&zombie.stderr).to_string();
    assert!(
        refusal.contains("orch/run-2"),
        "the refusal must name the live holder: {refusal}"
    );
    assert!(
        refusal.contains("superseded"),
        "the refusal must say the lease was replaced, not that none exists: {refusal}"
    );
    assert!(
        !refusal.contains(&live_token),
        "the refusal handed out the live lease token"
    );

    // The live runner is untouched by the zombie, and closes the task. `done`
    // atomically releases the lease in the same transaction as the checkpoint.
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &live_token,
            "--as",
            "orch/run-2",
            "--state",
            "done",
            "--summary",
            "the suite is green",
            "--intent",
            "nothing further is needed",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let closed = fixture.ok_json(&fixture.main, &["task", "show", "t-lh", "--json"]);
    assert_eq!(closed["status"], "done");
    assert!(
        !closed["completedAt"].is_null(),
        "done left no completion stamp"
    );
    assert!(
        closed["claim"].is_null(),
        "done must release the lease in the same transaction"
    );

    // And a lease released that way cannot be used again by anyone.
    let after_release = fixture.run(
        &fixture.main,
        &[
            "checkpoint",
            "t-lh",
            "--lease",
            &live_token,
            "--as",
            "orch/run-2",
            "--summary",
            "after the close",
            "--intent",
            "stale",
            "--next-action",
            "stale",
            "--json",
        ],
    );
    assert!(
        !after_release.status.success(),
        "a released lease still wrote"
    );
    assert!(
        String::from_utf8_lossy(&after_release.stderr).contains("no active lease"),
        "a genuinely unheld task must say so"
    );
}

/// Step 6 of `docs/integrating-atmux.md` requires a parity receipt against real
/// private state before a cutover. This is that receipt, green and red.
#[test]
fn a_parity_receipt_proves_the_board_holds_what_the_source_held() {
    let fixture = Fixture::new("parity");
    let source = fixture.root.join("atmux-state.db");
    fs::create_dir_all(&fixture.root).unwrap();
    let legacy = Connection::open(&source).unwrap();
    legacy
        .execute_batch(
            r#"
            CREATE TABLE epics(id TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,depends_on TEXT,stories TEXT,body TEXT,driver_ref TEXT,is_ready INTEGER,spawned_at INTEGER,extra TEXT);
            CREATE TABLE stories(id TEXT,epic TEXT,title TEXT,status TEXT,created_at INTEGER,completed_at INTEGER,advanced_at INTEGER,body TEXT,acceptance_criteria TEXT,review_signoff INTEGER,merge_task_id TEXT,merge_mode TEXT,extra TEXT);
            CREATE TABLE tasks(id TEXT,subject TEXT,status TEXT,created_at INTEGER,claimed_at INTEGER,completed_at INTEGER,epic TEXT,story TEXT,owner TEXT,deps TEXT,priority INTEGER,body TEXT,lane TEXT,deliverable TEXT,stale_min INTEGER,driver_only INTEGER,claimed_from TEXT,created_from TEXT,note TEXT,extra TEXT);
            INSERT INTO epics VALUES('e-a','Epic A','ready',1700000000,NULL,'[]','["s-a"]','epic body','driver-1',1,1700000500,'{"customEpicField":"keep me"}');
            INSERT INTO stories VALUES('s-a','e-a','Story A','review',1700000100,NULL,1700000600,'story body','AC text',1,'t-merge','feature-branch','{"customStoryField":"keep me too"}');
            INSERT INTO tasks VALUES('t-a','Task A','in_progress',1700000200,1700000300,NULL,'e-a','s-a','agent-7','["t-b"]',2,'task body','fe','the deliverable',45,1,'driver-2','planner','a legacy note','{"customTaskField":"and me"}');
            INSERT INTO tasks VALUES('t-b','Task B','done',1700000210,NULL,1700000400,'e-a',NULL,'agent-8','[]',1,NULL,'be',NULL,NULL,0,NULL,NULL,NULL,'{}');
            "#,
        )
        .unwrap();
    drop(legacy);

    fixture.ok_json(&fixture.main, &["init", "--name", "PARITY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );

    // A faithful import verifies, and says how much it looked at. A receipt
    // that reports "verified" without a scope is not evidence of anything.
    let green = fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--verify",
            "--json",
        ],
    );
    assert_eq!(green["verified"], true, "a faithful import failed parity");
    assert_eq!(green["compared"], 4);
    assert_eq!(green["matched"], 4);
    assert!(green["missing"].as_array().unwrap().is_empty());
    assert!(green["differing"].as_array().unwrap().is_empty());
    let fields = green["fields"].as_array().unwrap();
    for named in [
        "createdAt",
        "dependencies",
        "atmuxExtra",
        "note",
        "priority",
    ] {
        assert!(
            fields.iter().any(|f| f == named),
            "{named} is not in the stated scope"
        );
    }

    // Now the board drifts from the source, the way a partial or interfered-with
    // migration would leave it.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let tampered = Connection::open(&board).unwrap();
    tampered
        .execute(
            "UPDATE tasks SET title='TAMPERED', priority=9 WHERE id='t-a'",
            [],
        )
        .unwrap();
    tampered
        .execute("DELETE FROM tasks WHERE id='t-b'", [])
        .unwrap();
    drop(tampered);

    let red = fixture.ok_json(
        &fixture.main,
        &[
            "import",
            "atmux-sqlite",
            source.to_str().unwrap(),
            "--as",
            "operator",
            "--verify",
            "--json",
        ],
    );
    assert_eq!(red["verified"], false, "a drifted board still verified");
    assert_eq!(red["missing"], json!(["t-b"]));
    let differing = red["differing"].as_array().unwrap();
    let named = |field: &str| {
        differing
            .iter()
            .any(|d| d["id"] == "t-a" && d["field"] == field)
    };
    assert!(named("title"), "the retitle was not reported");
    assert!(named("priority"), "the repricing was not reported");
    assert!(
        named("dependencies"),
        "the dependency lost with t-b was not reported"
    );
    for entry in differing {
        assert_ne!(
            entry["source"], entry["board"],
            "a matching field was reported as differing"
        );
    }

    // A diagnostic never modifies what it diagnoses: the two verifications
    // above must not have repaired, re-imported, or otherwise touched the board.
    let after = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let ids = after
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !ids.contains(&"t-b"),
        "--verify re-imported the missing row"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-a", "--json"])["title"],
        "TAMPERED",
        "--verify repaired the row it was asked to report on"
    );

    // --verify reads; --reconcile, --force and --dry-run describe a write.
    // Asking for both is two requests, not a precedence puzzle.
    for conflicting in ["--reconcile", "--force", "--dry-run"] {
        let both = fixture.run(
            &fixture.main,
            &[
                "import",
                "atmux-sqlite",
                source.to_str().unwrap(),
                "--as",
                "operator",
                "--verify",
                conflicting,
            ],
        );
        assert!(
            !both.status.success(),
            "--verify {conflicting} was accepted"
        );
        assert!(
            String::from_utf8_lossy(&both.stderr).contains("writes nothing"),
            "{conflicting}"
        );
    }
}

/// ADR-001 §6: consumers receive narrow operations, and MCP/plugin adapters
/// expose the same ones the CLI does. The manifest is how an adapter gets them
/// without restating them -- and `readOnly` is only worth anything if it is
/// true, so this proves each labelled operation writes nothing.
#[test]
fn the_schema_describes_the_real_surface_and_read_only_really_is() {
    let fixture = Fixture::new("schema");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    assert_eq!(schema["version"], env!("CARGO_PKG_VERSION"));

    let operations = schema["operations"].as_array().unwrap();
    assert!(operations.len() > 25, "the manifest lost operations");

    // Every operation the parser accepts appears, and nothing else does.
    for name in [
        "task add",
        "task list",
        "claim",
        "checkpoint",
        "handoff accept",
        "doctor",
        "schema",
    ] {
        assert!(
            operations.iter().any(|o| o["name"] == name),
            "{name} is missing from the manifest"
        );
    }
    for operation in operations {
        // Positionals are named and ordered, so an adapter can build an
        // argument list instead of guessing what the slots mean.
        for positional in operation["positionals"].as_array().unwrap() {
            let name = positional.as_str().unwrap();
            assert!(
                !name.is_empty(),
                "an unnamed positional reached the manifest"
            );
        }
        for flag in operation["flags"].as_array().unwrap() {
            let kind = flag["kind"].as_str().unwrap();
            assert!(
                ["value", "boolean", "list"].contains(&kind),
                "unknown flag kind {kind}"
            );
        }
    }
    // A list-valued flag is described as one, or an adapter generates a tool
    // that can only ever pass a single dependency.
    let add = operations.iter().find(|o| o["name"] == "task add").unwrap();
    assert_eq!(add["positionals"], json!(["title"]));
    let move_op = operations
        .iter()
        .find(|o| o["name"] == "task move")
        .unwrap();
    assert_eq!(move_op["positionals"], json!(["id", "status"]));
    // `claim` takes an id or `--next`, so its positional is marked optional
    // rather than silently required.
    let claim = operations.iter().find(|o| o["name"] == "claim").unwrap();
    assert_eq!(claim["positionals"], json!(["?id"]));
    let depends = add["flags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "depends-on")
        .unwrap();
    assert_eq!(depends["kind"], "list");

    // Now the claim that matters. Set up a board with something to damage,
    // record its bytes, run every read-only operation, and require the file to
    // be untouched. A label an adapter trusts to withhold mutation has to be
    // measured, not asserted.
    fixture.ok_json(&fixture.main, &["init", "--name", "SCHEMA", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Some work", "--id", "t-1", "--json"],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Read-only schema probe.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let rule_id = rule["id"].as_str().unwrap().to_owned();
    let sprint = fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "new",
            "Readonly surface probe",
            "--target-version",
            "0.3.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "agent",
            "--json",
        ],
    );
    let sprint_id = sprint["id"].as_str().unwrap().to_owned();
    let deployment = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--as",
            "schema@e2e",
            "--json",
        ],
    );
    let deployment_id = deployment["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "subscription",
            "add",
            "--id",
            "sub-schema-readonly",
            "--consumer",
            "schema.probe",
            "--action",
            "observe",
            "--timeout-ms",
            "1000",
            "--max-retries",
            "0",
            "--rate-per-minute",
            "1",
            "--max-concurrency",
            "1",
            "--as",
            "schema@e2e",
            "--json",
        ],
    );
    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Read-only schema probe decision.",
            "--as",
            "schema@e2e",
            "--json",
        ],
    );
    let attention_id = attention["id"].as_str().unwrap().to_owned();
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();

    let arguments = |name: &str| -> Option<Vec<String>> {
        let base: Vec<&str> = match name {
            "workspace list" => vec!["workspace", "list"],
            "dashboard" => vec!["dashboard"],
            "doctor" => vec!["doctor"],
            "audit verify" => vec!["audit", "verify"],
            "search" => vec!["search", "Some work"],
            "task list" => vec!["task", "list"],
            "task show" => vec!["task", "show", "t-1"],
            "task verdict list" => vec!["task", "verdict", "list", "t-1"],
            "handoff list" => vec!["handoff", "list"],
            "attention list" => vec!["attention", "list"],
            "attention show" => vec!["attention", "show", &attention_id],
            "tag list" => vec!["tag", "list"],
            "rule list" => vec!["rule", "list"],
            "rule show" => vec!["rule", "show", &rule_id],
            "sitrep list" => vec!["sitrep", "list"],
            "subscription list" => vec!["subscription", "list"],
            "sprint list" => vec!["sprint", "list"],
            "sprint show" => vec!["sprint", "show", &sprint_id],
            "subscription show" => {
                vec!["subscription", "show", "sub-schema-readonly"]
            }
            "deploy show" => vec!["deploy", "show", &deployment_id],
            "deploy list" => vec!["deploy", "list"],
            "deploy current" => vec!["deploy", "current"],
            "schema" => vec!["schema"],
            "events" => vec!["events"],
            "stale" => vec!["stale"],
            "context" => vec!["context", "t-1"],
            "access principal show" => {
                vec!["access", "principal", "show", "--principal", "p-00000000"]
            }
            "access principal list" => vec!["access", "principal", "list"],
            "access explain" => vec![
                "access",
                "explain",
                "--principal",
                "p-00000000",
                "--capability",
                "read",
                "--scope",
                "registry",
            ],
            "access audit" => vec!["access", "audit"],
            "access enforcement show" => vec!["access", "enforcement", "show"],
            "worker show" => vec!["worker", "show", "w-00000000"],
            "worker list" => vec!["worker", "list"],
            // `batch` publishes no positionals but still needs its item list
            // (docs/specs/batch.md BA-01): one valid read item.
            "batch" => vec![
                "batch",
                "--items",
                r#"[{"name":"tag_list","arguments":{}}]"#,
            ],
            // `plugin call` writes nothing by spec (PLUGIN-06), so the loop
            // runs it for real against the action installed below.
            "plugin call" => vec!["plugin", "call", "acme", "lookup"],
            "plugin list" => vec!["plugin", "list"],
            _ => return None,
        };
        Some(base.into_iter().map(str::to_owned).collect())
    };

    // One installed plugin action, so `plugin call` above runs for real
    // (docs/specs/plugin.md PLUGIN-06: it writes no board row, no registry
    // row and no event). The data root must be private or the call refuses;
    // the fixture leaves its modes to the umask, so lock it down first.
    fs::set_permissions(&fixture.data, fs::Permissions::from_mode(0o700)).unwrap();
    let plugin_script = fixture.root.join("schema-plugin.sh");
    fs::write(
        &plugin_script,
        "#!/bin/sh\nIFS= read -r request || true\nprintf '%s\\n' \
         '{\"protocolVersion\":1,\"revision\":\"r1\",\"output\":{}}'\n",
    )
    .unwrap();
    fs::set_permissions(&plugin_script, fs::Permissions::from_mode(0o755)).unwrap();
    let plugin_sha = format!("{:x}", Sha256::digest(fs::read(&plugin_script).unwrap()));
    fs::write(
        fixture.data.join("dispatchers.json"),
        serde_json::to_vec_pretty(&json!({
            "version": 2,
            "consumers": {
                "acme": {
                    "capabilities": ["plugin.read"],
                    "secrets": {},
                    "actions": {
                        "lookup": {
                            "kind": "plugin",
                            "capability": "plugin.read",
                            "executable": plugin_script.to_str().unwrap(),
                            "args": [],
                            "revision": "r1",
                            "sha256": plugin_sha,
                            "timeoutMs": 30_000
                        }
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    // The dispatcher refuses a config any other user could read (PLUGIN), so
    // the fixture sets the mode rather than inheriting the umask.
    fs::set_permissions(
        fixture.data.join("dispatchers.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = fs::read(&board).unwrap();
    let mut covered = 0;
    // Long-running commands cannot be run to completion and compared, so `mcp`
    // and `watch` are excluded by the property the manifest publishes, not by
    // name, so a third one added later
    // is excluded by being declared rather than by editing this test.
    let servers = operations
        .iter()
        .filter(|o| o["longRunning"] == true)
        .map(|o| o["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        servers.contains(&"mcp") && servers.contains(&"watch"),
        "the long-running commands must declare themselves: {servers:?}"
    );
    for operation in operations
        .iter()
        .filter(|o| o["readOnly"] == true && o["longRunning"] != true)
    {
        let name = operation["name"].as_str().unwrap();
        let args = arguments(name)
            .unwrap_or_else(|| panic!("{name} is labelled readOnly but this test cannot run it"));
        let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = fixture.run(&fixture.main, &borrowed);
        if name.starts_with("worker ") {
            // Worker identity exists only under managed enforcement
            // (docs/specs/identity.md IDENT-01), and enforcement is read from
            // the canonical root, which this `KANBAN_DATA_DIR` fixture never
            // touches: so the read is the refusal, naming whichever unmanaged
            // state the host's canonical root is in, and it too writes nothing.
            assert!(!output.status.success(), "{name} ran outside managed");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("worker identity needs managed enforcement; this installation is "),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        assert_eq!(
            fs::read(&board).unwrap(),
            before,
            "{name} is labelled readOnly and modified the board"
        );
        covered += 1;
    }
    assert_eq!(
        covered,
        operations
            .iter()
            .filter(|o| o["readOnly"] == true && o["longRunning"] != true)
            .count(),
        "a readOnly operation went unexercised"
    );
    assert!(covered >= 9, "too few read-only operations were proven");

    // And the converse is not claimed by accident: an operation that plainly
    // writes must not be labelled read-only.
    for name in [
        "task add",
        "task move",
        "claim",
        "checkpoint",
        "attention raise",
        "attention resolve",
        "tag add",
        "tag remove",
        "rule add",
        "rule update",
        "rule retire",
        "handoff create",
        "restore",
        "backup",
    ] {
        let operation = operations.iter().find(|o| o["name"] == name).unwrap();
        assert_eq!(operation["readOnly"], false, "{name} is labelled readOnly");
    }
}

#[test]
fn the_watch_surface_matches_help_and_the_mcp_manifest_excludes_it() {
    let fixture = Fixture::new("watch-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH", "--json"]);

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();
    let watch = operations
        .iter()
        .find(|operation| operation["name"] == "watch")
        .expect("watch is missing from the manifest");
    assert_eq!(watch["readOnly"], true);
    assert_eq!(watch["longRunning"], true);
    assert_eq!(watch["positionals"], json!([]));
    let flags = watch["flags"].as_array().unwrap();
    let flag_kind = |name: &str| -> &str {
        flags.iter().find(|flag| flag["name"] == name).unwrap()["kind"]
            .as_str()
            .unwrap()
    };
    assert_eq!(flag_kind("cursor"), "value");
    assert_eq!(flag_kind("limit"), "value");
    assert_eq!(flag_kind("follow"), "boolean");
    assert_eq!(flag_kind("all"), "boolean");
    assert_eq!(flag_kind("task"), "value");
    assert_eq!(flag_kind("rule"), "value");
    assert_eq!(flag_kind("registry"), "boolean");
    for name in [
        "kind",
        "relation",
        "prior-status",
        "current-status",
        "tag",
        "lane",
        "note-kind",
    ] {
        assert_eq!(flag_kind(name), "list", "{name} is not repeatable");
    }

    let help = fixture.run(&fixture.main, &["watch", "--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("kanban watch"));
    assert!(help_text.contains("--cursor"));
    assert!(help_text.contains("--follow"));
    assert!(help_text.contains("--registry"));

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        !tools.iter().any(|tool| tool["name"] == "watch"),
        "the long-running watch command leaked into the MCP tool list"
    );
}

#[test]
fn compiled_binary_events_after_before_archive_and_schema_match() {
    let fixture = Fixture::new("events-after-before");
    fixture.ok_json(&fixture.main, &["init", "--name", "EVENTS", "--json"]);

    for (id, title) in [
        ("t-1", "first event"),
        ("t-2", "second event"),
        ("t-3", "third event"),
        ("t-4", "fourth event"),
    ] {
        fixture.ok_json(&fixture.main, &["task", "add", title, "--id", id, "--json"]);
    }

    let board_path = board_path_for_project(&fixture, &fixture.main, "EVENTS");
    let seqs = {
        let connection = Connection::open(&board_path).unwrap();
        let seqs = connection
            .prepare("SELECT seq FROM events ORDER BY seq")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            seqs.len() >= 4,
            "expected at least four ledger events, found {}",
            seqs.len()
        );
        for (index, seq) in seqs.iter().enumerate() {
            connection
                .execute(
                    "UPDATE events SET created_at=?, archived=? WHERE seq=?",
                    params![
                        1000_i64 * (index as i64 + 1),
                        if index == 0 { 1_i64 } else { 0_i64 },
                        seq,
                    ],
                )
                .unwrap();
        }
        seqs
    };
    let board_db = board_path.to_string_lossy().into_owned();

    let help = fixture.run(&fixture.main, &["events", "--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    for flag in [
        "--task",
        "--rule",
        "--registry",
        "--kind",
        "--after",
        "--before",
        "--limit",
        "--all",
    ] {
        assert!(
            help_text.contains(flag),
            "help text is missing {flag}: {help_text}"
        );
    }

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let events = schema["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|operation| operation["name"] == "events")
        .expect("events is missing from the manifest");
    let flags = events["flags"].as_array().unwrap();
    let flag_kind = |name: &str| -> &str {
        flags.iter().find(|flag| flag["name"] == name).unwrap()["kind"]
            .as_str()
            .unwrap()
    };
    for name in ["task", "rule", "kind", "after", "before", "limit"] {
        assert_eq!(flag_kind(name), "value", "--{name} should remain scalar");
    }
    assert_eq!(flag_kind("registry"), "boolean");
    assert_eq!(flag_kind("all"), "boolean");

    let half_open = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "2000", "--before", "3000", "--limit", "10",
            "--json",
        ],
    );
    assert!(half_open.status.success());
    let half_open_json: Value = serde_json::from_slice(&half_open.stdout).unwrap();
    let half_open_rows = half_open_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        half_open_rows,
        vec![seqs[1]],
        "half-open bounds should include the start and exclude the end: {}",
        String::from_utf8_lossy(&half_open.stdout)
    );

    let bounded = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "2000", "--before", "4000", "--limit", "1",
            "--json",
        ],
    );
    assert!(bounded.status.success());
    let bounded_json: Value = serde_json::from_slice(&bounded.stdout).unwrap();
    let bounded_rows = bounded_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        bounded_rows,
        vec![seqs[2]],
        "SQL filtering should happen before limit: {}",
        String::from_utf8_lossy(&bounded.stdout)
    );

    let equal_bounds = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--before", "3000", "--limit", "10",
            "--json",
        ],
    );
    assert!(equal_bounds.status.success());
    let equal_bounds_json: Value = serde_json::from_slice(&equal_bounds.stdout).unwrap();
    assert!(
        equal_bounds_json.as_array().unwrap().is_empty(),
        "equal bounds should be empty: {}",
        String::from_utf8_lossy(&equal_bounds.stdout)
    );

    let after_only = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--limit", "10", "--json",
        ],
    );
    assert!(after_only.status.success());
    let after_only_json: Value = serde_json::from_slice(&after_only.stdout).unwrap();
    let after_only_rows = after_only_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        after_only_rows,
        seqs[2..].iter().rev().copied().collect::<Vec<_>>(),
        "after-only bounds should include the lower edge and all later rows: {}",
        String::from_utf8_lossy(&after_only.stdout)
    );

    let before_only = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--before", "4000", "--limit", "10", "--json",
        ],
    );
    assert!(before_only.status.success());
    let before_only_json: Value = serde_json::from_slice(&before_only.stdout).unwrap();
    let before_only_rows = before_only_json
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        before_only_rows,
        vec![seqs[2], seqs[1]],
        "before-only bounds should exclude rows at or after the upper edge: {}",
        String::from_utf8_lossy(&before_only.stdout)
    );

    let default_rows = fixture.ok_json(
        &fixture.main,
        &["events", "--db", &board_db, "--limit", "10", "--json"],
    );
    let default_seqs = default_rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        default_seqs,
        seqs[1..].iter().rev().copied().collect::<Vec<_>>(),
        "default output lost seq-desc ordering"
    );
    assert!(
        default_rows
            .as_array()
            .unwrap()
            .iter()
            .all(|row| !row["archived"].as_bool().unwrap()),
        "default events output leaked archived history: {default_rows}"
    );

    let all_rows = fixture.ok_json(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--all", "--limit", "10", "--json",
        ],
    );
    let all_seqs = all_rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        all_seqs,
        seqs.iter().rev().copied().collect::<Vec<_>>(),
        "--all did not preserve seq-desc ordering"
    );
    assert!(
        all_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["archived"] == json!(true)),
        "--all hid the archived event: {all_rows}"
    );

    for args in [
        ["events", "--db", &board_db, "--after", "-1", "--json"].as_slice(),
        ["events", "--db", &board_db, "--before", "-1", "--json"].as_slice(),
    ] {
        let rejected = fixture.run(&fixture.main, args);
        assert!(!rejected.status.success());
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            stderr.contains("must be non-negative"),
            "negative bound was not rejected correctly: {stderr}"
        );
    }

    let reversed = fixture.run(
        &fixture.main,
        &[
            "events", "--db", &board_db, "--after", "3000", "--before", "2000", "--json",
        ],
    );
    assert!(!reversed.status.success());
    assert!(
        String::from_utf8_lossy(&reversed.stderr)
            .contains("--after must not be later than --before")
    );

    let registry_rejected = fixture.run(
        &fixture.main,
        &["events", "--registry", "--after", "2000", "--json"],
    );
    assert!(!registry_rejected.status.success());
    assert!(
        String::from_utf8_lossy(&registry_rejected.stderr)
            .contains("--after and --before only apply to board events")
    );

    let rule_rejected = fixture.run(
        &fixture.main,
        &["events", "--rule", "r-missing", "--after", "2000", "--json"],
    );
    assert!(!rule_rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rule_rejected.stderr)
            .contains("--after and --before only apply to board events")
    );
}

#[test]
fn watch_replays_resumes_and_respects_selector_boundaries() {
    let fixture = Fixture::new("watch-replay");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH-REPLAY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Main board task", "--id", "t-main", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-main",
            "Main board task note",
            "--as",
            "geoyws",
            "--kind",
            "progress",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Settled board task",
            "--id",
            "t-settled",
            "--json",
        ],
    );
    let settled_claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-settled", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-settled",
            "--lease",
            settled_claim["leaseToken"].as_str().unwrap(),
            "--as",
            "worker",
            "--state",
            "done",
            "--summary",
            "settled",
            "--intent",
            "close it",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-REPLAY");
    Connection::open(&board_path)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-settled'",
            [],
        )
        .unwrap();
    fixture.ok_json(
        &fixture.worktree,
        &["init", "--name", "WATCH-SECONDARY", "--json"],
    );
    fixture.ok_json(
        &fixture.worktree,
        &[
            "task",
            "add",
            "Second board task",
            "--id",
            "t-second",
            "--json",
        ],
    );
    let rule_alpha = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Alpha registry rule.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let rule_beta = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Beta registry rule.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let rule_alpha_id = rule_alpha["id"].as_str().unwrap().to_owned();
    let rule_beta_id = rule_beta["id"].as_str().unwrap().to_owned();
    let default_output = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(default_output.status.success());
    let default_rows = ndjson_values(&default_output);
    assert!(!default_rows.is_empty());
    assert!(
        default_rows
            .iter()
            .all(|row| row["payload"]["taskID"] != json!("t-second")),
        "default board watch leaked the second board: {}",
        String::from_utf8_lossy(&default_output.stdout)
    );
    let saved_cursor = default_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let saved_cursor_seq = decode_watch_cursor(&saved_cursor)["seq"].as_i64().unwrap();

    for args in [
        vec![
            "watch",
            "--task",
            "t-main",
            "--cursor",
            &saved_cursor,
            "--json",
        ],
        vec![
            "watch",
            "--kind",
            "task_added",
            "--cursor",
            &saved_cursor,
            "--json",
        ],
        vec!["watch", "--all", "--cursor", &saved_cursor, "--json"],
        vec!["watch", "--registry", "--cursor", &saved_cursor, "--json"],
        vec![
            "watch",
            "--rule",
            &rule_alpha_id,
            "--cursor",
            &saved_cursor,
            "--json",
        ],
    ] {
        let mismatch = fixture.run(&fixture.main, &args);
        assert!(
            !mismatch.status.success(),
            "selector mismatch unexpectedly reused the stream: {:?}",
            args
        );
        // Only the refusal reaches stdout: no events from the other stream.
        assert!(
            refusal_object(&mismatch).contains("different watch stream"),
            "selector mismatch did not name the stream boundary: {}",
            String::from_utf8_lossy(&mismatch.stderr)
        );
    }

    let later = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Later board task",
            "--id",
            "t-later",
            "--json",
        ],
    );
    assert_eq!(later["id"], json!("t-later"));
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--cursor",
            &saved_cursor,
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(resumed.status.success());
    let resumed_rows = ndjson_values(&resumed);
    assert!(!resumed_rows.is_empty());
    assert!(
        resumed_rows.iter().all(|row| {
            row["payload"]["seq"].as_i64().unwrap() > saved_cursor_seq
                && row["payload"]["taskID"] == json!("t-later")
                && row["payload"]["kind"] == json!("task_added")
        }),
        "resumed watch did not stay on the later task: {}",
        String::from_utf8_lossy(&resumed.stdout)
    );

    let main_task_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-main",
            "--kind",
            "task_added",
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(main_task_output.status.success());
    let main_task_rows = ndjson_values(&main_task_output);
    assert!(!main_task_rows.is_empty());
    assert!(
        main_task_rows
            .iter()
            .all(|row| row["payload"]["taskID"] == json!("t-main")
                && row["payload"]["kind"] == json!("task_added")),
        "task/kind watch leaked other rows: {}",
        String::from_utf8_lossy(&main_task_output.stdout)
    );

    let second_board_output = fixture.run(
        &fixture.worktree,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(second_board_output.status.success());
    let second_board_rows = ndjson_values(&second_board_output);
    assert!(
        second_board_rows
            .iter()
            .any(|row| row["payload"]["taskID"] == json!("t-second")),
        "second board watch never saw its task: {}",
        String::from_utf8_lossy(&second_board_output.stdout)
    );

    let registry_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--registry",
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(registry_output.status.success());
    let registry_rows = ndjson_values(&registry_output);
    assert!(!registry_rows.is_empty());
    let registry_rule_ids = registry_rows
        .iter()
        .filter_map(|row| {
            row["payload"]["payload"]["ruleID"]
                .as_str()
                .map(str::to_owned)
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        registry_rule_ids.contains(&rule_alpha_id) && registry_rule_ids.contains(&rule_beta_id),
        "registry watch did not include both rule IDs: {registry_rule_ids:?}"
    );
    assert!(
        registry_rows
            .iter()
            .all(|row| row["payload"]["taskID"].is_null()),
        "registry watch leaked board events: {}",
        String::from_utf8_lossy(&registry_output.stdout)
    );

    let rule_output = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--rule",
            &rule_alpha_id,
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ],
    );
    assert!(rule_output.status.success());
    let rule_rows = ndjson_values(&rule_output);
    assert!(!rule_rows.is_empty());
    assert!(
        rule_rows
            .iter()
            .all(|row| row["payload"]["payload"]["ruleID"] == json!(rule_alpha_id)),
        "rule watch crossed out of its selector: {}",
        String::from_utf8_lossy(&rule_output.stdout)
    );

    let archive = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(archive["tasks"], 1);
    let archived_default = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "64", "--json"],
    );
    assert!(archived_default.status.success());
    let archived_default_rows = ndjson_values(&archived_default);
    assert!(!archived_default_rows.is_empty());
    assert!(
        archived_default_rows
            .iter()
            .all(|row| row["payload"]["archived"] == json!(false)),
        "default watch emitted archived history: {}",
        String::from_utf8_lossy(&archived_default.stdout)
    );
    let archived_all = fixture.run(
        &fixture.main,
        &["watch", "--all", "--cursor", "0", "--limit", "64", "--json"],
    );
    assert!(archived_all.status.success());
    let archived_all_rows = ndjson_values(&archived_all);
    assert!(
        archived_all_rows
            .iter()
            .any(|row| row["payload"]["archived"] == json!(true)),
        "archive history stayed hidden from --all: {}",
        String::from_utf8_lossy(&archived_all.stdout)
    );

    let exact_board = board_path_for_project(&fixture, &fixture.main, "WATCH-REPLAY");
    let db_root = fixture.root.join("watch-db-only");
    fs::create_dir_all(&db_root).unwrap();
    let db_output = fixture
        .command_with_data_dir(&db_root, &fixture.root.join("watch-db-data"))
        .args([
            "watch",
            "--db",
            exact_board.to_str().unwrap(),
            "--cursor",
            "0",
            "--limit",
            "32",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        db_output.status.success(),
        "--db exact board path failed: stdout={} stderr={}",
        String::from_utf8_lossy(&db_output.stdout),
        String::from_utf8_lossy(&db_output.stderr)
    );
    let db_rows = ndjson_values(&db_output);
    assert!(!db_rows.is_empty());
    let canonical_board = exact_board.canonicalize().unwrap_or(exact_board);
    assert!(
        db_rows.iter().all(|row| {
            row["scope"]["sourceKind"] == json!("board")
                && row["scope"]["selectorKind"] == json!("board")
                && row["scope"]["source"] == json!(canonical_board.to_string_lossy().into_owned())
        }),
        "--db did not stay on the exact board path: {}",
        String::from_utf8_lossy(&db_output.stdout)
    );
}

#[test]
fn watch_follow_streams_new_events_and_keeps_outputs_separated() {
    let fixture = Fixture::new("watch-follow");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH-FOLLOW", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Follow me", "--id", "t-follow", "--json"],
    );
    let preflight = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "32", "--json"],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    assert!(!preflight_rows.is_empty());
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let start_cursor_json = decode_watch_cursor(&start_cursor);
    assert!(start_cursor_json.get("seq").is_some());
    assert!(start_cursor_json.get("lastSeq").is_none());
    assert_eq!(
        start_cursor_json["seq"].as_i64().unwrap(),
        preflight_rows.last().unwrap()["payload"]["seq"]
            .as_i64()
            .unwrap()
    );

    let watch = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "1",
            "--json",
        ],
    );
    let heartbeat = watch.next_stdout_json(Duration::from_secs(5));
    assert_eq!(heartbeat["type"], "heartbeat");
    assert_eq!(heartbeat["version"], 1);
    assert!(
        heartbeat["scope"]["sourceKind"] == json!("board")
            && heartbeat["scope"]["selectorKind"] == json!("board")
            && heartbeat["scope"]["selectorValue"].is_null()
    );
    let heartbeat_cursor = decode_watch_cursor(heartbeat["cursor"].as_str().unwrap());
    assert_eq!(heartbeat_cursor["seq"], start_cursor_json["seq"]);

    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-FOLLOW");
    let raw_secret_seq = insert_raw_board_event(
        &board_path,
        Some("t-follow"),
        "watch_secret_probe",
        "geoyws",
        json!({
            "token": "outer-secret",
            "tokenCount": 7,
            "snake_token": "snake-secret",
            "camelToken": "camel-secret",
            "nested": {
                "tokenCount": 9,
                "snake_token": "nested-snake-secret",
                "camelToken": "nested-camel-secret",
                "items": [
                    {
                        "tokenCount": 3,
                        "materialValue": "deep-secret",
                        "secretValue": "deeper-secret",
                        "keep": "ok"
                    }
                ]
            }
        }),
    );
    let next_event = watch.next_stdout_event_json(Duration::from_secs(10));
    assert_eq!(next_event["type"], "event");
    let next_cursor = decode_watch_cursor(next_event["cursor"].as_str().unwrap());
    assert_eq!(next_cursor["seq"].as_i64().unwrap(), raw_secret_seq);
    let payload = &next_event["payload"]["payload"];
    assert!(payload.get("token").is_none());
    assert!(payload.get("snake_token").is_none());
    assert!(payload.get("camelToken").is_none());
    assert_eq!(payload["tokenCount"], 7);
    assert!(payload["nested"].get("snake_token").is_none());
    assert!(payload["nested"].get("camelToken").is_none());
    assert_eq!(payload["nested"]["tokenCount"], 9);
    assert!(payload["nested"]["items"][0].get("materialValue").is_none());
    assert!(payload["nested"]["items"][0].get("secretValue").is_none());
    assert_eq!(payload["nested"]["items"][0]["tokenCount"], 3);
    assert_eq!(payload["nested"]["items"][0]["keep"], "ok");
    assert!(
        watch.stderr_snapshot().is_empty(),
        "watch wrote diagnostics to stderr"
    );
    let stderr = watch.finish();
    assert!(
        stderr.is_empty(),
        "watch did not keep stderr separate from NDJSON"
    );
}

#[test]
fn watch_emits_truthful_bounded_semantic_envelopes() {
    let fixture = Fixture::new("watch-semantic-envelope");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-SEMANTIC-ENVELOPE", "--json"],
    );
    for tag in ["geoyws/alpha", "geoyws/zeta"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Parent story",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Completed dependency",
            "--id",
            "t-base",
            "--status",
            "done",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Child task",
            "--id",
            "t-child",
            "--parent",
            "s-parent",
            "--depends-on",
            "t-base",
            "--tag",
            "geoyws/zeta",
            "--tag",
            "geoyws/alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );

    let audit = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(audit["healthy"], true, "{audit}");
    assert_eq!(audit["boards"][0]["audit"]["healthy"], true, "{audit}");

    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-SEMANTIC-ENVELOPE");
    let board_id = fs::canonicalize(&board_path)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let legacy_seq = insert_raw_board_event(
        &board_path,
        Some("t-child"),
        "legacy_semantic_probe",
        "geoyws",
        json!({
            "token": "outer-secret",
            "tokenized": "visible",
            "nested": {
                "secretValue": "inner-secret",
                "keep": "visible",
                "items": [{"materialValue": "deep-secret", "keep": "still-visible"}]
            }
        }),
    );
    let oversized_seq = insert_raw_board_event(
        &board_path,
        Some("t-child"),
        "legacy_oversized_probe",
        "geoyws",
        json!({"blob": "x".repeat(20_000)}),
    );

    let output = fixture.run(
        &fixture.main,
        &[
            "watch", "--task", "t-child", "--cursor", "0", "--limit", "16", "--json",
        ],
    );
    assert!(
        output.status.success(),
        "watch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = ndjson_values(&output);
    let find = |kind: &str| {
        rows.iter()
            .find(|row| row["payload"]["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {rows:#?}"))
    };
    let added = find("task_added");
    let moved = find("task_moved");

    for envelope in [added, moved] {
        let event = &envelope["payload"];
        assert_eq!(envelope["version"], 1, "{envelope}");
        assert_eq!(envelope["type"], "event", "{envelope}");
        assert_eq!(event["schemaVersion"], 1, "{event}");
        assert_eq!(event["board"]["id"], board_id, "{event}");
        assert_eq!(event["board"]["name"], "WATCH-SEMANTIC-ENVELOPE", "{event}");
        assert_eq!(event["eventID"], event["eventHash"], "{event}");
        assert!(
            event["eventID"].as_str().is_some_and(|id| !id.is_empty()),
            "{event}"
        );
        assert!(event["seq"].as_i64().is_some_and(|seq| seq > 0), "{event}");
        assert_eq!(event["timestamp"], event["createdAt"], "{event}");
        assert!(
            event["timestamp"].as_i64().is_some_and(|at| at > 0),
            "{event}"
        );
        assert_eq!(event["actor"], "geoyws", "{event}");
        assert_eq!(event["subject"], json!({"type":"task","id":"t-child"}));
        assert_eq!(event["tags"], json!(["geoyws/alpha", "geoyws/zeta"]));
        assert!(event["payload"].get("_semanticV1").is_none(), "{event}");
        assert!(serde_json::to_vec(&event["metadata"]).unwrap().len() <= 16_384);
        let relations = event["relations"].as_array().unwrap();
        for relation in [
            json!({"kind":"ancestor","type":"epic","id":"e-root"}),
            json!({"kind":"depends-on","type":"task","id":"t-base"}),
            json!({"kind":"parent","type":"story","id":"s-parent"}),
        ] {
            assert!(
                relations.contains(&relation),
                "missing {relation} in {event}"
            );
        }
    }
    assert_eq!(added["payload"]["priorStatus"], Value::Null);
    assert_eq!(added["payload"]["currentStatus"], "todo");
    assert_eq!(moved["payload"]["priorStatus"], "todo");
    assert_eq!(moved["payload"]["currentStatus"], "done");

    let legacy = &find("legacy_semantic_probe")["payload"];
    assert_eq!(legacy["seq"], legacy_seq);
    for field in [
        "subject",
        "relations",
        "priorStatus",
        "currentStatus",
        "tags",
    ] {
        assert!(
            legacy[field].is_null(),
            "{field} was reconstructed: {legacy}"
        );
    }
    assert!(legacy["payload"].get("token").is_none(), "{legacy}");
    assert_eq!(legacy["payload"]["tokenized"], "visible");
    assert!(
        legacy["payload"]["nested"].get("secretValue").is_none(),
        "{legacy}"
    );
    assert!(
        legacy["payload"]["nested"]["items"][0]
            .get("materialValue")
            .is_none(),
        "{legacy}"
    );
    assert_eq!(
        legacy["payload"]["nested"]["items"][0]["keep"],
        "still-visible"
    );
    assert_eq!(legacy["metadata"]["value"], legacy["payload"]);
    assert_eq!(legacy["metadata"]["truncated"], false);
    assert!(serde_json::to_vec(&legacy["metadata"]).unwrap().len() <= 16_384);

    let oversized = &find("legacy_oversized_probe")["payload"];
    assert_eq!(oversized["seq"], oversized_seq);
    assert_eq!(oversized["metadata"]["value"], Value::Null);
    assert_eq!(oversized["metadata"]["truncated"], true);
    assert!(oversized["metadata"]["bytes"].as_u64().unwrap() > 16_384);
    assert!(serde_json::to_vec(&oversized["metadata"]).unwrap().len() <= 16_384);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("_semanticV1"));
}

#[test]
fn watch_filters_sparse_history_and_binds_normalized_predicates_to_cursors() {
    let fixture = Fixture::new("watch-semantic-filters");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-SEMANTIC-FILTERS", "--json"],
    );
    for tag in ["geoyws/alpha", "geoyws/zeta"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Parent story",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Other relation target",
            "--id",
            "t-other",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Dependency",
            "--id",
            "t-base",
            "--status",
            "done",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Filtered child",
            "--id",
            "t-child",
            "--parent",
            "s-parent",
            "--depends-on",
            "t-base",
            "--tag",
            "geoyws/zeta",
            "--tag",
            "geoyws/alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );

    let kinds = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_moved",
            "--kind",
            "task_added",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(
        kinds.status.success(),
        "{}",
        String::from_utf8_lossy(&kinds.stderr)
    );
    let kind_rows = ndjson_values(&kinds);
    let kind_names = kind_rows
        .iter()
        .map(|row| row["payload"]["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        kind_names,
        std::collections::BTreeSet::from(["task_added", "task_moved"])
    );

    let filtered = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_moved",
            "--kind",
            "task_added",
            "--relation",
            "parent:s-parent",
            "--relation",
            "parent:t-other",
            "--prior-status",
            "todo",
            "--current-status",
            "done",
            "--tag",
            "geoyws/alpha",
            "--tag",
            "geoyws/zeta",
            "--cursor",
            "0",
            "--limit",
            "1",
            "--json",
        ],
    );
    assert!(
        filtered.status.success(),
        "sparse filtered watch failed: {}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let filtered_rows = ndjson_values(&filtered);
    assert_eq!(
        filtered_rows.len(),
        1,
        "{}",
        String::from_utf8_lossy(&filtered.stdout)
    );
    assert_eq!(filtered_rows[0]["payload"]["kind"], "task_moved");
    assert_eq!(filtered_rows[0]["payload"]["priorStatus"], "todo");
    assert_eq!(filtered_rows[0]["payload"]["currentStatus"], "done");
    let cursor = filtered_rows[0]["cursor"].as_str().unwrap().to_owned();
    let cursor_json = decode_watch_cursor(&cursor);
    assert_eq!(cursor_json["kinds"], json!(["task_added", "task_moved"]));
    assert_eq!(
        cursor_json["relations"],
        json!(["parent:s-parent", "parent:t-other"])
    );
    assert_eq!(cursor_json["priorStatuses"], json!(["todo"]));
    assert_eq!(cursor_json["currentStatuses"], json!(["done"]));
    assert_eq!(cursor_json["tags"], json!(["geoyws/alpha", "geoyws/zeta"]));

    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "todo", "--as", "geoyws", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-child", "done", "--as", "geoyws", "--json",
        ],
    );
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--tag",
            "geoyws/zeta",
            "--tag",
            "geoyws/alpha",
            "--current-status",
            "done",
            "--prior-status",
            "todo",
            "--relation",
            "parent:t-other",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_added",
            "--kind",
            "task_moved",
            "--cursor",
            &cursor,
            "--limit",
            "1",
            "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "normalized cursor did not resume: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    let resumed_rows = ndjson_values(&resumed);
    assert_eq!(resumed_rows.len(), 1);
    assert_eq!(resumed_rows[0]["payload"]["kind"], "task_moved");
    assert!(
        resumed_rows[0]["payload"]["seq"].as_i64().unwrap()
            > filtered_rows[0]["payload"]["seq"].as_i64().unwrap()
    );

    let mismatch = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-child",
            "--kind",
            "task_added",
            "--kind",
            "task_moved",
            "--relation",
            "parent:s-parent",
            "--relation",
            "parent:t-other",
            "--prior-status",
            "todo",
            "--current-status",
            "done",
            "--tag",
            "geoyws/alpha",
            "--cursor",
            &cursor,
            "--json",
        ],
    );
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("different watch stream"));

    let invalid_cases = [
        (vec!["--task", "t-never-existed"], "not present"),
        (vec!["--relation", "child:t-other"], "KIND:ID"),
        (
            vec!["--relation", "parent:t-never-existed"],
            "historical relation target",
        ),
        (
            vec!["--kind", "not_a_real_event"],
            "unknown watch event kind",
        ),
        (vec!["--prior-status", "not-a-status"], "must be one of"),
        (vec!["--current-status", "not-a-status"], "must be one of"),
        (vec!["--tag", "not-a-tag"], "master file"),
    ];
    for (flags, expected) in invalid_cases {
        let mut args = vec!["watch"];
        args.extend(flags);
        args.extend(["--cursor", "0", "--json"]);
        let rejected = fixture.run(&fixture.main, &args);
        assert!(
            !rejected.status.success(),
            "invalid watch succeeded: {args:?}"
        );
        let error = refusal_object(&rejected);
        assert!(
            error.contains(expected),
            "{args:?}: expected {expected:?} in {error}"
        );
    }
}

#[test]
fn watch_follow_delivers_an_event_queued_behind_interleaved_heartbeats() {
    let fixture = Fixture::new("watch-heartbeat-interleave");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-INTERLEAVE", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Queued behind heartbeats",
            "--id",
            "t-interleaved",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let preflight = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-interleaved",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let watcher = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--task",
            "t-interleaved",
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "8",
            "--json",
        ],
    );
    let idle = watcher.next_stdout_json(Duration::from_secs(5));
    assert_eq!(idle["type"], "heartbeat");
    assert_eq!(idle["payload"]["state"], "idle");

    // Hold the mutation back for several poll intervals so the follow loop
    // queues keep-alive heartbeats ahead of the event. Under real load the
    // same interleaving happens on its own but far too rarely to rely on, so
    // the delay is what makes the window deterministic. It constructs the
    // condition; the drained-count assertion below is what proves the reader
    // handled it.
    std::thread::sleep(WATCH_POLL_INTERVAL * 6);

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-interleaved",
            "in_progress",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let (moved, drained) = watcher.next_stdout_event_json_with_drain_count(Duration::from_secs(10));
    assert!(
        drained >= 1,
        "no heartbeat interleaved, so the drain never ran and this test silently \
         degraded to the pass-through case"
    );
    assert_eq!(moved["type"], "event");
    assert_eq!(moved["payload"]["kind"], "task_moved");
    assert_eq!(
        moved["payload"]["subject"],
        json!({"type":"task","id":"t-interleaved"})
    );
    assert_eq!(moved["payload"]["actor"], "geoyws");
    assert_eq!(moved["payload"]["priorStatus"], "todo");
    assert_eq!(moved["payload"]["currentStatus"], "in_progress");
    assert!(watcher.finish().is_empty());
}

fn watch_board_head(fixture: &Fixture, project: &str) -> i64 {
    let path = board_path_for_project(fixture, &fixture.main, project);
    Connection::open(path)
        .unwrap()
        .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn watch_seq(envelope: &Value) -> i64 {
    decode_watch_cursor(envelope["cursor"].as_str().unwrap())["seq"]
        .as_i64()
        .unwrap()
}

/// WATCH-01..04 and WATCH-12 (A1, A2, A3): `--lane` steers by the subject
/// row's lane, binds to the cursor, is echoed in scope, and is refused on
/// registry scope.
#[test]
fn watch_lane_steers_by_subject_lane_binds_the_cursor_and_echoes_the_set() {
    let fixture = Fixture::new("watch-lane");
    fixture.ok_json(&fixture.main, &["init", "--name", "WATCH-LANE", "--json"]);
    for (id, lane) in [("t-two", "driver-2"), ("t-three", "driver-3")] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task", "add", id, "--id", id, "--lane", lane, "--as", "geoyws", "--json",
            ],
        );
        fixture.ok_json(
            &fixture.main,
            &["note", id, "one note", "--as", "geoyws", "--json"],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "laneless", "--id", "t-none", "--as", "geoyws", "--json",
        ],
    );

    let steered = fixture.run(
        &fixture.main,
        &[
            "watch", "--lane", "driver-9", "--lane", "driver-2", "--lane", "driver-2", "--cursor",
            "0", "--limit", "100", "--json",
        ],
    );
    assert!(
        steered.status.success(),
        "{}",
        String::from_utf8_lossy(&steered.stderr)
    );
    let rows = ndjson_values(&steered);
    let kinds = rows
        .iter()
        .map(|row| row["payload"]["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(kinds, ["task_added", "note_added"], "{rows:?}");
    for row in &rows {
        assert_eq!(row["payload"]["taskID"], "t-two");
        assert_eq!(row["payload"]["lane"], "driver-2");
        // A2: the empty half of the set is visible, not silent.
        assert_eq!(row["scope"]["lanes"], json!(["driver-2", "driver-9"]));
    }
    let cursor = rows.last().unwrap()["cursor"].as_str().unwrap().to_owned();
    assert_eq!(
        decode_watch_cursor(&cursor)["lanes"],
        json!(["driver-2", "driver-9"])
    );

    // The same normalized set resumes; any other set is refused before a
    // row is read, with nothing on stdout but the refusal.
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch", "--lane", "driver-2", "--lane", "driver-9", "--cursor", &cursor, "--json",
        ],
    );
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(ndjson_values(&resumed).is_empty());
    let other = fixture.run(
        &fixture.main,
        &["watch", "--lane", "driver-3", "--cursor", &cursor, "--json"],
    );
    let error = refusal_object(&other);
    assert!(error.contains("different watch stream"), "{error}");
    assert!(error.contains("--lane"), "{error}");
    let unsteered = fixture.run(&fixture.main, &["watch", "--cursor", &cursor, "--json"]);
    assert!(refusal_object(&unsteered).contains("different watch stream"));

    // A3: registry scope refuses each steering flag by name.
    for (flag, value) in [("--lane", "driver-2"), ("--note-kind", "blocker")] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "watch",
                "--registry",
                flag,
                value,
                "--cursor",
                "0",
                "--json",
            ],
        );
        let error = refusal_object(&refused);
        assert!(error.contains(flag), "{flag}: {error}");
    }
    let zero = fixture.run(
        &fixture.main,
        &[
            "watch", "--lane", "driver-2", "--follow", "--limit", "0", "--json",
        ],
    );
    assert!(refusal_object(&zero).contains("at least 1"));

    // WATCH-12: the limit bounds the filtered set, not the raw scan.
    let limited = fixture.run(
        &fixture.main,
        &[
            "watch", "--lane", "driver-3", "--cursor", "0", "--limit", "1", "--json",
        ],
    );
    let limited = ndjson_values(&limited);
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0]["payload"]["taskID"], "t-three");
}

/// WATCH-05 and WATCH-13 (A4): `--note-kind` steers notes, `steer` is a
/// note kind that round-trips with its author, and an unknown kind is
/// refused naming every accepted kind without writing anything.
#[test]
fn watch_note_kind_steers_notes_and_a_steer_note_round_trips_with_its_actor() {
    let fixture = Fixture::new("watch-note-kind");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-NOTE-KIND", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "noted", "--id", "t-noted", "--lane", "driver", "--as", "geoyws",
            "--json",
        ],
    );
    for kind in ["blocker", "progress"] {
        fixture.ok_json(
            &fixture.main,
            &[
                "note", "t-noted", kind, "--kind", kind, "--as", "geoyws", "--json",
            ],
        );
    }
    let planner = "@:geoyws/kanban/planner";
    let steer = fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-noted",
            "look at t-noted now",
            "--kind",
            "steer",
            "--as",
            planner,
            "--json",
        ],
    );
    assert_eq!(steer["kind"], "steer");

    let replay = |kinds: &[&str]| {
        let mut args = vec!["watch", "--task", "t-noted"];
        for kind in kinds {
            args.extend(["--note-kind", kind]);
        }
        args.extend(["--cursor", "0", "--limit", "100", "--json"]);
        let output = fixture.run(&fixture.main, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        ndjson_values(&output)
    };
    let blocker = replay(&["blocker"]);
    assert_eq!(blocker.len(), 1, "{blocker:?}");
    assert_eq!(blocker[0]["payload"]["kind"], "note_added");
    assert_eq!(blocker[0]["payload"]["payload"]["kind"], "blocker");
    assert_eq!(blocker[0]["scope"]["noteKinds"], json!(["blocker"]));

    let steered = replay(&["steer"]);
    assert_eq!(steered.len(), 1, "{steered:?}");
    assert_eq!(steered[0]["payload"]["payload"]["kind"], "steer");
    assert_eq!(steered[0]["payload"]["actor"], planner);
    assert_eq!(steered[0]["payload"]["lane"], "driver");

    // ORed within the family; the task_added event never passes.
    let both = replay(&["steer", "blocker"]);
    let note_kinds = both
        .iter()
        .map(|row| row["payload"]["payload"]["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(note_kinds, ["blocker", "steer"]);

    let before = watch_board_head(&fixture, "WATCH-NOTE-KIND");
    let unknown = fixture.run(
        &fixture.main,
        &["watch", "--note-kind", "urgent", "--cursor", "0", "--json"],
    );
    let error = refusal_object(&unknown);
    for kind in [
        "plan", "progress", "blocker", "decision", "evidence", "done", "steer",
    ] {
        assert!(error.contains(kind), "{kind} missing from {error}");
    }
    let bad_note = fixture.run(
        &fixture.main,
        &[
            "note", "t-noted", "x", "--kind", "urgent", "--as", "geoyws", "--json",
        ],
    );
    assert!(refusal_object(&bad_note).contains("steer"));
    assert_eq!(watch_board_head(&fixture, "WATCH-NOTE-KIND"), before);
}

/// WATCH-06, WATCH-09, WATCH-10 (A5): board envelopes carry the subject
/// row's lane, type, priority and level beside the unchanged v1 keys;
/// registry, taskless and removed-subject envelopes carry explicit nulls;
/// no lease token leaks.
#[test]
fn watch_envelopes_carry_subject_lane_type_and_priority_with_explicit_nulls() {
    let fixture = Fixture::new("watch-envelope-fields");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-ENVELOPE", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "P1 task",
            "--id",
            "t-p1",
            "--lane",
            "driver-2",
            "--priority",
            "4",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-p1",
            "--as",
            "@:geoyws/kanban/driver-2",
            "--json",
        ],
    );
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-p1",
            "decided",
            "--kind",
            "decision",
            "--as",
            "@:geoyws/kanban/driver-2",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Gone", "--id", "t-gone", "--lane", "driver-2", "--as", "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-gone", "--as", "geoyws", "--json"],
    );

    let board = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "100", "--json"],
    );
    assert!(board.status.success());
    let stdout = String::from_utf8_lossy(&board.stdout).into_owned();
    assert!(
        !stdout.contains(&lease),
        "a lease token reached a watch envelope"
    );
    assert!(!stdout.contains("leaseToken"));
    let rows = ndjson_values(&board);
    let decision = rows
        .iter()
        .find(|row| row["payload"]["kind"] == "note_added")
        .expect("decision note event");
    let payload = &decision["payload"];
    assert_eq!(payload["lane"], "driver-2");
    assert_eq!(payload["type"], "task");
    assert_eq!(payload["priority"], 4);
    assert_eq!(payload["priorityLevel"], "P1");
    assert_eq!(payload["actor"], "@:geoyws/kanban/driver-2");
    for key in [
        "board",
        "eventID",
        "timestamp",
        "subject",
        "relations",
        "priorStatus",
        "currentStatus",
        "tags",
        "metadata",
    ] {
        assert!(payload.get(key).is_some(), "v1 key {key} vanished");
    }
    assert_eq!(payload["subject"], json!({"type":"task","id":"t-p1"}));

    // A removed subject has no row to read: explicit nulls, never absent.
    let gone = rows
        .iter()
        .filter(|row| row["payload"]["taskID"] == "t-gone")
        .collect::<Vec<_>>();
    assert!(!gone.is_empty());
    for row in gone {
        for key in ["lane", "type", "priority", "priorityLevel"] {
            assert_eq!(row["payload"].get(key), Some(&Value::Null), "{key}: {row}");
        }
    }

    let registry = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--registry",
            "--cursor",
            "0",
            "--limit",
            "100",
            "--json",
        ],
    );
    assert!(
        registry.status.success(),
        "{}",
        String::from_utf8_lossy(&registry.stderr)
    );
    let registry_rows = ndjson_values(&registry);
    assert!(
        !registry_rows.is_empty(),
        "the registry trail emitted nothing"
    );
    for row in &registry_rows {
        for key in ["lane", "type", "priority", "priorityLevel"] {
            assert_eq!(row["payload"].get(key), Some(&Value::Null), "{key}: {row}");
        }
    }
}

/// WATCH-08 and WATCH-11 (A7): a cursor minted before WATCH resumes under
/// a lane predicate, and a follow over an unmatched tail advances the
/// cursor once to the tail instead of re-reading it.
#[test]
fn watch_lane_follow_advances_over_an_unmatched_tail_and_resumes_a_pre_watch_cursor() {
    let fixture = Fixture::new("watch-lane-follow");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-LANE-FOLLOW", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "other lane",
            "--id",
            "t-other",
            "--lane",
            "driver-3",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let plain = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "100", "--json"],
    );
    let plain = ndjson_values(&plain);
    let mut old = decode_watch_cursor(plain.last().unwrap()["cursor"].as_str().unwrap());
    let object = old.as_object_mut().unwrap();
    assert!(object.remove("lanes").is_some(), "new cursors carry lanes");
    assert!(object.remove("noteKinds").is_some());
    let old = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&old).unwrap());

    for index in 0..50 {
        fixture.ok_json(
            &fixture.main,
            &[
                "note",
                "t-other",
                &format!("unmatched {index}"),
                "--as",
                "geoyws",
                "--json",
            ],
        );
    }
    let head = watch_board_head(&fixture, "WATCH-LANE-FOLLOW");

    let watcher = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--lane", "driver-2", "--cursor", &old, "--follow", "--limit", "64", "--json",
        ],
    );
    let first = watcher.next_stdout_json(Duration::from_secs(10));
    assert_eq!(first["type"], "heartbeat", "{first}");
    assert_eq!(first["payload"]["state"], "advanced");
    assert_eq!(
        watch_seq(&first),
        head,
        "the advance stopped short of the tail"
    );
    assert_eq!(
        decode_watch_cursor(first["cursor"].as_str().unwrap())["lanes"],
        json!(["driver-2"])
    );
    for _ in 0..3 {
        let next = watcher.next_stdout_json(Duration::from_secs(10));
        assert_eq!(
            next["payload"]["state"], "idle",
            "the stream re-looped: {next}"
        );
        assert_eq!(watch_seq(&next), head);
    }
    assert!(watcher.finish().is_empty());
}

#[test]
fn watch_replays_removed_subjects_and_keeps_registry_semantics_separate() {
    let fixture = Fixture::new("watch-removed-and-registry");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-REMOVED-REGISTRY", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root epic",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Historical parent",
            "--id",
            "s-parent",
            "--type",
            "story",
            "--parent",
            "e-root",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Removed subject",
            "--id",
            "t-removed",
            "--parent",
            "s-parent",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let preflight = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    let start_cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let watcher = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &[
            "--task",
            "t-removed",
            "--cursor",
            &start_cursor,
            "--follow",
            "--limit",
            "8",
            "--json",
        ],
    );
    let idle = watcher.next_stdout_json(Duration::from_secs(5));
    assert_eq!(idle["type"], "heartbeat");
    assert_eq!(idle["payload"]["state"], "idle");
    assert_eq!(idle["cursor"], start_cursor);

    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "t-removed", "--as", "geoyws", "--json"],
    );
    let removed = watcher.next_stdout_event_json(Duration::from_secs(10));
    assert_eq!(removed["type"], "event");
    assert_eq!(removed["payload"]["kind"], "task_removed");
    assert_eq!(
        removed["payload"]["subject"],
        json!({"type":"task","id":"t-removed"})
    );
    assert_eq!(removed["payload"]["priorStatus"], "todo");
    assert!(removed["payload"]["currentStatus"].is_null());
    assert!(watcher.finish().is_empty());

    fixture.ok_json(
        &fixture.main,
        &["task", "remove", "s-parent", "--as", "geoyws", "--json"],
    );
    let replay = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_removed",
            "--cursor",
            "0",
            "--limit",
            "8",
            "--json",
        ],
    );
    assert!(
        replay.status.success(),
        "historical replay failed: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_rows = ndjson_values(&replay);
    assert_eq!(replay_rows.len(), 1);
    assert_eq!(replay_rows[0]["payload"]["kind"], "task_removed");
    let replay_cursor = replay_rows[0]["cursor"].as_str().unwrap();
    let resumed = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--task",
            "t-removed",
            "--relation",
            "parent:s-parent",
            "--kind",
            "task_removed",
            "--cursor",
            replay_cursor,
            "--limit",
            "8",
            "--json",
        ],
    );
    assert!(resumed.status.success());
    assert!(ndjson_values(&resumed).is_empty());

    let rule = fixture.ok_json(
        &fixture.main,
        &["rule", "add", "Registry event", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            rule["id"].as_str().unwrap(),
            "--body",
            "Updated registry event",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let registry = fixture.run(
        &fixture.main,
        &[
            "watch",
            "--registry",
            "--kind",
            "rule_updated",
            "--kind",
            "rule_added",
            "--cursor",
            "0",
            "--limit",
            "16",
            "--json",
        ],
    );
    assert!(registry.status.success());
    let registry_rows = ndjson_values(&registry);
    let registry_kinds = registry_rows
        .iter()
        .map(|row| row["payload"]["kind"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        registry_kinds,
        std::collections::BTreeSet::from(["rule_added", "rule_updated"])
    );
    assert!(
        registry_rows
            .iter()
            .all(|row| row["payload"]["board"].is_null())
    );

    for (flag, value) in [
        ("--relation", "parent:s-parent"),
        ("--prior-status", "todo"),
        ("--current-status", "done"),
        ("--tag", "alpha"),
    ] {
        let rejected = fixture.run(
            &fixture.main,
            &[
                "watch",
                "--registry",
                flag,
                value,
                "--cursor",
                "0",
                "--json",
            ],
        );
        assert!(!rejected.status.success(), "registry accepted {flag}");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("apply only to board watch events")
        );
    }
}

#[test]
fn watch_drains_backlogs_in_bounded_batches_and_rejects_invalid_limits() {
    let fixture = Fixture::new("watch-bounded");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-BOUNDED", "--json"],
    );
    for (id, title) in [("t-one", "One"), ("t-two", "Two"), ("t-three", "Three")] {
        fixture.ok_json(&fixture.main, &["task", "add", title, "--id", id, "--json"]);
    }
    fixture.ok_json(
        &fixture.main,
        &["note", "t-one", "Backlog note", "--as", "geoyws", "--json"],
    );
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-BOUNDED");
    let expected_seqs = {
        let connection = Connection::open(&board_path).unwrap();
        connection
            .prepare("SELECT seq FROM events ORDER BY seq ASC")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };

    let bounded = WatchSession::start(
        &fixture,
        &fixture.main,
        &fixture.data,
        &["--cursor", "0", "--follow", "--limit", "2", "--json"],
    );
    let mut envelopes = Vec::new();
    let heartbeat = loop {
        let envelope = bounded.next_stdout_json(Duration::from_secs(5));
        match envelope["type"].as_str().unwrap() {
            "event" => envelopes.push(envelope),
            "heartbeat" => break envelope,
            other => panic!("unexpected watch envelope type {other}"),
        }
    };
    let seqs = envelopes
        .iter()
        .map(|envelope| envelope["payload"]["seq"].as_i64().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !seqs.is_empty(),
        "bounded follow never replayed the backlog"
    );
    assert_eq!(
        seqs, expected_seqs,
        "bounded follow replayed the wrong seqs"
    );
    let heartbeat_cursor = decode_watch_cursor(heartbeat["cursor"].as_str().unwrap());
    assert_eq!(
        heartbeat_cursor["seq"].as_i64().unwrap(),
        *expected_seqs.last().unwrap()
    );
    assert_eq!(heartbeat["scope"]["sourceKind"], json!("board"));
    assert_eq!(heartbeat["scope"]["selectorKind"], json!("board"));
    assert!(heartbeat["scope"]["selectorValue"].is_null());
    assert!(bounded.finish().is_empty());

    // The old thousand-row watch cap is gone: a thousand events is a real
    // page on a board with a year of history, so `watch` now shares the one
    // ceiling with every other `--limit` surface.
    let at_the_ceiling = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "1000000", "--json"],
    );
    assert!(
        at_the_ceiling.status.success(),
        "the ceiling itself was refused: {}",
        String::from_utf8_lossy(&at_the_ceiling.stderr)
    );

    let huge_limit = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "1000001", "--json"],
    );
    assert!(
        !huge_limit.status.success(),
        "an oversized watch limit was accepted"
    );
    let huge_limit_stderr = String::from_utf8_lossy(&huge_limit.stderr);
    assert!(
        huge_limit_stderr.contains(&over_ceiling_refusal("1000001")),
        "stderr did not carry the shared ceiling refusal: {huge_limit_stderr}"
    );

    let zero_follow = fixture.run(
        &fixture.main,
        &[
            "watch", "--cursor", "0", "--follow", "--limit", "0", "--json",
        ],
    );
    assert!(
        !zero_follow.status.success(),
        "--follow with --limit 0 stayed live instead of being rejected"
    );
}

#[test]
fn watch_rejects_malformed_unsupported_and_future_cursors() {
    let fixture = Fixture::new("watch-cursors");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "WATCH-CURSORS", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Cursor probe", "--id", "t-cursor", "--json"],
    );

    let preflight = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "1", "--json"],
    );
    assert!(preflight.status.success());
    let preflight_rows = ndjson_values(&preflight);
    assert!(!preflight_rows.is_empty());
    let cursor = preflight_rows.last().unwrap()["cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let cursor_json = decode_watch_cursor(&cursor);
    assert!(cursor_json.get("seq").is_some());
    assert!(cursor_json.get("lastSeq").is_none());
    let board_path = board_path_for_project(&fixture, &fixture.main, "WATCH-CURSORS");
    let head = Connection::open(&board_path)
        .unwrap()
        .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap();

    let malformed = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "not-a-token", "--json"],
    );
    assert!(
        !malformed.status.success(),
        "malformed cursor unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&malformed.stderr).contains("valid watch token"),
        "malformed cursor did not report a token error: {}",
        String::from_utf8_lossy(&malformed.stderr)
    );

    let mut unsupported_json = cursor_json.clone();
    unsupported_json["version"] = json!(2);
    let unsupported = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&unsupported_json).unwrap());
    let unsupported_run = fixture.run(
        &fixture.main,
        &["watch", "--cursor", &unsupported, "--json"],
    );
    assert!(
        !unsupported_run.status.success(),
        "unsupported cursor protocol version unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&unsupported_run.stderr).contains("unsupported protocol version"),
        "unsupported cursor version did not fail clearly: {}",
        String::from_utf8_lossy(&unsupported_run.stderr)
    );

    let mut future_json = cursor_json.clone();
    future_json["seq"] = json!(head + 1);
    let future = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&future_json).unwrap());
    let future_run = fixture.run(&fixture.main, &["watch", "--cursor", &future, "--json"]);
    assert!(
        !future_run.status.success(),
        "future cursor unexpectedly worked"
    );
    assert!(
        String::from_utf8_lossy(&future_run.stderr).contains("ahead of the current ledger head"),
        "future cursor did not report the current head check: {}",
        String::from_utf8_lossy(&future_run.stderr)
    );
}
