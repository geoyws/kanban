//! Linked delivery evidence (slice LINKED, row `t-9eff9257`) at the
//! compiled-process boundary.
//!
//! Spec: `docs/specs/linked.md` acceptance A5-A6 (`LINKED-15`..`LINKED-21`).
//! Every test spawns the real `kanban` binary against real SQLite files in a
//! temp `KANBAN_DATA_DIR`, following `tests/linked_e2e.rs`, and builds real
//! local git repositories in temp dirs for the read-only verification — no
//! network anywhere. The implementation under test reuses the shared records
//! (`rust/linked.rs` endpoints, bindings, audit chain) rather than a
//! parallel model.
//!
//! Out of scope here and owned elsewhere: companions and claim scope
//! (`t-0dcbb1a9`, `tests/linked_e2e.rs`) and CLI/MCP agreement, recovery,
//! and the sibling-slice complement (`t-db6937ba`, `LINKED-22`, `LINKED-24`,
//! `LINKED-25`).

use serde_json::Value;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const OP: &str = "op";
const WORKER: &str = "w";
const LANE: &str = "driver";
const SESSION: &str = "s1";
const HOST: &str = "hax";
const OBSERVED: &str = "1790900000000";
const IMPL_REPO: &str = "acme/billing";
const CONSUMER_REPO: &str = "acme/shop";

struct Estate {
    root: PathBuf,
    data: PathBuf,
}

impl Estate {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-evidence-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        fs::create_dir_all(&data).unwrap();
        Self { root, data }
    }

    fn workspace(&self, name: &str) -> PathBuf {
        let work = self.root.join("work").join(name);
        fs::create_dir_all(&work).unwrap();
        work
    }

    /// `init --name NAME` in its own workspace; returns the board UUID.
    fn board(&self, name: &str) -> String {
        let work = self.workspace(name);
        self.ok_json(&work, &["init", "--name", name, "--json"]);
        let listing = self.ok_json(&self.root, &["workspace", "list", "--all", "--json"]);
        let path = listing
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == name)
            .unwrap_or_else(|| panic!("board {name} is not listed: {listing}"))["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();
        Path::new(&path)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    fn run<S: AsRef<OsStr>>(&self, cwd: &Path, args: &[S]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanban"))
            .current_dir(cwd)
            .args(args)
            .env("KANBAN_DATA_DIR", &self.data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .env_remove("KANBAN_WORKER_CREDENTIAL")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap()
    }

    fn ok_json<S: AsRef<OsStr> + std::fmt::Debug>(&self, cwd: &Path, args: &[S]) -> Value {
        let output = self.run(cwd, args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON: {error}\nstdout: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    /// A refused command: nonzero, and its one error line — the `--json`
    /// `error` field when the command answers in JSON, else the raw output.
    fn refused<S: AsRef<OsStr> + std::fmt::Debug>(&self, cwd: &Path, args: &[S]) -> String {
        let output = self.run(cwd, args);
        assert!(
            !output.status.success(),
            "command should have been refused: {args:?}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        serde_json::from_str::<Value>(&stdout)
            .ok()
            .and_then(|value| value["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{stdout}{stderr}"))
    }

    fn add(&self, board: &str, id: &str) {
        let work = self.workspace(board);
        let output = self.run(
            &work,
            &["task", "add", id, "--id", id, "--as", OP, "--json"],
        );
        assert!(
            output.status.success(),
            "add {id} on {board} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Bind the worker to a fresh selected set holding every `(board, id)`
    /// pair given, so `contrib record` may evidence those tasks.
    fn bind(&self, set: &str, members: &[(&str, &str)]) {
        self.ok_json(
            &self.root,
            &["scope", "create", "--set", set, "--as", OP, "--json"],
        );
        let mut revision: i64 = 1;
        for (board, id) in members {
            self.ok_json(
                &self.root,
                &[
                    "scope",
                    "add",
                    "--set",
                    set,
                    "--board",
                    board,
                    "--id",
                    id,
                    "--expect-revision",
                    &revision.to_string(),
                    "--as",
                    OP,
                    "--json",
                ],
            );
            revision += 1;
        }
        self.ok_json(
            &self.root,
            &[
                "scope",
                "bind",
                "--set",
                set,
                "--actor",
                WORKER,
                "--lane",
                LANE,
                "--session",
                SESSION,
                "--expect-revision",
                &revision.to_string(),
                "--as",
                OP,
                "--json",
            ],
        );
    }

    fn declare(&self, board: &str, id: &str, deliverable: &str, kind: &str, repo: &str) -> Value {
        let mut args = vec![
            "contrib",
            "declare",
            "--board",
            board,
            "--id",
            id,
            "--deliverable",
            deliverable,
            "--kind",
            kind,
            "--as",
            OP,
            "--json",
        ];
        if !repo.is_empty() {
            args.push("--repo");
            args.push(repo);
        }
        self.ok_json(&self.root, &args)
    }

    /// The shared `contrib record` context flags for the bound worker.
    fn record(&self, extra: &[String]) -> Value {
        let mut args = vec![
            "contrib".to_owned(),
            "record".to_owned(),
            "--actor".to_owned(),
            WORKER.to_owned(),
            "--lane".to_owned(),
            LANE.to_owned(),
            "--session".to_owned(),
            SESSION.to_owned(),
            "--host".to_owned(),
            HOST.to_owned(),
            "--worktree".to_owned(),
            "/tmp/wt".to_owned(),
            "--branch".to_owned(),
            "wt/t-9eff9257-contrib".to_owned(),
            "--observed-at".to_owned(),
            OBSERVED.to_owned(),
            "--as".to_owned(),
            WORKER.to_owned(),
            "--json".to_owned(),
        ];
        args.extend(extra.iter().cloned());
        self.ok_json(&self.root, &args)
    }

    fn refuse_record(&self, extra: &[String]) -> String {
        let mut args = vec![
            "contrib".to_owned(),
            "record".to_owned(),
            "--actor".to_owned(),
            WORKER.to_owned(),
            "--lane".to_owned(),
            LANE.to_owned(),
            "--session".to_owned(),
            SESSION.to_owned(),
            "--host".to_owned(),
            HOST.to_owned(),
            "--worktree".to_owned(),
            "/tmp/wt".to_owned(),
            "--branch".to_owned(),
            "wt/t-9eff9257-contrib".to_owned(),
            "--observed-at".to_owned(),
            OBSERVED.to_owned(),
            "--as".to_owned(),
            WORKER.to_owned(),
            "--json".to_owned(),
        ];
        args.extend(extra.iter().cloned());
        self.refused(&self.root, &args)
    }

    fn show(&self, board: &str, id: &str) -> Value {
        self.ok_json(
            &self.root,
            &["contrib", "show", "--board", board, "--id", id, "--json"],
        )
    }

    fn status(&self, board: &str, id: &str) -> Value {
        self.ok_json(
            &self.root,
            &["contrib", "status", "--board", board, "--id", id, "--json"],
        )
    }

    fn registry_count(&self, sql: &str) -> i64 {
        let path = self.data.join("registry.db");
        let connection =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        connection.query_row(sql, [], |row| row.get(0)).unwrap()
    }
}

impl Drop for Estate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// ---------------------------------------------------------------------------
// Real local git fixtures: no network, temp dirs only.
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}{}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn fresh_repo(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-b", "main", "-q"]);
    git(&dir, &["config", "user.email", "evidence@example.invalid"]);
    git(&dir, &["config", "user.name", "Evidence"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    dir
}

fn commit_file(dir: &Path, name: &str, content: &str, message: &str) -> String {
    fs::write(dir.join(name), content).unwrap();
    git(&dir, &["add", name]);
    git(&dir, &["commit", "-qm", message]);
    git(dir, &["rev-parse", "HEAD"])
}

/// The implementation repo: starting HEAD `H`, implementation commits `C1`
/// and `C2` on a branch, squashed into `I` on main — plus a true merge `M`
/// absorbing `C3` for the merge-mapping half of LINKED-17.
struct ImplRepo {
    dir: PathBuf,
    h: String,
    c1: String,
    c2: String,
    squash: String,
    c3: String,
    merge: String,
}

fn impl_repo(parent: &Path) -> ImplRepo {
    let dir = fresh_repo(parent, "impl");
    let h = commit_file(&dir, "base.txt", "v1\n", "starting HEAD");
    git(&dir, &["checkout", "-qb", "sq"]);
    let c1 = commit_file(&dir, "f1.txt", "one\n", "C1");
    let c2 = commit_file(&dir, "f2.txt", "two\n", "C2");
    git(&dir, &["checkout", "-q", "main"]);
    git(&dir, &["merge", "--squash", "sq"]);
    git(&dir, &["commit", "-qm", "squash C1+C2"]);
    let squash = git(&dir, &["rev-parse", "HEAD"]);
    git(&dir, &["checkout", "-qb", "mg", "main"]);
    let c3 = commit_file(&dir, "f3.txt", "three\n", "C3");
    git(&dir, &["checkout", "-q", "main"]);
    git(&dir, &["merge", "--no-ff", "-qm", "merge C3", "mg"]);
    let merge = git(&dir, &["rev-parse", "HEAD"]);
    ImplRepo {
        dir,
        h,
        c1,
        c2,
        squash,
        c3,
        merge,
    }
}

/// Nested submodules: consumer `K` reaches the leaf through `vendor/mid`
/// then `libs/leaf`. Returns the consumer dir, the mid dir, `K`, a moved-on
/// consumer commit `K2`, the committed leaf pin, and a newer leaf commit
/// that makes a stale pin.
struct ConsumerRepo {
    dir: PathBuf,
    mid_dir: PathBuf,
    k: String,
    k2: String,
    leaf_pin: String,
    leaf_moved: String,
}

fn consumer_repo(parent: &Path) -> ConsumerRepo {
    let leaf = fresh_repo(parent, "leaf");
    let leaf_pin = commit_file(&leaf, "lib.txt", "leaf v1\n", "leaf");
    let mid = fresh_repo(parent, "mid");
    git(
        &mid,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            leaf.to_str().unwrap(),
            "libs/leaf",
        ],
    );
    git(&mid, &["commit", "-qm", "take leaf"]);
    // The leaf moves on after mid pinned it: the committed pin is now stale
    // against the leaf tip, which is exactly the A6 stale-pin shape.
    let leaf_moved = commit_file(&leaf, "lib.txt", "leaf v2\n", "leaf v2");
    let consumer = fresh_repo(parent, "consumer");
    git(
        &consumer,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            mid.to_str().unwrap(),
            "vendor/mid",
        ],
    );
    fs::write(consumer.join("README.md"), "consumer\n").unwrap();
    git(&consumer, &["add", "README.md"]);
    git(&consumer, &["commit", "-qm", "take mid"]);
    let k = git(&consumer, &["rev-parse", "HEAD"]);
    git(
        &consumer,
        &["commit", "-q", "--allow-empty", "-m", "move on"],
    );
    let k2 = git(&consumer, &["rev-parse", "HEAD"]);
    ConsumerRepo {
        dir: consumer,
        mid_dir: mid,
        k,
        k2,
        leaf_pin,
        leaf_moved,
    }
}

fn gitlink(repo: &Path, commit: &str, subpath: &str) -> String {
    let line = git(repo, &["ls-tree", commit, "--", subpath]);
    let mut parts = line.split_whitespace();
    assert_eq!(
        parts.next(),
        Some("160000"),
        "expected a gitlink at {subpath}"
    );
    assert_eq!(parts.next(), Some("commit"));
    parts.next().unwrap().to_owned()
}

/// Every field LINKED-15 requires on a code receipt.
const IDENTITY_FIELDS: [&str; 9] = [
    "repo", "commit", "role", "actor", "lane", "session", "host", "worktree", "branch",
];

fn identity_fields(receipt: &Value) -> Vec<(&'static str, String)> {
    IDENTITY_FIELDS
        .iter()
        .map(|field| {
            let value = receipt[*field].as_str().unwrap_or("").to_owned();
            (*field, value)
        })
        .collect()
}

/// A5 — Evidence records the four roles, the squash mapping, and the nested
/// path (`LINKED-15`, `LINKED-16`, `LINKED-17`, `LINKED-18`).
#[test]
fn evidence_records_four_roles_squash_mapping_and_nested_path() {
    let estate = Estate::new("a5");
    let unum = estate.board("unum");
    let acies = estate.board("acies");
    estate.add("unum", "t-u1");
    estate.add("acies", "t-a1");
    estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    estate.bind("joint-5", &[(unum.as_str(), "t-u1")]);
    let repos = estate.root.join("repos");
    fs::create_dir_all(&repos).unwrap();
    let r = impl_repo(&repos);
    let c = consumer_repo(&repos);
    let impl_path = r.dir.to_str().unwrap().to_owned();
    let mid_path = c.mid_dir.to_str().unwrap().to_owned();
    let consumer_path = c.dir.to_str().unwrap().to_owned();

    estate.declare(&unum, "t-u1", "ship", "code", IMPL_REPO);

    // A contribution missing any identity field is refused naming it.
    let missing_host = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
        "--host".to_owned(),
        "".to_owned(),
    ]);
    assert!(
        missing_host.contains("--host"),
        "missing field is named: {missing_host}"
    );

    let baseline = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
        "--evidence-ref".to_owned(),
        "note-1".to_owned(),
    ]);
    assert_eq!(baseline["verification"], "verified");
    for (field, value) in identity_fields(&baseline) {
        assert!(!value.is_empty(), "receipt carries {field}");
    }
    assert_eq!(baseline["commit"], Value::from(r.h.clone()));
    assert_eq!(baseline["observedAt"], Value::from(1790900000000i64));

    // A record with no role, two roles, or an unknown role is refused naming
    // the four roles and quoting what was declared.
    let no_role = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.c1.clone(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert!(
        no_role.contains("baseline") && no_role.contains("candidate"),
        "{no_role}"
    );
    // Two roles arrive whole (repeatable) so the role refusal fires, not a
    // parser refusal.
    let two_roles = estate.run(
        &estate.root,
        &[
            "contrib",
            "record",
            "--board",
            &unum,
            "--id",
            "t-u1",
            "--deliverable",
            "ship",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--host",
            HOST,
            "--worktree",
            "/tmp/wt",
            "--branch",
            "wt/x",
            "--observed-at",
            OBSERVED,
            "--repo",
            IMPL_REPO,
            "--commit",
            &r.c1,
            "--role",
            "baseline",
            "--role",
            "implementation",
            "--repo-path",
            &impl_path,
            "--as",
            WORKER,
            "--json",
        ],
    );
    assert!(!two_roles.status.success());
    // `--json` escapes quotes, so match the role names with either quoting.
    let two_roles = String::from_utf8_lossy(&two_roles.stdout).replace("\\\"", "\"")
        + &String::from_utf8_lossy(&two_roles.stderr);
    assert!(
        two_roles.contains("\"baseline\"") && two_roles.contains("\"implementation\""),
        "both declared roles are quoted: {two_roles}"
    );

    let c1 = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.c1.clone(),
        "--role".to_owned(),
        "implementation".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    let c2 = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.c2.clone(),
        "--role".to_owned(),
        "implementation".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert_eq!(c1["verification"], "verified");
    assert_eq!(c2["verification"], "verified");

    // An integration whose mapping leaves a recorded implementation uncovered
    // records verified but satisfies nothing yet.
    let partial = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.squash.clone(),
        "--role".to_owned(),
        "integration".to_owned(),
        "--source".to_owned(),
        r.c1.clone(),
        "--mapping".to_owned(),
        "squash".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert_eq!(partial["verification"], "verified");
    let status = estate.status(&unum, "t-u1");
    assert_eq!(status["evidenced"], Value::Bool(false));
    let gaps: Vec<String> = status["deliverables"][0]["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|gap| gap.as_str().unwrap().to_owned())
        .collect();
    assert!(
        gaps.iter()
            .any(|gap| gap.contains(&r.c2) && gap.contains("unmapped")),
        "the uncovered implementation is named: {gaps:?}"
    );

    // The correction keeps the commit, fixes the mapping, and names the
    // corrected receipt; the original bytes stay stored.
    let before = estate.show(&unum, "t-u1");
    let full = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.squash.clone(),
        "--role".to_owned(),
        "integration".to_owned(),
        "--source".to_owned(),
        r.c1.clone(),
        "--source".to_owned(),
        r.c2.clone(),
        "--mapping".to_owned(),
        "squash".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
        "--corrects".to_owned(),
        partial["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert_eq!(full["corrects"], partial["contributionID"]);
    assert_ne!(full["contributionID"], partial["contributionID"]);
    let after = estate.show(&unum, "t-u1");
    let find = |shown: &Value, id: &str| {
        shown["deliverables"][0]["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["contributionID"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(
        find(&before, partial["contributionID"].as_str().unwrap()),
        find(&after, partial["contributionID"].as_str().unwrap()),
        "the corrected receipt is byte-identical"
    );

    // A merge mapping additionally proves absorption: the squash commit is no
    // ancestor of its sources, so claiming merge for it is refused.
    let merge_claim = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.c3.clone(),
        "--role".to_owned(),
        "integration".to_owned(),
        "--source".to_owned(),
        r.c1.clone(),
        "--mapping".to_owned(),
        "merge".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
        "--corrects".to_owned(),
        full["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert!(merge_claim.contains("does not absorb"), "{merge_claim}");
    // The true merge absorbs: it accumulates beside the squash with its own
    // mapping, and coverage is the union across both (LINKED-17).
    let merged = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.merge.clone(),
        "--role".to_owned(),
        "integration".to_owned(),
        "--source".to_owned(),
        r.c3.clone(),
        "--mapping".to_owned(),
        "merge".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert_eq!(merged["verification"], "verified");

    // The candidate binds the exact consumer commit, the full nested path,
    // the consumed dependency commit, and the verification run — verifying
    // committed gitlinks only.
    let mid_pin = gitlink(&c.dir, &c.k, "vendor/mid");
    let leaf_pin = git(&c.mid_dir, &["ls-tree", &mid_pin, "--", "libs/leaf"]);
    let leaf_pin = leaf_pin.split_whitespace().nth(2).unwrap().to_owned();
    assert_eq!(leaf_pin, c.leaf_pin, "the fixture pins what mid committed");
    let candidate = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "vendor/mid".to_owned(),
        "--via".to_owned(),
        "libs/leaf".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-1".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--hop-path".to_owned(),
        mid_path.clone(),
    ]);
    assert_eq!(candidate["verification"], "verified");
    assert_eq!(
        candidate["consumerPath"],
        serde_json::json!(["vendor/mid", "libs/leaf"])
    );

    // Every role verified, every implementation mapped: the deliverable reads
    // as evidenced.
    let status = estate.status(&unum, "t-u1");
    assert_eq!(
        status["evidenced"],
        Value::Bool(true),
        "exact evidence: {status}"
    );

    // A query for one role never returns another: every stored commit sits
    // under exactly the role it was recorded with.
    let shown = estate.show(&unum, "t-u1");
    let records = shown["deliverables"][0]["records"].as_array().unwrap();
    let roles_of = |commit: &str| {
        records
            .iter()
            .filter(|record| record["commit"] == commit)
            .map(|record| record["role"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(roles_of(&r.h), vec!["baseline".to_owned()]);
    assert_eq!(roles_of(&r.c1), vec!["implementation".to_owned()]);
    assert!(roles_of(&c.k).iter().all(|role| role == "candidate"));

    // Retrying the identical receipt answers the stored row: still one row
    // for it.
    let replay = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.c1.clone(),
        "--role".to_owned(),
        "implementation".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert_eq!(replay["contributionID"], c1["contributionID"]);
    assert_eq!(
        estate.registry_count("SELECT COUNT(*) FROM linked_contributions"),
        7,
        "baseline, C1, C2, partial and corrected integrations, merge, candidate"
    );
}

/// A6 — Near-misses prove nothing, partial evidence closes nothing
/// (`LINKED-19`, `LINKED-20`, `LINKED-21`).
#[test]
fn near_misses_prove_nothing_and_partial_evidence_closes_nothing() {
    let estate = Estate::new("a6");
    let unum = estate.board("unum");
    let acies = estate.board("acies");
    estate.add("unum", "t-u1");
    estate.add("acies", "t-a1");
    estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    estate.bind(
        "joint-6",
        &[(unum.as_str(), "t-u1"), (acies.as_str(), "t-a1")],
    );
    let repos = estate.root.join("repos");
    fs::create_dir_all(&repos).unwrap();
    let r = impl_repo(&repos);
    let c = consumer_repo(&repos);
    let impl_path = r.dir.to_str().unwrap().to_owned();
    let consumer_path = c.dir.to_str().unwrap().to_owned();
    let mid_path = c.mid_dir.to_str().unwrap().to_owned();

    estate.declare(&unum, "t-u1", "ship", "code", IMPL_REPO);
    estate.declare(&unum, "t-u1", "review", "non-code", "");
    estate.declare(&acies, "t-a1", "ship", "code", IMPL_REPO);

    let base = |commit: &str| {
        vec![
            "--board".to_owned(),
            unum.clone(),
            "--id".to_owned(),
            "t-u1".to_owned(),
            "--deliverable".to_owned(),
            "ship".to_owned(),
            "--repo".to_owned(),
            IMPL_REPO.to_owned(),
            "--commit".to_owned(),
            commit.to_owned(),
            "--role".to_owned(),
            "baseline".to_owned(),
            "--repo-path".to_owned(),
            impl_path.clone(),
        ]
    };
    let count = || estate.registry_count("SELECT COUNT(*) FROM linked_contributions");

    // A moving `latest` pointer is not proof.
    let moving = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        "latest".to_owned(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert!(moving.contains("moving pointer"), "{moving}");

    // A shortened hash is refused, never expanded.
    let short_args = base(&r.h[..12]);
    let short = estate.refuse_record(&short_args);
    assert!(short.contains("shortened"), "{short}");

    // A right-length wrong hash names the exact mismatch.
    let wrong = estate.refuse_record(&base(&"f".repeat(40)));
    assert!(wrong.contains("is not an object"), "{wrong}");
    assert!(
        wrong.contains(&"f".repeat(40)),
        "the presented hash is quoted: {wrong}"
    );

    // The right hash at the wrong repo path names the path.
    let elsewhere = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
    ]);
    assert!(
        elsewhere.contains(&consumer_path),
        "expected versus presented path: {elsewhere}"
    );

    // No readable repository records unverified — never success, never a
    // refusal — and unverified evidence keeps the deliverable open.
    let blind = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        "/nonexistent/repo".to_owned(),
    ]);
    assert_eq!(blind["verification"], "unverified");
    let status = estate.status(&unum, "t-u1");
    assert_eq!(status["evidenced"], Value::Bool(false));
    assert!(
        status.to_string().contains("satisfies nothing"),
        "unverified evidence reads as partial: {status}"
    );
    // Correct the blind baseline with a verified one so the feature can
    // proceed below.
    let baseline = estate.record(
        &[
            base(&r.h)[..].to_vec(),
            vec![
                "--corrects".to_owned(),
                blind["contributionID"].as_str().unwrap().to_owned(),
            ],
        ]
        .concat(),
    );
    assert_eq!(baseline["verification"], "verified");

    // Record the real baseline and implementations for the close below.
    for commit in [&r.c1, &r.c2] {
        estate.record(&[
            "--board".to_owned(),
            unum.clone(),
            "--id".to_owned(),
            "t-u1".to_owned(),
            "--deliverable".to_owned(),
            "ship".to_owned(),
            "--repo".to_owned(),
            IMPL_REPO.to_owned(),
            "--commit".to_owned(),
            commit.clone(),
            "--role".to_owned(),
            "implementation".to_owned(),
            "--repo-path".to_owned(),
            impl_path.clone(),
        ]);
    }
    estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.squash.clone(),
        "--role".to_owned(),
        "integration".to_owned(),
        "--source".to_owned(),
        r.c1.clone(),
        "--source".to_owned(),
        r.c2.clone(),
        "--mapping".to_owned(),
        "squash".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    let candidate = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "vendor/mid".to_owned(),
        "--via".to_owned(),
        "libs/leaf".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-1".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--hop-path".to_owned(),
        mid_path.clone(),
    ]);

    // A stale pin: the leaf moved on, the committed gitlink did not.
    let stale = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "vendor/mid".to_owned(),
        "--via".to_owned(),
        "libs/leaf".to_owned(),
        "--consumes".to_owned(),
        c.leaf_moved.clone(),
        "--verify-run".to_owned(),
        "run-9".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--hop-path".to_owned(),
        mid_path.clone(),
        "--corrects".to_owned(),
        candidate["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert!(
        stale.contains("stale") && stale.contains(&c.leaf_moved),
        "{stale}"
    );

    // A changed candidate without `--corrects` proves nothing, even when
    // everything else about the record matches.
    let changed = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k2.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "vendor/mid".to_owned(),
        "--via".to_owned(),
        "libs/leaf".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-2".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--hop-path".to_owned(),
        mid_path.clone(),
    ]);
    assert!(
        changed.contains("--corrects") && changed.contains(&c.k),
        "{changed}"
    );

    // A path with a hop missing, and a non-submodule entry, satisfy nothing.
    let no_path = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-3".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--corrects".to_owned(),
        candidate["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert!(no_path.contains("dependency path"), "{no_path}");
    let flat = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "README.md".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-4".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--corrects".to_owned(),
        candidate["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert!(flat.contains("not a committed gitlink"), "{flat}");
    // Non-submodule dependency kinds are explicitly refused, not probed.
    let packaged = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        CONSUMER_REPO.to_owned(),
        "--commit".to_owned(),
        c.k.clone(),
        "--role".to_owned(),
        "candidate".to_owned(),
        "--via".to_owned(),
        "vendor/mid".to_owned(),
        "--dep-kind".to_owned(),
        "package".to_owned(),
        "--consumes".to_owned(),
        c.leaf_pin.clone(),
        "--verify-run".to_owned(),
        "run-5".to_owned(),
        "--repo-path".to_owned(),
        consumer_path.clone(),
        "--corrects".to_owned(),
        candidate["contributionID"].as_str().unwrap().to_owned(),
    ]);
    assert!(packaged.contains("explicitly refused"), "{packaged}");

    // Cross-kind records are refused naming the mismatch, both directions.
    let code_for_review = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "review".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
        "--note".to_owned(),
        "looks good".to_owned(),
    ]);
    assert!(
        code_for_review.contains("never satisfies"),
        "{code_for_review}"
    );
    let review_for_code = estate.refuse_record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--kind".to_owned(),
        "non-code".to_owned(),
        "--note".to_owned(),
        "looks good".to_owned(),
    ]);
    assert!(
        review_for_code.contains("never satisfies"),
        "{review_for_code}"
    );

    // A lane records only its own triple, and only inside its selected set.
    let other_lane = estate.run(
        &estate.root,
        &[
            "contrib",
            "record",
            "--board",
            &unum,
            "--id",
            "t-u1",
            "--deliverable",
            "ship",
            "--actor",
            WORKER,
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--host",
            HOST,
            "--worktree",
            "/tmp/wt",
            "--branch",
            "wt/x",
            "--observed-at",
            OBSERVED,
            "--repo",
            IMPL_REPO,
            "--commit",
            &r.h,
            "--role",
            "baseline",
            "--repo-path",
            &impl_path,
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(
        !other_lane.status.success(),
        "another lane's triple cannot record"
    );
    let unbound = estate.run(
        &estate.root,
        &[
            "contrib",
            "record",
            "--board",
            &unum,
            "--id",
            "t-u1",
            "--deliverable",
            "ship",
            "--actor",
            "stranger",
            "--lane",
            LANE,
            "--session",
            SESSION,
            "--host",
            HOST,
            "--worktree",
            "/tmp/wt",
            "--branch",
            "wt/x",
            "--observed-at",
            OBSERVED,
            "--repo",
            IMPL_REPO,
            "--commit",
            &r.h,
            "--role",
            "baseline",
            "--repo-path",
            &impl_path,
            "--as",
            "stranger",
            "--json",
        ],
    );
    assert!(
        !unbound.status.success(),
        "outside the selected set, no record"
    );
    let unbound = String::from_utf8_lossy(&unbound.stdout).to_string()
        + &String::from_utf8_lossy(&unbound.stderr);
    assert!(unbound.contains("selected set"), "{unbound}");

    // The close with the review still open is refused naming it — and the
    // Acies side, which has no evidence yet, is named too. Neither board
    // reads as jointly delivered.
    let early = estate.refused(
        &estate.root,
        &[
            "contrib",
            "close",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(early.contains("not closable"), "{early}");
    assert!(
        early.contains("review"),
        "the open deliverable is named: {early}"
    );
    assert!(
        early.contains("t-a1"),
        "the unevidenced side is named: {early}"
    );
    assert_eq!(
        estate.status(&unum, "t-u1")["evidenced"],
        Value::Bool(false)
    );
    assert_eq!(
        estate.status(&acies, "t-a1")["evidenced"],
        Value::Bool(false)
    );
    assert_eq!(
        estate.registry_count("SELECT COUNT(*) FROM linked_closures"),
        0,
        "a refused close writes no closure"
    );

    // Evidence the Acies side fully, then the close still names only the
    // review: partial evidence closes nothing.
    for (role, commit, extra) in [
        ("baseline", r.h.clone(), Vec::new()),
        ("implementation", r.c1.clone(), Vec::new()),
        (
            "integration",
            r.squash.clone(),
            vec![
                "--source".to_owned(),
                r.c1.clone(),
                "--mapping".to_owned(),
                "squash".to_owned(),
            ],
        ),
        (
            "candidate",
            c.k.clone(),
            vec![
                "--via".to_owned(),
                "vendor/mid".to_owned(),
                "--via".to_owned(),
                "libs/leaf".to_owned(),
                "--consumes".to_owned(),
                c.leaf_pin.clone(),
                "--verify-run".to_owned(),
                "run-7".to_owned(),
                "--hop-path".to_owned(),
                mid_path.clone(),
            ],
        ),
    ] {
        let mut args = vec![
            "--board".to_owned(),
            acies.clone(),
            "--id".to_owned(),
            "t-a1".to_owned(),
            "--deliverable".to_owned(),
            "ship".to_owned(),
            "--repo".to_owned(),
            if role == "candidate" {
                CONSUMER_REPO
            } else {
                IMPL_REPO
            }
            .to_owned(),
            "--commit".to_owned(),
            commit,
            "--role".to_owned(),
            role.to_owned(),
            "--repo-path".to_owned(),
            if role == "candidate" {
                consumer_path.clone()
            } else {
                impl_path.clone()
            },
        ];
        args.extend(extra);
        // The Acies side needs C2 mapped too: record it first for the
        // integration below.
        if role == "integration" {
            estate.record(&[
                "--board".to_owned(),
                acies.clone(),
                "--id".to_owned(),
                "t-a1".to_owned(),
                "--deliverable".to_owned(),
                "ship".to_owned(),
                "--repo".to_owned(),
                IMPL_REPO.to_owned(),
                "--commit".to_owned(),
                r.c2.clone(),
                "--role".to_owned(),
                "implementation".to_owned(),
                "--repo-path".to_owned(),
                impl_path.clone(),
            ]);
            args.push("--source".to_owned());
            args.push(r.c2.clone());
        }
        // Implementation C2 on the Acies side is recorded above; the loop's
        // own implementation row is C1.
        estate.record(&args);
    }
    let only_review = estate.refused(
        &estate.root,
        &[
            "contrib",
            "close",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert!(only_review.contains("review"), "{only_review}");
    assert!(
        !only_review.contains("t-a1"),
        "the evidenced side is not named: {only_review}"
    );

    // The review decision lands as non-code evidence; then the close lands,
    // and retrying it answers the stored closure.
    let review = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "review".to_owned(),
        "--kind".to_owned(),
        "non-code".to_owned(),
        "--note".to_owned(),
        "accepted on review-9".to_owned(),
        "--evidence-ref".to_owned(),
        "review-9".to_owned(),
    ]);
    assert_eq!(review["kind"], "non-code");
    let closed = estate.ok_json(
        &estate.root,
        &[
            "contrib",
            "close",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_eq!(closed["closedBy"], Value::from(OP));
    let replay = estate.ok_json(
        &estate.root,
        &[
            "contrib",
            "close",
            "--a-board",
            &acies,
            "--a-id",
            "t-a1",
            "--b-board",
            &unum,
            "--b-id",
            "t-u1",
            "--as",
            OP,
            "--json",
        ],
    );
    assert_eq!(
        replay, closed,
        "the identical close retried answers the stored closure"
    );
    assert_eq!(
        estate.registry_count("SELECT COUNT(*) FROM linked_closures"),
        1,
        "one closure, never doubled"
    );
    let receipts = count();
    let _ = estate.status(&unum, "t-u1");
    let _ = estate.show(&acies, "t-a1");
    assert_eq!(count(), receipts, "reads write no receipts");
}

/// Per-side history from both boards, without marking the untouched peer as
/// worked: recording for one endpoint leaves the peer empty, and reads write
/// nothing anywhere.
#[test]
fn per_side_history_leaves_the_untouched_peer_unmarked() {
    let estate = Estate::new("sides");
    let unum = estate.board("unum");
    let acies = estate.board("acies");
    estate.add("unum", "t-u1");
    estate.add("acies", "t-a1");
    estate.ok_json(
        &estate.root,
        &[
            "link",
            "add",
            "--a-board",
            &unum,
            "--a-id",
            "t-u1",
            "--b-board",
            &acies,
            "--b-id",
            "t-a1",
            "--as",
            OP,
            "--json",
        ],
    );
    estate.bind("joint-s", &[(unum.as_str(), "t-u1")]);
    let repos = estate.root.join("repos");
    fs::create_dir_all(&repos).unwrap();
    let r = impl_repo(&repos);
    let impl_path = r.dir.to_str().unwrap().to_owned();
    estate.declare(&unum, "t-u1", "ship", "code", IMPL_REPO);

    let before = (
        estate.registry_count("SELECT COUNT(*) FROM linked_contributions"),
        estate.registry_count("SELECT COUNT(*) FROM linked_deliverables"),
    );
    let baseline = estate.record(&[
        "--board".to_owned(),
        unum.clone(),
        "--id".to_owned(),
        "t-u1".to_owned(),
        "--deliverable".to_owned(),
        "ship".to_owned(),
        "--repo".to_owned(),
        IMPL_REPO.to_owned(),
        "--commit".to_owned(),
        r.h.clone(),
        "--role".to_owned(),
        "baseline".to_owned(),
        "--repo-path".to_owned(),
        impl_path.clone(),
    ]);
    assert_eq!(baseline["boardID"], Value::from(unum.clone()));

    let unum_history = estate.show(&unum, "t-u1");
    assert_eq!(unum_history["deliverables"].as_array().unwrap().len(), 1);
    assert_eq!(
        unum_history["deliverables"][0]["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let acies_history = estate.show(&acies, "t-a1");
    assert_eq!(
        acies_history["deliverables"].as_array().unwrap().len(),
        0,
        "the untouched peer shows no deliverables and no records"
    );
    assert_eq!(
        estate.status(&acies, "t-a1")["evidenced"],
        Value::Bool(false)
    );
    // Reads write nothing: show and status leave every evidence table as it
    // stood before the records above plus the one baseline.
    let _ = estate.status(&unum, "t-u1");
    let after = (
        estate.registry_count("SELECT COUNT(*) FROM linked_contributions"),
        estate.registry_count("SELECT COUNT(*) FROM linked_deliverables"),
    );
    assert_eq!(
        after,
        (before.0 + 1, before.1),
        "one receipt landed; reads wrote nothing"
    );
}
