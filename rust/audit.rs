use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;

const VERSION: &str = "1";
pub(crate) const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

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
    Ok(render_hex(&hash.finalize()))
}

/// sha256 of bytes already in memory. Callers that must parse what they hashed
/// read the file once and hash the buffer, so the bytes they verified are the
/// bytes they go on to use.
pub fn bytes_sha256(bytes: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(bytes);
    render_hex(&hash.finalize())
}

fn render_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

pub(crate) struct EntryFields<'a> {
    pub(crate) seq: i64,
    pub(crate) subject: Option<&'a str>,
    pub(crate) kind: &'a str,
    pub(crate) actor: Option<&'a str>,
    pub(crate) payload: &'a str,
    pub(crate) created_at: i64,
}

pub(crate) fn digest(domain: &str, previous: &str, entry: EntryFields<'_>) -> String {
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

/// Returns whether the chain was initialized by this call; a board that
/// already carried one is left alone and reports `false`.
pub fn initialize_board_chain(connection: &mut Connection) -> Result<bool> {
    if metadata_i64(connection, "board_meta", "audit_chain_version")?.is_some() {
        return Ok(false);
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
    Ok(true)
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
    // Every audited board mutation passes through here on the transaction's
    // own connection, after the rows it changed and after the search_*
    // triggers have added their documents with a NULL embedding. Embedding
    // here, in the same transaction, is what makes "every write embeds" true
    // by construction rather than per method: a write path that forgot to
    // call it cannot exist, because a write path that forgot to audit
    // cannot exist either. On a healthy board the scan finds nothing.
    crate::search::embed_missing(connection)?;
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

/// Compute the next `(seq, prev_hash, event_hash)` for one of the policy
/// journals (ADR-038 clause 4), chaining it into ADR-029's registry hash chain
/// exactly the way [`append_registry_event`] chains `rule_events`.
///
/// The caller performs the `INSERT` so each journal keeps its own column list.
/// `payload` is the canonical JSON of the whole row; the digest covers it, so
/// truncation, reordering, or substitution in any policy journal fails
/// [`verify_registry`].
pub(crate) fn next_chained(
    connection: &Connection,
    table: &str,
    domain: &str,
    subject: Option<&str>,
    kind: &str,
    payload: &str,
    created_at: i64,
) -> Result<(i64, String, String)> {
    let (last_seq, previous) = connection
        .query_row(
            &format!("SELECT seq,event_hash FROM {table} ORDER BY seq DESC LIMIT 1"),
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?
        .map(|(seq, hash)| {
            hash.map(|value| (seq, value))
                .context("policy journal chain is uninitialized at its current head")
        })
        .transpose()?
        .unwrap_or((0, GENESIS.to_owned()));
    let seq = last_seq + 1;
    let event_hash = digest(
        domain,
        &previous,
        EntryFields {
            seq,
            subject,
            kind,
            actor: None,
            payload,
            created_at,
        },
    );
    Ok((seq, previous, event_hash))
}

/// Walk one policy journal chain and return `(entries, head, errors)`.
///
/// `subject_sql`, `kind_sql`, `payload_sql`, and `created_sql` name the
/// columns supplying the digest's subject, kind, payload, and created-at
/// fields; they are hard-coded by the callers, never user input. The digest is
/// recomputed from the stored columns, so any tampered column — including the
/// payload — changes the recomputed hash and fails verification.
pub(crate) fn verify_journal(
    connection: &Connection,
    domain: &str,
    table: &str,
    subject_sql: &str,
    kind_sql: &str,
    payload_sql: &str,
    created_sql: &str,
) -> Result<(i64, String, Vec<String>)> {
    let sql = format!(
        "SELECT seq,{subject_sql},{kind_sql},{payload_sql},{created_sql},prev_hash,event_hash \
         FROM {table} ORDER BY seq"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut previous = GENESIS.to_owned();
    let mut errors = Vec::new();
    let mut prior_seq = None;
    for (seq, subject, kind, payload, created_at, stored_prev, stored_hash) in &rows {
        if prior_seq.is_some_and(|prior| *seq <= prior) {
            errors.push(format!("{table} sequence {seq} is not strictly increasing"));
        }
        if stored_prev.as_deref() != Some(previous.as_str()) {
            errors.push(format!(
                "{table} sequence {seq} previous hash does not match"
            ));
        }
        let expected = digest(
            domain,
            &previous,
            EntryFields {
                seq: *seq,
                subject: subject.as_deref(),
                kind,
                actor: None,
                payload,
                created_at: *created_at,
            },
        );
        if stored_hash.as_deref() != Some(expected.as_str()) {
            errors.push(format!("{table} sequence {seq} event hash does not match"));
        }
        previous = stored_hash.clone().unwrap_or(expected);
        prior_seq = Some(*seq);
    }
    Ok((rows.len() as i64, previous, errors))
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
    // The three policy journals (ADR-038 clause 4) join the same registry hash
    // chain. Their chains are independent but verified together, so `audit
    // verify` and `doctor` fail on truncation, reordering, or substitution in
    // any one of them, not only in `rule_events`.
    if sqlite_table_present(connection, "policy_events") {
        for (domain, table, subject_sql, kind_sql, payload_sql, created_sql) in [
            (
                "policy_events",
                "policy_events",
                "event_id",
                "kind",
                "payload",
                "occurred_at",
            ),
            (
                "policy_epochs",
                "policy_epochs",
                "CAST(epoch AS TEXT)",
                "'policy_epoch'",
                "payload",
                "occurred_at",
            ),
            (
                "access_audit",
                "access_audit",
                "event_id",
                "'access_audit'",
                "payload",
                "occurred_at",
            ),
        ] {
            let (_, _, journal_errors) = verify_journal(
                connection,
                domain,
                table,
                subject_sql,
                kind_sql,
                payload_sql,
                created_sql,
            )?;
            errors.extend(journal_errors);
        }
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

/// Whether `table` exists, without importing the db module's helper here.
fn sqlite_table_present(connection: &Connection, table: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .unwrap_or(false)
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

    #[test]
    fn bytes_and_file_sha256_agree_on_the_same_content() {
        let root = std::env::temp_dir().join(format!(
            "kanban-audit-sha256-{}-{}",
            std::process::id(),
            crate::registry::now_ms()
        ));
        fs::create_dir_all(&root).expect("create temp sha256 dir");
        let guard = TempRoot(root.clone());

        // Spans the 64 KiB read buffer `file_sha256` streams with, so the two
        // agree across a chunk boundary and not only on short inputs.
        for content in [
            Vec::new(),
            b"{\"kind\":\"protocol-schema\"}".to_vec(),
            vec![0xab_u8; 64 * 1024 + 7],
        ] {
            let path = guard.0.join("payload.bin");
            fs::write(&path, &content).expect("write payload");
            assert_eq!(bytes_sha256(&content), file_sha256(&path).unwrap());
        }

        assert_eq!(
            bytes_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
