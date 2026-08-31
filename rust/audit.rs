use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const VERSION: &str = "1";
const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditReport {
    pub journal: String,
    pub healthy: bool,
    pub entries: i64,
    pub last_seq: i64,
    pub legacy_entries: i64,
    pub head: String,
    pub errors: Vec<String>,
}

pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

struct EntryFields<'a> {
    seq: i64,
    subject: Option<&'a str>,
    kind: &'a str,
    actor: Option<&'a str>,
    payload: &'a str,
    created_at: i64,
}

fn digest(domain: &str, previous: &str, entry: EntryFields<'_>) -> String {
    let mut hash = Sha256::new();
    for value in [
        b"kanban-audit".as_slice(),
        VERSION.as_bytes(),
        domain.as_bytes(),
        entry.seq.to_string().as_bytes(),
        previous.as_bytes(),
        entry.subject.unwrap_or("").as_bytes(),
        entry.kind.as_bytes(),
        entry.actor.unwrap_or("").as_bytes(),
        entry.payload.as_bytes(),
        entry.created_at.to_string().as_bytes(),
    ] {
        field(&mut hash, value);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn metadata_i64(connection: &Connection, table: &str, key: &str) -> Result<Option<i64>> {
    let sql = format!("SELECT value FROM {table} WHERE key=?");
    connection
        .query_row(&sql, [key], |row| row.get::<_, String>(0))
        .optional()?
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("invalid {key} metadata"))
        })
        .transpose()
}

pub fn initialize_board_chain(connection: &mut Connection) -> Result<()> {
    if metadata_i64(connection, "board_meta", "audit_chain_version")?.is_some() {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT seq,task_id,kind,actor,payload,created_at,archived FROM events ORDER BY seq",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)? != 0,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut previous = GENESIS.to_owned();
    for (seq, subject, kind, actor, payload, created_at, _archived) in &rows {
        let event_hash = digest(
            "board",
            &previous,
            EntryFields {
                seq: *seq,
                subject: subject.as_deref(),
                kind,
                actor: actor.as_deref(),
                payload,
                created_at: *created_at,
            },
        );
        transaction.execute(
            "UPDATE events SET prev_hash=?,event_hash=? WHERE seq=?",
            params![previous, event_hash, seq],
        )?;
        previous = event_hash;
    }
    for (key, value) in [
        ("audit_chain_version", VERSION.to_owned()),
        ("audit_chain_legacy_entries", rows.len().to_string()),
        (
            "audit_chain_initialized_at",
            crate::registry::now_ms().to_string(),
        ),
    ] {
        transaction.execute(
            "INSERT INTO board_meta(key,value) VALUES(?,?)",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn initialize_registry_chain(connection: &mut Connection) -> Result<()> {
    if metadata_i64(connection, "registry_meta", "audit_chain_version")?.is_some() {
        return Ok(());
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT seq,rule_id,kind,actor,payload,created_at FROM rule_events ORDER BY seq",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut previous = GENESIS.to_owned();
    for (seq, subject, kind, actor, payload, created_at) in &rows {
        let event_hash = digest(
            "registry",
            &previous,
            EntryFields {
                seq: *seq,
                subject: Some(subject),
                kind,
                actor: Some(actor),
                payload,
                created_at: *created_at,
            },
        );
        transaction.execute(
            "UPDATE rule_events SET prev_hash=?,event_hash=? WHERE seq=?",
            params![previous, event_hash, seq],
        )?;
        previous = event_hash;
    }
    for (key, value) in [
        ("audit_chain_version", VERSION.to_owned()),
        ("audit_chain_legacy_entries", rows.len().to_string()),
        (
            "audit_chain_initialized_at",
            crate::registry::now_ms().to_string(),
        ),
    ] {
        transaction.execute(
            "INSERT INTO registry_meta(key,value) VALUES(?,?)",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

pub fn append_board_event(
    connection: &Connection,
    task_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &str,
    created_at: i64,
) -> Result<()> {
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("actor is required for audited mutation");
    }
    let (last_seq, previous) = connection
        .query_row(
            "SELECT seq,event_hash FROM events ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .map(|(seq, hash)| {
            hash.map(|value| (seq, value))
                .context("audit chain is uninitialized at its current head")
        })
        .transpose()?
        .unwrap_or((0, GENESIS.to_owned()));
    let seq = last_seq + 1;
    let event_hash = digest(
        "board",
        &previous,
        EntryFields {
            seq,
            subject: task_id,
            kind,
            actor: Some(actor),
            payload,
            created_at,
        },
    );
    connection.execute(
        "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) VALUES(?,?,?,?,?,?,0,?,?)",
        params![seq, task_id, kind, actor, payload, created_at, previous, event_hash],
    )?;
    Ok(())
}

pub fn append_registry_event(
    connection: &Connection,
    rule_id: &str,
    kind: &str,
    actor: &str,
    payload: &str,
    created_at: i64,
) -> Result<()> {
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("actor is required for audited mutation");
    }
    let (last_seq, previous) = connection
        .query_row(
            "SELECT seq,event_hash FROM rule_events ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .map(|(seq, hash)| {
            hash.map(|value| (seq, value))
                .context("registry audit chain is uninitialized at its current head")
        })
        .transpose()?
        .unwrap_or((0, GENESIS.to_owned()));
    let seq = last_seq + 1;
    let event_hash = digest(
        "registry",
        &previous,
        EntryFields {
            seq,
            subject: Some(rule_id),
            kind,
            actor: Some(actor),
            payload,
            created_at,
        },
    );
    connection.execute(
        "INSERT INTO rule_events(seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash) VALUES(?,?,?,?,?,?,?,?)",
        params![seq, rule_id, kind, actor, payload, created_at, previous, event_hash],
    )?;
    Ok(())
}

pub fn verify_board(connection: &Connection) -> Result<AuditReport> {
    let legacy = metadata_i64(connection, "board_meta", "audit_chain_legacy_entries")?
        .context("board audit chain metadata is missing")?;
    let mut statement = connection.prepare(
        "SELECT seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash FROM events ORDER BY seq",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)? != 0,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut previous = GENESIS.to_owned();
    let mut errors = Vec::new();
    let mut prior_seq = None;
    for (seq, subject, kind, actor, payload, created_at, _archived, stored_prev, stored_hash) in
        &rows
    {
        if prior_seq.is_some_and(|prior| *seq <= prior) {
            errors.push(format!("sequence {seq} is not strictly increasing"));
        }
        if *seq > legacy && actor.as_deref().is_none_or(|value| value.trim().is_empty()) {
            errors.push(format!("sequence {seq} has no actor"));
        }
        if stored_prev.as_deref() != Some(previous.as_str()) {
            errors.push(format!("sequence {seq} previous hash does not match"));
        }
        let expected = digest(
            "board",
            &previous,
            EntryFields {
                seq: *seq,
                subject: subject.as_deref(),
                kind,
                actor: actor.as_deref(),
                payload,
                created_at: *created_at,
            },
        );
        if stored_hash.as_deref() != Some(expected.as_str()) {
            errors.push(format!("sequence {seq} event hash does not match"));
        }
        previous = stored_hash.clone().unwrap_or(expected);
        prior_seq = Some(*seq);
    }
    Ok(AuditReport {
        journal: "board".to_owned(),
        healthy: errors.is_empty(),
        entries: rows.len() as i64,
        last_seq: rows.last().map_or(0, |row| row.0),
        legacy_entries: legacy,
        head: previous,
        errors,
    })
}

pub fn verify_registry(connection: &Connection) -> Result<AuditReport> {
    let legacy = metadata_i64(connection, "registry_meta", "audit_chain_legacy_entries")?
        .context("registry audit chain metadata is missing")?;
    let mut statement = connection.prepare(
        "SELECT seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash FROM rule_events ORDER BY seq",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut previous = GENESIS.to_owned();
    let mut errors = Vec::new();
    for (seq, subject, kind, actor, payload, created_at, stored_prev, stored_hash) in &rows {
        if actor.trim().is_empty() {
            errors.push(format!("sequence {seq} has no actor"));
        }
        if stored_prev.as_deref() != Some(previous.as_str()) {
            errors.push(format!("sequence {seq} previous hash does not match"));
        }
        let expected = digest(
            "registry",
            &previous,
            EntryFields {
                seq: *seq,
                subject: Some(subject),
                kind,
                actor: Some(actor),
                payload,
                created_at: *created_at,
            },
        );
        if stored_hash.as_deref() != Some(expected.as_str()) {
            errors.push(format!("sequence {seq} event hash does not match"));
        }
        previous = stored_hash.clone().unwrap_or(expected);
    }
    Ok(AuditReport {
        journal: "registry".to_owned(),
        healthy: errors.is_empty(),
        entries: rows.len() as i64,
        last_seq: rows.last().map_or(0, |row| row.0),
        legacy_entries: legacy,
        head: previous,
        errors,
    })
}

pub fn hash_at(connection: &Connection, table: &str, seq: i64) -> Result<Option<String>> {
    if !matches!(table, "events" | "rule_events") {
        bail!("unknown audit table {table}");
    }
    connection
        .query_row(
            &format!("SELECT event_hash FROM {table} WHERE seq=?"),
            [seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_connection(label: &str) -> (TempRoot, Connection) {
        let root = std::env::temp_dir().join(format!(
            "kanban-audit-{label}-{}-{}",
            std::process::id(),
            crate::registry::now_ms()
        ));
        fs::create_dir_all(&root).expect("create temp audit dir");
        let connection = Connection::open(root.join("registry.db")).expect("open registry db");
        (TempRoot(root), connection)
    }

    fn create_registry_tables(connection: &Connection) {
        connection
            .execute_batch(
                r#"
                CREATE TABLE registry_meta (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                ) STRICT;
                CREATE TABLE rule_events (
                    seq INTEGER PRIMARY KEY NOT NULL,
                    rule_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    actor TEXT NOT NULL,
                    payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
                    created_at INTEGER NOT NULL,
                    prev_hash TEXT,
                    event_hash TEXT
                ) STRICT;
                "#,
            )
            .expect("create audit schema");
    }

    #[test]
    fn canonical_encoding_is_sensitive_to_boundaries_and_order() {
        let make = |seq, subject, kind| {
            digest(
                "board",
                GENESIS,
                EntryFields {
                    seq,
                    subject: Some(subject),
                    kind,
                    actor: Some("d"),
                    payload: "{}",
                    created_at: 1,
                },
            )
        };
        let a = make(1, "ab", "c");
        let b = make(1, "a", "bc");
        let c = make(2, "ab", "c");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn registry_chain_bootstraps_and_helper_guards_fail_closed() {
        let (_root, mut connection) = temp_connection("bootstrap");
        create_registry_tables(&connection);
        connection
            .execute(
                "INSERT INTO rule_events(seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash) \
                 VALUES(1,'rule-1','rule_created','codex','{}',10,NULL,NULL)",
                [],
            )
            .expect("insert legacy registry event");

        initialize_registry_chain(&mut connection).expect("bootstrap registry chain");
        initialize_registry_chain(&mut connection).expect("idempotent bootstrap");

        let legacy_entries: i64 = connection
            .query_row(
                "SELECT value FROM registry_meta WHERE key='audit_chain_legacy_entries'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("read legacy count")
            .parse()
            .expect("parse legacy count");
        assert_eq!(legacy_entries, 1);
        let version: String = connection
            .query_row(
                "SELECT value FROM registry_meta WHERE key='audit_chain_version'",
                [],
                |row| row.get(0),
            )
            .expect("read chain version");
        assert_eq!(version, "1");
        let (prev_hash, event_hash): (String, String) = connection
            .query_row(
                "SELECT prev_hash,event_hash FROM rule_events WHERE seq=1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("read bootstrapped event");
        assert_eq!(
            prev_hash,
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(event_hash.len(), 64);

        let report = verify_registry(&connection).expect("verify bootstrapped chain");
        assert!(report.healthy);
        assert_eq!(report.legacy_entries, 1);
        assert_eq!(report.entries, 1);
        assert_eq!(report.last_seq, 1);

        let blank_actor =
            append_registry_event(&connection, "rule-1", "rule_updated", " ", "{}", 11)
                .expect_err("blank actor must be rejected")
                .to_string();
        assert!(blank_actor.contains("actor is required"), "{blank_actor}");

        let (_guard_root, guard_connection) = temp_connection("uninitialized-head");
        create_registry_tables(&guard_connection);
        guard_connection
            .execute(
                "INSERT INTO rule_events(seq,rule_id,kind,actor,payload,created_at,prev_hash,event_hash) \
                 VALUES(1,'rule-1','rule_created','codex','{}',10,NULL,NULL)",
                [],
            )
            .expect("insert legacy row with uninitialized head");
        let head = append_registry_event(
            &guard_connection,
            "rule-1",
            "rule_updated",
            "codex",
            "{}",
            11,
        )
        .expect_err("uninitialized head must be rejected")
        .to_string();
        assert!(head.contains("uninitialized at its current head"), "{head}");
    }
}
