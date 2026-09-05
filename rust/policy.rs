//! Linux principal, broker policy, and bootstrap state (ADR-038, ADR-033).
//!
//! This module owns the registry-side policy schema and store APIs: the three
//! append-only journals (`policy_events`, `policy_epochs`, `access_audit`),
//! their rebuildable projections (principals, grants, SSO mappings, enforcement
//! state, proofs), the clause-5 capability lattice, and the clause-1/6/7/8
//! state each implies. It is the storage and evaluation layer only: the
//! `SO_PEERCRED` authenticator and sealed `PrincipalContext` (t-1f92e2b8), the
//! routing CLI/MCP hop (t-f2aa39aa), and the `access` command grammar and
//! generated schemas (t-86eb4fb3) are separate and intentionally absent here.

use crate::audit;
use crate::registry::Registry;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

/// One capability level. Declaration order is the ordering: `read < write <
/// admin`. `none` is never stored; it is the absence of a grant row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    Read,
    Write,
    Admin,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Read => "read",
            Capability::Write => "write",
            Capability::Admin => "admin",
        }
    }
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Capability::Read),
            "write" => Some(Capability::Write),
            "admin" => Some(Capability::Admin),
            _ => None,
        }
    }
}

/// A normalized scope tuple from the closed clause-5 vocabulary. Tuples are
/// mutually incomparable; the only cross-tuple relation is the wildcard
/// satisfier rule in [`satisfies`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ScopeTuple {
    Registry,
    Board { board_id: String },
    BoardTag { board_id: String, tag: String },
    BoardWildcard { board_id: String },
}

impl ScopeTuple {
    /// The canonical atom list, in canonical order (`registry` alone; else
    /// `board:` first, then `tag:` or `*`).
    pub fn to_atoms(&self) -> Vec<String> {
        match self {
            ScopeTuple::Registry => vec!["registry".to_owned()],
            ScopeTuple::Board { board_id } => vec![format!("board:{board_id}")],
            ScopeTuple::BoardTag { board_id, tag } => {
                vec![format!("board:{board_id}"), format!("tag:{tag}")]
            }
            ScopeTuple::BoardWildcard { board_id } => {
                vec![format!("board:{board_id}"), "*".to_owned()]
            }
        }
    }

    /// Normalize a set of scope atoms into exactly one valid tuple, refusing
    /// duplicate atoms, unknown atoms, and combinations that do not form one
    /// tuple (ADR-033's closed vocabulary).
    pub fn from_atoms(atoms: &[String]) -> Result<Self> {
        let mut seen = std::collections::HashSet::new();
        let mut registry = false;
        let mut wildcard = false;
        let mut board: Option<String> = None;
        let mut tag: Option<String> = None;
        for atom in atoms {
            if !seen.insert(atom.clone()) {
                bail!("duplicate scope atom {atom}");
            }
            if atom == "registry" {
                registry = true;
            } else if atom == "*" {
                wildcard = true;
            } else if let Some(id) = atom.strip_prefix("board:") {
                if id.is_empty() {
                    bail!("empty board scope");
                }
                if board.replace(id.to_owned()).is_some() {
                    bail!("two board atoms");
                }
            } else if let Some(slug) = atom.strip_prefix("tag:") {
                if slug.is_empty() {
                    bail!("empty tag scope");
                }
                if tag.replace(slug.to_owned()).is_some() {
                    bail!("two tag atoms");
                }
            } else {
                bail!("unknown scope atom {atom}");
            }
        }
        match (registry, board, tag, wildcard) {
            (true, None, None, false) => Ok(ScopeTuple::Registry),
            (false, Some(board_id), None, false) => Ok(ScopeTuple::Board { board_id }),
            (false, Some(board_id), Some(tag), false) => Ok(ScopeTuple::BoardTag { board_id, tag }),
            (false, Some(board_id), None, true) => Ok(ScopeTuple::BoardWildcard { board_id }),
            _ => bail!("scope atoms do not form exactly one valid tuple"),
        }
    }
}

/// A grant row's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantState {
    Active,
    Revoked,
    Retired,
}

impl GrantState {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantState::Active => "active",
            GrantState::Revoked => "revoked",
            GrantState::Retired => "retired",
        }
    }
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "active" => Some(GrantState::Active),
            "revoked" => Some(GrantState::Revoked),
            "retired" => Some(GrantState::Retired),
            _ => None,
        }
    }
}

/// Where a grant row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantOrigin {
    Bootstrap,
    BoardSeed,
    Grant,
    RebindTransfer,
    BreakglassRegistryAdmin,
}

impl GrantOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            GrantOrigin::Bootstrap => "bootstrap",
            GrantOrigin::BoardSeed => "board_seed",
            GrantOrigin::Grant => "grant",
            GrantOrigin::RebindTransfer => "rebind_transfer",
            GrantOrigin::BreakglassRegistryAdmin => "breakglass_registry_admin",
        }
    }
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "bootstrap" => Some(GrantOrigin::Bootstrap),
            "board_seed" => Some(GrantOrigin::BoardSeed),
            "grant" => Some(GrantOrigin::Grant),
            "rebind_transfer" => Some(GrantOrigin::RebindTransfer),
            "breakglass_registry_admin" => Some(GrantOrigin::BreakglassRegistryAdmin),
            _ => None,
        }
    }
}

/// The fourteen policy-event kinds that advance the epoch by exactly one
/// (ADR-038 clause 8). Authorization decisions, proof issuance/consumption and
/// expiry, root attempts, and denials do not.
pub fn kind_advances_epoch(kind: &str) -> bool {
    matches!(
        kind,
        "bootstrap"
            | "principal_bound"
            | "principal_rebound"
            | "principal_disabled"
            | "grant_added"
            | "grant_revoked"
            | "sso_mapped"
            | "sso_unmapped"
            | "board_seeded"
            | "enforcement_prepared"
            | "enforcement_activated"
            | "breakglass_principal_rebound"
            | "breakglass_registry_admin"
            | "breakglass_sso_mapped"
    )
}

/// A short, opaque, prefixed identifier (`p-`, `pg-`, `ps-`, `pf-`, `pc-`,
/// `rq-`, `pa-`), eight hex chars from a fresh UUID — disjoint from every
/// board-row prefix in `rust/model.rs`. Policy-event IDs are the exception:
/// they are `pe-` plus the zero-padded sequence number (ADR-038).
pub(crate) fn short_id(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

/// The immutable frozen principal value carried on bind and rebind events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalValue {
    pub id: String,
    pub username: String,
    pub uid: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
}

/// The audit context object shared by policy events and access-audit rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyContext {
    pub authn_kind: String,
    #[serde(rename = "peerUID")]
    pub peer_uid: u32,
    #[serde(rename = "realUID")]
    pub real_uid: Option<u32>,
    #[serde(rename = "effectiveUID")]
    pub effective_uid: Option<u32>,
    pub client_kind: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
    pub claimed_actor: Option<String>,
    pub reason: Option<String>,
    pub provider: Option<String>,
    pub subject: Option<String>,
}

/// The seven ID lists an event records about the rows it created or retired.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDelta {
    #[serde(rename = "seededGrantIDs", default)]
    pub seeded_grant_ids: Vec<String>,
    #[serde(rename = "grantedGrantIDs", default)]
    pub granted_grant_ids: Vec<String>,
    #[serde(rename = "revokedGrantIDs", default)]
    pub revoked_grant_ids: Vec<String>,
    #[serde(rename = "activatedGrantIDs", default)]
    pub activated_grant_ids: Vec<String>,
    #[serde(rename = "retiredGrantIDs", default)]
    pub retired_grant_ids: Vec<String>,
    #[serde(rename = "mappedMappingIDs", default)]
    pub mapped_mapping_ids: Vec<String>,
    #[serde(rename = "unmappedMappingIDs", default)]
    pub unmapped_mapping_ids: Vec<String>,
}

/// One grant row (the policy row of clause 4, ADR-038's projection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: String,
    #[serde(rename = "principalID")]
    pub principal_id: String,
    pub capability: Capability,
    pub scope: Vec<String>,
    pub state: GrantState,
    pub origin: GrantOrigin,
    pub granted_at_epoch: i64,
    #[serde(rename = "grantedByPrincipalID")]
    pub granted_by_principal_id: Option<String>,
    #[serde(rename = "grantedByEventID")]
    pub granted_by_event_id: String,
    pub retired_at_epoch: Option<i64>,
    #[serde(rename = "retiredByEventID")]
    pub retired_by_event_id: Option<String>,
    #[serde(rename = "transferredFromGrantID")]
    pub transferred_from_grant_id: Option<String>,
}

impl Grant {
    pub fn scope_tuple(&self) -> ScopeTuple {
        ScopeTuple::from_atoms(&self.scope).expect("stored grant scope is a valid tuple")
    }
}

/// One SSO mapping row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoMapping {
    pub id: String,
    #[serde(rename = "principalID")]
    pub principal_id: String,
    pub provider: String,
    pub subject: String,
    pub mapped_at_epoch: i64,
    #[serde(rename = "mappedByEventID")]
    pub mapped_by_event_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmapped_at_epoch: Option<i64>,
    #[serde(rename = "unmappedByEventID", skip_serializing_if = "Option::is_none")]
    pub unmapped_by_event_id: Option<String>,
}

/// One principal row, without its denormalized grants and mappings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalRow {
    pub id: String,
    pub username: String,
    pub uid: u32,
    pub enabled: bool,
    pub bound_at_epoch: i64,
    #[serde(rename = "boundByEventID")]
    pub bound_by_event_id: String,
    pub disabled_at_epoch: Option<i64>,
    #[serde(rename = "disabledByEventID")]
    pub disabled_by_event_id: Option<String>,
    #[serde(rename = "successorID")]
    pub successor_id: Option<String>,
    #[serde(rename = "predecessorID")]
    pub predecessor_id: Option<String>,
    pub replaces: Vec<String>,
}

/// A principal as `principal show` emits it: the row plus its grants and
/// mappings.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    #[serde(flatten)]
    pub row: PrincipalRow,
    pub grants: Vec<Grant>,
    pub sso_mappings: Vec<SsoMapping>,
}

/// The complete projection delta one policy event applies. Stored on the event
/// so replay from epoch 0 reconstructs the materialized tables; it is the
/// store-internal complement of the public `delta` ID lists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEffect {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub principals: Vec<PrincipalRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retired_grants: Vec<GrantRetirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mappings: Vec<SsoMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmapped_mappings: Vec<MappingRetirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enforcement: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GrantRetirement {
    pub id: String,
    pub retired_at_epoch: i64,
    #[serde(rename = "retiredByEventID")]
    pub retired_by_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MappingRetirement {
    pub id: String,
    pub unmapped_at_epoch: i64,
    #[serde(rename = "unmappedByEventID")]
    pub unmapped_by_event_id: String,
}

/// The canonical body of a `policy_events` row, stored as its `payload` JSON
/// (and covered by the ADR-029 chain digest). A superset of the access-audit
/// projection: `effect` is what makes replay possible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEventPayload {
    pub id: String,
    pub kind: String,
    pub occurred_at: i64,
    pub before_epoch: i64,
    pub after_epoch: i64,
    #[serde(rename = "accessAuditEventID")]
    pub access_audit_event_id: Option<String>,
    #[serde(rename = "actorPrincipalID")]
    pub actor_principal_id: Option<String>,
    pub actor_username: String,
    #[serde(rename = "actorUID")]
    pub actor_uid: u32,
    pub context: PolicyContext,
    #[serde(rename = "targetPrincipalID")]
    pub target_principal_id: Option<String>,
    #[serde(rename = "targetMappingID")]
    pub target_mapping_id: Option<String>,
    pub source: Option<PrincipalValue>,
    pub successor: Option<PrincipalValue>,
    pub delta: PolicyDelta,
    pub effect: PolicyEffect,
}

/// The canonical body of a `policy_epochs` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyEpochPayload {
    pub epoch: i64,
    #[serde(rename = "policyEventSeq")]
    pub policy_event_seq: i64,
    #[serde(rename = "policyEventHash")]
    pub policy_event_hash: String,
    #[serde(rename = "previousStateHash")]
    pub previous_state_hash: String,
    #[serde(rename = "resultingStateHash")]
    pub resulting_state_hash: String,
    pub occurred_at: i64,
}

/// The canonical body of an `access_audit` row (ADR-033's version-1 shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessAuditPayload {
    pub id: String,
    pub schema_version: i64,
    pub occurred_at: i64,
    pub operation: String,
    pub outcome: String,
    pub decision_stage: Option<String>,
    pub decision_code: Option<String>,
    pub policy_epoch: i64,
    pub enforcement_state: String,
    #[serde(rename = "actorPrincipalID")]
    pub actor_principal_id: Option<String>,
    pub actor_username: String,
    #[serde(rename = "actorUID")]
    pub actor_uid: u32,
    pub authn_kind: String,
    pub evidence_source: String,
    #[serde(rename = "peerPID")]
    pub peer_pid: Option<i64>,
    #[serde(rename = "peerUID")]
    pub peer_uid: Option<i64>,
    #[serde(rename = "peerGID")]
    pub peer_gid: Option<i64>,
    #[serde(rename = "realUID")]
    pub real_uid: Option<i64>,
    #[serde(rename = "effectiveUID")]
    pub effective_uid: Option<i64>,
    pub provider: Option<String>,
    pub subject: Option<String>,
    #[serde(rename = "verifierID")]
    pub verifier_id: Option<String>,
    #[serde(rename = "proxyRouteID")]
    pub proxy_route_id: Option<String>,
    #[serde(rename = "sourceProofID")]
    pub source_proof_id: Option<String>,
    #[serde(rename = "subjectProofID")]
    pub subject_proof_id: Option<String>,
    #[serde(rename = "requestID")]
    pub request_id: String,
    pub client_kind: String,
    pub claimed_actor: Option<String>,
    pub reason: Option<String>,
    #[serde(rename = "clientBinaryID")]
    pub client_binary_id: Option<String>,
    #[serde(rename = "brokerBinaryID")]
    pub broker_binary_id: Option<String>,
    pub broker_protocol_version: Option<i64>,
    pub command_schema_hash: Option<String>,
    pub policy_schema_version: Option<i64>,
    pub board_schema_versions: Vec<BoardSchemaVersion>,
    pub requested_capability: Option<String>,
    pub required_scopes: Vec<Vec<String>>,
    pub matched_grant_ids: Vec<String>,
    pub visible_target_ids: Vec<String>,
    pub redacted_target_digests: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSchemaVersion {
    #[serde(rename = "boardID")]
    pub board_id: String,
    pub schema_version: i64,
}

/// The actor a store mutation runs as. This is the registry-side data the
/// authenticator (t-1f92e2b8) derives; it is not the sealed `PrincipalContext`.
#[derive(Debug, Clone)]
pub struct PolicyActor {
    /// Null exactly on the two root-only paths (bootstrap, break-glass).
    pub principal_id: Option<String>,
    pub username: String,
    pub uid: u32,
    /// The epoch and state hash minted into the caller's context (clause 8).
    pub epoch: i64,
    pub state_hash: String,
    pub context: PolicyContext,
}

// ---------------------------------------------------------------------------
// Clause 5: the capability lattice.
// ---------------------------------------------------------------------------

/// The pointwise join `A(t) = max { c : (t, c) is an active grant }`. `none` is
/// the default at every tuple and is never stored.
pub fn authority(
    grants: impl IntoIterator<Item = (ScopeTuple, Capability)>,
) -> HashMap<ScopeTuple, Capability> {
    let mut out: HashMap<ScopeTuple, Capability> = HashMap::new();
    for (tuple, capability) in grants {
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

/// Whether a requirement `(tuple, required)` is satisfied by an authority map.
///
/// The one cross-tuple rule: a requirement `({board:b, tag:s}, c)` is satisfied
/// when `A({board:b, tag:s}) >= c` **or** `A({board:b, *}) >= c`. The wildcard
/// is an alternative satisfier for every tag tuple on that one board, not an
/// ordering above the tag tuples.
pub fn satisfies(
    authority: &HashMap<ScopeTuple, Capability>,
    tuple: &ScopeTuple,
    required: Capability,
) -> bool {
    match tuple {
        ScopeTuple::BoardTag { board_id, .. } => {
            authority.get(tuple).is_some_and(|c| *c >= required)
                || authority
                    .get(&ScopeTuple::BoardWildcard {
                        board_id: board_id.clone(),
                    })
                    .is_some_and(|c| *c >= required)
        }
        _ => authority.get(tuple).is_some_and(|c| *c >= required),
    }
}

/// The active grant IDs that satisfy a requirement, for `explain` receipts.
pub fn matched_grant_ids(
    grants: &[Grant],
    tuple: &ScopeTuple,
    required: Capability,
) -> Vec<String> {
    let mut out = Vec::new();
    for grant in grants {
        if grant.state != GrantState::Active {
            continue;
        }
        let grant_tuple = grant.scope_tuple();
        let matches = match tuple {
            ScopeTuple::BoardTag { board_id, .. } => {
                grant_tuple == *tuple
                    || grant_tuple
                        == ScopeTuple::BoardWildcard {
                            board_id: board_id.clone(),
                        }
            }
            _ => grant_tuple == *tuple,
        };
        if matches && grant.capability >= required {
            out.push(grant.id.clone());
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Store helpers.
// ---------------------------------------------------------------------------

fn journal_next_seq(connection: &Connection, table: &str) -> Result<i64> {
    let last: i64 = connection.query_row(
        &format!("SELECT COALESCE(MAX(seq),0) FROM {table}"),
        [],
        |row| row.get(0),
    )?;
    Ok(last + 1)
}

fn policy_event_id(seq: i64) -> String {
    format!("pe-{seq:08}")
}

fn enforcement_state_on(connection: &Connection) -> Result<String> {
    connection
        .query_row(
            "SELECT state FROM enforcement_state WHERE id=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .map_err(Into::into)
}

fn policy_epoch_on(connection: &Connection) -> Result<i64> {
    connection
        .query_row(
            "SELECT COALESCE(MAX(epoch),0) FROM policy_epochs",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(Into::into)
}

fn principal_row_on(connection: &Connection, id: &str) -> Result<Option<PrincipalRow>> {
    connection
        .query_row(
            "SELECT id,username,uid,enabled,bound_at_epoch,bound_by_event_id,\
                    disabled_at_epoch,disabled_by_event_id,successor_id,predecessor_id,replaces \
             FROM principals WHERE id=?",
            [id],
            principal_row_from_row,
        )
        .optional()
        .map_err(Into::into)
}

fn principal_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PrincipalRow> {
    Ok(PrincipalRow {
        id: row.get(0)?,
        username: row.get(1)?,
        uid: row.get::<_, i64>(2)? as u32,
        enabled: row.get::<_, i64>(3)? != 0,
        bound_at_epoch: row.get(4)?,
        bound_by_event_id: row.get(5)?,
        disabled_at_epoch: row.get(6)?,
        disabled_by_event_id: row.get(7)?,
        successor_id: row.get(8)?,
        predecessor_id: row.get(9)?,
        replaces: serde_json::from_str::<Vec<String>>(&row.get::<_, String>(10)?)
            .unwrap_or_default(),
    })
}

fn all_principal_rows_on(connection: &Connection) -> Result<Vec<PrincipalRow>> {
    let mut statement = connection.prepare(
        "SELECT id,username,uid,enabled,bound_at_epoch,bound_by_event_id,\
                disabled_at_epoch,disabled_by_event_id,successor_id,predecessor_id,replaces \
         FROM principals ORDER BY id",
    )?;
    let rows = statement
        .query_map([], principal_row_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn grant_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Grant> {
    Ok(Grant {
        id: row.get(0)?,
        principal_id: row.get(1)?,
        capability: Capability::from_str(&row.get::<_, String>(2)?).unwrap_or(Capability::Read),
        scope: serde_json::from_str::<Vec<String>>(&row.get::<_, String>(3)?).unwrap_or_default(),
        state: GrantState::from_str(&row.get::<_, String>(4)?).unwrap_or(GrantState::Retired),
        origin: GrantOrigin::from_str(&row.get::<_, String>(5)?).unwrap_or(GrantOrigin::Grant),
        granted_at_epoch: row.get(6)?,
        granted_by_principal_id: row.get(7)?,
        granted_by_event_id: row.get(8)?,
        retired_at_epoch: row.get(9)?,
        retired_by_event_id: row.get(10)?,
        transferred_from_grant_id: row.get(11)?,
    })
}

const GRANT_COLUMNS: &str = "id,principal_id,capability,scope,state,origin,granted_at_epoch,\
     granted_by_principal_id,granted_by_event_id,retired_at_epoch,retired_by_event_id,\
     transferred_from_grant_id";

fn all_grants_on(connection: &Connection) -> Result<Vec<Grant>> {
    let mut statement =
        connection.prepare(&format!("SELECT {GRANT_COLUMNS} FROM grants ORDER BY id"))?;
    let rows = statement
        .query_map([], grant_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn grants_for_principal_on(connection: &Connection, principal_id: &str) -> Result<Vec<Grant>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {GRANT_COLUMNS} FROM grants WHERE principal_id=? ORDER BY id"
    ))?;
    let rows = statement
        .query_map([principal_id], grant_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn active_grants_for_principal_on(
    connection: &Connection,
    principal_id: &str,
) -> Result<Vec<Grant>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {GRANT_COLUMNS} FROM grants WHERE principal_id=? AND state='active' ORDER BY id"
    ))?;
    let rows = statement
        .query_map([principal_id], grant_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn mapping_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SsoMapping> {
    Ok(SsoMapping {
        id: row.get(0)?,
        principal_id: row.get(1)?,
        provider: row.get(2)?,
        subject: row.get(3)?,
        mapped_at_epoch: row.get(4)?,
        mapped_by_event_id: row.get(5)?,
        unmapped_at_epoch: row.get(6)?,
        unmapped_by_event_id: row.get(7)?,
    })
}

const MAPPING_COLUMNS: &str = "id,principal_id,provider,subject,mapped_at_epoch,mapped_by_event_id,\
     unmapped_at_epoch,unmapped_by_event_id";

fn mappings_for_principal_on(
    connection: &Connection,
    principal_id: &str,
) -> Result<Vec<SsoMapping>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {MAPPING_COLUMNS} FROM sso_mappings WHERE principal_id=? ORDER BY id"
    ))?;
    let rows = statement
        .query_map([principal_id], mapping_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn all_mappings_on(connection: &Connection) -> Result<Vec<SsoMapping>> {
    let mut statement = connection.prepare(&format!(
        "SELECT {MAPPING_COLUMNS} FROM sso_mappings ORDER BY id"
    ))?;
    let rows = statement
        .query_map([], mapping_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The deterministic state hash over the complete projection — principals,
/// grants, mappings, and enforcement state — compared for equality at commit
/// (clause 8) and matched against every `policy_epochs` row by replay.
pub fn compute_state_hash(connection: &Connection) -> Result<String> {
    let mut parts = vec![format!("enforcement={}", enforcement_state_on(connection)?)];
    for principal in all_principal_rows_on(connection)? {
        parts.push(format!(
            "principal={}",
            serde_json::to_string(&principal).expect("principal serializes")
        ));
    }
    for grant in all_grants_on(connection)? {
        parts.push(format!(
            "grant={}",
            serde_json::to_string(&grant).expect("grant serializes")
        ));
    }
    for mapping in all_mappings_on(connection)? {
        parts.push(format!(
            "mapping={}",
            serde_json::to_string(&mapping).expect("mapping serializes")
        ));
    }
    Ok(audit::bytes_sha256(parts.join("\n").as_bytes()))
}

/// The live `(epoch, resulting_state_hash)`, or the empty epoch-0 state when no
/// policy event has committed yet.
fn live_policy_state_on(connection: &Connection) -> Result<(i64, String)> {
    let payload: Option<String> = connection
        .query_row(
            "SELECT payload FROM policy_epochs ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match payload {
        Some(payload) => {
            let epoch: PolicyEpochPayload =
                serde_json::from_str(&payload).context("parse latest policy epoch")?;
            Ok((epoch.epoch, epoch.resulting_state_hash))
        }
        None => Ok((0, compute_state_hash(connection)?)),
    }
}

/// Append an access-audit row, chained into ADR-029's registry hash chain.
fn append_access_audit(connection: &Connection, payload: &AccessAuditPayload) -> Result<()> {
    let json = serde_json::to_string(payload)?;
    let (seq, prev_hash, event_hash) = audit::next_chained(
        connection,
        "access_audit",
        "access_audit",
        Some(&payload.id),
        "access_audit",
        &json,
        payload.occurred_at,
    )?;
    connection.execute(
        "INSERT INTO access_audit(seq,event_id,occurred_at,prev_hash,event_hash,payload) \
         VALUES(?,?,?,?,?,?)",
        params![
            seq,
            payload.id,
            payload.occurred_at,
            prev_hash,
            event_hash,
            json
        ],
    )?;
    Ok(())
}

/// Append a policy event, chained, returning `(seq, event_hash)`.
fn append_policy_event(
    connection: &Connection,
    kind: &str,
    occurred_at: i64,
    payload: &PolicyEventPayload,
) -> Result<(i64, String)> {
    let json = serde_json::to_string(payload)?;
    let seq = journal_next_seq(connection, "policy_events")?;
    let event_id = policy_event_id(seq);
    let (seq, prev_hash, event_hash) = audit::next_chained(
        connection,
        "policy_events",
        "policy_events",
        Some(&event_id),
        kind,
        &json,
        occurred_at,
    )?;
    connection.execute(
        "INSERT INTO policy_events(seq,event_id,kind,occurred_at,prev_hash,event_hash,payload) \
         VALUES(?,?,?,?,?,?,?)",
        params![
            seq,
            event_id,
            kind,
            occurred_at,
            prev_hash,
            event_hash,
            json
        ],
    )?;
    Ok((seq, event_hash))
}

/// Append a policy epoch row, chained.
fn append_policy_epoch(connection: &Connection, payload: &PolicyEpochPayload) -> Result<()> {
    let json = serde_json::to_string(payload)?;
    let (seq, prev_hash, event_hash) = audit::next_chained(
        connection,
        "policy_epochs",
        "policy_epochs",
        Some(&payload.epoch.to_string()),
        "policy_epoch",
        &json,
        payload.occurred_at,
    )?;
    connection.execute(
        "INSERT INTO policy_epochs(seq,epoch,occurred_at,prev_hash,event_hash,payload) \
         VALUES(?,?,?,?,?,?)",
        params![
            seq,
            payload.epoch,
            payload.occurred_at,
            prev_hash,
            event_hash,
            json
        ],
    )?;
    Ok(())
}

/// Apply one event's projection effect to the materialized tables.
fn apply_effect(connection: &Connection, effect: &PolicyEffect) -> Result<()> {
    for principal in &effect.principals {
        connection.execute(
            "INSERT INTO principals(id,username,uid,enabled,bound_at_epoch,bound_by_event_id,\
                    disabled_at_epoch,disabled_by_event_id,successor_id,predecessor_id,replaces) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET \
               username=excluded.username,uid=excluded.uid,enabled=excluded.enabled,\
               bound_at_epoch=excluded.bound_at_epoch,bound_by_event_id=excluded.bound_by_event_id,\
               disabled_at_epoch=excluded.disabled_at_epoch,disabled_by_event_id=excluded.disabled_by_event_id,\
               successor_id=excluded.successor_id,predecessor_id=excluded.predecessor_id,\
               replaces=excluded.replaces",
            params![
                principal.id,
                principal.username,
                principal.uid as i64,
                principal.enabled as i64,
                principal.bound_at_epoch,
                principal.bound_by_event_id,
                principal.disabled_at_epoch,
                principal.disabled_by_event_id,
                principal.successor_id,
                principal.predecessor_id,
                serde_json::to_string(&principal.replaces)?,
            ],
        )?;
    }
    for retirement in &effect.retired_grants {
        connection.execute(
            "UPDATE grants SET state='retired',retired_at_epoch=?,retired_by_event_id=? WHERE id=?",
            params![
                retirement.retired_at_epoch,
                retirement.retired_by_event_id,
                retirement.id
            ],
        )?;
    }
    // Retire mappings before inserting their transferred successors, so the
    // partial unique index on active `(provider, subject)` never sees the same
    // pair in two active rows during a rebind transfer.
    for retirement in &effect.unmapped_mappings {
        connection.execute(
            "UPDATE sso_mappings SET unmapped_at_epoch=?,unmapped_by_event_id=? WHERE id=?",
            params![
                retirement.unmapped_at_epoch,
                retirement.unmapped_by_event_id,
                retirement.id
            ],
        )?;
    }
    for grant in &effect.grants {
        connection.execute(
            "INSERT INTO grants(id,principal_id,capability,scope,state,origin,granted_at_epoch,\
                    granted_by_principal_id,granted_by_event_id,retired_at_epoch,retired_by_event_id,\
                    transferred_from_grant_id) \
             VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                grant.id,
                grant.principal_id,
                grant.capability.as_str(),
                serde_json::to_string(&grant.scope)?,
                grant.state.as_str(),
                grant.origin.as_str(),
                grant.granted_at_epoch,
                grant.granted_by_principal_id,
                grant.granted_by_event_id,
                grant.retired_at_epoch,
                grant.retired_by_event_id,
                grant.transferred_from_grant_id,
            ],
        )?;
    }
    for mapping in &effect.mappings {
        connection.execute(
            "INSERT INTO sso_mappings(id,principal_id,provider,subject,mapped_at_epoch,\
                    mapped_by_event_id,unmapped_at_epoch,unmapped_by_event_id) \
             VALUES(?,?,?,?,?,?,?,?)",
            params![
                mapping.id,
                mapping.principal_id,
                mapping.provider,
                mapping.subject,
                mapping.mapped_at_epoch,
                mapping.mapped_by_event_id,
                mapping.unmapped_at_epoch,
                mapping.unmapped_by_event_id,
            ],
        )?;
    }
    if let Some(state) = &effect.enforcement {
        connection.execute("UPDATE enforcement_state SET state=? WHERE id=1", [state])?;
    }
    Ok(())
}

/// Build a [`Principal`] (row plus grants and mappings) from a row, reading via
/// any `&Connection` (including an open transaction).
fn principal_with_on(connection: &Connection, row: &PrincipalRow) -> Result<Principal> {
    Ok(Principal {
        grants: grants_for_principal_on(connection, &row.id)?,
        sso_mappings: mappings_for_principal_on(connection, &row.id)?,
        row: row.clone(),
    })
}

/// The authority map for an enabled principal, from its active grants.
fn principal_authority_on(
    connection: &Connection,
    principal_id: &str,
) -> Result<HashMap<ScopeTuple, Capability>> {
    let grants = active_grants_for_principal_on(connection, principal_id)?;
    Ok(authority(
        grants.into_iter().map(|g| (g.scope_tuple(), g.capability)),
    ))
}

/// Verify the frozen `{username, uid}` pair against the store's frozen
/// principals and refuse any divergence (clause 1). A pair resolves only when
/// an enabled principal matches *both* halves; a username-only or uid-only
/// match — the UID-reuse hazard — returns `None`, so the authenticator denies
/// rather than guessing.
fn resolve_enabled_principal(
    connection: &Connection,
    username: &str,
    uid: u32,
) -> Result<Option<PrincipalRow>> {
    let mut statement = connection.prepare(
        "SELECT id,username,uid,enabled,bound_at_epoch,bound_by_event_id,\
                disabled_at_epoch,disabled_by_event_id,successor_id,predecessor_id,replaces \
         FROM principals WHERE enabled=1 AND username=?1 AND uid=?2",
    )?;
    let rows = statement
        .query_map(params![username, uid as i64], principal_row_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // At most one enabled principal may hold a pair, but never trust that
    // silently: more than one match is a divergence.
    Ok(if rows.len() == 1 {
        rows.into_iter().next()
    } else {
        None
    })
}

/// Enforce "an active username or UID may belong to only one principal" and the
/// `--replaces` collision acknowledgement, fail-closed (ADR-033, clause 1).
///
/// `implicit` names a principal being disabled by this very operation (the
/// rebind source), which is implicitly acknowledged and never a collision.
fn check_pair_collision(
    connection: &Connection,
    username: &str,
    uid: u32,
    replaces: &[String],
    implicit: Option<&str>,
) -> Result<()> {
    let mut statement =
        connection.prepare("SELECT id,enabled FROM principals WHERE username=?1 OR uid=?2")?;
    let rows = statement
        .query_map(params![username, uid as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
        })?
        .collect::<rusqlite::Result<Vec<(String, bool)>>>()?;

    let mut colliders: Vec<String> = Vec::new();
    for (id, enabled) in rows {
        if Some(id.as_str()) == implicit {
            continue;
        }
        if enabled {
            bail!(
                "principal {id} already holds an active username or UID colliding with \
                 {username}:{uid}"
            );
        }
        colliders.push(id);
    }
    colliders.sort();
    let mut acknowledged: Vec<String> = replaces.to_vec();
    acknowledged.sort();
    acknowledged.dedup();
    if colliders != acknowledged {
        bail!(
            "username or UID reuse must acknowledge every colliding disabled principal \
             (expected {:?}, got {:?})",
            colliders,
            acknowledged
        );
    }
    Ok(())
}

/// Append a denied access-audit row and return the generic denial error.
fn deny(
    connection: &Connection,
    actor: &PolicyActor,
    operation: &str,
    stage: &str,
    code: &str,
    epoch: i64,
) -> anyhow::Error {
    let occurred_at = crate::registry::now_ms();
    let payload = AccessAuditPayload {
        id: short_id("pa"),
        schema_version: 1,
        occurred_at,
        operation: operation.to_owned(),
        outcome: "denied".to_owned(),
        decision_stage: Some(stage.to_owned()),
        decision_code: Some(code.to_owned()),
        policy_epoch: epoch,
        enforcement_state: enforcement_state_on(connection).unwrap_or_else(|_| "direct".into()),
        actor_principal_id: actor.principal_id.clone(),
        actor_username: actor.username.clone(),
        actor_uid: actor.uid,
        authn_kind: actor.context.authn_kind.clone(),
        evidence_source: "kernel_so_peercred".to_owned(),
        peer_pid: None,
        peer_uid: Some(actor.context.peer_uid as i64),
        peer_gid: None,
        real_uid: None,
        effective_uid: None,
        provider: actor.context.provider.clone(),
        subject: actor.context.subject.clone(),
        verifier_id: None,
        proxy_route_id: None,
        source_proof_id: None,
        subject_proof_id: None,
        request_id: actor.context.request_id.clone(),
        client_kind: actor.context.client_kind.clone(),
        claimed_actor: actor.context.claimed_actor.clone(),
        reason: actor.context.reason.clone(),
        client_binary_id: None,
        broker_binary_id: None,
        broker_protocol_version: None,
        command_schema_hash: None,
        policy_schema_version: None,
        board_schema_versions: Vec::new(),
        requested_capability: None,
        required_scopes: Vec::new(),
        matched_grant_ids: Vec::new(),
        visible_target_ids: Vec::new(),
        redacted_target_digests: Vec::new(),
    };
    let _ = append_access_audit(connection, &payload);
    anyhow::anyhow!("denied or not found")
}

/// Require the live epoch and state hash to equal the caller's minted context
/// (clause 8). A mismatch is a stale context and is refused generically.
fn require_context(connection: &Connection, actor: &PolicyActor, operation: &str) -> Result<()> {
    let (epoch, state_hash) = live_policy_state_on(connection)?;
    if epoch != actor.epoch || state_hash != actor.state_hash {
        return Err(deny(
            connection,
            actor,
            operation,
            "epoch",
            "stale_context",
            epoch,
        ));
    }
    Ok(())
}

/// Require the actor to hold `admin` on `{registry}`.
fn require_registry_admin(
    connection: &Connection,
    actor: &PolicyActor,
    operation: &str,
    epoch: i64,
) -> Result<()> {
    let grantor = match &actor.principal_id {
        Some(id) => id.clone(),
        None => {
            return Err(deny(
                connection,
                actor,
                operation,
                "principal",
                "no_principal",
                epoch,
            ));
        }
    };
    let authority = principal_authority_on(connection, &grantor)?;
    if !satisfies(&authority, &ScopeTuple::Registry, Capability::Admin) {
        return Err(deny(
            connection,
            actor,
            operation,
            "scope",
            "missing_registry_admin",
            epoch,
        ));
    }
    Ok(())
}

/// Non-escalation: the grantor must satisfy the exact `(tuple, capability)` it
/// is granting or revoking.
fn require_grantor_holds(
    connection: &Connection,
    actor: &PolicyActor,
    tuple: &ScopeTuple,
    capability: Capability,
    operation: &str,
    epoch: i64,
) -> Result<()> {
    let grantor = match &actor.principal_id {
        Some(id) => id.clone(),
        None => {
            return Err(deny(
                connection,
                actor,
                operation,
                "principal",
                "no_principal",
                epoch,
            ));
        }
    };
    let authority = principal_authority_on(connection, &grantor)?;
    if !satisfies(&authority, tuple, capability) {
        return Err(deny(
            connection,
            actor,
            operation,
            "nonEscalation",
            "missing_capability",
            epoch,
        ));
    }
    Ok(())
}

/// Build the allowed access-audit payload for a policy mutation.
fn allowed_audit(
    actor: &PolicyActor,
    operation: &str,
    epoch: i64,
    enforcement: &str,
    requested_capability: Option<Capability>,
    required_scopes: &[ScopeTuple],
    matched_grant_ids: &[String],
) -> AccessAuditPayload {
    let occurred_at = crate::registry::now_ms();
    AccessAuditPayload {
        id: short_id("pa"),
        schema_version: 1,
        occurred_at,
        operation: operation.to_owned(),
        outcome: "allowed".to_owned(),
        decision_stage: None,
        decision_code: None,
        policy_epoch: epoch,
        enforcement_state: enforcement.to_owned(),
        actor_principal_id: actor.principal_id.clone(),
        actor_username: actor.username.clone(),
        actor_uid: actor.uid,
        authn_kind: actor.context.authn_kind.clone(),
        evidence_source: if actor.principal_id.is_none() {
            "kernel_so_peercred_root_bootstrap".to_owned()
        } else {
            "kernel_so_peercred".to_owned()
        },
        peer_pid: None,
        peer_uid: Some(actor.context.peer_uid as i64),
        peer_gid: None,
        real_uid: None,
        effective_uid: None,
        provider: actor.context.provider.clone(),
        subject: actor.context.subject.clone(),
        verifier_id: None,
        proxy_route_id: None,
        source_proof_id: None,
        subject_proof_id: None,
        request_id: actor.context.request_id.clone(),
        client_kind: actor.context.client_kind.clone(),
        claimed_actor: actor.context.claimed_actor.clone(),
        reason: actor.context.reason.clone(),
        client_binary_id: None,
        broker_binary_id: None,
        broker_protocol_version: None,
        command_schema_hash: None,
        policy_schema_version: None,
        board_schema_versions: Vec::new(),
        requested_capability: requested_capability.map(|c| c.as_str().to_owned()),
        required_scopes: required_scopes.iter().map(ScopeTuple::to_atoms).collect(),
        matched_grant_ids: matched_grant_ids.to_vec(),
        visible_target_ids: Vec::new(),
        redacted_target_digests: Vec::new(),
    }
}

/// The active (non-archived) board UUIDs, extracted from each registered
/// board's published `<uuid>.db` path (ADR-032).
fn active_board_ids(connection: &Connection) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT board_path FROM boards WHERE archived=0 ORDER BY board_path")?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut ids = Vec::new();
    for path in paths {
        if let Some(stem) = std::path::Path::new(&path)
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|stem| !stem.is_empty())
        {
            ids.push(stem.to_owned());
        }
    }
    Ok(ids)
}

/// The empty-policy state hash: a projection with no principals, grants, or
/// mappings, and enforcement `direct`.
fn compute_empty_state_hash() -> String {
    audit::bytes_sha256(b"enforcement=direct")
}

// ---------------------------------------------------------------------------
// Registry store APIs.
// ---------------------------------------------------------------------------

impl Registry {
    /// The live policy epoch (0 before bootstrap).
    pub fn policy_epoch(&self) -> Result<i64> {
        policy_epoch_on(&self.connection)
    }

    /// The live `(epoch, resulting_state_hash)`.
    pub fn live_policy_state(&self) -> Result<(i64, String)> {
        live_policy_state_on(&self.connection)
    }

    /// The enforcement state (`direct`, `prepared`, or `managed`).
    pub fn enforcement_state(&self) -> Result<String> {
        enforcement_state_on(&self.connection)
    }

    /// Whether this registry has reached `REGISTRY_V14`, which introduced
    /// `enforcement_state`.
    ///
    /// The managed-mode gate needs to tell a legacy registry (no such table,
    /// legitimately unmanaged) apart from one it simply could not read
    /// (ambiguous, and per ADR-008 fails closed). Collapsing the two would
    /// make every read failure look like a fresh single-user install.
    pub fn has_enforcement_state_table(&self) -> Result<bool> {
        let count: i64 = self.connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='enforcement_state'",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// The deterministic state hash of the complete projection.
    pub fn policy_state_hash(&self) -> Result<String> {
        compute_state_hash(&self.connection)
    }

    /// Fail-closed resolution of a `{username, uid}` pair to exactly one
    /// enabled principal. Divergence in either half returns `None`.
    pub fn resolve_principal(&self, username: &str, uid: u32) -> Result<Option<Principal>> {
        match resolve_enabled_principal(&self.connection, username, uid)? {
            Some(row) => Ok(Some(self.principal_with(&row)?)),
            None => Ok(None),
        }
    }

    fn principal_with(&self, row: &PrincipalRow) -> Result<Principal> {
        principal_with_on(&self.connection, row)
    }

    pub fn principal(&self, id: &str) -> Result<Option<Principal>> {
        match principal_row_on(&self.connection, id)? {
            Some(row) => Ok(Some(self.principal_with(&row)?)),
            None => Ok(None),
        }
    }

    pub fn principals(&self, include_disabled: bool) -> Result<Vec<Principal>> {
        let rows = all_principal_rows_on(&self.connection)?;
        let mut out = Vec::new();
        for row in rows {
            if !include_disabled && !row.enabled {
                continue;
            }
            out.push(self.principal_with(&row)?);
        }
        Ok(out)
    }

    /// The effective authority map for a principal.
    pub fn principal_authority(
        &self,
        principal_id: &str,
    ) -> Result<HashMap<ScopeTuple, Capability>> {
        principal_authority_on(&self.connection, principal_id)
    }

    /// Evaluate a requirement without creating authority (the lattice of
    /// clause 5). Read-only.
    pub fn explain(
        &self,
        principal_id: &str,
        tuple: &ScopeTuple,
        capability: Capability,
    ) -> Result<ExplainReceipt> {
        let grants = active_grants_for_principal_on(&self.connection, principal_id)?;
        let authority = authority(grants.iter().map(|g| (g.scope_tuple(), g.capability)));
        let allowed = satisfies(&authority, tuple, capability);
        let (epoch, state_hash) = live_policy_state_on(&self.connection)?;
        Ok(ExplainReceipt {
            principal_id: principal_id.to_owned(),
            capability,
            required_scopes: vec![tuple.to_atoms()],
            policy_epoch: epoch,
            policy_state_hash: state_hash,
            enforcement_state: enforcement_state_on(&self.connection)?,
            outcome: if allowed {
                "allowed".into()
            } else {
                "denied".into()
            },
            matched_grant_ids: matched_grant_ids(&grants, tuple, capability),
            denial_reason: if allowed {
                None
            } else {
                Some("denied or not found".into())
            },
        })
    }

    /// Clause 6: empty-policy bootstrap. Seeds the first non-root principal and
    /// `admin` on `{registry}`, `{board:<id>}`, and `{board:<id>, *}` for every
    /// registered board, all in one `bootstrap` event at epoch 0.
    pub fn policy_bootstrap(
        &mut self,
        username: &str,
        uid: u32,
        actor: &PolicyActor,
    ) -> Result<Principal> {
        if uid == 0 {
            bail!("bootstrap refuses a root pair");
        }
        let username = username.trim();
        if username.is_empty() {
            bail!("bootstrap requires a username");
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Principal> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            let enforcement = enforcement_state_on(&tx)?;
            if epoch != 0 {
                return Err(deny(
                    &tx,
                    actor,
                    "access bootstrap",
                    "enforcement",
                    "not_empty_policy",
                    epoch,
                ));
            }
            if enforcement != "direct" {
                return Err(deny(
                    &tx,
                    actor,
                    "access bootstrap",
                    "enforcement",
                    "not_direct",
                    epoch,
                ));
            }

            let principal_id = short_id("p");
            let principal = PrincipalRow {
                id: principal_id.clone(),
                username: username.to_owned(),
                uid,
                enabled: true,
                bound_at_epoch: 1,
                bound_by_event_id: policy_event_id(1),
                disabled_at_epoch: None,
                disabled_by_event_id: None,
                successor_id: None,
                predecessor_id: None,
                replaces: Vec::new(),
            };

            let mut grants = Vec::new();
            let mut seeded: Vec<String> = Vec::new();
            let push_grant =
                |tuple: &ScopeTuple, grants: &mut Vec<Grant>, seeded: &mut Vec<String>| {
                    let id = short_id("pg");
                    seeded.push(id.clone());
                    grants.push(Grant {
                        id,
                        principal_id: principal_id.clone(),
                        capability: Capability::Admin,
                        scope: tuple.to_atoms(),
                        state: GrantState::Active,
                        origin: GrantOrigin::Bootstrap,
                        granted_at_epoch: 1,
                        granted_by_principal_id: None,
                        granted_by_event_id: policy_event_id(1),
                        retired_at_epoch: None,
                        retired_by_event_id: None,
                        transferred_from_grant_id: None,
                    });
                };
            push_grant(&ScopeTuple::Registry, &mut grants, &mut seeded);
            for board_id in active_board_ids(&tx)? {
                push_grant(
                    &ScopeTuple::Board {
                        board_id: board_id.clone(),
                    },
                    &mut grants,
                    &mut seeded,
                );
                push_grant(
                    &ScopeTuple::BoardWildcard { board_id },
                    &mut grants,
                    &mut seeded,
                );
            }

            let effect = PolicyEffect {
                principals: vec![principal.clone()],
                grants: grants.clone(),
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();

            let audit = allowed_audit(actor, "access bootstrap", 1, &enforcement, None, &[], &[]);
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();

            let event = PolicyEventPayload {
                id: policy_event_id(1),
                kind: "bootstrap".to_owned(),
                occurred_at,
                before_epoch: 0,
                after_epoch: 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: None,
                actor_username: "root".to_owned(),
                actor_uid: 0,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.clone()),
                target_mapping_id: None,
                source: None,
                successor: None,
                delta: PolicyDelta {
                    seeded_grant_ids: seeded,
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) = append_policy_event(&tx, "bootstrap", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: compute_empty_state_hash(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            principal_with_on(&tx, &principal)
        })();
        match outcome {
            Ok(principal) => {
                tx.commit()?;
                Ok(principal)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Clause 1: bind a frozen `{username, uid}` pair as a new principal with
    /// no grants or mappings.
    pub fn bind_principal(
        &mut self,
        username: &str,
        uid: u32,
        replaces: &[String],
        actor: &PolicyActor,
    ) -> Result<Principal> {
        let username = username.trim().to_owned();
        if username.is_empty() {
            bail!("bind requires a username");
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Principal> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access principal bind")?;
            require_registry_admin(&tx, actor, "access principal bind", epoch)?;
            if let Err(error) = check_pair_collision(&tx, &username, uid, replaces, None) {
                let _ = deny(
                    &tx,
                    actor,
                    "access principal bind",
                    "principal",
                    "collision",
                    epoch,
                );
                bail!("{error}");
            }

            let principal_id = short_id("p");
            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            let principal = PrincipalRow {
                id: principal_id.clone(),
                username,
                uid,
                enabled: true,
                bound_at_epoch: epoch + 1,
                bound_by_event_id: event_id.clone(),
                disabled_at_epoch: None,
                disabled_by_event_id: None,
                successor_id: None,
                predecessor_id: None,
                replaces: replaces.to_vec(),
            };
            let effect = PolicyEffect {
                principals: vec![principal.clone()],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access principal bind",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                None,
                &[],
                &[],
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "principal_bound".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.clone()),
                target_mapping_id: None,
                source: None,
                successor: None,
                delta: PolicyDelta::default(),
                effect,
            };
            let (seq, event_hash) =
                append_policy_event(&tx, "principal_bound", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            principal_with_on(&tx, &principal)
        })();
        match outcome {
            Ok(principal) => {
                tx.commit()?;
                Ok(principal)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Disable a principal, making every grant and mapping ineffective without
    /// deleting anything.
    pub fn disable_principal(
        &mut self,
        principal_id: &str,
        actor: &PolicyActor,
    ) -> Result<Principal> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Principal> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access principal disable")?;
            require_registry_admin(&tx, actor, "access principal disable", epoch)?;
            let mut principal = principal_row_on(&tx, principal_id)?.ok_or_else(|| {
                deny(
                    &tx,
                    actor,
                    "access principal disable",
                    "principal",
                    "not_found",
                    epoch,
                )
            })?;
            if !principal.enabled {
                return Err(deny(
                    &tx,
                    actor,
                    "access principal disable",
                    "principal",
                    "already_disabled",
                    epoch,
                ));
            }
            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            principal.enabled = false;
            principal.disabled_at_epoch = Some(epoch + 1);
            principal.disabled_by_event_id = Some(event_id.clone());
            let effect = PolicyEffect {
                principals: vec![principal.clone()],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access principal disable",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                None,
                &[],
                &[],
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "principal_disabled".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.to_owned()),
                target_mapping_id: None,
                source: None,
                successor: None,
                delta: PolicyDelta::default(),
                effect,
            };
            let (seq, event_hash) =
                append_policy_event(&tx, "principal_disabled", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            principal_with_on(&tx, &principal)
        })();
        match outcome {
            Ok(principal) => {
                tx.commit()?;
                Ok(principal)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Clause 1 rebind: disable the source, mint a successor, transfer the
    /// source's active grants and mappings to new versioned rows, and retire the
    /// old rows, all in one `principal_rebound` event.
    pub fn rebind_principal(
        &mut self,
        source_id: &str,
        username: &str,
        uid: u32,
        replaces: &[String],
        actor: &PolicyActor,
    ) -> Result<Principal> {
        let username = username.trim().to_owned();
        if username.is_empty() {
            bail!("rebind requires a username");
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Principal> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access principal rebind")?;
            require_registry_admin(&tx, actor, "access principal rebind", epoch)?;
            let mut source = principal_row_on(&tx, source_id)?.ok_or_else(|| {
                deny(
                    &tx,
                    actor,
                    "access principal rebind",
                    "principal",
                    "not_found",
                    epoch,
                )
            })?;
            if !source.enabled {
                return Err(deny(
                    &tx,
                    actor,
                    "access principal rebind",
                    "principal",
                    "not_enabled",
                    epoch,
                ));
            }
            if source.username == username && source.uid == uid {
                return Err(deny(
                    &tx,
                    actor,
                    "access principal rebind",
                    "principal",
                    "same_pair",
                    epoch,
                ));
            }
            if let Err(error) = check_pair_collision(&tx, &username, uid, replaces, Some(source_id))
            {
                let _ = deny(
                    &tx,
                    actor,
                    "access principal rebind",
                    "principal",
                    "collision",
                    epoch,
                );
                bail!("{error}");
            }

            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            let successor_id = short_id("p");
            let successor = PrincipalRow {
                id: successor_id.clone(),
                username,
                uid,
                enabled: true,
                bound_at_epoch: epoch + 1,
                bound_by_event_id: event_id.clone(),
                disabled_at_epoch: None,
                disabled_by_event_id: None,
                successor_id: None,
                predecessor_id: Some(source_id.to_owned()),
                replaces: replaces.to_vec(),
            };
            source.enabled = false;
            source.disabled_at_epoch = Some(epoch + 1);
            source.disabled_by_event_id = Some(event_id.clone());
            source.successor_id = Some(successor_id.clone());

            let active_grants = active_grants_for_principal_on(&tx, source_id)?;
            let active_mappings = mappings_for_principal_on(&tx, source_id)?
                .into_iter()
                .filter(|m| m.unmapped_at_epoch.is_none())
                .collect::<Vec<_>>();

            let mut new_grants = Vec::new();
            let mut retired = Vec::new();
            let mut granted_ids = Vec::new();
            let mut retired_ids = Vec::new();
            for old in &active_grants {
                let id = short_id("pg");
                granted_ids.push(id.clone());
                retired_ids.push(old.id.clone());
                retired.push(GrantRetirement {
                    id: old.id.clone(),
                    retired_at_epoch: epoch + 1,
                    retired_by_event_id: event_id.clone(),
                });
                new_grants.push(Grant {
                    id,
                    principal_id: successor_id.clone(),
                    capability: old.capability,
                    scope: old.scope.clone(),
                    state: GrantState::Active,
                    origin: GrantOrigin::RebindTransfer,
                    granted_at_epoch: epoch + 1,
                    granted_by_principal_id: actor.principal_id.clone(),
                    granted_by_event_id: event_id.clone(),
                    retired_at_epoch: None,
                    retired_by_event_id: None,
                    transferred_from_grant_id: Some(old.id.clone()),
                });
            }
            let mut new_mappings = Vec::new();
            let mut unmapped = Vec::new();
            let mut mapped_ids = Vec::new();
            let mut unmapped_ids = Vec::new();
            for old in &active_mappings {
                let id = short_id("ps");
                mapped_ids.push(id.clone());
                unmapped_ids.push(old.id.clone());
                unmapped.push(MappingRetirement {
                    id: old.id.clone(),
                    unmapped_at_epoch: epoch + 1,
                    unmapped_by_event_id: event_id.clone(),
                });
                new_mappings.push(SsoMapping {
                    id,
                    principal_id: successor_id.clone(),
                    provider: old.provider.clone(),
                    subject: old.subject.clone(),
                    mapped_at_epoch: epoch + 1,
                    mapped_by_event_id: event_id.clone(),
                    unmapped_at_epoch: None,
                    unmapped_by_event_id: None,
                });
            }

            let effect = PolicyEffect {
                principals: vec![source.clone(), successor.clone()],
                grants: new_grants,
                retired_grants: retired,
                mappings: new_mappings,
                unmapped_mappings: unmapped,
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access principal rebind",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                None,
                &[],
                &[],
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let source_value = PrincipalValue {
                id: source.id.clone(),
                username: source.username.clone(),
                uid: source.uid,
                replaces: source.replaces.clone(),
            };
            let successor_value = PrincipalValue {
                id: successor.id.clone(),
                username: successor.username.clone(),
                uid: successor.uid,
                replaces: successor.replaces.clone(),
            };
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "principal_rebound".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: Some(successor_id.clone()),
                target_mapping_id: None,
                source: Some(source_value),
                successor: Some(successor_value),
                delta: PolicyDelta {
                    granted_grant_ids: granted_ids,
                    retired_grant_ids: retired_ids,
                    mapped_mapping_ids: mapped_ids,
                    unmapped_mapping_ids: unmapped_ids,
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) =
                append_policy_event(&tx, "principal_rebound", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            principal_with_on(&tx, &successor)
        })();
        match outcome {
            Ok(principal) => {
                tx.commit()?;
                Ok(principal)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Clause 5: grant an exact `(principal, tuple, capability)` triple.
    pub fn grant(
        &mut self,
        principal_id: &str,
        tuple: &ScopeTuple,
        capability: Capability,
        actor: &PolicyActor,
    ) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Grant> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access grant")?;
            require_registry_admin(&tx, actor, "access grant", epoch)?;
            require_grantor_holds(&tx, actor, tuple, capability, "access grant", epoch)?;
            let principal = principal_row_on(&tx, principal_id)?
                .ok_or_else(|| deny(&tx, actor, "access grant", "principal", "not_found", epoch))?;
            if !principal.enabled {
                return Err(deny(
                    &tx,
                    actor,
                    "access grant",
                    "principal",
                    "not_enabled",
                    epoch,
                ));
            }
            let existing = active_grants_for_principal_on(&tx, principal_id)?;
            if existing
                .iter()
                .any(|g| g.scope_tuple() == *tuple && g.capability == capability)
            {
                return Err(deny(
                    &tx,
                    actor,
                    "access grant",
                    "scope",
                    "already_held",
                    epoch,
                ));
            }

            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            let grant_id = short_id("pg");
            let grant = Grant {
                id: grant_id.clone(),
                principal_id: principal_id.to_owned(),
                capability,
                scope: tuple.to_atoms(),
                state: GrantState::Active,
                origin: GrantOrigin::Grant,
                granted_at_epoch: epoch + 1,
                granted_by_principal_id: actor.principal_id.clone(),
                granted_by_event_id: event_id.clone(),
                retired_at_epoch: None,
                retired_by_event_id: None,
                transferred_from_grant_id: None,
            };
            let effect = PolicyEffect {
                grants: vec![grant.clone()],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access grant",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                Some(capability),
                std::slice::from_ref(tuple),
                std::slice::from_ref(&grant_id),
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "grant_added".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.to_owned()),
                target_mapping_id: None,
                source: None,
                successor: None,
                delta: PolicyDelta {
                    granted_grant_ids: vec![grant_id],
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) = append_policy_event(&tx, "grant_added", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            Ok(grant)
        })();
        match outcome {
            Ok(grant) => {
                tx.commit()?;
                Ok(grant)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Clause 5: revoke the exact `(principal, tuple, capability)` triple,
    /// retiring exactly that row and refusing when no active row matches.
    pub fn revoke(
        &mut self,
        principal_id: &str,
        tuple: &ScopeTuple,
        capability: Capability,
        actor: &PolicyActor,
    ) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Grant> =
            (|| {
                let epoch = policy_epoch_on(&tx)?;
                require_context(&tx, actor, "access revoke")?;
                require_registry_admin(&tx, actor, "access revoke", epoch)?;
                require_grantor_holds(&tx, actor, tuple, capability, "access revoke", epoch)?;
                let mut grant = active_grants_for_principal_on(&tx, principal_id)?
                .into_iter()
                .find(|g| g.scope_tuple() == *tuple && g.capability == capability)
                .ok_or_else(|| {
                    // Refuse by naming the exact triple, per clause 5, and
                    // append the denied access-audit row (clause 4).
                    let _ = deny(&tx, actor, "access revoke", "scope", "missing_triple", epoch);
                    anyhow::anyhow!(
                        "denied or not found: no active grant for principal {principal_id} on {} \
                         at capability {}",
                        tuple.to_atoms().join(" "),
                        capability.as_str()
                    )
                })?;

                let seq = journal_next_seq(&tx, "policy_events")?;
                let event_id = policy_event_id(seq);
                grant.state = GrantState::Revoked;
                grant.retired_at_epoch = Some(epoch + 1);
                grant.retired_by_event_id = Some(event_id.clone());
                let effect = PolicyEffect {
                    retired_grants: vec![GrantRetirement {
                        id: grant.id.clone(),
                        retired_at_epoch: epoch + 1,
                        retired_by_event_id: event_id.clone(),
                    }],
                    ..Default::default()
                };
                apply_effect(&tx, &effect)?;
                let resulting_state_hash = compute_state_hash(&tx)?;
                let occurred_at = crate::registry::now_ms();
                let audit = allowed_audit(
                    actor,
                    "access revoke",
                    epoch + 1,
                    &enforcement_state_on(&tx)?,
                    Some(capability),
                    std::slice::from_ref(tuple),
                    &[grant.id.clone()],
                );
                append_access_audit(&tx, &audit)?;
                let audit_id = audit.id.clone();
                let event = PolicyEventPayload {
                    id: event_id.clone(),
                    kind: "grant_revoked".to_owned(),
                    occurred_at,
                    before_epoch: epoch,
                    after_epoch: epoch + 1,
                    access_audit_event_id: Some(audit_id),
                    actor_principal_id: actor.principal_id.clone(),
                    actor_username: actor.username.clone(),
                    actor_uid: actor.uid,
                    context: actor.context.clone(),
                    target_principal_id: Some(principal_id.to_owned()),
                    target_mapping_id: None,
                    source: None,
                    successor: None,
                    delta: PolicyDelta {
                        revoked_grant_ids: vec![grant.id.clone()],
                        ..Default::default()
                    },
                    effect,
                };
                let (seq, event_hash) =
                    append_policy_event(&tx, "grant_revoked", occurred_at, &event)?;
                append_policy_epoch(
                    &tx,
                    &PolicyEpochPayload {
                        epoch: epoch + 1,
                        policy_event_seq: seq,
                        policy_event_hash: event_hash,
                        previous_state_hash: actor.state_hash.clone(),
                        resulting_state_hash,
                        occurred_at,
                    },
                )?;
                Ok(grant)
            })();
        match outcome {
            Ok(grant) => {
                tx.commit()?;
                Ok(grant)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Link `(provider, subject)` to an existing enabled principal.
    pub fn map_sso(
        &mut self,
        principal_id: &str,
        provider: &str,
        subject: &str,
        actor: &PolicyActor,
    ) -> Result<SsoMapping> {
        if provider != "google" {
            bail!("the only supported SSO provider is google");
        }
        if subject.is_empty()
            || subject.len() > 255
            || !subject
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
        {
            bail!("invalid SSO subject");
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<SsoMapping> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access map-sso")?;
            require_registry_admin(&tx, actor, "access map-sso", epoch)?;
            let principal = principal_row_on(&tx, principal_id)?.ok_or_else(|| {
                deny(
                    &tx,
                    actor,
                    "access map-sso",
                    "principal",
                    "not_found",
                    epoch,
                )
            })?;
            if !principal.enabled {
                return Err(deny(
                    &tx,
                    actor,
                    "access map-sso",
                    "principal",
                    "not_enabled",
                    epoch,
                ));
            }
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM sso_mappings WHERE provider=?1 AND subject=?2 AND unmapped_at_epoch IS NULL",
                    params![provider, subject],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Err(deny(
                    &tx,
                    actor,
                    "access map-sso",
                    "scope",
                    "already_mapped",
                    epoch,
                ));
            }

            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            let mapping_id = short_id("ps");
            let mapping = SsoMapping {
                id: mapping_id.clone(),
                principal_id: principal_id.to_owned(),
                provider: provider.to_owned(),
                subject: subject.to_owned(),
                mapped_at_epoch: epoch + 1,
                mapped_by_event_id: event_id.clone(),
                unmapped_at_epoch: None,
                unmapped_by_event_id: None,
            };
            let effect = PolicyEffect {
                mappings: vec![mapping.clone()],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access map-sso",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                None,
                &[],
                &[],
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "sso_mapped".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.to_owned()),
                target_mapping_id: Some(mapping_id.clone()),
                source: None,
                successor: None,
                delta: PolicyDelta {
                    mapped_mapping_ids: vec![mapping_id],
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) = append_policy_event(&tx, "sso_mapped", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            Ok(mapping)
        })();
        match outcome {
            Ok(mapping) => {
                tx.commit()?;
                Ok(mapping)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Retire the `(provider, subject)` mapping.
    pub fn unmap_sso(
        &mut self,
        provider: &str,
        subject: &str,
        actor: &PolicyActor,
    ) -> Result<SsoMapping> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<SsoMapping> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            require_context(&tx, actor, "access unmap-sso")?;
            require_registry_admin(&tx, actor, "access unmap-sso", epoch)?;
            let mut mapping = tx
                .query_row(
                    "SELECT id,principal_id,provider,subject,mapped_at_epoch,mapped_by_event_id,\
                            unmapped_at_epoch,unmapped_by_event_id \
                     FROM sso_mappings WHERE provider=?1 AND subject=?2 AND unmapped_at_epoch IS NULL",
                    params![provider, subject],
                    mapping_row,
                )
                .optional()?
                .ok_or_else(|| {
                    anyhow::anyhow!("denied or not found: no active mapping for {provider}:{subject}")
                })?;

            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            mapping.unmapped_at_epoch = Some(epoch + 1);
            mapping.unmapped_by_event_id = Some(event_id.clone());
            let effect = PolicyEffect {
                unmapped_mappings: vec![MappingRetirement {
                    id: mapping.id.clone(),
                    unmapped_at_epoch: epoch + 1,
                    unmapped_by_event_id: event_id.clone(),
                }],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access unmap-sso",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                None,
                &[],
                &[],
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "sso_unmapped".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: actor.principal_id.clone(),
                actor_username: actor.username.clone(),
                actor_uid: actor.uid,
                context: actor.context.clone(),
                target_principal_id: None,
                target_mapping_id: Some(mapping.id.clone()),
                source: None,
                successor: None,
                delta: PolicyDelta {
                    unmapped_mapping_ids: vec![mapping.id.clone()],
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) = append_policy_event(&tx, "sso_unmapped", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            Ok(mapping)
        })();
        match outcome {
            Ok(mapping) => {
                tx.commit()?;
                Ok(mapping)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Clause 7 break-glass STATE: append one `admin` grant on `{registry}` to
    /// an existing, enabled principal, with origin `breakglass_registry_admin`,
    /// `actorUsername: "root"`, and a null `grantedByPrincipalID`. The command
    /// that invokes it is t-86eb4fb3.
    pub fn breakglass_grant_registry_admin(
        &mut self,
        principal_id: &str,
        actor: &PolicyActor,
    ) -> Result<Grant> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome: Result<Grant> = (|| {
            let epoch = policy_epoch_on(&tx)?;
            if epoch == 0 {
                return Err(deny(
                    &tx,
                    actor,
                    "access breakglass registry-admin",
                    "enforcement",
                    "empty_policy",
                    epoch,
                ));
            }
            let principal = principal_row_on(&tx, principal_id)?.ok_or_else(|| {
                deny(
                    &tx,
                    actor,
                    "access breakglass registry-admin",
                    "principal",
                    "not_found",
                    epoch,
                )
            })?;
            if !principal.enabled {
                return Err(deny(
                    &tx,
                    actor,
                    "access breakglass registry-admin",
                    "principal",
                    "not_enabled",
                    epoch,
                ));
            }
            if active_grants_for_principal_on(&tx, principal_id)?
                .iter()
                .any(|g| {
                    g.scope_tuple() == ScopeTuple::Registry && g.capability == Capability::Admin
                })
            {
                return Err(deny(
                    &tx,
                    actor,
                    "access breakglass registry-admin",
                    "scope",
                    "already_admin",
                    epoch,
                ));
            }

            let seq = journal_next_seq(&tx, "policy_events")?;
            let event_id = policy_event_id(seq);
            let grant_id = short_id("pg");
            let grant = Grant {
                id: grant_id.clone(),
                principal_id: principal_id.to_owned(),
                capability: Capability::Admin,
                scope: ScopeTuple::Registry.to_atoms(),
                state: GrantState::Active,
                origin: GrantOrigin::BreakglassRegistryAdmin,
                granted_at_epoch: epoch + 1,
                granted_by_principal_id: None,
                granted_by_event_id: event_id.clone(),
                retired_at_epoch: None,
                retired_by_event_id: None,
                transferred_from_grant_id: None,
            };
            let effect = PolicyEffect {
                grants: vec![grant.clone()],
                ..Default::default()
            };
            apply_effect(&tx, &effect)?;
            let resulting_state_hash = compute_state_hash(&tx)?;
            let occurred_at = crate::registry::now_ms();
            let audit = allowed_audit(
                actor,
                "access breakglass registry-admin",
                epoch + 1,
                &enforcement_state_on(&tx)?,
                Some(Capability::Admin),
                std::slice::from_ref(&ScopeTuple::Registry),
                std::slice::from_ref(&grant_id),
            );
            append_access_audit(&tx, &audit)?;
            let audit_id = audit.id.clone();
            let event = PolicyEventPayload {
                id: event_id.clone(),
                kind: "breakglass_registry_admin".to_owned(),
                occurred_at,
                before_epoch: epoch,
                after_epoch: epoch + 1,
                access_audit_event_id: Some(audit_id),
                actor_principal_id: None,
                actor_username: "root".to_owned(),
                actor_uid: 0,
                context: actor.context.clone(),
                target_principal_id: Some(principal_id.to_owned()),
                target_mapping_id: None,
                source: None,
                successor: None,
                delta: PolicyDelta {
                    granted_grant_ids: vec![grant_id],
                    ..Default::default()
                },
                effect,
            };
            let (seq, event_hash) =
                append_policy_event(&tx, "breakglass_registry_admin", occurred_at, &event)?;
            append_policy_epoch(
                &tx,
                &PolicyEpochPayload {
                    epoch: epoch + 1,
                    policy_event_seq: seq,
                    policy_event_hash: event_hash,
                    previous_state_hash: actor.state_hash.clone(),
                    resulting_state_hash,
                    occurred_at,
                },
            )?;
            Ok(grant)
        })();
        match outcome {
            Ok(grant) => {
                tx.commit()?;
                Ok(grant)
            }
            Err(error) => {
                let _ = tx.commit();
                Err(error)
            }
        }
    }

    /// Rebuild the projection from `policy_events` and verify it against the
    /// materialized tables and every `policy_epochs` state hash.
    pub fn replay_policy(&self) -> Result<()> {
        replay_policy_on(&self.connection)
    }
}

/// Replay `policy_events` from epoch 0 into a scratch projection and require it
/// to reproduce the materialized tables and the state hash at every epoch.
fn replay_policy_on(connection: &Connection) -> Result<()> {
    let events: Vec<String> = {
        let mut statement = connection.prepare("SELECT payload FROM policy_events ORDER BY seq")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let epochs: Vec<PolicyEpochPayload> = {
        let mut statement = connection.prepare("SELECT payload FROM policy_epochs ORDER BY seq")?;
        let payloads: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        payloads
            .into_iter()
            .map(|payload| {
                serde_json::from_str::<PolicyEpochPayload>(&payload).context("parse epoch payload")
            })
            .collect::<Result<Vec<_>>>()?
    };

    let mut principals: HashMap<String, PrincipalRow> = HashMap::new();
    let mut grants: HashMap<String, Grant> = HashMap::new();
    let mut mappings: HashMap<String, SsoMapping> = HashMap::new();
    let mut enforcement = "direct".to_owned();
    let mut prior_epoch = 0;

    for (index, payload) in events.iter().enumerate() {
        let event: PolicyEventPayload =
            serde_json::from_str(payload).context("parse policy event payload")?;
        if event.before_epoch != prior_epoch || event.after_epoch != prior_epoch + 1 {
            bail!(
                "policy event {} epochs are not monotonic: {} -> {} after {}",
                index + 1,
                event.before_epoch,
                event.after_epoch,
                prior_epoch
            );
        }
        prior_epoch = event.after_epoch;
        for principal in &event.effect.principals {
            principals.insert(principal.id.clone(), principal.clone());
        }
        for grant in &event.effect.grants {
            grants.insert(grant.id.clone(), grant.clone());
        }
        for retirement in &event.effect.retired_grants {
            if let Some(grant) = grants.get_mut(&retirement.id) {
                grant.state = GrantState::Retired;
                grant.retired_at_epoch = Some(retirement.retired_at_epoch);
                grant.retired_by_event_id = Some(retirement.retired_by_event_id.clone());
            }
        }
        for mapping in &event.effect.mappings {
            mappings.insert(mapping.id.clone(), mapping.clone());
        }
        for retirement in &event.effect.unmapped_mappings {
            if let Some(mapping) = mappings.get_mut(&retirement.id) {
                mapping.unmapped_at_epoch = Some(retirement.unmapped_at_epoch);
                mapping.unmapped_by_event_id = Some(retirement.unmapped_by_event_id.clone());
            }
        }
        if let Some(state) = &event.effect.enforcement {
            enforcement = state.clone();
        }
    }

    let mut live_principals: Vec<PrincipalRow> = all_principal_rows_on(connection)?;
    live_principals.sort_by(|a, b| a.id.cmp(&b.id));
    let mut replayed_principals: Vec<PrincipalRow> = principals.into_values().collect();
    replayed_principals.sort_by(|a, b| a.id.cmp(&b.id));
    if live_principals != replayed_principals {
        bail!("replayed principals do not match the materialized projection");
    }

    let mut live_grants: Vec<Grant> = all_grants_on(connection)?;
    live_grants.sort_by(|a, b| a.id.cmp(&b.id));
    let mut replayed_grants: Vec<Grant> = grants.into_values().collect();
    replayed_grants.sort_by(|a, b| a.id.cmp(&b.id));
    if live_grants != replayed_grants {
        bail!("replayed grants do not match the materialized projection");
    }

    let mut live_mappings: Vec<SsoMapping> = all_mappings_on(connection)?;
    live_mappings.sort_by(|a, b| a.id.cmp(&b.id));
    let mut replayed_mappings: Vec<SsoMapping> = mappings.into_values().collect();
    replayed_mappings.sort_by(|a, b| a.id.cmp(&b.id));
    if live_mappings != replayed_mappings {
        bail!("replayed mappings do not match the materialized projection");
    }

    if enforcement_state_on(connection)? != enforcement {
        bail!("replayed enforcement state does not match the materialized projection");
    }

    // Verify the state hash against every epoch row.
    let mut state_hash = compute_empty_state_hash();
    let mut expected_epoch = 0;
    for epoch in &epochs {
        if epoch.epoch != expected_epoch + 1 {
            bail!("epochs are not consecutive");
        }
        if epoch.previous_state_hash != state_hash {
            bail!(
                "epoch {} previous state hash does not match replay",
                epoch.epoch
            );
        }
        state_hash = epoch.resulting_state_hash.clone();
        expected_epoch = epoch.epoch;
    }
    let final_hash = compute_state_hash(connection)?;
    if final_hash != state_hash {
        bail!("final replayed state hash does not match the materialized projection");
    }
    Ok(())
}

/// An `explain` receipt (clause 5 read path).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExplainReceipt {
    #[serde(rename = "principalID")]
    pub principal_id: String,
    pub capability: Capability,
    pub required_scopes: Vec<Vec<String>>,
    pub policy_epoch: i64,
    pub policy_state_hash: String,
    pub enforcement_state: String,
    pub outcome: String,
    pub matched_grant_ids: Vec<String>,
    pub denial_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_registry(name: &str) -> Registry {
        let root = std::env::temp_dir().join(format!(
            "kanban-policy-{name}-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create temp policy dir");
        Registry::open_test_at(&root).expect("open test registry")
    }

    fn context() -> PolicyContext {
        PolicyContext {
            authn_kind: "socket_peer".to_owned(),
            peer_uid: 1000,
            real_uid: None,
            effective_uid: None,
            client_kind: "cli".to_owned(),
            request_id: short_id("rq"),
            claimed_actor: Some("geoyws".to_owned()),
            reason: Some("test".to_owned()),
            provider: None,
            subject: None,
        }
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
            context: context(),
        }
    }

    /// Re-mint an actor at the live epoch (clause 8: each operation runs under
    /// a fresh context).
    fn mint(registry: &Registry, principal_id: &str, username: &str, uid: u32) -> PolicyActor {
        let (epoch, hash) = registry.live_policy_state().unwrap();
        actor(Some(principal_id), username, uid, epoch, &hash)
    }

    fn root(registry: &Registry) -> PolicyActor {
        let (epoch, hash) = registry.live_policy_state().unwrap();
        actor(None, "root", 0, epoch, &hash)
    }

    /// Bootstrap a registry with one registered board (`b1`); returns
    /// `(registry, admin principal id)`. Registering the board before bootstrap
    /// seeds the admin `admin` on `{board:b1}` and `{board:b1,*}` — without it
    /// the bootstrap admin holds only `{registry}` (clause 5: registry admin
    /// implies nothing on any board) and could not grant board tuples.
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
        let root_actor = root(&registry);
        let admin = registry
            .policy_bootstrap("geoyws", 1000, &root_actor)
            .unwrap();
        (registry, admin.row.id.clone())
    }

    // -- clause 5 lattice ---------------------------------------------------

    #[test]
    fn lattice_defaults_to_none_at_every_tuple_shape() {
        let authority = authority(Vec::new());
        for tuple in [
            ScopeTuple::Registry,
            ScopeTuple::Board {
                board_id: "b1".into(),
            },
            ScopeTuple::BoardTag {
                board_id: "b1".into(),
                tag: "s".into(),
            },
            ScopeTuple::BoardWildcard {
                board_id: "b1".into(),
            },
        ] {
            assert!(
                !satisfies(&authority, &tuple, Capability::Read),
                "empty authority must deny read on {tuple:?}"
            );
        }
    }

    #[test]
    fn lattice_joins_two_levels_on_one_tuple_to_the_max() {
        let tuple = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        let authority = authority([
            (tuple.clone(), Capability::Read),
            (tuple.clone(), Capability::Write),
        ]);
        assert!(satisfies(&authority, &tuple, Capability::Write));
        assert!(satisfies(&authority, &tuple, Capability::Read));
        assert!(!satisfies(&authority, &tuple, Capability::Admin));
        assert_eq!(authority.get(&tuple), Some(&Capability::Write));
    }

    #[test]
    fn lattice_tuples_are_incomparable() {
        let registry_tuple = ScopeTuple::Registry;
        let board_tuple = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        let tag_tuple = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        let other_board_tag = ScopeTuple::BoardTag {
            board_id: "b2".into(),
            tag: "s".into(),
        };

        let authority = authority([
            (registry_tuple.clone(), Capability::Admin),
            (board_tuple.clone(), Capability::Admin),
        ]);
        // registry admin grants nothing on a board or tag tuple
        assert!(!satisfies(&authority, &tag_tuple, Capability::Read));
        // board admin grants nothing on a tag tuple
        assert!(!satisfies(&authority, &tag_tuple, Capability::Read));
        // no cross-board leak
        assert!(!satisfies(&authority, &other_board_tag, Capability::Read));
        // but the exact tuples themselves hold
        assert!(satisfies(&authority, &registry_tuple, Capability::Admin));
        assert!(satisfies(&authority, &board_tuple, Capability::Admin));
    }

    #[test]
    fn wildcard_satisfies_tag_tuples_at_equal_and_higher_capability() {
        let wildcard = ScopeTuple::BoardWildcard {
            board_id: "b1".into(),
        };
        let tag = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        let authority = authority([(wildcard.clone(), Capability::Write)]);

        assert!(satisfies(&authority, &tag, Capability::Read));
        assert!(satisfies(&authority, &tag, Capability::Write));
        // NON-satisfaction below the wildcard's capability
        assert!(!satisfies(&authority, &tag, Capability::Admin));
    }

    #[test]
    fn wildcard_is_not_a_parent_of_tag_tuples() {
        let wildcard = ScopeTuple::BoardWildcard {
            board_id: "b1".into(),
        };
        let tag = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        let wildcard_authority = authority([(wildcard.clone(), Capability::Write)]);
        let tag_authority = authority([(tag.clone(), Capability::Admin)]);

        // A later tag grant does not exceed the wildcard: the wildcard authority
        // alone is still only write.
        assert!(!satisfies(&wildcard_authority, &tag, Capability::Admin));
        // And a tag grant does not lower or raise the wildcard itself.
        assert!(!satisfies(&tag_authority, &wildcard, Capability::Read));
        assert!(satisfies(&tag_authority, &tag, Capability::Admin));
        // The wildcard only satisfies tags on its own board.
        let other_board_tag = ScopeTuple::BoardTag {
            board_id: "b2".into(),
            tag: "s".into(),
        };
        assert!(!satisfies(
            &wildcard_authority,
            &other_board_tag,
            Capability::Read
        ));
    }

    // -- grantor non-escalation --------------------------------------------

    #[test]
    fn grantor_with_write_cannot_grant_admin_on_the_same_tuple() {
        let (mut registry, admin_id) = bootstrapped("no-escalation-1");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let carol = registry
            .bind_principal("carol", 3000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .grant(&bob, &ScopeTuple::Registry, Capability::Admin, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &board, Capability::Write, &admin_actor)
            .unwrap();
        let bob_actor = mint(&registry, &bob, "bob", 2000);

        // bob (registry admin, but only write on the board) cannot grant admin.
        let err = registry
            .grant(&carol, &board, Capability::Admin, &bob_actor)
            .expect_err("bob with write must not grant admin");
        assert_eq!(err.to_string(), "denied or not found");
        // bob can grant at or below write.
        registry
            .grant(&carol, &board, Capability::Write, &bob_actor)
            .unwrap();
    }

    #[test]
    fn grantor_cannot_grant_on_a_tuple_it_holds_nothing_at() {
        let (mut registry, admin_id) = bootstrapped("no-escalation-2");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let carol = registry
            .bind_principal("carol", 3000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .grant(&bob, &ScopeTuple::Registry, Capability::Admin, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let b1 = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &b1, Capability::Write, &admin_actor)
            .unwrap();
        let bob_actor = mint(&registry, &bob, "bob", 2000);

        let b2 = ScopeTuple::Board {
            board_id: "b2".into(),
        };
        let err = registry
            .grant(&carol, &b2, Capability::Read, &bob_actor)
            .expect_err("bob holds nothing on b2");
        assert_eq!(err.to_string(), "denied or not found");
    }

    #[test]
    fn grantor_can_grant_via_the_wildcard_rule() {
        let (mut registry, admin_id) = bootstrapped("wildcard-grant");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let carol = registry
            .bind_principal("carol", 3000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .grant(&bob, &ScopeTuple::Registry, Capability::Admin, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let wildcard = ScopeTuple::BoardWildcard {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &wildcard, Capability::Write, &admin_actor)
            .unwrap();
        let bob_actor = mint(&registry, &bob, "bob", 2000);

        let tag = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        // bob satisfies (tag, write) via the wildcard, so may grant it.
        registry
            .grant(&carol, &tag, Capability::Write, &bob_actor)
            .unwrap();
        // but may not grant admin (above what the wildcard satisfies).
        let err = registry
            .grant(&carol, &tag, Capability::Admin, &bob_actor)
            .expect_err("wildcard write must not escalate to admin");
        assert_eq!(err.to_string(), "denied or not found");
    }

    #[test]
    fn revoke_of_a_missing_active_triple_refuses_naming_the_triple() {
        let (mut registry, admin_id) = bootstrapped("revoke-missing");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        let err = registry
            .revoke(&bob, &board, Capability::Read, &admin_actor)
            .expect_err("revoking a never-granted triple must refuse");
        let message = err.to_string();
        assert!(message.contains(&bob), "{message}");
        assert!(message.contains("b1"), "{message}");
        assert!(message.contains("read"), "{message}");
    }

    // -- store behavior ----------------------------------------------------

    #[test]
    fn bind_freezes_a_pair_and_fail_closed_uid_reuse_denies_divergence() {
        let (mut registry, admin_id) = bootstrapped("bind-fail-closed");
        let admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let alice = registry
            .bind_principal("alice", 1001, &[], &admin_actor)
            .unwrap()
            .row;
        assert!(alice.id.starts_with("p-"));
        assert_eq!(alice.username, "alice");
        assert_eq!(alice.uid, 1001);
        assert!(alice.enabled);
        let full = registry.principal(&alice.id).unwrap().unwrap();
        assert!(full.grants.is_empty());
        assert!(full.sso_mappings.is_empty());

        // The two-way check: a username-only or uid-only divergence denies.
        assert!(registry.resolve_principal("alice", 9999).unwrap().is_none());
        assert!(
            registry
                .resolve_principal("mallory", 1001)
                .unwrap()
                .is_none()
        );
        let resolved = registry.resolve_principal("alice", 1001).unwrap();
        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap().row.id, alice.id);
    }

    #[test]
    fn disable_makes_grants_ineffective_without_deleting() {
        let (mut registry, admin_id) = bootstrapped("disable");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &board, Capability::Read, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry.disable_principal(&bob, &admin_actor).unwrap();
        let principal = registry.principal(&bob).unwrap().unwrap();
        assert!(!principal.row.enabled);
        assert!(principal.row.disabled_at_epoch.is_some());
        assert!(registry.resolve_principal("bob", 2000).unwrap().is_none());
        assert_eq!(principal.grants.len(), 1);
    }

    #[test]
    fn rebind_transfers_grants_and_retires_the_source_in_one_event() {
        let (mut registry, admin_id) = bootstrapped("rebind");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let alice = registry
            .bind_principal("alice", 1001, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&alice, &board, Capability::Write, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let tag = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        registry
            .grant(&alice, &tag, Capability::Read, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .map_sso(&alice, "google", "subject-1", &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);

        let successor = registry
            .rebind_principal(&alice, "alice2", 1002, &[], &admin_actor)
            .unwrap()
            .row;
        assert_eq!(successor.predecessor_id.as_deref(), Some(alice.as_str()));
        assert_eq!(successor.uid, 1002);

        let old = registry.principal(&alice).unwrap().unwrap().row;
        assert!(!old.enabled);
        assert_eq!(old.successor_id.as_deref(), Some(successor.id.as_str()));

        let successor_full = registry.principal(&successor.id).unwrap().unwrap();
        assert_eq!(successor_full.grants.len(), 2);
        for grant in &successor_full.grants {
            assert_eq!(grant.origin, GrantOrigin::RebindTransfer);
            assert!(grant.transferred_from_grant_id.is_some());
        }
        assert_eq!(successor_full.sso_mappings.len(), 1);
        let old_grants = registry.principal(&alice).unwrap().unwrap().grants;
        assert_eq!(old_grants.len(), 2);
        assert!(old_grants.iter().all(|g| g.state == GrantState::Retired));
    }

    #[test]
    fn bootstrap_seeds_registry_and_every_board_tuple() {
        let mut registry = test_registry("bootstrap-boards");
        registry
            .connection
            .execute(
                "INSERT INTO boards(board_path,name,created_at,last_used_at,archived) \
                 VALUES('/root/boards/11111111-1111-1111-1111-111111111111.db','a',1,1,0)",
                [],
            )
            .unwrap();
        registry
            .connection
            .execute(
                "INSERT INTO boards(board_path,name,created_at,last_used_at,archived) \
                 VALUES('/root/boards/22222222-2222-2222-2222-222222222222.db','b',1,1,0)",
                [],
            )
            .unwrap();
        let root_actor = root(&registry);
        let admin = registry
            .policy_bootstrap("geoyws", 1000, &root_actor)
            .unwrap();
        // 1 registry + 2 boards * (board + wildcard) = 5 seeded grants.
        assert_eq!(admin.grants.len(), 5);
        assert_eq!(registry.policy_epoch().unwrap(), 1);
        for grant in &admin.grants {
            assert_eq!(grant.origin, GrantOrigin::Bootstrap);
            assert!(grant.granted_by_principal_id.is_none());
            assert_eq!(grant.capability, Capability::Admin);
        }
        assert!(
            admin
                .grants
                .iter()
                .any(|g| g.scope_tuple() == ScopeTuple::Registry)
        );
        assert!(admin.grants.iter().any(|g| {
            g.scope_tuple()
                == ScopeTuple::Board {
                    board_id: "11111111-1111-1111-1111-111111111111".into(),
                }
        }));
        assert!(admin.grants.iter().any(|g| {
            g.scope_tuple()
                == ScopeTuple::BoardWildcard {
                    board_id: "22222222-2222-2222-2222-222222222222".into(),
                }
        }));
    }

    #[test]
    fn breakglass_registry_admin_grants_with_root_origin_and_null_grantor() {
        let (mut registry, admin_id) = bootstrapped("breakglass");
        let admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        let root_actor = root(&registry);
        let grant = registry
            .breakglass_grant_registry_admin(&bob, &root_actor)
            .unwrap();
        assert_eq!(grant.origin, GrantOrigin::BreakglassRegistryAdmin);
        assert!(grant.granted_by_principal_id.is_none());
        assert_eq!(grant.scope_tuple(), ScopeTuple::Registry);
        assert_eq!(grant.capability, Capability::Admin);
    }

    #[test]
    fn replay_rebuilds_the_projection_and_matches_every_epoch_hash() {
        let (mut registry, admin_id) = bootstrapped("replay");
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &board, Capability::Write, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let tag = ScopeTuple::BoardTag {
            board_id: "b1".into(),
            tag: "s".into(),
        };
        registry
            .grant(&bob, &tag, Capability::Read, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .revoke(&bob, &board, Capability::Write, &admin_actor)
            .unwrap();
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry
            .map_sso(&bob, "google", "sub-123", &admin_actor)
            .unwrap();
        registry
            .replay_policy()
            .expect("replay must reproduce the projection");

        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        registry.disable_principal(&bob, &admin_actor).unwrap();
        registry.replay_policy().expect("replay after disable");
    }

    #[test]
    fn stale_context_is_refused_with_a_generic_denial() {
        let (mut registry, admin_id) = bootstrapped("stale-context");
        let admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        let stale = actor(Some(&admin_id), "geoyws", 1000, 0, "deadbeef");
        let err = registry
            .grant(&bob, &board, Capability::Read, &stale)
            .expect_err("a stale context must be refused");
        assert_eq!(err.to_string(), "denied or not found");
    }

    fn active_registry(name: &str) -> Registry {
        let (mut registry, admin_id) = bootstrapped(name);
        let mut admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let bob = registry
            .bind_principal("bob", 2000, &[], &admin_actor)
            .unwrap()
            .row
            .id;
        admin_actor = mint(&registry, &admin_id, "geoyws", 1000);
        let board = ScopeTuple::Board {
            board_id: "b1".into(),
        };
        registry
            .grant(&bob, &board, Capability::Read, &admin_actor)
            .unwrap();
        registry
    }

    #[test]
    fn audit_verify_fails_on_a_tampered_policy_event() {
        let registry = active_registry("tamper-policy-events");
        assert!(
            audit::verify_registry(&registry.connection)
                .unwrap()
                .healthy
        );
        registry
            .connection
            .execute(
                "UPDATE policy_events SET payload='{}' WHERE seq=(SELECT MAX(seq) FROM policy_events)",
                [],
            )
            .unwrap();
        let report = audit::verify_registry(&registry.connection).unwrap();
        assert!(!report.healthy, "tampered policy_events must fail verify");
    }

    #[test]
    fn audit_verify_fails_on_a_tampered_policy_epoch() {
        let registry = active_registry("tamper-policy-epochs");
        assert!(
            audit::verify_registry(&registry.connection)
                .unwrap()
                .healthy
        );
        registry
            .connection
            .execute(
                "UPDATE policy_epochs SET payload='{}' WHERE seq=(SELECT MAX(seq) FROM policy_epochs)",
                [],
            )
            .unwrap();
        let report = audit::verify_registry(&registry.connection).unwrap();
        assert!(!report.healthy, "tampered policy_epochs must fail verify");
    }

    #[test]
    fn audit_verify_fails_on_a_tampered_access_audit_row() {
        let registry = active_registry("tamper-access-audit");
        assert!(
            audit::verify_registry(&registry.connection)
                .unwrap()
                .healthy
        );
        registry
            .connection
            .execute(
                "UPDATE access_audit SET payload='{}' WHERE seq=(SELECT MAX(seq) FROM access_audit)",
                [],
            )
            .unwrap();
        let report = audit::verify_registry(&registry.connection).unwrap();
        assert!(!report.healthy, "tampered access_audit must fail verify");
    }
}
