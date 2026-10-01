//! Delegated workers: registry records narrowed from a managed principal
//! (docs/specs/identity.md IDENT-01 .. IDENT-18; ADR-059).
//!
//! A worker is a policy record, not a principal. It is registered by a
//! managed principal (or by one of that principal's workers), carries its own
//! grant list and optional task root, and proves itself with a credential the
//! registry stores only as a SHA-256 digest. Its effective authority is
//! recomputed on every call from the principal's live grants down the
//! worker's ancestry, so it can only ever narrow (IDENT-04, IDENT-06).
//!
//! Registration and retirement are policy events: chained, epoch-advancing,
//! replayable, and checked against the caller's minted context exactly as
//! every other policy mutation is (IDENT-03). The credential digest is the
//! one worker column that is not on the event and not in the replayed
//! projection, because no event may hold a credential or its digest.

use crate::audit;
use crate::policy::{
    self, Capability, PolicyActor, PolicyDelta, PolicyEffect, PolicyEpochPayload,
    PolicyEventPayload, ScopeTuple, satisfies,
};
use crate::registry::Registry;
use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;

/// The one channel a process presents a worker credential through (IDENT-05).
pub const CREDENTIAL_ENV: &str = "KANBAN_WORKER_CREDENTIAL";

const CREDENTIAL_PREFIX: &str = "kwc_";

/// The deepest delegation chain a credential may resolve through. A real
/// chain is a handful deep; the bound only stops a corrupt parent link from
/// looping.
const MAX_DEPTH: usize = 64;

/// `worker list`'s default and ceiling (IDENT-15).
pub const LIST_DEFAULT: i64 = 50;
pub const LIST_CEILING: i64 = 500;

/// The one generic refusal, byte-identical to the policy and row guards'.
const DENIED: &str = "denied or not found";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerState {
    Active,
    Retired,
}

impl WorkerState {
    fn as_str(self) -> &'static str {
        match self {
            WorkerState::Active => "active",
            WorkerState::Retired => "retired",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "active" => Ok(WorkerState::Active),
            "retired" => Ok(WorkerState::Retired),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                format!("unknown worker state {other}").into(),
            )),
        }
    }
}

/// One `(capability, scope tuple)` pair of a worker's own grant list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerGrant {
    pub capability: Capability,
    pub scope: Vec<String>,
}

impl WorkerGrant {
    /// Parse one `--grant CAPABILITY=ATOM[,ATOM...]` value (IDENT-04).
    pub fn parse(value: &str) -> Result<Self> {
        let (capability, atoms) = value
            .split_once('=')
            .with_context(|| format!("--grant must be CAPABILITY=ATOM[,ATOM...], got {value:?}"))?;
        let capability = Capability::from_str(capability).with_context(|| {
            format!(
                "--grant capability must be one of {}, got {capability:?}",
                policy::CAPABILITIES.join(", ")
            )
        })?;
        let atoms: Vec<String> = atoms.split(',').map(str::to_owned).collect();
        let tuple = ScopeTuple::from_atoms(&atoms)
            .with_context(|| format!("--grant {value:?} does not name one scope tuple"))?;
        Ok(Self {
            capability,
            scope: tuple.to_atoms(),
        })
    }

    fn tuple(&self) -> ScopeTuple {
        ScopeTuple::from_atoms(&self.scope).expect("a stored worker grant is a valid tuple")
    }
}

/// The task a worker is confined to, with its board (IDENT-04).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRoot {
    pub board_id: String,
    pub task_id: String,
}

/// One worker record. Only `state` and `retired_at` ever change (IDENT-02).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRow {
    #[serde(rename = "workerId")]
    pub id: String,
    pub principal_id: String,
    pub parent_worker_id: Option<String>,
    pub run_id: String,
    pub harness_agent_id: Option<String>,
    pub lane_actor: String,
    pub task_root: Option<TaskRoot>,
    pub grants: Vec<WorkerGrant>,
    pub state: WorkerState,
    pub registered_at: i64,
    pub registered_epoch: i64,
    pub retired_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerRetirement {
    pub id: String,
    pub retired_at: i64,
}

/// A credential resolved to its worker, with the authority it holds now.
#[derive(Debug, Clone)]
pub struct ResolvedWorker {
    pub row: WorkerRow,
    /// The effective authority E (IDENT-06), computed for this call.
    pub authority: HashMap<ScopeTuple, Capability>,
}

/// What `worker register` asks for, after the CLI has parsed it.
pub struct RegisterWorker {
    pub run_id: String,
    pub harness_agent_id: Option<String>,
    pub lane_actor: String,
    pub grants: Vec<WorkerGrant>,
    pub task_root: TaskRootChoice,
}

/// The task root a registration asked for, as the board read resolved it.
pub enum TaskRootChoice {
    /// No `--task`: inherit the registrant's root, or none.
    Inherit,
    /// `--task` named a task the registrant may confine a worker to.
    Named(TaskRoot),
    /// `--task` was refused; the sentence is the refusal.
    Refused(String),
}

/// The registration receipt: the record and the credential, printed once.
#[derive(Debug, Serialize)]
pub struct Registration {
    #[serde(flatten)]
    pub worker: WorkerRow,
    pub credential: String,
}

// ---------------------------------------------------------------------------
// Validation.
// ---------------------------------------------------------------------------

/// `--run` and `--harness-agent`: 1-128 ASCII characters, starting with a
/// letter or digit, then letters, digits, dot, underscore, colon or hyphen.
pub fn validate_label(flag: &str, value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let ok = (1..=128).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'));
    if !ok {
        bail!(
            "--{flag} must be 1 to 128 ASCII characters, starting with a letter or digit, then \
             letters, digits, '.', '_', ':' or '-'"
        );
    }
    Ok(())
}

/// `--lane-actor`: `@:<team>/<board>/<lane>`, three non-empty segments, at
/// most 128 characters.
pub fn validate_lane_actor(value: &str) -> Result<()> {
    let ok = value.len() <= 128
        && value.strip_prefix("@:").is_some_and(|rest| {
            let segments: Vec<&str> = rest.split('/').collect();
            segments.len() == 3 && segments.iter().all(|segment| !segment.is_empty())
        });
    if !ok {
        bail!(
            "--lane-actor must be @:<team>/<board>/<lane> with three non-empty segments and at \
             most 128 characters"
        );
    }
    Ok(())
}

/// Whether a string has a credential's shape: `kwc_` and 64 lowercase hex.
fn credential_shaped(value: &str) -> bool {
    value.strip_prefix(CREDENTIAL_PREFIX).is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Mint a credential from 32 bytes of the kernel's randomness.
fn mint_credential() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .context("read 32 random bytes for a worker credential")?;
    let mut credential = String::with_capacity(CREDENTIAL_PREFIX.len() + 64);
    credential.push_str(CREDENTIAL_PREFIX);
    for byte in bytes {
        credential.push_str(&format!("{byte:02x}"));
    }
    Ok(credential)
}

// ---------------------------------------------------------------------------
// Authority.
// ---------------------------------------------------------------------------

/// E(t) for each tuple of a worker's own grant list: the greatest capability
/// at most the granted one that the parent's effective authority satisfies
/// at t; absent when none does (IDENT-06).
pub fn narrow(
    parent: &HashMap<ScopeTuple, Capability>,
    grants: &[WorkerGrant],
) -> HashMap<ScopeTuple, Capability> {
    let mut out: HashMap<ScopeTuple, Capability> = HashMap::new();
    for grant in grants {
        let tuple = grant.tuple();
        let Some(capability) = [Capability::Admin, Capability::Write, Capability::Read]
            .into_iter()
            .find(|capability| {
                *capability <= grant.capability && satisfies(parent, &tuple, *capability)
            })
        else {
            continue;
        };
        out.entry(tuple)
            .and_modify(|existing| {
                if capability > *existing {
                    *existing = capability;
                }
            })
            .or_insert(capability);
    }
    out
}

// ---------------------------------------------------------------------------
// Rows.
// ---------------------------------------------------------------------------

const WORKER_COLUMNS: &str = "id,principal_id,parent_worker_id,run_id,harness_agent_id,lane_actor,\
     task_root,grants,state,registered_at,registered_epoch,retired_at";

fn worker_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkerRow> {
    let json_error = |error: serde_json::Error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    };
    let task_root: Option<String> = row.get("task_root")?;
    let grants: String = row.get("grants")?;
    let state: String = row.get("state")?;
    Ok(WorkerRow {
        id: row.get("id")?,
        principal_id: row.get("principal_id")?,
        parent_worker_id: row.get("parent_worker_id")?,
        run_id: row.get("run_id")?,
        harness_agent_id: row.get("harness_agent_id")?,
        lane_actor: row.get("lane_actor")?,
        task_root: task_root
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(json_error)?,
        grants: serde_json::from_str(&grants).map_err(json_error)?,
        state: WorkerState::parse(&state)?,
        registered_at: row.get("registered_at")?,
        registered_epoch: row.get("registered_epoch")?,
        retired_at: row.get("retired_at")?,
    })
}

/// Insert one worker row: [`policy::apply_effect`]'s half for a
/// `worker_registered` event, so the live write and replay share it.
pub(crate) fn insert_worker_on(connection: &Connection, worker: &WorkerRow) -> Result<()> {
    connection.execute(
        &format!("INSERT INTO workers({WORKER_COLUMNS}) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)"),
        params![
            worker.id,
            worker.principal_id,
            worker.parent_worker_id,
            worker.run_id,
            worker.harness_agent_id,
            worker.lane_actor,
            worker
                .task_root
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            serde_json::to_string(&worker.grants)?,
            worker.state.as_str(),
            worker.registered_at,
            worker.registered_epoch,
            worker.retired_at,
        ],
    )?;
    Ok(())
}

/// Every worker, ordered by id: the projection the state hash and replay
/// read.
pub(crate) fn all_workers_on(connection: &Connection) -> Result<Vec<WorkerRow>> {
    let mut statement =
        connection.prepare(&format!("SELECT {WORKER_COLUMNS} FROM workers ORDER BY id"))?;
    let rows = statement
        .query_map([], worker_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn worker_on(connection: &Connection, id: &str) -> Result<Option<WorkerRow>> {
    connection
        .query_row(
            &format!("SELECT {WORKER_COLUMNS} FROM workers WHERE id=?"),
            [id],
            worker_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn workers_of_principal_on(connection: &Connection, principal_id: &str) -> Result<Vec<WorkerRow>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {WORKER_COLUMNS} FROM workers WHERE principal_id=? \
         ORDER BY registered_at DESC, id DESC"
    ))?;
    let rows = statement
        .query_map([principal_id], worker_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Resolve a credential to an active worker of `principal_id` whose every
/// ancestor is active and of the same principal, with its effective authority
/// computed now. `None` for every way that fails: one generic answer
/// (IDENT-05).
pub(crate) fn resolve_on(
    connection: &Connection,
    principal_id: &str,
    credential: &str,
) -> Result<Option<ResolvedWorker>> {
    if !credential_shaped(credential) {
        return Ok(None);
    }
    let digest = audit::bytes_sha256(credential.as_bytes());
    let Some(worker_id) = connection
        .query_row(
            "SELECT worker_id FROM worker_credentials WHERE credential_sha256=?",
            [&digest],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    else {
        return Ok(None);
    };
    let Some(worker) = worker_on(connection, &worker_id)? else {
        return Ok(None);
    };
    // The worker first, then each ancestor, nearest first.
    let mut chain = vec![worker];
    while let Some(parent_id) = chain.last().and_then(|row| row.parent_worker_id.clone()) {
        if chain.len() > MAX_DEPTH {
            return Ok(None);
        }
        let Some(parent) = worker_on(connection, &parent_id)? else {
            return Ok(None);
        };
        chain.push(parent);
    }
    if chain
        .iter()
        .any(|row| row.state != WorkerState::Active || row.principal_id != principal_id)
    {
        return Ok(None);
    }
    let mut authority = policy::principal_authority_on(connection, principal_id)?;
    for row in chain.iter().rev() {
        authority = narrow(&authority, &row.grants);
    }
    let row = chain.swap_remove(0);
    Ok(Some(ResolvedWorker { row, authority }))
}

/// The ids of `root` and every worker below it, among `workers`.
fn subtree(workers: &[WorkerRow], root: &str) -> HashSet<String> {
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    for worker in workers {
        if let Some(parent) = &worker.parent_worker_id {
            children
                .entry(parent.as_str())
                .or_default()
                .push(&worker.id);
        }
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut queue = vec![root];
    while let Some(id) = queue.pop() {
        if seen.insert(id.to_owned()) {
            queue.extend(children.get(id).into_iter().flatten().copied());
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// The registry surface.
// ---------------------------------------------------------------------------

/// Which workers a `worker list` wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateFilter {
    Active,
    Retired,
    All,
}

impl StateFilter {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(StateFilter::Active),
            "retired" => Ok(StateFilter::Retired),
            "all" => Ok(StateFilter::All),
            other => bail!("--state must be active, retired or all, got {other}"),
        }
    }

    fn keeps(self, state: WorkerState) -> bool {
        match self {
            StateFilter::Active => state == WorkerState::Active,
            StateFilter::Retired => state == WorkerState::Retired,
            StateFilter::All => true,
        }
    }
}

impl Registry {
    /// Resolve a presented credential for `principal_id` (IDENT-05).
    pub fn resolve_worker(
        &self,
        principal_id: &str,
        credential: &str,
    ) -> Result<Option<ResolvedWorker>> {
        resolve_on(&self.connection, principal_id, credential)
    }

    /// `worker register` (IDENT-02, IDENT-03, IDENT-04, IDENT-05).
    ///
    /// The registrant is `actor`'s principal, or — when `presented` is a
    /// credential — that principal's worker. Every requested pair must be
    /// satisfied by the registrant's effective authority; a refusal appends a
    /// denied access-audit row and leaves the epoch where it was.
    pub fn register_worker(
        &mut self,
        actor: &PolicyActor,
        presented: Option<&str>,
        input: RegisterWorker,
    ) -> Result<Registration> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Registration> = (|| {
            const OPERATION: &str = "worker register";
            let epoch = policy::policy_epoch_on(&tx)?;
            policy::require_context(&tx, actor, OPERATION)?;
            let principal_id = actor.principal_id.clone().ok_or_else(|| {
                policy::deny(&tx, actor, OPERATION, "principal", "no_principal", epoch)
            })?;
            let (authority, parent, parent_root) = match presented {
                Some(credential) => {
                    let registrant =
                        resolve_on(&tx, &principal_id, credential)?.ok_or_else(|| {
                            policy::deny(&tx, actor, OPERATION, "worker", "unresolved", epoch)
                        })?;
                    (
                        registrant.authority,
                        Some(registrant.row.id),
                        registrant.row.task_root,
                    )
                }
                None => (
                    policy::principal_authority_on(&tx, &principal_id)?,
                    None,
                    None,
                ),
            };
            for grant in &input.grants {
                let tuple = grant.tuple();
                if tuple == ScopeTuple::Registry {
                    return Err(policy::deny(
                        &tx,
                        actor,
                        OPERATION,
                        "scope",
                        "registry_tuple",
                        epoch,
                    ));
                }
                if !satisfies(&authority, &tuple, grant.capability) {
                    return Err(policy::deny(
                        &tx,
                        actor,
                        OPERATION,
                        "nonEscalation",
                        "missing_capability",
                        epoch,
                    ));
                }
            }
            let task_root = match input.task_root {
                TaskRootChoice::Inherit => parent_root,
                TaskRootChoice::Named(root) => Some(root),
                TaskRootChoice::Refused(sentence) => {
                    // The audit row, as every refused policy mutation leaves;
                    // the caller is told the specific sentence.
                    let _ = policy::deny(&tx, actor, OPERATION, "taskRoot", "outside", epoch);
                    return Err(anyhow!(sentence));
                }
            };

            let seq = policy::journal_next_seq(&tx, "policy_events")?;
            let event_id = policy::policy_event_id(seq);
            let occurred_at = crate::registry::now_ms();
            let worker = WorkerRow {
                id: format!("w-{}", uuid::Uuid::new_v4()),
                principal_id: principal_id.clone(),
                parent_worker_id: parent,
                run_id: input.run_id,
                harness_agent_id: input.harness_agent_id,
                lane_actor: input.lane_actor,
                task_root,
                grants: input.grants,
                state: WorkerState::Active,
                registered_at: occurred_at,
                registered_epoch: epoch + 1,
                retired_at: None,
            };
            let effect = PolicyEffect {
                workers: vec![worker.clone()],
                ..Default::default()
            };
            policy::apply_effect(&tx, &effect)?;
            let credential = mint_credential()?;
            tx.execute(
                "INSERT INTO worker_credentials(worker_id,credential_sha256) VALUES(?,?)",
                params![worker.id, audit::bytes_sha256(credential.as_bytes())],
            )?;
            let scopes: Vec<ScopeTuple> = worker.grants.iter().map(WorkerGrant::tuple).collect();
            append_event(
                &tx,
                actor,
                epoch,
                &event_id,
                occurred_at,
                "worker_registered",
                OPERATION,
                &principal_id,
                &scopes,
                effect,
            )?;
            Ok(Registration { worker, credential })
        })();
        finish(tx, outcome)
    }

    /// `worker retire` (IDENT-07): by the worker's principal without a
    /// credential, by any ancestor worker, or by the worker itself.
    pub fn retire_worker(
        &mut self,
        actor: &PolicyActor,
        presented: Option<&str>,
        worker_id: &str,
    ) -> Result<WorkerRow> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<WorkerRow> = (|| {
            const OPERATION: &str = "worker retire";
            let epoch = policy::policy_epoch_on(&tx)?;
            policy::require_context(&tx, actor, OPERATION)?;
            let principal_id = actor.principal_id.clone().ok_or_else(|| {
                policy::deny(&tx, actor, OPERATION, "principal", "no_principal", epoch)
            })?;
            let target = worker_on(&tx, worker_id)?
                .filter(|row| row.principal_id == principal_id)
                .ok_or_else(|| policy::deny(&tx, actor, OPERATION, "worker", "not_found", epoch))?;
            if let Some(credential) = presented {
                let caller = resolve_on(&tx, &principal_id, credential)?.ok_or_else(|| {
                    policy::deny(&tx, actor, OPERATION, "worker", "unresolved", epoch)
                })?;
                let mut lineage = vec![target.id.clone()];
                let mut cursor = target.parent_worker_id.clone();
                while let Some(id) = cursor {
                    if lineage.len() > MAX_DEPTH {
                        break;
                    }
                    cursor = worker_on(&tx, &id)?.and_then(|row| row.parent_worker_id);
                    lineage.push(id);
                }
                if !lineage.contains(&caller.row.id) {
                    return Err(policy::deny(
                        &tx,
                        actor,
                        OPERATION,
                        "worker",
                        "not_ancestor",
                        epoch,
                    ));
                }
            }
            if target.state == WorkerState::Retired {
                let _ = policy::deny(&tx, actor, OPERATION, "state", "already_retired", epoch);
                bail!("worker {worker_id} is already retired");
            }
            let seq = policy::journal_next_seq(&tx, "policy_events")?;
            let event_id = policy::policy_event_id(seq);
            let occurred_at = crate::registry::now_ms();
            let effect = PolicyEffect {
                retired_workers: vec![WorkerRetirement {
                    id: target.id.clone(),
                    retired_at: occurred_at,
                }],
                ..Default::default()
            };
            policy::apply_effect(&tx, &effect)?;
            append_event(
                &tx,
                actor,
                epoch,
                &event_id,
                occurred_at,
                "worker_retired",
                OPERATION,
                &principal_id,
                &[],
                effect,
            )?;
            worker_on(&tx, worker_id)?.context("the retired worker is gone")
        })();
        finish(tx, outcome)
    }

    /// The workers a caller may see (IDENT-15): the principal's, or — for a
    /// worker call — the worker and its descendants. Newest first, at most
    /// `limit`, and whether more were left out.
    pub fn visible_workers(
        &self,
        principal_id: &str,
        caller: Option<&ResolvedWorker>,
        filter: StateFilter,
        limit: i64,
    ) -> Result<(Vec<WorkerRow>, bool)> {
        let all = workers_of_principal_on(&self.connection, principal_id)?;
        let visible = caller.map(|caller| subtree(&all, &caller.row.id));
        let mut rows: Vec<WorkerRow> = all
            .into_iter()
            .filter(|row| visible.as_ref().is_none_or(|set| set.contains(&row.id)))
            .filter(|row| filter.keeps(row.state))
            .collect();
        let truncated = rows.len() > limit as usize;
        rows.truncate(limit as usize);
        Ok((rows, truncated))
    }

    /// One worker under the same visibility, or the generic refusal.
    pub fn visible_worker(
        &self,
        principal_id: &str,
        caller: Option<&ResolvedWorker>,
        worker_id: &str,
    ) -> Result<WorkerRow> {
        let (rows, _) = self.visible_workers(principal_id, caller, StateFilter::All, i64::MAX)?;
        rows.into_iter()
            .find(|row| row.id == worker_id)
            .ok_or_else(|| anyhow!(DENIED))
    }
}

/// Append the allowed audit row, the chained policy event and the next epoch
/// for one worker mutation.
#[allow(clippy::too_many_arguments)]
fn append_event(
    tx: &Connection,
    actor: &PolicyActor,
    epoch: i64,
    event_id: &str,
    occurred_at: i64,
    kind: &str,
    operation: &str,
    principal_id: &str,
    scopes: &[ScopeTuple],
    effect: PolicyEffect,
) -> Result<()> {
    let resulting_state_hash = policy::compute_state_hash(tx)?;
    let audit = policy::allowed_audit(
        actor,
        operation,
        epoch + 1,
        &policy::enforcement_state_on(tx)?,
        None,
        scopes,
        &[],
    );
    policy::append_access_audit(tx, &audit)?;
    let event = PolicyEventPayload {
        id: event_id.to_owned(),
        kind: kind.to_owned(),
        occurred_at,
        before_epoch: epoch,
        after_epoch: epoch + 1,
        access_audit_event_id: Some(audit.id.clone()),
        actor_principal_id: actor.principal_id.clone(),
        actor_username: actor.username.clone(),
        actor_uid: actor.uid,
        context: actor.context.clone(),
        target_principal_id: Some(principal_id.to_owned()),
        target_mapping_id: None,
        source: None,
        successor: None,
        delta: PolicyDelta::default(),
        enforcement: None,
        effect,
    };
    let (seq, event_hash) = policy::append_policy_event(tx, kind, occurred_at, &event)?;
    policy::append_policy_epoch(
        tx,
        &PolicyEpochPayload {
            epoch: epoch + 1,
            policy_event_seq: seq,
            policy_event_hash: event_hash,
            previous_state_hash: actor.state_hash.clone(),
            resulting_state_hash,
            occurred_at,
        },
    )
}

/// Commit on success; on refusal, commit too, so the denied audit row the
/// refusal appended survives — exactly as `access grant` does. Nothing else
/// was written before a refusal.
fn finish<T>(tx: rusqlite::Transaction<'_>, outcome: Result<T>) -> Result<T> {
    match outcome {
        Ok(value) => {
            tx.commit()?;
            Ok(value)
        }
        Err(error) => {
            let _ = tx.commit();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(id: &str) -> ScopeTuple {
        ScopeTuple::Board {
            board_id: id.to_owned(),
        }
    }

    #[test]
    fn narrowing_takes_the_lesser_of_the_grant_and_the_parent() {
        let parent = HashMap::from([(board("b"), Capability::Write)]);
        let asked = [
            WorkerGrant::parse("admin=board:b").unwrap(),
            WorkerGrant::parse("read=board:c").unwrap(),
        ];
        let narrowed = narrow(&parent, &asked);
        assert_eq!(narrowed.get(&board("b")), Some(&Capability::Write));
        assert_eq!(narrowed.get(&board("c")), None);
    }

    #[test]
    fn a_credential_has_one_shape() {
        let minted = mint_credential().unwrap();
        assert!(credential_shaped(&minted), "{minted}");
        assert!(!credential_shaped(&minted.to_uppercase()));
        assert!(!credential_shaped(&minted[..minted.len() - 1]));
        assert!(!credential_shaped("kwc_"));
    }

    #[test]
    fn labels_and_lane_actors_are_refused_by_rule() {
        assert!(validate_label("run", "run-1").is_ok());
        assert!(validate_label("run", "-run").is_err());
        assert!(validate_label("run", "").is_err());
        assert!(validate_label("run", &"a".repeat(129)).is_err());
        assert!(validate_lane_actor("@:t/b/driver").is_ok());
        assert!(validate_lane_actor("@:t/b").is_err());
        assert!(validate_lane_actor("@:t//driver").is_err());
        assert!(validate_lane_actor("t/b/driver").is_err());
    }
}
