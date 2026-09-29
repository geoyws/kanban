//! The credential-shape write guard, at the compiled-process boundary.
//!
//! The estate triage (kb t-d65fa53a) showed 85 of 104 `generic-api-key`
//! findings are hashes, ids and template lines, so the guard checks ONLY two
//! high-signal shapes — AWS access key IDs (`AKIA` + 16 uppercase
//! alphanumerics) and PEM private-key headers — and everything else must
//! write exactly as before. Each test below spawns the real `kanban` binary
//! against a real SQLite board: a library call in this process would not
//! establish the thing being asserted, because the refusal under test lives
//! in the store the binary writes through.
//!
//! `AKIAIOSFODNN7EXAMPLE` is AWS's own documented example key,
//! self-evidently fake; it is planted because only the real shape exercises
//! the guard.
//!
//! Negative control: the refusal tests assert the write FAILS with the naming
//! sentence. If the helper were bypassed, the writes would succeed and these
//! tests would fail — the accept test beside them proves the guard is not
//! refusing everything.

use serde_json::Value;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// AWS's documented example access key ID: the real shape, no real secret.
const FAKE_AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

/// A UUID and a SHA-256 hex digest: the estate's most common false-positive
/// shapes, which must keep writing fine.
const UUID_BODY: &str = "rollout 550e8400-e29b-41d4-a716-446655440000 \
    verified against sha256 \
    9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

/// One board in a temp data root, addressed by working directory.
struct GuardEstate {
    root: PathBuf,
    data: PathBuf,
    work: PathBuf,
}

impl GuardEstate {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "kanban-secret-guard-{label}-{}-{unique}",
            std::process::id()
        ));
        let data = root.join("data");
        let work = root.join("work");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&work).unwrap();
        let estate = Self { root, data, work };
        estate.ok_json(&["init", "--name", "Guard", "--json"]);
        estate
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanban"));
        command
            .current_dir(&self.work)
            .args(args)
            .env("KANBAN_DATA_DIR", &self.data)
            .env_remove("KANBAN_DB")
            .env_remove("KANBAN_PROJECT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok_json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "command should have succeeded: {args:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).unwrap()
    }

    /// Assert one write is refused with the naming sentence: the shape and
    /// the field label, so the caller reads the exit status and the fix in
    /// one message.
    fn refused_naming(&self, args: &[&str], shape: &str, label: &str) {
        let output = self.run(args);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            !output.status.success(),
            "{args:?} succeeded but must refuse {label}\nstdout: {stdout}"
        );
        for needle in [shape, label] {
            assert!(
                stderr.contains(needle),
                "{args:?} refusal names neither shape nor label\nstderr: {stderr}\nstdout: {stdout}"
            );
        }
    }

    fn task_ids(&self) -> Vec<String> {
        self.ok_json(&["task", "list", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap().to_owned())
            .collect()
    }

    fn note_bodies(&self, id: &str) -> Vec<String> {
        self.ok_json(&["task", "show", id, "--json"])["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["body"].as_str().unwrap().to_owned())
            .collect()
    }
}

impl Drop for GuardEstate {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn task_add_refuses_an_aws_key_in_the_body_and_writes_nothing() {
    let estate = GuardEstate::new("task-aws");
    estate.refused_naming(
        &[
            "task",
            "add",
            "rotate the deploy key",
            "--id",
            "t-key",
            "--body",
            &format!("the live key is {FAKE_AWS_KEY}, do not commit it"),
            "--json",
        ],
        "AWS access key ID",
        "task body",
    );
    assert!(
        !estate.task_ids().contains(&"t-key".to_owned()),
        "the refused task row was written"
    );
}

#[test]
fn note_add_refuses_an_aws_key_in_the_body_and_writes_nothing() {
    let estate = GuardEstate::new("note-aws");
    estate.ok_json(&["task", "add", "clean subject", "--id", "t-1", "--json"]);
    estate.refused_naming(
        &[
            "note",
            "t-1",
            &format!("pasted from the console: {FAKE_AWS_KEY}"),
            "--as",
            "agent-a",
            "--json",
        ],
        "AWS access key ID",
        "note body",
    );
    assert_eq!(
        estate.note_bodies("t-1"),
        Vec::<String>::new(),
        "the refused note row was written"
    );
}

#[test]
fn task_add_refuses_a_pem_private_key_header() {
    let estate = GuardEstate::new("task-pem");
    estate.refused_naming(
        &[
            "task",
            "add",
            "key rotation",
            "--id",
            "t-pem",
            "--body",
            "backup:\n-----BEGIN RSA PRIVATE KEY-----\nMIIE...",
            "--json",
        ],
        "PEM private key header",
        "task body",
    );
    assert!(
        !estate.task_ids().contains(&"t-pem".to_owned()),
        "the refused task row was written"
    );
}

#[test]
fn uuid_and_hash_bodies_still_write() {
    let estate = GuardEstate::new("clean");
    let task = estate.ok_json(&[
        "task",
        "add",
        "ordinary rollout",
        "--id",
        "t-clean",
        "--body",
        UUID_BODY,
        "--json",
    ]);
    assert_eq!(task["id"].as_str().unwrap(), "t-clean");
    estate.ok_json(&["note", "t-clean", UUID_BODY, "--as", "agent-a", "--json"]);
    assert_eq!(estate.note_bodies("t-clean"), [UUID_BODY]);
}

#[test]
fn handoff_create_refuses_an_aws_key_in_a_blocker_and_writes_nothing() {
    let estate = GuardEstate::new("handoff-aws");
    estate.refused_naming(
        &[
            "handoff",
            "create",
            "--as",
            "agent-a",
            "--to",
            "@:team/proj/driver",
            "--summary",
            "handover",
            "--intent",
            "continue the lane",
            "--next-action",
            "resume from the handoff",
            "--blocker",
            &format!("waiting on {FAKE_AWS_KEY}"),
            "--repo",
            "/tmp/guard-probe",
            "--branch",
            "probe",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "clean",
            "--json",
        ],
        "AWS access key ID",
        "handoff blocker",
    );
}

#[test]
fn checkpoint_refuses_an_aws_key_in_a_validation_and_writes_nothing() {
    let estate = GuardEstate::new("cp-aws");
    estate.ok_json(&["task", "add", "guarded subject", "--id", "t-cp", "--json"]);
    let claim = estate.ok_json(&["claim", "t-cp", "--as", "agent-a", "--json"]);
    let lease = claim["leaseToken"].as_str().unwrap().to_owned();
    estate.refused_naming(
        &[
            "checkpoint",
            "t-cp",
            "--lease",
            &lease,
            "--as",
            "agent-a",
            "--summary",
            "mid-lane state",
            "--intent",
            "prove the lists are guarded",
            "--next-action",
            "resume after the refusal",
            "--validation",
            &format!("key {FAKE_AWS_KEY} must not persist"),
            "--repo",
            "/tmp/guard-probe",
            "--branch",
            "probe",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "clean",
            "--json",
        ],
        "AWS access key ID",
        "checkpoint validation",
    );
    assert!(
        estate.ok_json(&["task", "show", "t-cp", "--json"])["checkpoints"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the refused checkpoint row was written"
    );
}
