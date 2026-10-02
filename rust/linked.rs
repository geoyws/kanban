//! Cross-board companion pairings and bounded claim scope (slice LINKED).
//!
//! Row `t-0dcbb1a9`; spec `docs/specs/linked.md` LINKED-01..LINKED-08 and
//! LINKED-10..LINKED-14; decisions ADR-051 §§1-3 and ADR-041 (a `transact`
//! batch never spans boards, so these registry commands refuse board
//! selectors and never run inside one).
//!
//! Three things live here and nothing else: the registry-owned companion rows
//! (one stored pairing, projected identically from either side), the selected
//! work sets with their audited revision journals and worker bindings, and
//! the one claim-scope gate every claim path checks through. Contributions
//! and delivery evidence are `t-9eff9257`'s slice, exposure and workflow
//! proof `t-db6937ba`'s; the tables here carry no column those slices would
//! have to rename, only additive ones to extend.
//!
//! Lock ordering is registry first, then board, everywhere: a claim takes the
//! registry scope guard ([`ScopeGuard`]) before its board write scope, and a
//! membership write takes only the registry. No path ever takes them in the
//! opposite order, so racing revocations and claims serialize instead of
//! deadlocking: exactly one wins and the loser observes the winner.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::authz::AuthzContext;
use crate::model::DependencyRef;

/// The refusal an endpoint gets when it cannot be used, whatever the cause.
/// One sentence for every cause — unknown board UUID, unknown item ID,
/// retired board or item, foreign registry, or an item the caller may not
/// read — so the answer never confirms which applied and never carries a
/// content byte (LINKED-06, mirroring `cross::UNAVAILABLE_SENTENCE`).
pub(crate) const UNAVAILABLE_ENDPOINT: &str =
    "cannot be used as a shared endpoint: it is unavailable to this caller. Nothing was written";

/// The shape every shared endpoint is named in: the spelling commissioned
/// for both slices, defined here from the approved scope rather than taken
/// from the sibling (LINKED-02, LINKED-05).
pub(crate) const ENDPOINT_SHAPE: &str =
    "shared endpoints name (boardID, id) with boardID a board UUID in canonical \
     lowercase hyphenated form and id the exact item ID; board names, board paths, \
     and @# tokens are display conveniences, never persisted identity and never \
     resolved as identity at write time";

/// A selected-set member as stored: the exact board UUID plus the exact item
/// ID, and nothing else (LINKED-02).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SetMember {
    #[serde(rename = "boardID")]
    pub board_id: String,
    pub id: String,
}

/// The `(actor, lane, session)` triple a binding names (LINKED-14), with
/// absent lane/session normalized to the empty string so an omitted flag and
/// an explicitly empty one compare identically on every path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeTriple {
    pub actor: String,
    pub lane: String,
    pub session: String,
}

pub(crate) fn normalize_triple(actor: &str, lane: Option<&str>, session: Option<&str>) -> ScopeTriple {
    ScopeTriple {
        actor: actor.to_owned(),
        lane: lane.unwrap_or("").to_owned(),
        session: session.unwrap_or("").to_owned(),
    }
}

fn triple_sentence(triple: &ScopeTriple) -> String {
    format!(
        "actor {}, lane {}, session {}",
        triple.actor,
        if triple.lane.is_empty() {
            "(none)"
        } else {
            &triple.lane
        },
        if triple.session.is_empty() {
            "(none)"
        } else {
            &triple.session
        }
    )
}

/// Whether this registry file holds the LINKED tables (registry v18). A
/// pre-upgrade registry has no shared state at all, so writes refuse naming
/// the owner upgrade while reads — including the claim gate — behave exactly
/// as if nobody ever paired anything.
pub(crate) fn linked_tables_present(connection: &Connection) -> Result<bool> {
    for table in [
        "linked_companions",
        "linked_sets",
        "linked_set_revisions",
        "linked_bindings",
        "linked_deliverables",
        "linked_contributions",
        "linked_closures",
    ] {
        let exists: i64 = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        if exists == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn require_linked_tables(connection: &Connection) -> Result<()> {
    if linked_tables_present(connection)? {
        return Ok(());
    }
    bail!(
        "this registry has not taken its linked upgrade (registry v18), so it holds no \
         companions, selected sets, bindings, or delivery evidence. Its owner upgrades it by \
         running the same `kanban init --name NAME` that registered its boards; unpaired \
         boards keep working exactly as before"
    )
}

/// The caller for registry-level LINKED reads and writes: the process's own
/// kernel identity and worker credential, minted the way
/// [`crate::routing::board_authz`] mints a board open, but without a board —
/// the endpoints re-scope it per board below. In a direct estate the guard
/// no-ops exactly like every board open there.
fn linked_caller() -> Result<AuthzContext> {
    let enforcement = crate::routing::enforcement_state()?;
    if !enforcement.is_managed() {
        return Ok(AuthzContext::direct(String::new()));
    }
    // Under `managed` a failure to establish authority is an absence of
    // authority, mirroring `board_authz`: an empty map denies through the one
    // generic refusal rather than erroring into an oracle.
    match crate::routing::local_caller() {
        Ok(caller) => Ok(AuthzContext::new(enforcement, caller.authority, String::new())
            .with_caller(caller.principal_id, caller.worker)),
        Err(_) => Ok(AuthzContext::new(enforcement, HashMap::new(), String::new())),
    }
}

/// The caller's authority re-scoped to one endpoint's board, including the
/// task-root confinement `board_authz` applies: a root-confined worker
/// reaches only its root's subtree on its root's own board (IDENT-04,
/// IDENT-06), and nothing anywhere else.
fn confine_to_task_root(
    caller: &AuthzContext,
    board_path: &Path,
    board_id: &str,
) -> Result<AuthzContext> {
    let Some(root) = caller
        .worker()
        .and_then(|worker| worker.task_root.clone())
    else {
        return Ok(caller.for_board(board_id.to_owned()));
    };
    if root.board_id != board_id {
        bail!("confined worker reaches only its root board");
    }
    let connection = crate::db::open_board_readonly(board_path)?;
    let scope = crate::store::Store::task_subtree_on(&connection, &root.task_id)?;
    Ok(caller
        .for_board(board_id.to_owned())
        .with_task_scope(scope))
}

/// Read one item's tags for an authority check. An endpoint that resolved
/// live has a readable row; a row that cannot be read contributes no tags, so
/// board scope alone decides.
fn item_tags(board_path: &Path, item_id: &str) -> Vec<String> {
    crate::db::open_board_readonly(board_path)
        .and_then(|connection| {
            let mut statement =
                connection.prepare("SELECT tag FROM task_tags WHERE task_id=? ORDER BY tag")?;
            let tags = statement
                .query_map([item_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(tags)
        })
        .unwrap_or_default()
}

/// Resolve one `(boardID, id)` reference to its pinned incarnation, or `None`
/// for every way it can fail to be a readable, current, registered,
/// token-matching row this caller may read: unknown board, unknown item,
/// retired board or item, foreign registry, or denied. One answer, so the
/// caller learns nothing about which cause it was (LINKED-06); the sibling's
/// resolver does the fail-closed work, this wrapper adds only the task-root
/// confinement that resolver predates.
fn resolve_endpoint(
    root: &Path,
    caller: &AuthzContext,
    reference: &DependencyRef,
) -> Option<crate::cross::ResolvedSource> {
    let registered = crate::cross::registered_board(root, &reference.board_id).ok()??;
    let scoped = confine_to_task_root(caller, &registered.path, &reference.board_id).ok()?;
    let resolved = crate::cross::resolve_source(root, &scoped, reference)?;
    scoped.check_task(&reference.id).ok()?;
    Some(resolved)
}

/// Validate the `(boardID, id)` shape both endpoints arrive in, before
/// anything is resolved or written (LINKED-02).
fn require_endpoint_shape(board_id: &str, id: &str, which: &str) -> Result<DependencyRef> {
    if !crate::cross::is_canonical_uuid(board_id) || id.is_empty() {
        bail!("{which} endpoint must name {ENDPOINT_SHAPE}");
    }
    Ok(DependencyRef {
        board_id: board_id.to_owned(),
        id: id.to_owned(),
    })
}

/// Whether a SQLite error is a constraint failure (unique, primary key, or
/// check): the shape a lost write race takes.
fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::ConstraintViolation,
                ..
            },
            _,
        )
    )
}

/// Order two resolved endpoints canonically, so the pair `(X, Y)` and the
/// pair `(Y, X)` address the same stored row and the symmetry is a property
/// of that row rather than of two rows kept in step (LINKED-03).
fn order_resolved(
    first: crate::cross::ResolvedSource,
    second: crate::cross::ResolvedSource,
) -> (
    crate::cross::ResolvedSource,
    crate::cross::ResolvedSource,
) {
    if (second.board_id.clone(), second.item_id.clone())
        < (first.board_id.clone(), first.item_id.clone())
    {
        (second, first)
    } else {
        (first, second)
    }
}

/// One stored companion pairing as `link show` projects it: the same pair
/// with the same attribution from either side (LINKED-01), each endpoint
/// carrying the incarnation it was paired with plus its state as it stands
/// now (LINKED-04).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompanionView {
    #[serde(rename = "pairingID")]
    pub pairing_id: String,
    pub endpoints: Vec<CompanionEndpointView>,
    pub created_by: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompanionEndpointView {
    #[serde(rename = "boardID")]
    pub board_id: String,
    pub id: String,
    pub incarnation: String,
    /// `live`, `recreated`, or `retired`: the pinned incarnation still
    /// current, the ID reused under a new incarnation, or the endpoint gone.
    /// Never the replacement row's content (LINKED-04).
    pub state: String,
}

/// The liveness of one pinned endpoint as it stands now.
fn endpoint_state(
    root: &Path,
    board_id: &str,
    item_id: &str,
    pinned: &str,
) -> String {
    let registered = match crate::cross::registered_board(root, board_id) {
        Ok(registered) => registered,
        Err(_) => return "retired".to_owned(),
    };
    let Some(registered) = registered else {
        return "retired".to_owned();
    };
    let board = match crate::db::open_board_readonly(&registered.path) {
        Ok(board) => board,
        Err(_) => return "retired".to_owned(),
    };
    let current: Option<Option<String>> = board
        .query_row(
            "SELECT incarnation FROM tasks WHERE id=? AND archived=0",
            [item_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);
    match current {
        None => "retired".to_owned(),
        Some(None) => "retired".to_owned(),
        Some(Some(current)) if current == pinned => "live".to_owned(),
        Some(Some(_)) => "recreated".to_owned(),
    }
}

#[derive(Debug, Clone)]
struct CompanionRow {
    id: String,
    board_a_id: String,
    item_a_id: String,
    incarnation_a: String,
    board_b_id: String,
    item_b_id: String,
    incarnation_b: String,
    created_by: String,
    created_at: i64,
}

fn companion_row(row: &rusqlite::Row) -> rusqlite::Result<CompanionRow> {
    Ok(CompanionRow {
        id: row.get("id")?,
        board_a_id: row.get("board_a_id")?,
        item_a_id: row.get("item_a_id")?,
        incarnation_a: row.get("item_a_incarnation")?,
        board_b_id: row.get("board_b_id")?,
        item_b_id: row.get("item_b_id")?,
        incarnation_b: row.get("item_b_incarnation")?,
        created_by: row.get("created_by")?,
        created_at: row.get("created_at")?,
    })
}

fn live_pairing(
    connection: &Connection,
    board_a_id: &str,
    item_a_id: &str,
    board_b_id: &str,
    item_b_id: &str,
) -> Result<Option<CompanionRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_companions WHERE board_a_id=? AND item_a_id=? AND \
             board_b_id=? AND item_b_id=? AND retired=0",
            params![board_a_id, item_a_id, board_b_id, item_b_id],
            companion_row,
        )
        .optional()?)
}

fn retired_pairing(
    connection: &Connection,
    board_a_id: &str,
    item_a_id: &str,
    board_b_id: &str,
    item_b_id: &str,
) -> Result<Option<CompanionRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_companions WHERE board_a_id=? AND item_a_id=? AND \
             board_b_id=? AND item_b_id=? AND retired<>0 ORDER BY retired_at DESC, rowid DESC LIMIT 1",
            params![board_a_id, item_a_id, board_b_id, item_b_id],
            companion_row,
        )
        .optional()?)
}

/// Require write authority on both endpoints' boards for a pairing write, so
/// a caller who may read but not write — including a bound worker — is
/// refused before any row lands, naming the missing authority (LINKED-01).
fn require_pairing_authority(
    caller: &AuthzContext,
    first: &crate::cross::ResolvedSource,
    first_path: &Path,
    second: &crate::cross::ResolvedSource,
    second_path: &Path,
) -> Result<()> {
    for (resolved, path) in [(first, first_path), (second, second_path)] {
        let scoped = caller.for_board(resolved.board_id.clone());
        let tags = item_tags(path, &resolved.item_id);
        if scoped.check_write(&tags, &tags).is_err() {
            bail!(
                "pairing needs write authority on both endpoints' boards, and this caller may \
                 not write board {}; nothing was written",
                resolved.board_id
            );
        }
    }
    Ok(())
}

fn companion_receipt(row: &CompanionRow, retired: bool) -> serde_json::Value {
    json!({
        "pairingID": row.id,
        "boardA": {"boardID": row.board_a_id, "id": row.item_a_id, "incarnation": row.incarnation_a},
        "boardB": {"boardID": row.board_b_id, "id": row.item_b_id, "incarnation": row.incarnation_b},
        "createdBy": row.created_by,
        "createdAt": row.created_at,
        "retired": retired,
    })
}

/// Pair two endpoints by `(boardID, id)` on both ends: audited, idempotent,
/// symmetric (LINKED-01..LINKED-04, LINKED-06).
pub(crate) fn add_companion(
    connection: &Connection,
    root: &Path,
    a_board: &str,
    a_id: &str,
    b_board: &str,
    b_id: &str,
    actor: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let first = require_endpoint_shape(a_board, a_id, "first")?;
    let second = require_endpoint_shape(b_board, b_id, "second")?;
    if first == second {
        bail!("cannot pair an item with itself: the two endpoints name the same (boardID, id)");
    }
    let caller = linked_caller()?;
    let Some(resolved_first) = resolve_endpoint(root, &caller, &first) else {
        bail!("first endpoint {UNAVAILABLE_ENDPOINT}");
    };
    let Some(resolved_second) = resolve_endpoint(root, &caller, &second) else {
        bail!("second endpoint {UNAVAILABLE_ENDPOINT}");
    };
    let (one, two) = order_resolved(resolved_first, resolved_second);
    let path_one = root
        .join("boards")
        .join(format!("{}.db", one.board_id));
    let path_two = root
        .join("boards")
        .join(format!("{}.db", two.board_id));
    require_pairing_authority(&caller, &one, &path_one, &two, &path_two)?;
    if let Some(existing) = live_pairing(
        connection,
        &one.board_id,
        &one.item_id,
        &two.board_id,
        &two.item_id,
    )? {
        if existing.incarnation_a == one.incarnation && existing.incarnation_b == two.incarnation
        {
            // The identical write retried: answer the stored record rather
            // than writing a second row (LINKED-05, LINKED-24).
            return Ok(companion_receipt(&existing, false));
        }
        bail!(
            "a live pairing already pairs board {} item {} with board {} item {} under different \
             incarnations; re-pairing is explicit: retire it with `link remove` citing this \
             attempt, then pair again. Nothing was written",
            one.board_id, one.item_id, two.board_id, two.item_id
        );
    }

    let id = format!("lc-{}", uuid::Uuid::new_v4().simple());
    // A racing identical add may land its row between the live check above
    // and this INSERT; the live-only unique index turns that race into a
    // constraint error, which answers the stored row rather than failing the
    // retry (LINKED-24). Any other error propagates untouched.
    if let Err(error) = connection.execute(
        "INSERT INTO linked_companions(id,board_a_id,board_a_registration,item_a_id,\
         item_a_incarnation,board_b_id,board_b_registration,item_b_id,item_b_incarnation,\
         created_by,created_at,retired) VALUES(?,?,?,?,?,?,?,?,?,?,?,0)",
        params![
            id,
            one.board_id,
            one.registration,
            one.item_id,
            one.incarnation,
            two.board_id,
            two.registration,
            two.item_id,
            two.incarnation,
            actor,
            now,
        ],
    ) {
        if !is_unique_violation(&error) {
            return Err(error.into());
        }
        // The winner answers only when it pins the same incarnations this
        // attempt resolved: a conflicting duplicate still refuses rather
        // than adopting a stranger's row.
        if let Some(winner) = live_pairing(
            connection,
            &one.board_id,
            &one.item_id,
            &two.board_id,
            &two.item_id,
        )? {
            if winner.incarnation_a == one.incarnation
                && winner.incarnation_b == two.incarnation
            {
                return Ok(companion_receipt(&winner, false));
            }
        }
        return Err(error.into());
    }
    crate::audit::append_registry_event(
        connection,
        &id,
        "linked_companion_added",
        actor,
        &json!({
            "pairingID": id,
            "boardA": {"boardID": one.board_id, "registration": one.registration, "id": one.item_id, "incarnation": one.incarnation},
            "boardB": {"boardID": two.board_id, "registration": two.registration, "id": two.item_id, "incarnation": two.incarnation},
        })
        .to_string(),
        now,
    )?;
    let row = live_pairing(
        connection,
        &one.board_id,
        &one.item_id,
        &two.board_id,
        &two.item_id,
    )?
    .context("pairing was not stored")?;
    Ok(companion_receipt(&row, false))
}

/// Retire a pairing from either side: both exposures retire in the same
/// change, because there is only one stored row (LINKED-03).
pub(crate) fn remove_companion(
    connection: &Connection,
    root: &Path,
    a_board: &str,
    a_id: &str,
    b_board: &str,
    b_id: &str,
    actor: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let first = require_endpoint_shape(a_board, a_id, "first")?;
    let second = require_endpoint_shape(b_board, b_id, "second")?;
    if first == second {
        bail!("cannot unpair an item from itself: the two endpoints name the same (boardID, id)");
    }
    let caller = linked_caller()?;
    // Retiring is authorized like pairing, but a retired endpoint can no
    // longer resolve live: authorize against the stored pins instead, so a
    // pairing whose item has since retired can still be retired.
    let (a_board_id, a_item_id, b_board_id, b_item_id) =
        if (second.board_id.clone(), second.id.clone()) < (first.board_id.clone(), first.id.clone())
        {
            (second.board_id, second.id, first.board_id, first.id)
        } else {
            (first.board_id, first.id, second.board_id, second.id)
        };
    let live = live_pairing(
        connection,
        &a_board_id,
        &a_item_id,
        &b_board_id,
        &b_item_id,
    )?;
    let Some(live) = live else {
        if let Some(retired) = retired_pairing(
            connection,
            &a_board_id,
            &a_item_id,
            &b_board_id,
            &b_item_id,
        )? {
            return Ok(companion_receipt(&retired, true));
        }
        bail!(
            "no live pairing pairs board {a_board_id} item {a_item_id} with board {b_board_id} \
             item {b_item_id}; nothing was retired"
        );
    };
    for (board_id, item_id) in [(&a_board_id, &a_item_id), (&b_board_id, &b_item_id)] {
        let Some(registered) = crate::cross::registered_board(root, board_id)? else {
            bail!("first endpoint {UNAVAILABLE_ENDPOINT}");
        };
        let scoped = confine_to_task_root(&caller, &registered.path, board_id)
            .map(|scoped| scoped.for_board(board_id.clone()))
            .unwrap_or_else(|_| caller.for_board(board_id.clone()));
        let tags = item_tags(&registered.path, item_id);
        if scoped.check_write(&tags, &tags).is_err() {
            bail!(
                "retiring a pairing needs write authority on both endpoints' boards, and this \
                 caller may not write board {board_id}; nothing was written"
            );
        }
    }
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("`link remove` needs --reason stating why the pairing retires; nothing was retired");
    }
    connection.execute(
        "UPDATE linked_companions SET retired=1,retired_by=?,retired_at=?,retire_reason=? WHERE id=?",
        params![actor, now, reason, live.id],
    )?;
    crate::audit::append_registry_event(
        connection,
        &live.id,
        "linked_companion_removed",
        actor,
        &json!({"pairingID": live.id, "reason": reason}).to_string(),
        now,
    )?;
    let row = retired_pairing(
        connection,
        &a_board_id,
        &a_item_id,
        &b_board_id,
        &b_item_id,
    )?
    .context("pairing retirement was not stored")?;
    Ok(companion_receipt(&row, true))
}

/// Read the live pairings touching one endpoint, projected identically from
/// either side (LINKED-01, LINKED-03, LINKED-05).
pub(crate) fn show_companions(
    connection: &Connection,
    root: &Path,
    board_id: &str,
    item_id: &str,
) -> Result<Vec<CompanionView>> {
    require_endpoint_shape(board_id, item_id, "queried")?;
    let caller = linked_caller()?;
    let mut statement = connection.prepare(
        "SELECT * FROM linked_companions WHERE retired=0 AND ((board_a_id=? AND item_a_id=?) OR \
         (board_b_id=? AND item_b_id=?)) ORDER BY created_at,id",
    )?;
    let rows = statement
        .query_map(
            params![board_id, item_id, board_id, item_id],
            companion_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        let mut endpoints = Vec::with_capacity(2);
        for (endpoint_board, endpoint_item, pinned) in [
            (
                row.board_a_id.clone(),
                row.item_a_id.clone(),
                row.incarnation_a.clone(),
            ),
            (
                row.board_b_id.clone(),
                row.item_b_id.clone(),
                row.incarnation_b.clone(),
            ),
        ] {
            let reference = DependencyRef {
                board_id: endpoint_board.clone(),
                id: endpoint_item.clone(),
            };
            // A live endpoint resolves and carries its current state; an
            // endpoint that no longer resolves is either retired (projected
            // as dangling, LINKED-04) or unreadable to this caller (refused
            // whole, LINKED-06) — never the replacement row's content.
            let state = match resolve_endpoint(root, &caller, &reference) {
                Some(_) => endpoint_state(root, &endpoint_board, &endpoint_item, &pinned),
                None => retired_or_refused(root, &caller, &reference)?,
            };
            endpoints.push(CompanionEndpointView {
                state,
                board_id: endpoint_board,
                id: endpoint_item,
                incarnation: pinned,
            });
        }
        views.push(CompanionView {
            pairing_id: row.id.clone(),
            endpoints,
            created_by: row.created_by.clone(),
            created_at: row.created_at,
        });
    }
    Ok(views)
}

/// The state of an endpoint that no longer resolves: `retired` when its row
/// is genuinely gone, or the uniform refusal when the row is still there but
/// unreadable to this caller (denied, corrupt, or token-mismatched — all fail
/// closed) or the caller may not read its board at all.
fn retired_or_refused(
    root: &Path,
    caller: &AuthzContext,
    reference: &DependencyRef,
) -> Result<String> {
    let refusal = || {
        anyhow::anyhow!("queried endpoint {UNAVAILABLE_ENDPOINT}")
    };
    let Some(registered) = crate::cross::registered_board(root, &reference.board_id)
        .map_err(|_| refusal())?
    else {
        // Unknown, ambiguous, or archived board: nothing exists to read, and
        // board scope already decided below — but without a board file there
        // is no row to deny, so check board scope and project retired.
        caller
            .for_board(reference.board_id.clone())
            .check_read(&[])
            .map_err(|_| refusal())?;
        return Ok("retired".to_owned());
    };
    let scoped = confine_to_task_root(&caller, &registered.path, &reference.board_id)
        .map_err(|_| refusal())?;
    scoped.check_read(&[]).map_err(|_| refusal())?;
    scoped.check_task(&reference.id).map_err(|_| refusal())?;
    let board = crate::db::open_board_readonly(&registered.path).map_err(|_| refusal())?;
    let present: bool = board
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?)",
            [&reference.id],
            |row| row.get(0),
        )
        .map_err(|_| refusal())?;
    if present {
        // The row stands but does not resolve to this caller: denied,
        // corrupt, or token-mismatched. Refuse rather than project.
        return Err(refusal());
    }
    Ok("retired".to_owned())
}

// ---------------------------------------------------------------------------
// Selected sets, revisions, bindings.
// ---------------------------------------------------------------------------

/// The revision kinds `linked_set_revisions` records: every change to a set
/// appends one audited revision carrying its number, author, reason, and the
/// exact resulting member list (LINKED-12).
const REVISION_KINDS: [&str; 9] = [
    "create", "add", "remove", "freeze", "unfreeze", "bind", "exit", "rebind", "release",
];

fn require_set_id(set_id: &str) -> Result<String> {
    let trimmed = set_id.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        bail!("a selected set is named by a non-empty id of at most 128 characters");
    }
    Ok(trimmed.to_owned())
}

fn parse_members(raw: &str) -> Result<Vec<SetMember>> {
    serde_json::from_str(raw).context("stored selected-set members are not valid JSON")
}

/// Canonical member list: sorted by `(boardID, id)` with repeats collapsed,
/// so the stored list — and the hash over it — is order-independent.
fn canonical_members(members: Vec<SetMember>) -> Vec<SetMember> {
    let mut members = members;
    members.sort();
    members.dedup();
    members
}

struct SetHead {
    revision: i64,
    members: Vec<SetMember>,
}

fn set_head(connection: &Connection, set_id: &str) -> Result<Option<(bool, SetHead)>> {
    let frozen: Option<i64> = connection
        .query_row(
            "SELECT frozen FROM linked_sets WHERE id=?",
            [set_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(frozen) = frozen else {
        return Ok(None);
    };
    let head: Option<(i64, String)> = connection
        .query_row(
            "SELECT revision,members FROM linked_set_revisions WHERE set_id=? ORDER BY revision DESC LIMIT 1",
            [set_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((revision, members)) = head else {
        bail!("selected set {set_id} has no revisions; its creation revision is missing");
    };
    Ok(Some((
        frozen != 0,
        SetHead {
            revision,
            members: parse_members(&members)?,
        },
    )))
}

/// Append one audited revision, chained into the registry hash chain the same
/// way the policy journals chain it, so `audit verify` covers every revision
/// (LINKED-12).
fn append_revision(
    connection: &Connection,
    set_id: &str,
    revision: i64,
    kind: &str,
    author: &str,
    reason: &str,
    members: &[SetMember],
    now: i64,
) -> Result<()> {
    debug_assert!(REVISION_KINDS.contains(&kind));
    let members_json = serde_json::to_string(members)?;
    let payload = json!({
        "set": set_id,
        "revision": revision,
        "kind": kind,
        "author": author,
        "reason": reason,
        "members": members,
    })
    .to_string();
    let (seq, prev_hash, event_hash) = crate::audit::next_chained(
        connection,
        "linked_set_revisions",
        "linked_set_revisions",
        Some(set_id),
        kind,
        &payload,
        now,
    )?;
    connection.execute(
        "INSERT INTO linked_set_revisions(seq,set_id,revision,kind,author,reason,members,\
         created_at,prev_hash,event_hash,payload) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
        params![
            seq, set_id, revision, kind, author, reason, members_json, now, prev_hash, event_hash,
            payload
        ],
    )?;
    Ok(())
}

fn set_receipt(set_id: &str, revision: i64, kind: &str, members: &[SetMember]) -> serde_json::Value {
    json!({
        "set": set_id,
        "revision": revision,
        "kind": kind,
        "members": members,
    })
}

/// Create an empty selected set at revision 1.
pub(crate) fn create_set(
    connection: &Connection,
    set_id: &str,
    actor: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM linked_sets WHERE id=?)",
            [&set_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if exists {
        bail!("selected set {set_id} already exists");
    }
    let actor = nonempty_actor(actor)?;
    connection.execute(
        "INSERT INTO linked_sets(id,created_by,created_at,frozen) VALUES(?,?,?,0)",
        params![set_id, actor, now],
    )?;
    let members: Vec<SetMember> = Vec::new();
    append_revision(
        connection,
        &set_id,
        1,
        "create",
        &actor,
        reason.trim(),
        &members,
        now,
    )?;
    Ok(set_receipt(&set_id, 1, "create", &members))
}

fn nonempty_actor(actor: &str) -> Result<String> {
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("an actor is required for an audited linked write");
    }
    Ok(actor.to_owned())
}

/// Require membership-write authority over the boards a set change touches:
/// every current member's board, plus the affected member's. A bound worker's
/// narrow authority fails here while an operator's passes, which is what
/// makes worker revisions unwritable to workers (LINKED-12).
fn require_membership_authority(
    caller: &AuthzContext,
    root: &Path,
    members: &[SetMember],
) -> Result<()> {
    for member in members {
        let Some(registered) = crate::cross::registered_board(root, &member.board_id)? else {
            bail!("member endpoint {UNAVAILABLE_ENDPOINT}");
        };
        let scoped = confine_to_task_root(&caller, &registered.path, &member.board_id)
            .map(|scoped| scoped.for_board(member.board_id.clone()))
            .unwrap_or_else(|_| caller.for_board(member.board_id.clone()));
        let tags = item_tags(&registered.path, &member.id);
        if scoped.check_write(&tags, &tags).is_err() {
            bail!(
                "changing a selected set needs write authority on every member's board, and this \
                 caller may not write board {}; nothing was written",
                member.board_id
            );
        }
    }
    Ok(())
}

/// Apply one membership change against its expected revision: a revision
/// written against a stale number is refused whole and must be re-read and
/// re-applied (LINKED-11). An already-true change answers the stored
/// revision instead of appending a duplicate.
pub(crate) fn mutate_set(
    connection: &Connection,
    root: &Path,
    set_id: &str,
    expected_revision: i64,
    kind: &str,
    member: Option<SetMember>,
    actor: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    if !["add", "remove", "freeze", "unfreeze"].contains(&kind) {
        bail!("unknown selected-set change {kind:?}");
    }
    let Some((frozen, head)) = set_head(connection, &set_id)? else {
        bail!("no such selected set {set_id}; nothing was written");
    };
    if head.revision != expected_revision {
        bail!(
            "selected set {set_id} is at revision {}, not {expected_revision}; re-read it with \
             `scope show --set {set_id}` and re-apply the change against the current revision. \
             Nothing was written",
            head.revision
        );
    }
    let caller = linked_caller()?;
    let actor = nonempty_actor(actor)?;
    let mut members = head.members.clone();
    match kind {
        "add" => {
            let member = member.context("`scope add` needs --board and --id naming the member")?;
            if member.board_id.is_empty() || member.id.is_empty() {
                bail!("added member must name {ENDPOINT_SHAPE}");
            }
            if frozen {
                bail!(
                    "selected set {set_id} is frozen: it takes no new members until an audited \
                     `scope unfreeze` revision thaws it. Nothing was written"
                );
            }
            let reference = require_endpoint_shape(&member.board_id, &member.id, "added")?;
            if resolve_endpoint(root, &caller, &reference).is_none() {
                bail!("added member {UNAVAILABLE_ENDPOINT}");
            }
            require_membership_authority(&caller, root, &[member.clone()])?;
            let canonical = SetMember {
                board_id: reference.board_id,
                id: reference.id,
            };
            if members.contains(&canonical) {
                return Ok(set_receipt(&set_id, head.revision, "add", &members));
            }
            members.push(canonical);
            members = canonical_members(members);
        }
        "remove" => {
            let member = member.context("`scope remove` needs --board and --id naming the member")?;
            require_membership_authority(&caller, root, &[member.clone()])?;
            let before = members.len();
            members.retain(|existing| existing != &member);
            if members.len() == before {
                return Ok(set_receipt(&set_id, head.revision, "remove", &members));
            }
        }
        "freeze" => {
            require_membership_authority(&caller, root, &members)?;
            if frozen {
                return Ok(set_receipt(&set_id, head.revision, "freeze", &members));
            }
        }
        "unfreeze" => {
            require_membership_authority(&caller, root, &members)?;
            if !frozen {
                return Ok(set_receipt(&set_id, head.revision, "unfreeze", &members));
            }
        }
        _ => unreachable!("checked above"),
    }
    let revision = head.revision + 1;
    if kind == "freeze" {
        connection.execute(
            "UPDATE linked_sets SET frozen=1 WHERE id=?",
            [&set_id],
        )?;
    } else if kind == "unfreeze" {
        connection.execute(
            "UPDATE linked_sets SET frozen=0 WHERE id=?",
            [&set_id],
        )?;
    }
    append_revision(
        connection,
        &set_id,
        revision,
        kind,
        &actor,
        reason.trim(),
        &members,
        now,
    )?;
    Ok(set_receipt(&set_id, revision, kind, &members))
}

// ---------------------------------------------------------------------------
// Bindings.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct BindingRow {
    id: String,
    set_id: String,
    actor: String,
    lane: String,
    session: String,
    revision_checked: i64,
    status: String,
    expires_at: i64,
    end_reason: Option<String>,
}

fn binding_row(row: &rusqlite::Row) -> rusqlite::Result<BindingRow> {
    Ok(BindingRow {
        id: row.get("id")?,
        set_id: row.get("set_id")?,
        actor: row.get("actor")?,
        lane: row.get("lane")?,
        session: row.get("session")?,
        revision_checked: row.get("revision_checked")?,
        status: row.get("status")?,
        expires_at: row.get("expires_at")?,
        end_reason: row.get("end_reason")?,
    })
}

/// The latest binding row for one exact triple on one set, if any.
fn latest_binding(
    connection: &Connection,
    set_id: &str,
    triple: &ScopeTriple,
) -> Result<Option<BindingRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_bindings WHERE set_id=? AND actor=? AND lane=? AND session=? \
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
            params![set_id, triple.actor, triple.lane, triple.session],
            binding_row,
        )
        .optional()?)
}

/// Every live binding one actor holds on one lane, across sets and sessions:
/// at most one may exist, so candidates and claims read the same binding.
fn live_bindings_for_lane(
    connection: &Connection,
    actor: &str,
    lane: &str,
    now: i64,
) -> Result<Vec<BindingRow>> {
    let mut statement = connection.prepare(
        "SELECT * FROM linked_bindings WHERE actor=? AND lane=? AND status='active' AND \
         expires_at>? ORDER BY created_at DESC, rowid DESC",
    )?;
    let rows = statement
        .query_map(params![actor, lane, now], binding_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every live binding one actor holds on any lane: a different lane naming
/// the same agent never inherits the binding (LINKED-14).
fn live_bindings_for_actor(
    connection: &Connection,
    actor: &str,
    now: i64,
) -> Result<Vec<BindingRow>> {
    let mut statement = connection.prepare(
        "SELECT * FROM linked_bindings WHERE actor=? AND status='active' AND expires_at>? \
         ORDER BY created_at DESC, rowid DESC",
    )?;
    let rows = statement
        .query_map(params![actor, now], binding_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The actor's most recently ended-by-revocation binding, across lanes,
/// sessions, and sets: what keeps refusing under any relabeled triple until an
/// audited rebind re-arms the actor (LINKED-13, LINKED-14).
fn latest_revoked_for_actor(
    connection: &Connection,
    actor: &str,
) -> Result<Option<BindingRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_bindings WHERE actor=? AND status='revoked' ORDER BY ended_at \
             DESC, created_at DESC, rowid DESC LIMIT 1",
            [actor],
            binding_row,
        )
        .optional()?)
}

fn binding_receipt(row: &BindingRow, revision: i64, kind: &str) -> serde_json::Value {
    json!({
        "bindingID": row.id,
        "set": row.set_id,
        "actor": row.actor,
        "lane": row.lane,
        "session": row.session,
        "revision": revision,
        "kind": kind,
        "status": row.status,
        "expiresAt": row.expires_at,
    })
}

/// Bind one `(actor, lane, session)` triple to a set: audited, exactly one
/// live binding per lane, rebind only by revoking first (LINKED-14).
#[allow(clippy::too_many_arguments)]
pub(crate) fn bind(
    connection: &Connection,
    root: &Path,
    set_id: &str,
    expected_revision: i64,
    actor: &str,
    lane: Option<&str>,
    session: Option<&str>,
    lease_minutes: i64,
    author: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    let triple = normalize_triple(actor, lane, session);
    if triple.actor.trim().is_empty() {
        bail!("`scope bind` needs --actor naming the worker being bound");
    }
    if !(15..=43200).contains(&lease_minutes) {
        bail!("--lease-minutes must be between 15 and 43200, got {lease_minutes}");
    }
    let Some((_, head)) = set_head(connection, &set_id)? else {
        bail!("no such selected set {set_id}; nothing was written");
    };
    if head.revision != expected_revision {
        bail!(
            "selected set {set_id} is at revision {}, not {expected_revision}; re-read it with \
             `scope show --set {set_id}` and re-apply the change against the current revision. \
             Nothing was written",
            head.revision
        );
    }
    // The latest row for this exact triple decides the kind: a live one
    // replays its stored receipt, while an ended one — revoked, released, or
    // lapsed — re-arms only through a new audited revision (LINKED-14: the old
    // triple resumes only through a new revision).
    let predecessor: Option<BindingRow> = match latest_binding(connection, &set_id, &triple)? {
        Some(existing) if existing.status == "active" && existing.expires_at > now => {
            return Ok(binding_receipt(&existing, head.revision, "bind"));
        }
        Some(existing) => Some(existing),
        None => None,
    };
    let rebind = predecessor.is_some();
    // One lane holds one live binding: any other live triple on this lane
    // refuses, so candidates and every claim path always read the same
    // binding (LINKED-08, LINKED-14).
    let live = live_bindings_for_lane(connection, &triple.actor, &triple.lane, now)?;
    if let Some(held) = live.first() {
        bail!(
            "{} is already bound to selected set {} (session {}); a lane holds one binding at a \
             time. Revoke or release it first, then bind again. Nothing was written",
            triple_sentence(&triple),
            held.set_id,
            if held.session.is_empty() {
                "(none)"
            } else {
                &held.session
            }
        );
    }
    let caller = linked_caller()?;
    require_membership_authority(&caller, root, &head.members)?;
    let author = nonempty_actor(author)?;
    let kind = if rebind { "rebind" } else { "bind" };
    let id = format!("lb-{}", uuid::Uuid::new_v4().simple());
    // A lapsed predecessor still reads `active` but holds no authority; record
    // its ending before the new row lands, so the live-only unique index never
    // sees two live rows for one triple. Revoked and released predecessors are
    // already ended and stay untouched as history.
    let lapsed_id: Option<String> = match &predecessor {
        Some(old) if old.status == "active" => Some(old.id.clone()),
        _ => None,
    };
    if let Some(old_id) = lapsed_id {
        connection.execute(
            "UPDATE linked_bindings SET status='released',ended_at=?,end_reason=? WHERE id=?",
            params![now, "binding lapsed; superseded by an audited rebind", old_id],
        )?;
    }
    let revision = head.revision + 1;
    connection.execute(
        "INSERT INTO linked_bindings(id,set_id,actor,lane,session,revision_checked,status,\
         created_at,expires_at) VALUES(?,?,?,?,?,?,'active',?,?)",
        params![
            id,
            set_id,
            triple.actor,
            triple.lane,
            triple.session,
            head.revision,
            now,
            now + lease_minutes * 60_000,
        ],
    )?;
    append_revision(
        connection,
        &set_id,
        revision,
        kind,
        &author,
        reason.trim(),
        &head.members,
        now,
    )?;
    let row = latest_binding(connection, &set_id, &triple)?.context("binding was not stored")?;
    Ok(binding_receipt(&row, revision, kind))
}

/// The operator's authorized exit: end one binding by revocation, stating the
/// reason in an audited revision (LINKED-13, LINKED-14). Taking ends
/// immediately; leases already granted keep their heartbeat until expiry or
/// release, but take nothing further.
#[allow(clippy::too_many_arguments)]
pub(crate) fn revoke(
    connection: &Connection,
    root: &Path,
    set_id: &str,
    expected_revision: i64,
    actor: &str,
    lane: Option<&str>,
    session: Option<&str>,
    author: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    let triple = normalize_triple(actor, lane, session);
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("`scope revoke` needs --reason stating why the binding ends; nothing was revoked");
    }
    let Some((_, head)) = set_head(connection, &set_id)? else {
        bail!("no such selected set {set_id}; nothing was written");
    };
    if head.revision != expected_revision {
        bail!(
            "selected set {set_id} is at revision {}, not {expected_revision}; re-read it with \
             `scope show --set {set_id}` and re-apply the change against the current revision. \
             Nothing was written",
            head.revision
        );
    }
    let Some(existing) = latest_binding(connection, &set_id, &triple)? else {
        bail!(
            "{} holds no binding on selected set {set_id}; nothing was revoked",
            triple_sentence(&triple)
        );
    };
    if existing.status == "revoked" {
        return Ok(binding_receipt(&existing, head.revision, "exit"));
    }
    if existing.status != "active" || existing.expires_at <= now {
        bail!(
            "{} holds no live binding on selected set {set_id}; nothing was revoked",
            triple_sentence(&triple)
        );
    }
    let caller = linked_caller()?;
    require_membership_authority(&caller, root, &head.members)?;
    let author = nonempty_actor(author)?;
    let revision = head.revision + 1;
    connection.execute(
        "UPDATE linked_bindings SET status='revoked',ended_at=?,end_reason=? WHERE id=?",
        params![now, reason, existing.id],
    )?;
    append_revision(
        connection,
        &set_id,
        revision,
        "exit",
        &author,
        reason,
        &head.members,
        now,
    )?;
    let row = latest_binding(connection, &set_id, &triple)?.context("revocation was not stored")?;
    Ok(binding_receipt(&row, revision, "exit"))
}

/// The bound worker releases its own binding: one of the triple's three
/// endings, needing no revision race check because contention resolves on
/// the live-binding lookup itself (LINKED-14).
pub(crate) fn release_binding(
    connection: &Connection,
    set_id: &str,
    actor: &str,
    lane: Option<&str>,
    session: Option<&str>,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    let triple = normalize_triple(actor, lane, session);
    let Some((_, head)) = set_head(connection, &set_id)? else {
        bail!("no such selected set {set_id}; nothing was written");
    };
    let Some(existing) = latest_binding(connection, &set_id, &triple)? else {
        bail!(
            "{} holds no binding on selected set {set_id}; nothing was released",
            triple_sentence(&triple)
        );
    };
    if existing.status == "released" {
        return Ok(binding_receipt(&existing, head.revision, "release"));
    }
    if existing.status != "active" || existing.expires_at <= now {
        bail!(
            "{} holds no live binding on selected set {set_id}; nothing was released",
            triple_sentence(&triple)
        );
    }
    let revision = head.revision + 1;
    connection.execute(
        "UPDATE linked_bindings SET status='released',ended_at=?,end_reason=? WHERE id=?",
        params![now, reason.trim(), existing.id],
    )?;
    append_revision(
        connection,
        &set_id,
        revision,
        "release",
        &triple.actor,
        reason.trim(),
        &head.members,
        now,
    )?;
    let row = latest_binding(connection, &set_id, &triple)?.context("release was not stored")?;
    Ok(binding_receipt(&row, revision, "release"))
}

/// Read one set with its current revision, members, and live bindings.
pub(crate) fn show_set(
    connection: &Connection,
    root: &Path,
    set_id: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let set_id = require_set_id(set_id)?;
    let Some((frozen, head)) = set_head(connection, &set_id)? else {
        bail!("no such selected set {set_id}");
    };
    let caller = linked_caller()?;
    let mut members = Vec::with_capacity(head.members.len());
    for member in &head.members {
        let reference = DependencyRef {
            board_id: member.board_id.clone(),
            id: member.id.clone(),
        };
        if resolve_endpoint(root, &caller, &reference).is_none() {
            bail!("member endpoint {UNAVAILABLE_ENDPOINT}");
        }
        members.push(json!({"boardID": member.board_id, "id": member.id}));
    }
    let mut statement = connection.prepare(
        "SELECT * FROM linked_bindings WHERE set_id=? AND status='active' AND expires_at>? \
         ORDER BY created_at",
    )?;
    let bindings = statement
        .query_map(params![set_id, now], binding_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    Ok(json!({
        "set": set_id,
        "revision": head.revision,
        "frozen": frozen,
        "members": members,
        "bindings": bindings.iter().map(|row| json!({
            "bindingID": row.id,
            "actor": row.actor,
            "lane": row.lane,
            "session": row.session,
            "revisionChecked": row.revision_checked,
            "expiresAt": row.expires_at,
        })).collect::<Vec<_>>(),
    }))
}

/// The board UUID behind one open connection: the file stem, or empty when
/// the connection names no file (in-memory tests), where no registry state
/// can constrain anything.
pub(crate) fn board_id_of(connection: &Connection) -> String {
    connection
        .path()
        .and_then(|path| {
            Path::new(path)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_owned)
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The one claim-scope gate.
// ---------------------------------------------------------------------------

/// What the gate decided for one claim path: the binding it was checked
/// against and the set revision it was checked against, recorded on every
/// granted claim's event (LINKED-07).
#[derive(Debug, Clone)]
pub(crate) struct ScopeDecision {
    pub set_id: String,
    pub revision: i64,
}

#[derive(Debug)]
enum GuardMode {
    /// No binding constrains this caller: proceed exactly as today.
    Pass,
    /// One live binding holds: only its set's members are claimable.
    Bound {
        set_id: String,
        revision: i64,
        frozen: bool,
        members: Vec<SetMember>,
    },
    /// The presented triple was revoked: nothing further is granted on it.
    Revoked { set_id: String, reason: String },
    /// The actor holds a live binding under a different triple: the presented
    /// triple matches nothing it holds.
    Mismatch {
        presented: ScopeTriple,
        held: ScopeTriple,
        held_set: String,
    },
}

/// The scope guard one claim holds across its board grant: the registry
/// connection with `BEGIN IMMEDIATE` taken, so a revocation or membership
/// revision racing the claim serializes against it instead of slipping
/// between its check and its lease (LINKED-11). Registry first, board second,
/// on every path; membership writes take only the registry, so no opposite
/// order exists to deadlock against.
pub(crate) struct ScopeGuard {
    connection: Option<Connection>,
    board_id: String,
    triple: ScopeTriple,
    mode: GuardMode,
    finished: bool,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if !self.finished {
            if let Some(connection) = self.connection.take() {
                let _ = connection.execute_batch("ROLLBACK");
            }
        }
    }
}

impl ScopeGuard {
    /// Take the guard for one claim triple on the board behind
    /// `board_connection`. Unbound callers — and boards with no registry or
    /// no linked state — get a passing guard that changes nothing.
    pub(crate) fn take(
        board_connection: &Connection,
        agent: &str,
        lane: Option<&str>,
        session: Option<&str>,
    ) -> Result<ScopeGuard> {
        let triple = normalize_triple(agent, lane, session);
        let blank = ScopeGuard {
            connection: None,
            board_id: String::new(),
            triple: triple.clone(),
            mode: GuardMode::Pass,
            finished: true,
        };
        let Some(path) = board_connection.path().map(PathBuf::from) else {
            return Ok(blank);
        };
        let Some(board_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(str::to_owned)
        else {
            return Ok(blank);
        };
        let root = match crate::db::registry_root_of(&path)? {
            Some(root) => root,
            None => return Ok(blank),
        };
        let registry_path = root.join("registry.db");
        // Fast path without any lock: a read-only look at whether this triple
        // is even constrained. Unbound callers never take the write intent.
        let readable = match crate::db::open_registry_readonly(&registry_path) {
            Ok(connection) => connection,
            Err(_) => return Ok(blank),
        };
        if !linked_tables_present(&readable)? {
            return Ok(blank);
        }
        let now = crate::registry::now_ms();
        // The binding lookup below is set-scoped, so scan the sets: cheap,
        // because estates hold a handful of sets, not a table of tasks.
        enum PreCheck {
            Free,
            Maybe,
        }
        let pre = {
            let mut statement = readable.prepare("SELECT id FROM linked_sets ORDER BY id")?;
            let sets = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let mut found = PreCheck::Free;
            for set_id in sets {
                if latest_binding(&readable, &set_id, &triple)?.is_some() {
                    found = PreCheck::Maybe;
                    break;
                }
                if !live_bindings_for_actor(&readable, &triple.actor, now)?.is_empty() {
                    found = PreCheck::Maybe;
                    break;
                }
            }
            // A standing revocation constrains the actor under any relabeled
            // triple, so it takes the serializing guard too.
            if matches!(found, PreCheck::Free)
                && latest_revoked_for_actor(&readable, &triple.actor)?.is_some()
            {
                found = PreCheck::Maybe;
            }
            found
        };
        drop(readable);
        if matches!(pre, PreCheck::Free) {
            return Ok(blank);
        }
        // Constrained: upgrade to the serializing open and re-read under the
        // write intent, so what the gate decides is what the grant sees.
        let connection = match crate::db::open_registry(&registry_path) {
            Ok(connection) => connection,
            Err(error) => bail!(
                "the claim-scope gate cannot lock the registry for {}: {error:#}; refusing rather \
                 than granting out of scope",
                triple_sentence(&triple)
            ),
        };
        if !linked_tables_present(&connection)? {
            return Ok(blank);
        }
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let mut guard = ScopeGuard {
            connection: Some(connection),
            board_id,
            triple,
            mode: GuardMode::Pass,
            finished: false,
        };
        guard.resolve(now)?;
        Ok(guard)
    }

    /// Re-read the binding state under the held write intent.
    fn resolve(&mut self, now: i64) -> Result<()> {
        let connection = self.connection.as_ref().context("scope guard lost its registry")?;
        let mut statement = connection.prepare("SELECT id FROM linked_sets ORDER BY id")?;
        let sets = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        // The presented triple first: a live binding scopes, a revoked one
        // refuses, whatever any other triple holds.
        for set_id in &sets {
            if let Some(binding) = latest_binding(connection, set_id, &self.triple)? {
                if binding.status == "active" && binding.expires_at > now {
                    let Some((frozen, head)) = set_head(connection, set_id)? else {
                        continue;
                    };
                    self.mode = GuardMode::Bound {
                        set_id: set_id.clone(),
                        revision: head.revision,
                        frozen,
                        members: head.members,
                    };
                    return Ok(());
                }
                if binding.status == "revoked" {
                    self.mode = GuardMode::Revoked {
                        set_id: set_id.clone(),
                        reason: binding
                            .end_reason
                            .clone()
                            .unwrap_or_else(|| "no reason recorded".to_owned()),
                    };
                    return Ok(());
                }
            }
        }
        // No live binding on the presented triple, but the actor holds one
        // elsewhere: the presented triple matches nothing it holds. Prefer a
        // binding on the same lane for the message; any live one mismatches.
        let live = live_bindings_for_actor(connection, &self.triple.actor, now)?;
        if !live.is_empty() {
            let held = live
                .iter()
                .find(|row| row.lane == self.triple.lane)
                .or(live.first())
                .expect("non-empty live bindings");
            let held_set = held.set_id.clone();
            self.mode = GuardMode::Mismatch {
                presented: self.triple.clone(),
                held: ScopeTriple {
                    actor: held.actor.clone(),
                    lane: held.lane.clone(),
                    session: held.session.clone(),
                },
                held_set,
            };
            return Ok(());
        }
        // No live binding anywhere for this actor, but a revocation stands:
        // the actor takes nothing further under any relabeled triple until an
        // audited rebind re-arms it (LINKED-13, LINKED-14).
        if let Some(revoked) = latest_revoked_for_actor(connection, &self.triple.actor)? {
            self.mode = GuardMode::Revoked {
                set_id: revoked.set_id.clone(),
                reason: revoked
                    .end_reason
                    .clone()
                    .unwrap_or_else(|| "no reason recorded".to_owned()),
            };
            return Ok(());
        }
        self.mode = GuardMode::Pass;
        Ok(())
    }

    /// Check one named task: `Ok(Some(decision))` grants with the revision to
    /// record, `Ok(None)` is unconstrained, `Err` is the one refusal every
    /// path shares (LINKED-08).
    pub(crate) fn check(&self, task_id: &str) -> Result<Option<ScopeDecision>> {
        match &self.mode {
            GuardMode::Pass => Ok(None),
            GuardMode::Revoked { set_id, reason } => bail!(
                "task {task_id} is not claimable under selected set {set_id}: {} was revoked \
                 ({reason}). Taking ended with the revocation: live leases keep their heartbeat \
                 until expiry or release with `release ID --lease TOKEN`, and no new claim, \
                 handoff, or resumption will be granted on this triple",
                triple_sentence(&self.triple)
            ),
            GuardMode::Mismatch {
                presented,
                held,
                held_set,
            } => bail!(
                "task {task_id} is not claimable under selected set {held_set}: {} matches no \
                 live binding; the actor holds lane {}, session {} on that set. Present the \
                 bound triple, or rebind with an audited `scope bind` revision",
                triple_sentence(presented),
                if held.lane.is_empty() {
                    "(none)".to_owned()
                } else {
                    held.lane.clone()
                },
                if held.session.is_empty() {
                    "(none)".to_owned()
                } else {
                    held.session.clone()
                }
            ),
            GuardMode::Bound {
                set_id,
                revision,
                frozen,
                members,
            } => {
                if *frozen {
                    bail!(
                        "task {task_id} is not claimable under selected set {set_id}: the set is \
                         frozen at revision {revision}, which parks taking while holding runs on. \
                         Live leases keep their heartbeat; new claims wait for an audited \
                         `scope unfreeze` revision"
                    );
                }
                let wanted = SetMember {
                    board_id: self.board_id.clone(),
                    id: task_id.to_owned(),
                };
                if !members.contains(&wanted) {
                    bail!(
                        "task {task_id} is not in selected set {set_id} (revision {revision}): only \
                         explicitly selected tasks are claimable, and no child, parent, epic-mate, \
                         or depends-on neighbour joins silently. Ask the operator to select it \
                         with `scope add --set {set_id} --board {} --id {task_id} \
                         --expect-revision {revision} --as OPERATOR`",
                        self.board_id
                    );
                }
                Ok(Some(ScopeDecision {
                    set_id: set_id.clone(),
                    revision: *revision,
                }))
            }
        }
    }

    /// Whether one pool row survives the gate: the filter behind candidates,
    /// `--next`, and every other enumeration (LINKED-08).
    pub(crate) fn allows(&self, task_id: &str) -> bool {
        match &self.mode {
            GuardMode::Pass => true,
            GuardMode::Revoked { .. } | GuardMode::Mismatch { .. } => false,
            GuardMode::Bound {
                frozen, members, ..
            } => {
                !frozen
                    && members.contains(&SetMember {
                        board_id: self.board_id.clone(),
                        id: task_id.to_owned(),
                    })
            }
        }
    }

    /// The refusal a `--next` pool emptied by the gate reports: the same
    /// sentence the named path would give, naming the set (LINKED-08).
    pub(crate) fn empty_pool_sentence(&self) -> Option<String> {
        match &self.mode {
            GuardMode::Pass => None,
            GuardMode::Revoked { set_id, reason } => Some(format!(
                "no claimable task under selected set {set_id}: {} was revoked ({reason}). Taking \
                 ended with the revocation; live leases keep their heartbeat until expiry or \
                 release",
                triple_sentence(&self.triple)
            )),
            GuardMode::Mismatch {
                presented,
                held,
                held_set,
            } => Some(format!(
                "no claimable task under selected set {held_set}: {} matches no live binding",
                triple_sentence(presented)
            )),
            GuardMode::Bound {
                set_id,
                revision,
                frozen,
                ..
            } => {
                if *frozen {
                    Some(format!(
                        "no claimable task under selected set {set_id}: the set is frozen at \
                         revision {revision}, which parks taking while holding runs on"
                    ))
                } else {
                    Some(format!(
                        "no claimable task under selected set {set_id} (revision {revision}): only \
                         explicitly selected tasks are claimable"
                    ))
                }
            }
        }
    }

    /// Release the registry write intent after the board grant committed.
    pub(crate) fn commit(mut self) -> Result<()> {
        self.finished = true;
        if let Some(connection) = self.connection.take() {
            connection.execute_batch("COMMIT")?;
        }
        Ok(())
    }
}

/// The read-only scope snapshot behind `claim --candidates`: the bound set's
/// members for the caller's lane, or `None` when nothing constrains the
/// caller. Revoked and lookalike callers see an empty set — candidates never
/// refuses, it only withholds (LINKED-08).
pub(crate) struct CandidateScope {
    pub frozen: bool,
    pub members: Vec<SetMember>,
}

impl CandidateScope {
    pub(crate) fn allows(&self, board_id: &str, task_id: &str) -> bool {
        !self.frozen
            && self.members.contains(&SetMember {
                board_id: board_id.to_owned(),
                id: task_id.to_owned(),
            })
    }
}

pub(crate) fn candidate_scope(
    board_connection: &Connection,
    agent: &str,
    lane: Option<&str>,
) -> Result<Option<CandidateScope>> {
    let Some(path) = board_connection.path().map(PathBuf::from) else {
        return Ok(None);
    };
    let Some(root) = crate::db::registry_root_of(&path)? else {
        return Ok(None);
    };
    let registry_path = root.join("registry.db");
    let connection = match crate::db::open_registry_readonly(&registry_path) {
        Ok(connection) => connection,
        Err(_) => return Ok(None),
    };
    if !linked_tables_present(&connection)? {
        return Ok(None);
    }
    let triple = normalize_triple(agent, lane, Some(""));
    let now = crate::registry::now_ms();
    // The caller's lane, across sessions: one lane holds at most one live
    // binding, so this is the same binding every claim path reads.
    let live = live_bindings_for_lane(&connection, &triple.actor, &triple.lane, now)?;
    if let Some(binding) = live.first() {
        let scope_set = binding.set_id.clone();
        if let Some((frozen, head)) = set_head(&connection, &scope_set)? {
            return Ok(Some(CandidateScope {
                frozen,
                members: head.members,
            }));
        }
        return Ok(None);
    }
    // No live binding on the lane: an actor bound elsewhere, or revoked, is
    // offered nothing the atomic path would grant.
    if !live_bindings_for_actor(&connection, &triple.actor, now)?.is_empty() {
        return Ok(Some(CandidateScope {
            frozen: true,
            members: Vec::new(),
        }));
    }
    // No live binding anywhere for this actor, but a revocation stands: offer
    // nothing under any relabeled lane, exactly as the atomic path refuses.
    if latest_revoked_for_actor(&connection, &triple.actor)?.is_some() {
        return Ok(Some(CandidateScope {
            frozen: true,
            members: Vec::new(),
        }));
    }
    Ok(None)
}

/// Whether the holder of one task's live lease may hand it onward: a revoked
/// binding's leases end only via `release`, never by handoff onward
/// (LINKED-13). Returns the revocation reason when barred.
pub(crate) fn holder_handoff_barred(
    board_connection: &Connection,
    task_id: &str,
    agent: &str,
    session: Option<&str>,
) -> Result<Option<String>> {
    let Some(path) = board_connection.path().map(PathBuf::from) else {
        return Ok(None);
    };
    let Some(board_id) = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let Some(root) = crate::db::registry_root_of(&path)? else {
        return Ok(None);
    };
    let registry_path = root.join("registry.db");
    let connection = match crate::db::open_registry_readonly(&registry_path) {
        Ok(connection) => connection,
        Err(_) => return Ok(None),
    };
    if !linked_tables_present(&connection)? {
        return Ok(None);
    }
    let session = session.unwrap_or("").to_owned();
    let now = crate::registry::now_ms();
    let wanted = SetMember {
        board_id: board_id.clone(),
        id: task_id.to_owned(),
    };
    // A lease held under a live binding covering this task hands onward: the
    // acceptor's gate still scopes the other end.
    let mut statement = connection.prepare(
        "SELECT * FROM linked_bindings WHERE actor=? AND session=? AND status='active' AND \
         expires_at>? ORDER BY created_at DESC, rowid DESC",
    )?;
    let live = statement
        .query_map(params![agent, session, now], binding_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for binding in live {
        if let Some((_, head)) = set_head(&connection, &binding.set_id)? {
            if head.members.contains(&wanted) {
                return Ok(None);
            }
        }
    }
    // Otherwise any standing revocation covering this task bars the handoff,
    // whatever lane the holder relabels it under: a revoked lease ends only
    // via `release`, never by handoff onward.
    let mut statement = connection.prepare(
        "SELECT * FROM linked_bindings WHERE actor=? AND status='revoked' ORDER BY ended_at \
         DESC, created_at DESC",
    )?;
    let revoked = statement
        .query_map([agent], binding_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for binding in revoked {
        if let Some((_, head)) = set_head(&connection, &binding.set_id)? {
            if head.members.contains(&wanted) {
                return Ok(Some(
                    binding
                        .end_reason
                        .clone()
                        .unwrap_or_else(|| "no reason recorded".to_owned()),
                ));
            }
        }
    }
    Ok(None)
}

/// Open the estate registry for a LINKED write: migrates forward when the
/// ordinary open may, and refuses plainly on a pre-CROSS registry whose owner
/// upgrade is still pending.
pub(crate) fn open_linked_registry_for_write() -> Result<(PathBuf, Connection)> {
    let root = crate::registry::data_root()?;
    let connection = crate::db::open_registry(&root.join("registry.db"))?;
    require_linked_tables(&connection)?;
    Ok((root, connection))
}

/// Open the estate registry for a LINKED read, migrating forward on the way
/// past exactly like [`crate::registry::Registry::open_for_read`].
pub(crate) fn open_linked_registry_for_read() -> Result<(PathBuf, Connection)> {
    let root = crate::registry::data_root()?;
    let path = root.join("registry.db");
    let connection = if crate::db::registry_schema_is_current(&path) {
        crate::db::open_registry_readonly(&path)?
    } else {
        crate::db::open_registry(&path)?
    };
    require_linked_tables(&connection)?;
    Ok((root, connection))
}

// ---------------------------------------------------------------------------
// Delivery evidence: append-only contribution and integration receipts.
// ---------------------------------------------------------------------------
//
// Slice LINKED delivery evidence (docs/specs/linked.md LINKED-15..LINKED-21,
// row `t-9eff9257`), built on the shared records above rather than beside
// them: every receipt names its `(boardID, id)` endpoint through
// [`require_endpoint_shape`], resolves it through [`resolve_endpoint`] so an
// unknown or denied endpoint gets the one [`UNAVAILABLE_ENDPOINT`] sentence,
// and joins the registry audit chain through
// [`crate::audit::append_registry_event`] exactly like the companion writes.
// Nothing here edits or deletes: a correction is a new receipt naming the
// receipt it corrects, and the superseded bytes stay stored byte-identical.
//
// Verification runs read-only git plumbing (`cat-file`, `merge-base`,
// `ls-tree`) against caller-given local machine paths, with
// `--no-optional-locks` and prompting disabled so a verify never writes and
// never waits on a credential. No credential flag exists and none is stored.
// A path that holds no readable repository records the receipt `unverified`,
// which satisfies nothing: missing access is never success (LINKED-19,
// LINKED-20). A readable repository whose objects contradict the record —
// wrong hash, wrong path, stale pin, non-submodule entry, changed candidate
// without a correction — refuses the write with the exact mismatch.

/// The four evidence roles, exactly one of which every code contribution
/// declares (LINKED-16).
pub(crate) const EVIDENCE_ROLES: [&str; 4] =
    ["baseline", "implementation", "integration", "candidate"];

/// The refusal a record with no role, two roles, or an unknown role gets: it
/// names the four roles and quotes what was declared (LINKED-16).
fn role_refusal(roles: &[String]) -> String {
    let declared = if roles.is_empty() {
        "none".to_owned()
    } else {
        roles
            .iter()
            .map(|role| format!("{role:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "a contribution declares exactly one evidence role — baseline (the starting HEAD the \
         work began from), implementation (a commit the work produced), integration (the \
         accepted merge or squash commit), or candidate (the tested consumer commit) — but \
         this record declares {declared}. A query for one role never returns another; nothing \
         was appended"
    )
}

/// A full object ID and nothing shorter: 40 hex (SHA-1) or 64 hex (SHA-256),
/// lowercased on the way in. Short hashes are refused at write time, never
/// expanded, and a moving `latest`/`HEAD` pointer is refused as what it is:
/// not proof of any deliverable (LINKED-15, LINKED-19).
fn require_full_sha(value: &str, what: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed == "latest" || trimmed == "HEAD" || trimmed == "head" {
        bail!(
            "{what} names {trimmed:?}, a moving pointer: it is not proof of any deliverable. \
             Record the full 40- or 64-character object ID observed at the observed time; \
             nothing was appended"
        );
    }
    if (trimmed.len() == 40 || trimmed.len() == 64)
        && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(trimmed.to_lowercase());
    }
    if trimmed.len() >= 7 && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!(
            "{what} carries a shortened object ID {trimmed:?}: short hashes are refused at \
             write time, never expanded. Record the full 40- or 64-character object ID; \
             nothing was appended"
        );
    }
    bail!(
        "{what} must be the full 40- or 64-character hexadecimal object ID, got {trimmed:?}; \
         nothing was appended"
    );
}

/// A stable repository identity: an opaque non-empty name, never a credential
/// carrier. A URL with userinfo is refused before it lands, because no
/// credential is ever stored on a receipt; machine access travels separately
/// in `--repo-path`, a local path.
fn require_repo_identity(value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!(
            "a contribution missing its repository identity is refused naming the missing \
             field: name the stable repository identity; nothing was appended"
        );
    }
    if let Some(at) = trimmed.find('@')
        && trimmed[..at].contains("://")
    {
        bail!(
            "repository identity {trimmed:?} carries credentials in a URL: no credentials are \
             stored on a contribution receipt. Name the stable repository identity without \
             userinfo (machine access goes through --repo-path, a local path); nothing was \
             appended"
        );
    }
    Ok(trimmed.to_owned())
}

/// A read-only machine path: local filesystem only. Remote addresses are out
/// of scope — a verify must never reach the network — so they are refused
/// with the local-path fix rather than probed.
fn require_local_path(value: &str, flag: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!(
            "--{flag} names an empty path: pass a local filesystem path or omit the flag (the \
             receipt is then recorded unverified); nothing was appended"
        );
    }
    if trimmed.contains("://") {
        bail!(
            "--{flag} names {trimmed:?}: machine paths are local filesystem paths read \
             read-only; remote repositories are out of scope. Point at a local checkout or \
             omit the flag (the receipt is then recorded unverified); nothing was appended"
        );
    }
    Ok(trimmed.to_owned())
}

/// The observed time as milliseconds since the Unix epoch: when the work was
/// observed, distinct from the recorded time the registry stamps itself.
fn require_observed_at(raw: &str) -> Result<i64> {
    match raw.trim().parse::<i64>() {
        Ok(ms) if ms > 0 => Ok(ms),
        _ => bail!(
            "--observed-at must be the observed time as milliseconds since the Unix epoch, got \
             {:?}; nothing was appended",
            raw.trim()
        ),
    }
}

/// Any other required receipt field: the actor, host, worktree, branch, and
/// the deliverable name. A contribution missing any identity field is refused
/// naming the missing field (LINKED-15).
fn require_evidence_field(value: &str, flag: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!(
            "a contribution missing --{flag} is refused naming the missing field; nothing was \
             appended"
        );
    }
    Ok(trimmed.to_owned())
}

/// Whether the machine path holds a readable repository, probed read-only:
/// `--no-optional-locks` takes no locks and `GIT_TERMINAL_PROMPT=0` never
/// prompts, so the probe neither writes nor waits on a credential.
fn git_repo_readable(path: &str) -> bool {
    std::process::Command::new("git")
        .args(["--no-optional-locks", "-C", path, "rev-parse", "--git-dir"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether `sha` is an object in the readable repository at `path`. The
/// caller checks readability first; a missing object there is a mismatch to
/// refuse, not missing access.
fn git_object_present(path: &str, sha: &str) -> bool {
    std::process::Command::new("git")
        .args(["--no-optional-locks", "-C", path, "cat-file", "-e", sha])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Whether `ancestor` reaches `descendant` in the readable repository at
/// `path`: `Some(true)` absorbs, `Some(false)` does not, `None` when git
/// itself could not answer (read as unverified, never as proof either way).
fn git_is_ancestor(path: &str, ancestor: &str, descendant: &str) -> Option<bool> {
    let output = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "-C",
            path,
            "merge-base",
            "--is-ancestor",
            ancestor,
            descendant,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    if output.status.success() {
        return Some(true);
    }
    if output.status.code() == Some(1) {
        return Some(false);
    }
    None
}

/// The gitlink committed for `subpath` at `commit` in the readable repository
/// at `path`: `Ok(sha)` for a `160000 commit` entry, `Err(detail)` carrying
/// the exact refusal — a missing entry, or an entry that is not a submodule —
/// for everything else. Only committed gitlinks are ever verified, never the
/// working tree (LINKED-18).
fn git_committed_gitlink(path: &str, commit: &str, subpath: &str) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args([
            "--no-optional-locks",
            "-C",
            path,
            "ls-tree",
            commit,
            "--",
            subpath,
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|error| format!("could not read the repository at {path}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "dependency path {subpath:?} names no entry committed at consumer commit \
             {commit} in the repository at {path}"
        ));
    }
    let raw = output
        .stdout
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default()
        .to_owned();
    let entry = String::from_utf8_lossy(&raw).trim().to_owned();
    let mut parts = entry.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("160000"), Some("commit"), Some(sha)) => Ok(sha.to_owned()),
        _ => Err(format!(
            "dependency path {subpath:?} at consumer commit {commit} is not a committed \
             gitlink (submodule): the committed entry is {entry:?}. Only submodule \
             dependencies carry consumer proof; subtrees, vendored copies, and \
             language-package pins are explicitly refused"
        )),
    }
}

/// One declared deliverable for one joint task: the unit `contrib close`
/// evidences and refuses over. A `code` deliverable pins the implementation
/// repository every baseline/implementation/integration record must name; a
/// `non-code` one carries no repository, because there is no commit to hash
/// (LINKED-21).
#[derive(Debug, Clone)]
struct DeliverableRow {
    board_id: String,
    item_id: String,
    name: String,
    kind: String,
    repo: String,
    declared_by: String,
    declared_at: i64,
}

fn deliverable_row(row: &rusqlite::Row) -> rusqlite::Result<DeliverableRow> {
    Ok(DeliverableRow {
        board_id: row.get("board_id")?,
        item_id: row.get("item_id")?,
        name: row.get("name")?,
        kind: row.get("kind")?,
        repo: row.get("repo")?,
        declared_by: row.get("declared_by")?,
        declared_at: row.get("declared_at")?,
    })
}

fn deliverable(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    name: &str,
) -> Result<Option<DeliverableRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_deliverables WHERE board_id=? AND item_id=? AND name=?",
            params![board_id, item_id, name],
            deliverable_row,
        )
        .optional()?)
}

fn deliverables_for(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
) -> Result<Vec<DeliverableRow>> {
    let mut statement = connection.prepare(
        "SELECT * FROM linked_deliverables WHERE board_id=? AND item_id=? ORDER BY name",
    )?;
    let rows = statement
        .query_map(params![board_id, item_id], deliverable_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn deliverable_receipt(row: &DeliverableRow) -> serde_json::Value {
    json!({
        "boardID": row.board_id,
        "id": row.item_id,
        "deliverable": row.name,
        "kind": row.kind,
        "repo": row.repo,
        "declaredBy": row.declared_by,
        "declaredAt": row.declared_at,
    })
}

/// Declare one deliverable for one joint task, audited and idempotent: the
/// identical declaration retried answers the stored row, while re-declaring
/// the same name with a different kind or repository is refused whole.
#[allow(clippy::too_many_arguments)]
pub(crate) fn declare_deliverable(
    connection: &Connection,
    root: &Path,
    board: &str,
    id: &str,
    name: &str,
    kind: &str,
    repo: &str,
    actor: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let endpoint = require_endpoint_shape(board, id, "declared")?;
    let caller = linked_caller()?;
    if resolve_endpoint(root, &caller, &endpoint).is_none() {
        bail!("declared endpoint {UNAVAILABLE_ENDPOINT}");
    }
    let name = require_evidence_field(name, "deliverable")?;
    let kind = match kind.trim() {
        "code" => "code",
        "non-code" => "non-code",
        other => bail!(
            "deliverable kind must be code or non-code, got {other:?}: a code record never \
             satisfies a non-code deliverable, and a non-code record never satisfies a code \
             one (LINKED-21). Nothing was declared"
        ),
    };
    let repo = if kind == "code" {
        require_repo_identity(repo)?
    } else if repo.trim().is_empty() {
        String::new()
    } else {
        bail!(
            "a non-code deliverable carries no repository: there is no commit to hash and no \
             role to declare (LINKED-21). Omit --repo; nothing was declared"
        )
    };
    let actor = actor.trim();
    if actor.is_empty() {
        bail!(
            "a deliverable missing its declaring actor is refused naming the missing field: \
             pass --as; nothing was declared"
        );
    }
    require_membership_authority(
        &caller,
        root,
        &[SetMember {
            board_id: endpoint.board_id.clone(),
            id: endpoint.id.clone(),
        }],
    )?;
    if let Some(existing) = deliverable(connection, &endpoint.board_id, &endpoint.id, &name)? {
        if existing.kind == kind && existing.repo == repo {
            // The identical declaration retried: answer the stored row.
            return Ok(deliverable_receipt(&existing));
        }
        bail!(
            "deliverable {name:?} for board {} item {} is already declared {} with repository \
             {:?}: re-declaring it differently is refused whole. Nothing was declared",
            endpoint.board_id,
            endpoint.id,
            existing.kind,
            existing.repo
        );
    }
    connection.execute(
        "INSERT INTO linked_deliverables(board_id,item_id,name,kind,repo,declared_by,\
         declared_at) VALUES(?,?,?,?,?,?,?)",
        params![endpoint.board_id, endpoint.id, name, kind, repo, actor, now],
    )?;
    crate::audit::append_registry_event(
        connection,
        &format!("{}:{}:{}", endpoint.board_id, endpoint.id, name),
        "linked_deliverable_declared",
        actor,
        &json!({
            "boardID": endpoint.board_id,
            "id": endpoint.id,
            "deliverable": name,
            "kind": kind,
            "repo": repo,
        })
        .to_string(),
        now,
    )?;
    let stored = deliverable(connection, &endpoint.board_id, &endpoint.id, &name)?
        .context("deliverable was not stored")?;
    Ok(deliverable_receipt(&stored))
}

/// One stored receipt: the full identity LINKED-15 requires — repository,
/// full object IDs, actor/lane/session, host/worktree/branch, observed and
/// recorded times, evidence references — plus the LINKED-17 mapping, the
/// LINKED-18 consumer path, and the verification outcome. Correction links
/// point at superseded receipts; no row is ever updated or deleted.
#[derive(Debug, Clone)]
struct ContributionRow {
    id: String,
    board_id: String,
    item_id: String,
    deliverable: String,
    kind: String,
    repo: String,
    commit_sha: String,
    role: String,
    actor: String,
    lane: String,
    session: String,
    host: String,
    worktree: String,
    branch: String,
    observed_at: i64,
    recorded_at: i64,
    evidence_refs: Vec<String>,
    note: String,
    corrects: Option<String>,
    sources: Vec<String>,
    mapping: String,
    consumer_path: Vec<String>,
    consumes: String,
    verify_run: String,
    dep_kind: String,
    verification: String,
    verify_detail: String,
    repo_path: String,
    hop_paths: Vec<String>,
}

fn contribution_row(row: &rusqlite::Row) -> rusqlite::Result<ContributionRow> {
    let id: String = row.get("id")?;
    let evidence_refs: String = row.get("evidence_refs")?;
    let sources: String = row.get("sources")?;
    let consumer_path: String = row.get("consumer_path")?;
    let hop_paths: String = row.get("hop_paths")?;
    Ok(ContributionRow {
        id: id.clone(),
        board_id: row.get("board_id")?,
        item_id: row.get("item_id")?,
        deliverable: row.get("deliverable")?,
        kind: row.get("kind")?,
        repo: row.get("repo")?,
        commit_sha: row.get("commit_sha")?,
        role: row.get("role")?,
        actor: row.get("actor")?,
        lane: row.get("lane")?,
        session: row.get("session")?,
        host: row.get("host")?,
        worktree: row.get("worktree")?,
        branch: row.get("branch")?,
        observed_at: row.get("observed_at")?,
        recorded_at: row.get("recorded_at")?,
        evidence_refs: serde_json::from_str(&evidence_refs).unwrap_or_default(),
        note: row.get("note")?,
        corrects: row.get("corrects")?,
        sources: serde_json::from_str(&sources).unwrap_or_default(),
        mapping: row.get("mapping")?,
        consumer_path: serde_json::from_str(&consumer_path).unwrap_or_default(),
        consumes: row.get("consumes")?,
        verify_run: row.get("verify_run")?,
        dep_kind: row.get("dep_kind")?,
        verification: row.get("verification")?,
        verify_detail: row.get("verify_detail")?,
        repo_path: row.get("repo_path")?,
        hop_paths: serde_json::from_str(&hop_paths).unwrap_or_default(),
    })
}

fn contribution_by_id(connection: &Connection, id: &str) -> Result<Option<ContributionRow>> {
    Ok(connection
        .query_row(
            "SELECT * FROM linked_contributions WHERE id=?",
            [id],
            contribution_row,
        )
        .optional()?)
}

fn contributions_for(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    deliverable: &str,
) -> Result<Vec<ContributionRow>> {
    let mut statement = connection.prepare(
        "SELECT * FROM linked_contributions WHERE board_id=? AND item_id=? AND deliverable=? \
         ORDER BY recorded_at,id",
    )?;
    let rows = statement
        .query_map(params![board_id, item_id, deliverable], contribution_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn current_records(records: &[ContributionRow]) -> Vec<&ContributionRow> {
    let superseded: std::collections::HashSet<&str> = records
        .iter()
        .filter_map(|record| record.corrects.as_deref())
        .collect();
    records
        .iter()
        .filter(|record| !superseded.contains(record.id.as_str()))
        .collect()
}

/// Whether the triple holds a live binding whose selected set contains the
/// endpoint: the §2 in-scope rule that contributions are recorded only inside
/// the selected set the worker is bound to. Frozen sets still take evidence —
/// freezing parks taking claims, never the receipts for work already running.
fn binding_covers(
    connection: &Connection,
    triple: &ScopeTriple,
    board_id: &str,
    item_id: &str,
    now: i64,
) -> Result<bool> {
    let mut statement = connection.prepare(
        "SELECT set_id FROM linked_bindings WHERE actor=? AND lane=? AND session=? AND \
         status='active' AND expires_at>?",
    )?;
    let sets = statement
        .query_map(
            params![triple.actor, triple.lane, triple.session, now],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    for set_id in sets {
        let Some((_, head)) = set_head(connection, &set_id)? else {
            continue;
        };
        if head
            .members
            .iter()
            .any(|member| member.board_id == board_id && member.id == item_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Everything `contrib record` carries, collected by the dispatcher from its
/// flags. Repeatable flags arrive whole (`--role` twice is a role refusal,
/// not a parser refusal); every absence and mismatch is refused below with
/// the field it is missing.
#[derive(Debug, Default)]
pub(crate) struct ContributionInput {
    pub board: String,
    pub id: String,
    pub deliverable: String,
    pub kind: String,
    pub actor: String,
    pub lane: Option<String>,
    pub session: Option<String>,
    pub host: String,
    pub worktree: String,
    pub branch: String,
    pub observed_at: String,
    pub repo: String,
    pub commit: String,
    pub roles: Vec<String>,
    pub sources: Vec<String>,
    pub mapping: String,
    pub vias: Vec<String>,
    pub hop_paths: Vec<String>,
    pub consumes: String,
    pub dep_kind: String,
    pub verify_run: String,
    pub repo_path: String,
    pub evidence_refs: Vec<String>,
    pub note: String,
    pub corrects: String,
    pub author: String,
}

fn contribution_receipt(row: &ContributionRow) -> serde_json::Value {
    json!({
        "contributionID": row.id,
        "boardID": row.board_id,
        "id": row.item_id,
        "deliverable": row.deliverable,
        "kind": row.kind,
        "repo": row.repo,
        "commit": row.commit_sha,
        "role": row.role,
        "actor": row.actor,
        "lane": row.lane,
        "session": row.session,
        "host": row.host,
        "worktree": row.worktree,
        "branch": row.branch,
        "observedAt": row.observed_at,
        "recordedAt": row.recorded_at,
        "evidenceRefs": row.evidence_refs,
        "note": row.note,
        "corrects": row.corrects,
        "sources": row.sources,
        "mapping": row.mapping,
        "consumerPath": row.consumer_path,
        "consumes": row.consumes,
        "verifyRun": row.verify_run,
        "depKind": row.dep_kind,
        "verification": row.verification,
        "verifyDetail": row.verify_detail,
        "repoPath": row.repo_path,
        "hopPaths": row.hop_paths,
    })
}

/// Verify one code record against readable repositories, or mark it
/// unverified. Returns `(verification, verify_detail)` and bails only for a
/// contradiction a readable repository proves: the wrong hash in the stated
/// repo, a non-ancestor source under a merge mapping, a missing or
/// non-submodule hop, or a stale leaf pin.
#[allow(clippy::too_many_arguments)]
fn verify_code_evidence(
    repo: &str,
    commit: &str,
    role: &str,
    sources: &[String],
    mapping: &str,
    vias: &[String],
    hop_paths: &[String],
    consumes: &str,
    repo_path: Option<&str>,
) -> Result<(String, String)> {
    let unverified =
        |detail: String| -> Result<(String, String)> { Ok(("unverified".to_owned(), detail)) };
    let Some(path) = repo_path else {
        return unverified(format!(
            "no --repo-path was given for repository {repo:?}: the receipt is recorded \
             unverified, and unverified evidence satisfies no deliverable"
        ));
    };
    if !git_repo_readable(path) {
        return unverified(format!(
            "no readable repository at {path:?} for repository {repo:?}: recorded \
             unverified, and missing access is never success"
        ));
    }
    if !git_object_present(path, commit) {
        bail!(
            "commit {commit:?} is not an object in repository {repo:?} at {path:?}: expected \
             an existing full object ID, presented {commit:?} (LINKED-19). Nothing was appended"
        );
    }
    for source in sources {
        if !git_object_present(path, source) {
            bail!(
                "--source commit {source:?} is not an object in repository {repo:?} at \
                 {path:?}: expected an existing full object ID, presented {source:?} \
                 (LINKED-19). Nothing was appended"
            );
        }
    }
    if role == "integration" && mapping == "merge" {
        for source in sources {
            match git_is_ancestor(path, source, commit) {
                Some(true) => {}
                Some(false) => bail!(
                    "integration commit {commit:?} does not absorb --source {source:?}: the \
                     source is no ancestor of the merge in repository {repo:?} at {path:?} \
                     (LINKED-17). Nothing was appended"
                ),
                None => {
                    return unverified(format!(
                        "ancestry in the repository at {path:?} could not be read: recorded \
                         unverified, and missing access is never success"
                    ));
                }
            }
        }
    }
    if role == "candidate" {
        let mut resolved = commit.to_owned();
        for (index, hop) in vias.iter().enumerate() {
            let parent: &str = if index == 0 {
                path
            } else {
                let Some(hop_repo) = hop_paths.get(index - 1) else {
                    return unverified(format!(
                        "no machine path for dependency hop {} ({hop:?}): pass one --hop-path \
                         per nested hop after the first, or accept an unverified receipt. \
                         Recorded unverified; unverified evidence satisfies no deliverable",
                        index + 1
                    ));
                };
                if !git_repo_readable(hop_repo) {
                    return unverified(format!(
                        "no readable repository at {hop_repo:?} for dependency hop {} \
                         ({hop:?}): recorded unverified, and missing access is never success",
                        index + 1
                    ));
                }
                hop_repo.as_str()
            };
            if index > 0 && !git_object_present(parent, &resolved) {
                bail!(
                    "dependency hop {} ({hop:?}) expects commit {resolved:?}, which is not an \
                     object in the repository at {parent}: the recorded pin is stale \
                     (LINKED-19). Nothing was appended",
                    index + 1
                );
            }
            match git_committed_gitlink(parent, &resolved, hop) {
                Ok(gitlink) => resolved = gitlink,
                Err(detail) => bail!("{detail} (LINKED-18, LINKED-19). Nothing was appended"),
            }
        }
        if resolved != consumes {
            bail!(
                "the committed gitlinks resolve to {resolved:?}, but the record consumes \
                 {consumes:?}: the pin is stale — expected the committed leaf, presented \
                 {consumes:?} (LINKED-19). Nothing was appended"
            );
        }
        return Ok((
            "verified".to_owned(),
            format!(
                "consumer commit {commit} is an object in {repo:?} at {path}; {} committed \
                 gitlink(s) walked to the consumed dependency commit {consumes}",
                vias.len()
            ),
        ));
    }
    let mut detail = format!("commit {commit} is an object in {repo:?} at {path}");
    if role == "integration" {
        detail.push_str(&format!(
            "; {mapping} mapping covers {} source(s)",
            sources.len()
        ));
    }
    Ok(("verified".to_owned(), detail))
}

/// Append one contribution receipt: the LINKED-15 identity (repository, full
/// object IDs, actor/lane/session, host/worktree/branch, observed time,
/// evidence references), exactly one of the four LINKED-16 roles, the
/// LINKED-17 source mapping on integrations, and the LINKED-18 consumer
/// commit with its full nested gitlink path on candidates. The lane that did
/// the work records its own triple, and only inside the selected set that
/// triple is bound to; every refusal below lands before any row does.
pub(crate) fn record_contribution(
    connection: &Connection,
    root: &Path,
    input: &ContributionInput,
    now: i64,
) -> Result<serde_json::Value> {
    let endpoint = require_endpoint_shape(&input.board, &input.id, "recorded")?;
    let caller = linked_caller()?;
    if resolve_endpoint(root, &caller, &endpoint).is_none() {
        bail!("recorded endpoint {UNAVAILABLE_ENDPOINT}");
    }
    let actor = input.actor.trim();
    if actor.is_empty() || input.author.trim() != actor {
        bail!(
            "contributions are recorded by the lane that did the work: --as {:?} must equal \
             --actor {:?}; no lane records for another lane's triple. Nothing was appended",
            input.author,
            input.actor
        );
    }
    let triple = normalize_triple(
        actor,
        input.lane.as_deref().map(str::trim),
        input.session.as_deref().map(str::trim),
    );
    if !binding_covers(connection, &triple, &endpoint.board_id, &endpoint.id, now)? {
        bail!(
            "no live binding for {} covers board {} item {}: record contributions only inside \
             the selected set the worker is bound to. Nothing was appended",
            triple_sentence(&triple),
            endpoint.board_id,
            endpoint.id
        );
    }
    let deliverable_name = require_evidence_field(&input.deliverable, "deliverable")?;
    let Some(declared) = deliverable(
        connection,
        &endpoint.board_id,
        &endpoint.id,
        &deliverable_name,
    )?
    else {
        bail!(
            "no deliverable {deliverable_name:?} is declared for board {} item {}: declare it \
             with `contrib declare` first. Nothing was appended",
            endpoint.board_id,
            endpoint.id
        );
    };
    let kind = if input.kind.trim().is_empty() {
        "code"
    } else {
        input.kind.trim()
    };
    if kind != "code" && kind != "non-code" {
        bail!(
            "record kind must be code or non-code, got {:?}. Nothing was appended",
            input.kind
        );
    }
    if kind != declared.kind {
        bail!(
            "kind mismatch: deliverable {deliverable_name:?} is declared {}, but the record is \
             {kind}: a non-code record never satisfies a code deliverable, and a code record \
             never satisfies a non-code one (LINKED-21). Nothing was appended",
            declared.kind
        );
    }
    let host = require_evidence_field(&input.host, "host")?;
    let worktree = require_evidence_field(&input.worktree, "worktree")?;
    let branch = require_evidence_field(&input.branch, "branch")?;
    let observed_at = require_observed_at(&input.observed_at)?;
    for reference in &input.evidence_refs {
        if reference.trim().is_empty() {
            bail!(
                "--evidence-ref names an empty reference: pass a reference or omit the flag. \
                 Nothing was appended"
            );
        }
    }
    if kind == "non-code" {
        return record_non_code(
            connection,
            &endpoint.board_id,
            &endpoint.id,
            &declared,
            &triple,
            input,
            &host,
            &worktree,
            &branch,
            observed_at,
            now,
        );
    }
    if input.roles.len() != 1 {
        bail!("{}", role_refusal(&input.roles));
    }
    let role = input.roles[0].trim().to_lowercase();
    if !EVIDENCE_ROLES.contains(&role.as_str()) {
        bail!("{}", role_refusal(&input.roles));
    }
    let repo = require_repo_identity(&input.repo)?;
    if repo != declared.repo {
        bail!(
            "deliverable {deliverable_name:?} pins repository {:?}, but the record names \
             {repo:?}: the right hash at the wrong repository proves nothing (LINKED-19). \
             Nothing was appended",
            declared.repo
        );
    }
    let commit = require_full_sha(&input.commit, "the record's commit")?;
    let mut sources = Vec::with_capacity(input.sources.len());
    for source in &input.sources {
        sources.push(require_full_sha(source, "a --source commit")?);
    }
    sources.sort();
    sources.dedup();
    let mapping = match role.as_str() {
        "integration" => {
            if sources.is_empty() {
                bail!(
                    "an integration commit absorbs implementation commits: pass each absorbed \
                     commit with --source and the shape with --mapping merge|squash. A squash \
                     whose mapping is absent reads as unevidenced, not as self-evident \
                     (LINKED-17). Nothing was appended"
                );
            }
            match input.mapping.trim() {
                "merge" => "merge".to_owned(),
                "squash" => "squash".to_owned(),
                other => {
                    bail!("--mapping must be merge or squash, got {other:?}. Nothing was appended")
                }
            }
        }
        _ => {
            if !sources.is_empty() || !input.mapping.trim().is_empty() {
                bail!(
                    "only an integration record carries --source/--mapping: a {role} record \
                     with absorbed commits is two roles in one (LINKED-16). Nothing was \
                     appended"
                );
            }
            String::new()
        }
    };
    let (vias, consumes, verify_run, dep_kind) = match role.as_str() {
        "candidate" => {
            if input.vias.is_empty() || input.vias.iter().any(|hop| hop.trim().is_empty()) {
                bail!(
                    "a consumer-side proof names the exact consumer commit tested and the exact \
                     dependency path that reached it, including every nested hop (LINKED-18): \
                     pass each hop with --via. A proof that names the commit but not the path, \
                     or a path with a hop missing, satisfies nothing. Nothing was appended"
                );
            }
            let dep_kind = if input.dep_kind.trim().is_empty() {
                "submodule"
            } else {
                input.dep_kind.trim()
            };
            if dep_kind != "submodule" {
                bail!(
                    "dependency kind {dep_kind:?} is explicitly refused: only submodule \
                     (gitlink) dependencies carry consumer proof. Subtrees, vendored copies, \
                     and language-package pins are out of scope; consume the dependency as a \
                     submodule or record no candidate. Nothing was appended"
                );
            }
            (
                input.vias.iter().map(|hop| hop.trim().to_owned()).collect(),
                require_full_sha(&input.consumes, "the consumed dependency commit")?,
                require_evidence_field(&input.verify_run, "verify-run")?,
                dep_kind.to_owned(),
            )
        }
        _ => {
            let stray = [
                ("via", !input.vias.is_empty()),
                ("hop-path", !input.hop_paths.is_empty()),
                ("consumes", !input.consumes.trim().is_empty()),
                ("dep-kind", !input.dep_kind.trim().is_empty()),
                ("verify-run", !input.verify_run.trim().is_empty()),
            ]
            .into_iter()
            .find(|(_, present)| *present)
            .map(|(flag, _)| flag);
            if let Some(flag) = stray {
                bail!(
                    "only a candidate record carries --via/--hop-path/--consumes/--dep-kind/\
                     --verify-run: a {role} record with a consumer path is two roles in one \
                     (LINKED-16). --{flag} does not belong on it; nothing was appended"
                );
            }
            (Vec::new(), String::new(), String::new(), String::new())
        }
    };
    let mut hop_paths = Vec::with_capacity(input.hop_paths.len());
    for hop_path in &input.hop_paths {
        hop_paths.push(require_local_path(hop_path, "hop-path")?);
    }
    let repo_path = if input.repo_path.trim().is_empty() {
        None
    } else {
        Some(require_local_path(&input.repo_path, "repo-path")?)
    };
    // The identical receipt retried answers the stored row instead of writing
    // a second one.
    let sources_json = serde_json::to_string(&sources)?;
    let consumer_path_json = serde_json::to_string(&vias)?;
    let hop_paths_json = serde_json::to_string(&hop_paths)?;
    if let Some(existing) = duplicate_record(
        connection,
        &endpoint.board_id,
        &endpoint.id,
        &deliverable_name,
        kind,
        &repo,
        &commit,
        &role,
        &sources_json,
        &mapping,
        &consumer_path_json,
        &consumes,
        repo_path.as_deref().unwrap_or(""),
        &hop_paths_json,
        &verify_run,
    )? {
        return Ok(contribution_receipt(&existing));
    }
    // A new baseline or candidate commit for the same deliverable and role
    // supersedes explicitly or not at all: a changed candidate without
    // `--corrects` proves nothing (LINKED-19). Multiple implementation
    // commits coexist by design, and integrations accumulate with their own
    // mappings — coverage is the union across them (LINKED-17).
    let corrects = input.corrects.trim();
    if corrects.is_empty()
        && (role == "baseline" || role == "candidate")
        && let Some(current) = current_role_record(
            connection,
            &endpoint.board_id,
            &endpoint.id,
            &deliverable_name,
            &role,
        )?
        && current.commit_sha != commit
    {
        bail!(
            "{role} for deliverable {deliverable_name:?} already pins commit {} (receipt {}): \
             a different commit {commit:?} needs --corrects {} naming the receipt it \
             supersedes. A changed candidate cannot satisfy a deliverable on its own \
             (LINKED-19). Nothing was appended",
            current.commit_sha,
            current.id,
            current.id
        );
    }
    let corrects = if corrects.is_empty() {
        None
    } else {
        correction_target(
            connection,
            &endpoint.board_id,
            &endpoint.id,
            &deliverable_name,
            corrects,
            Some(&role),
        )?;
        Some(corrects.to_owned())
    };
    let (verification, verify_detail) = verify_code_evidence(
        &repo,
        &commit,
        &role,
        &sources,
        &mapping,
        &vias,
        &hop_paths,
        &consumes,
        repo_path.as_deref(),
    )?;
    insert_contribution(
        connection,
        &endpoint.board_id,
        &endpoint.id,
        &deliverable_name,
        kind,
        &repo,
        &commit,
        &role,
        &triple,
        &host,
        &worktree,
        &branch,
        observed_at,
        now,
        &input.evidence_refs,
        &input.note,
        corrects.as_deref(),
        &sources,
        &mapping,
        &vias,
        &consumes,
        &verify_run,
        &dep_kind,
        &verification,
        &verify_detail,
        repo_path.as_deref().unwrap_or(""),
        &hop_paths,
        actor,
    )
}

/// The identical receipt retried: the same endpoint, deliverable, kind,
/// repository, commit, role, mapping, consumer path, and verification inputs
/// answer the stored row instead of writing a second one.
#[allow(clippy::too_many_arguments)]
fn duplicate_record(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    deliverable: &str,
    kind: &str,
    repo: &str,
    commit: &str,
    role: &str,
    sources_json: &str,
    mapping: &str,
    consumer_path_json: &str,
    consumes: &str,
    repo_path: &str,
    hop_paths_json: &str,
    verify_run: &str,
) -> Result<Option<ContributionRow>> {
    // The key covers the mapping, consumer path, and verification inputs as
    // well as the commit: a correction that keeps the commit but fixes the
    // mapping — or re-records once a machine path is reachable — still lands
    // as a new receipt, while an identical retry answers the stored row.
    Ok(connection
        .query_row(
            "SELECT * FROM linked_contributions WHERE board_id=? AND item_id=? AND deliverable=? \
             AND kind=? AND repo=? AND commit_sha=? AND role=? AND sources=? AND mapping=? AND \
             consumer_path=? AND consumes=? AND repo_path=? AND hop_paths=? AND verify_run=? \
             ORDER BY recorded_at,id LIMIT 1",
            params![
                board_id,
                item_id,
                deliverable,
                kind,
                repo,
                commit,
                role,
                sources_json,
                mapping,
                consumer_path_json,
                consumes,
                repo_path,
                hop_paths_json,
                verify_run
            ],
            contribution_row,
        )
        .optional()?)
}

/// The current (non-superseded) receipt for one deliverable and role, if any.
fn current_role_record(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    deliverable: &str,
    role: &str,
) -> Result<Option<ContributionRow>> {
    let records = contributions_for(connection, board_id, item_id, deliverable)?;
    Ok(current_records(&records)
        .into_iter()
        .find(|record| record.role == role)
        .cloned())
}

/// Check one `--corrects` link: it names a live receipt for this task and
/// deliverable (and this role, for code), which no later receipt already
/// corrects. Corrections chain onto the current record; the corrected bytes
/// stay stored.
fn correction_target(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    deliverable: &str,
    corrects: &str,
    role: Option<&str>,
) -> Result<()> {
    let Some(target) = contribution_by_id(connection, corrects)? else {
        bail!(
            "--corrects names {corrects:?}, which is no contribution receipt: a correction is \
             a new record naming the record it corrects. Nothing was appended"
        );
    };
    if target.board_id != board_id || target.item_id != item_id || target.deliverable != deliverable
    {
        bail!(
            "--corrects names {corrects:?}, which evidences board {} item {} deliverable \
             {:?}: a correction names a record for this task and deliverable. Nothing was \
             appended",
            target.board_id,
            target.item_id,
            target.deliverable
        );
    }
    if let Some(role) = role {
        let target_role = if target.kind == "code" {
            target.role.clone()
        } else {
            "non-code".to_owned()
        };
        if target_role != role {
            bail!(
                "--corrects names {corrects:?}, a {target_role} record: a {role} correction \
                 names a {role} record. Nothing was appended"
            );
        }
    }
    let superseded: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM linked_contributions WHERE corrects=?)",
        [corrects],
        |row| row.get(0),
    )?;
    if superseded {
        bail!(
            "--corrects names {corrects:?}, which another record already corrects: corrections \
             chain onto the current record. Nothing was appended"
        );
    }
    Ok(())
}

/// Append a `non-code` receipt: the same actor/lane/session, task, and
/// evidence discipline as LINKED-15, exempt from the object-ID and role rules
/// (LINKED-21). A `non-code` record never satisfies a code deliverable — the
/// kind gate in [`record_contribution`] already refused the cross — and there
/// is no commit to verify, so verification is recorded as having nothing to
/// check rather than as a passing check.
#[allow(clippy::too_many_arguments)]
fn record_non_code(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    declared: &DeliverableRow,
    triple: &ScopeTriple,
    input: &ContributionInput,
    host: &str,
    worktree: &str,
    branch: &str,
    observed_at: i64,
    now: i64,
) -> Result<serde_json::Value> {
    let stray = [
        ("commit", !input.commit.trim().is_empty()),
        ("role", !input.roles.is_empty()),
        ("repo", !input.repo.trim().is_empty()),
        ("repo-path", !input.repo_path.trim().is_empty()),
        ("source", !input.sources.is_empty()),
        ("mapping", !input.mapping.trim().is_empty()),
        ("via", !input.vias.is_empty()),
        ("hop-path", !input.hop_paths.is_empty()),
        ("consumes", !input.consumes.trim().is_empty()),
        ("dep-kind", !input.dep_kind.trim().is_empty()),
        ("verify-run", !input.verify_run.trim().is_empty()),
    ]
    .into_iter()
    .find(|(_, present)| *present)
    .map(|(flag, _)| flag);
    if let Some(flag) = stray {
        bail!(
            "a non-code record carries no commit and declares no role: it is exempt from the \
             object-ID and role rules of LINKED-15..LINKED-19 (LINKED-21). --{flag} does not \
             belong on it. Nothing was appended"
        );
    }
    if input.note.trim().is_empty() && input.evidence_refs.is_empty() {
        bail!(
            "a non-code record still carries evidence: pass --note or at least one \
             --evidence-ref naming the decision, review, document, or approval. Nothing was \
             appended"
        );
    }
    let corrects = input.corrects.trim();
    let corrects = if corrects.is_empty() {
        None
    } else {
        correction_target(
            connection,
            board_id,
            item_id,
            &declared.name,
            corrects,
            None,
        )?;
        Some(corrects.to_owned())
    };
    insert_contribution(
        connection,
        board_id,
        item_id,
        &declared.name,
        "non-code",
        "",
        "",
        "",
        triple,
        host,
        worktree,
        branch,
        observed_at,
        now,
        &input.evidence_refs,
        input.note.trim(),
        corrects.as_deref(),
        &[],
        "",
        &[],
        "",
        "",
        "",
        "verified",
        "non-code record: no commit to verify; the lane's evidence stands as recorded",
        "",
        &[],
        triple.actor.as_str(),
    )
}

/// Store one validated receipt and join the registry audit chain, then answer
/// the stored row.
#[allow(clippy::too_many_arguments)]
fn insert_contribution(
    connection: &Connection,
    board_id: &str,
    item_id: &str,
    deliverable: &str,
    kind: &str,
    repo: &str,
    commit: &str,
    role: &str,
    triple: &ScopeTriple,
    host: &str,
    worktree: &str,
    branch: &str,
    observed_at: i64,
    now: i64,
    evidence_refs: &[String],
    note: &str,
    corrects: Option<&str>,
    sources: &[String],
    mapping: &str,
    consumer_path: &[String],
    consumes: &str,
    verify_run: &str,
    dep_kind: &str,
    verification: &str,
    verify_detail: &str,
    repo_path: &str,
    hop_paths: &[String],
    actor: &str,
) -> Result<serde_json::Value> {
    let id = format!("le-{}", uuid::Uuid::new_v4().simple());
    let evidence_refs_json = serde_json::to_string(
        &evidence_refs
            .iter()
            .map(|reference| reference.trim().to_owned())
            .collect::<Vec<_>>(),
    )?;
    let sources_json = serde_json::to_string(sources)?;
    let consumer_path_json = serde_json::to_string(consumer_path)?;
    let hop_paths_json = serde_json::to_string(hop_paths)?;
    connection.execute(
        "INSERT INTO linked_contributions(id,board_id,item_id,deliverable,kind,repo,commit_sha,\
         role,actor,lane,session,host,worktree,branch,observed_at,recorded_at,evidence_refs,\
         note,corrects,sources,mapping,consumer_path,consumes,verify_run,dep_kind,\
         verification,verify_detail,repo_path,hop_paths) \
         VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            id,
            board_id,
            item_id,
            deliverable,
            kind,
            repo,
            commit,
            role,
            triple.actor,
            triple.lane,
            triple.session,
            host,
            worktree,
            branch,
            observed_at,
            now,
            evidence_refs_json,
            note,
            corrects,
            sources_json,
            mapping,
            consumer_path_json,
            consumes,
            verify_run,
            dep_kind,
            verification,
            verify_detail,
            repo_path,
            hop_paths_json,
        ],
    )?;
    crate::audit::append_registry_event(
        connection,
        &id,
        "linked_contribution_recorded",
        actor,
        &json!({
            "contributionID": id,
            "boardID": board_id,
            "id": item_id,
            "deliverable": deliverable,
            "kind": kind,
            "role": role,
            "commit": commit,
            "verification": verification,
        })
        .to_string(),
        now,
    )?;
    let stored = contribution_by_id(connection, &id)?.context("contribution was not stored")?;
    Ok(contribution_receipt(&stored))
}

/// The checkable core of one code deliverable, assembled from its current
/// (non-superseded) receipts: whether each of the four roles carries verified
/// proof, and which unverified receipts are waiting on machine access. Every
/// current integration keeps its own mapping, so a squash and a later merge
/// each retain theirs (LINKED-17); coverage is the union across them.
#[derive(Debug, Default)]
struct CodeEvidence {
    baseline: bool,
    implementations: Vec<String>,
    integrations: Vec<(String, Vec<String>)>,
    candidate: bool,
    unverified: Vec<String>,
}

/// What a code deliverable still lacks: one sentence per gap, so a close
/// names each open deliverable with its reason (LINKED-20). Unverified
/// receipts are listed too — they read as partial, never as joint
/// (LINKED-19, LINKED-20) — as is an integration whose mapping leaves
/// recorded implementation commits uncovered (LINKED-17).
fn code_missing(view: &CodeEvidence, deliverable: &str) -> Vec<String> {
    let mut missing = Vec::new();
    if !view.baseline {
        missing.push(format!(
            "deliverable {deliverable:?}: no verified baseline (the starting HEAD) is recorded"
        ));
    }
    if view.implementations.is_empty() {
        missing.push(format!(
            "deliverable {deliverable:?}: no verified implementation commits are recorded"
        ));
    }
    if view.integrations.is_empty() {
        missing.push(format!(
            "deliverable {deliverable:?}: no verified integration with its source mapping is \
             recorded"
        ));
    } else {
        let unmapped: Vec<&str> = view
            .implementations
            .iter()
            .filter(|commit| {
                !view
                    .integrations
                    .iter()
                    .any(|(_, sources)| sources.contains(commit))
            })
            .map(String::as_str)
            .collect();
        if !unmapped.is_empty() {
            let covering: Vec<&str> = view
                .integrations
                .iter()
                .map(|(integrated, _)| integrated.as_str())
                .collect();
            missing.push(format!(
                "deliverable {deliverable:?}: integrations {} leave unmapped implementation \
                 commits {}: record the explicit mapping (LINKED-17)",
                covering.join(", "),
                unmapped.join(", ")
            ));
        }
    }
    if !view.candidate {
        missing.push(format!(
            "deliverable {deliverable:?}: no verified consumer candidate with its complete \
             nested path is recorded"
        ));
    }
    for pending in &view.unverified {
        missing.push(format!(
            "deliverable {deliverable:?}: {pending}; unverified evidence satisfies nothing"
        ));
    }
    missing
}

/// One deliverable with its receipts and its verdict: evidenced only when
/// every role carries exact verified proof (LINKED-20).
struct DeliverableEvidence {
    row: DeliverableRow,
    records: Vec<ContributionRow>,
    missing: Vec<String>,
    evidenced: bool,
}

fn deliverable_evidence(
    connection: &Connection,
    row: &DeliverableRow,
) -> Result<DeliverableEvidence> {
    let records = contributions_for(connection, &row.board_id, &row.item_id, &row.name)?;
    let current = current_records(&records);
    let (missing, evidenced) = if row.kind == "non-code" {
        if current.is_empty() {
            (
                vec![format!(
                    "deliverable {:?}: no non-code record (decision, review, document, or \
                     approval) is recorded",
                    row.name
                )],
                false,
            )
        } else {
            (Vec::new(), true)
        }
    } else {
        let mut view = CodeEvidence::default();
        for record in &current {
            if record.verification != "verified" {
                view.unverified.push(format!(
                    "receipt {} ({} {}) is {}: {}",
                    record.id,
                    record.role,
                    record.commit_sha,
                    record.verification,
                    record.verify_detail
                ));
                continue;
            }
            match record.role.as_str() {
                "baseline" => view.baseline = true,
                "implementation" => view.implementations.push(record.commit_sha.clone()),
                "integration" => {
                    view.integrations
                        .push((record.commit_sha.clone(), record.sources.clone()));
                }
                "candidate" => view.candidate = true,
                _ => {}
            }
        }
        view.implementations.sort();
        view.implementations.dedup();
        let missing = code_missing(&view, &row.name);
        let evidenced = missing.is_empty();
        (missing, evidenced)
    };
    Ok(DeliverableEvidence {
        row: row.clone(),
        records,
        missing,
        evidenced,
    })
}

fn deliverable_history(evidence: &DeliverableEvidence) -> serde_json::Value {
    json!({
        "deliverable": evidence.row.name,
        "kind": evidence.row.kind,
        "repo": evidence.row.repo,
        "declaredBy": evidence.row.declared_by,
        "declaredAt": evidence.row.declared_at,
        "evidenced": evidence.evidenced,
        "missing": evidence.missing,
        "records": evidence.records.iter().map(contribution_receipt).collect::<Vec<_>>(),
    })
}

/// Per-side history: every deliverable of one endpoint with every receipt,
/// read from either board's side without writing anything and without marking
/// the untouched peer as worked — a read here appends no row anywhere.
pub(crate) fn show_contributions(
    connection: &Connection,
    root: &Path,
    board: &str,
    id: &str,
) -> Result<serde_json::Value> {
    let endpoint = require_endpoint_shape(board, id, "queried")?;
    let caller = linked_caller()?;
    if resolve_endpoint(root, &caller, &endpoint).is_none() {
        bail!("queried endpoint {UNAVAILABLE_ENDPOINT}");
    }
    let mut deliverables = Vec::new();
    for row in deliverables_for(connection, &endpoint.board_id, &endpoint.id)? {
        deliverables.push(deliverable_history(&deliverable_evidence(
            connection, &row,
        )?));
    }
    Ok(json!({
        "boardID": endpoint.board_id,
        "id": endpoint.id,
        "deliverables": deliverables,
    }))
}

/// Per-deliverable evidence status for one endpoint: what reads as evidenced
/// and, for the rest, the exact gap. Partial evidence reads as partial, by
/// deliverable, never as joint (LINKED-20).
pub(crate) fn contribution_status(
    connection: &Connection,
    root: &Path,
    board: &str,
    id: &str,
) -> Result<serde_json::Value> {
    let endpoint = require_endpoint_shape(board, id, "queried")?;
    let caller = linked_caller()?;
    if resolve_endpoint(root, &caller, &endpoint).is_none() {
        bail!("queried endpoint {UNAVAILABLE_ENDPOINT}");
    }
    let mut deliverables = Vec::new();
    let mut evidenced = true;
    for row in deliverables_for(connection, &endpoint.board_id, &endpoint.id)? {
        let evidence = deliverable_evidence(connection, &row)?;
        evidenced &= evidence.evidenced;
        deliverables.push(json!({
            "deliverable": evidence.row.name,
            "kind": evidence.row.kind,
            "repo": evidence.row.repo,
            "evidenced": evidence.evidenced,
            "records": evidence.records.len(),
            "missing": evidence.missing,
        }));
    }
    Ok(json!({
        "boardID": endpoint.board_id,
        "id": endpoint.id,
        "evidenced": evidenced && !deliverables.is_empty(),
        "deliverables": deliverables,
    }))
}

fn closure_row(row: &rusqlite::Row) -> rusqlite::Result<(String, String, i64, String)> {
    Ok((
        row.get("pairing_id")?,
        row.get("closed_by")?,
        row.get("closed_at")?,
        row.get("reason")?,
    ))
}

fn closure_receipt(
    pairing: &CompanionRow,
    closed_by: &str,
    closed_at: i64,
    reason: &str,
) -> serde_json::Value {
    json!({
        "pairingID": pairing.id,
        "boardA": {"boardID": pairing.board_a_id, "id": pairing.item_a_id},
        "boardB": {"boardID": pairing.board_b_id, "id": pairing.item_b_id},
        "closedBy": closed_by,
        "closedAt": closed_at,
        "reason": reason,
    })
}

/// Close joint work for one live pairing: every deliverable declared on both
/// endpoints must carry exact verified evidence, or the close is refused
/// naming each open deliverable with its reason — nothing closes and no
/// evidence is rewritten (LINKED-20). The identical close retried answers
/// the stored closure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn close_joint(
    connection: &Connection,
    root: &Path,
    a_board: &str,
    a_id: &str,
    b_board: &str,
    b_id: &str,
    actor: &str,
    reason: &str,
    now: i64,
) -> Result<serde_json::Value> {
    let first = require_endpoint_shape(a_board, a_id, "first")?;
    let second = require_endpoint_shape(b_board, b_id, "second")?;
    if first == second {
        bail!(
            "cannot close joint work of an item with itself: the two endpoints name the same (boardID, id)"
        );
    }
    let caller = linked_caller()?;
    let Some(resolved_first) = resolve_endpoint(root, &caller, &first) else {
        bail!("first endpoint {UNAVAILABLE_ENDPOINT}");
    };
    let Some(resolved_second) = resolve_endpoint(root, &caller, &second) else {
        bail!("second endpoint {UNAVAILABLE_ENDPOINT}");
    };
    let (one, two) = order_resolved(resolved_first, resolved_second);
    let Some(pairing) = live_pairing(
        connection,
        &one.board_id,
        &one.item_id,
        &two.board_id,
        &two.item_id,
    )?
    else {
        if retired_pairing(
            connection,
            &one.board_id,
            &one.item_id,
            &two.board_id,
            &two.item_id,
        )?
        .is_some()
        {
            bail!(
                "the pairing of board {} item {} with board {} item {} is retired: joint work \
                 closes only on a live pairing. Nothing was closed",
                one.board_id,
                one.item_id,
                two.board_id,
                two.item_id
            );
        }
        bail!(
            "no live pairing pairs board {} item {} with board {} item {}: joint work closes \
             only for paired endpoints. Nothing was closed",
            one.board_id,
            one.item_id,
            two.board_id,
            two.item_id
        );
    };
    if let Some((_, closed_by, closed_at, stored_reason)) = connection
        .query_row(
            "SELECT * FROM linked_closures WHERE pairing_id=?",
            [&pairing.id],
            closure_row,
        )
        .optional()?
    {
        // The identical close retried: answer the stored closure.
        return Ok(closure_receipt(
            &pairing,
            &closed_by,
            closed_at,
            &stored_reason,
        ));
    }
    let path_one = root.join("boards").join(format!("{}.db", one.board_id));
    let path_two = root.join("boards").join(format!("{}.db", two.board_id));
    require_pairing_authority(&caller, &one, &path_one, &two, &path_two)?;
    let mut open = Vec::new();
    for (board_id, item_id) in [
        (one.board_id.clone(), one.item_id.clone()),
        (two.board_id.clone(), two.item_id.clone()),
    ] {
        let rows = deliverables_for(connection, &board_id, &item_id)?;
        if rows.is_empty() {
            open.push(format!(
                "board {board_id} item {item_id}: no deliverables are declared — declare each \
                 deliverable with `contrib declare` and evidence it first"
            ));
        }
        for row in &rows {
            let evidence = deliverable_evidence(connection, row)?;
            for gap in &evidence.missing {
                open.push(format!("board {board_id} item {item_id}: {gap}"));
            }
        }
    }
    if !open.is_empty() {
        bail!(
            "joint work is not closable: {}. Nothing was closed",
            open.join("; ")
        );
    }
    let actor = actor.trim();
    if actor.is_empty() {
        bail!(
            "a close missing its closing actor is refused naming the missing field: pass \
             --as. Nothing was closed"
        );
    }
    connection.execute(
        "INSERT INTO linked_closures(pairing_id,closed_by,closed_at,reason) VALUES(?,?,?,?)",
        params![pairing.id, actor, now, reason.trim()],
    )?;
    crate::audit::append_registry_event(
        connection,
        &pairing.id,
        "linked_joint_closed",
        actor,
        &json!({
            "pairingID": pairing.id,
            "boardA": {"boardID": pairing.board_a_id, "id": pairing.item_a_id},
            "boardB": {"boardID": pairing.board_b_id, "id": pairing.item_b_id},
        })
        .to_string(),
        now,
    )?;
    Ok(closure_receipt(&pairing, actor, now, reason.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ROOTS: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fresh estate registry: born with the LINKED tables, like every new
    /// estate (ordinary step above the CROSS ceiling).
    fn temp_registry() -> (TempRoot, Connection) {
        let root = std::env::temp_dir().join(format!(
            "kanban-linked-{}-{}-{}",
            std::process::id(),
            crate::registry::now_ms(),
            ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        let connection =
            crate::db::open_registry(&root.join("registry.db")).expect("open temp registry");
        assert!(
            linked_tables_present(&connection).expect("probe linked tables"),
            "a fresh registry is born with the linked tables"
        );
        (TempRoot(root), connection)
    }

    #[test]
    fn endpoint_shape_rejects_names_paths_tokens_and_empty_ids() {
        let uuid = "3818ff5a-579e-4415-a5b5-0187252bc81d";
        let good = require_endpoint_shape(uuid, "t-1", "first").expect("canonical shape accepts");
        assert_eq!(good.board_id, uuid);
        for (board, id) in [
            ("acme", "t-1"),
            ("boards/acme.db", "t-1"),
            ("@:acme#t-1", "t-1"),
            ("3818FF5A-579E-4415-A5B5-0187252BC81D", "t-1"),
            ("3818ff5a579e4415a5b50187252bc81d", "t-1"),
            (uuid, ""),
            ("", "t-1"),
        ] {
            let error = require_endpoint_shape(board, id, "first").unwrap_err();
            assert!(
                format!("{error:#}").contains("(boardID, id)"),
                "shape refusal names the required shape for {board:?}/{id:?}"
            );
        }
    }

    #[test]
    fn self_pairing_refuses_before_anything_resolves() {
        // Shape-identical endpoints never reach resolution: no registry state
        // is consulted and nothing can leak.
        let uuid = "3818ff5a-579e-4415-a5b5-0187252bc81d";
        assert!(
            require_endpoint_shape(uuid, "t-1", "first").expect("shape")
                == require_endpoint_shape(uuid, "t-1", "second").expect("shape")
        );
    }

    #[test]
    fn ordered_endpoints_canonicalize_either_direction() {
        let mk = |board: &str, item: &str| crate::cross::ResolvedSource {
            board_id: board.to_owned(),
            registration: "reg".to_owned(),
            item_id: item.to_owned(),
            incarnation: "inc".to_owned(),
        };
        let (one, two) = order_resolved(mk("b-board", "t-2"), mk("a-board", "t-9"));
        assert_eq!((one.board_id, one.item_id), ("a-board".to_owned(), "t-9".to_owned()));
        assert_eq!((two.board_id, two.item_id), ("b-board".to_owned(), "t-2".to_owned()));
        // Already ordered passes through untouched.
        let (one, two) = order_resolved(mk("a-board", "t-1"), mk("a-board", "t-2"));
        assert_eq!((one.item_id, two.item_id), ("t-1".to_owned(), "t-2".to_owned()));
    }

    #[test]
    fn triples_normalize_missing_lane_and_session() {
        assert_eq!(
            normalize_triple("w", None, None),
            normalize_triple("w", Some(""), Some(""))
        );
        assert_ne!(
            normalize_triple("w", Some("driver"), Some("s1")),
            normalize_triple("w", Some("driver"), Some("s2"))
        );
    }

    #[test]
    fn members_canonicalize_sorted_and_deduped() {
        let members = canonical_members(vec![
            SetMember { board_id: "b".into(), id: "t-2".into() },
            SetMember { board_id: "a".into(), id: "t-1".into() },
            SetMember { board_id: "b".into(), id: "t-2".into() },
        ]);
        assert_eq!(
            members,
            vec![
                SetMember { board_id: "a".into(), id: "t-1".into() },
                SetMember { board_id: "b".into(), id: "t-2".into() },
            ]
        );
    }

    #[test]
    fn empty_set_lifecycle_revises_freezes_binds_and_exits() {
        let (_root, connection) = temp_registry();
        let now = crate::registry::now_ms();
        let receipt = create_set(&connection, "joint-1", "op", "joint feature", now).unwrap();
        assert_eq!(receipt["revision"], json!(1));
        // Freezing parks taking; the frozen flag and revision 2 persist.
        let receipt = mutate_set(
            &connection, Path::new("/none"), "joint-1", 1, "freeze", None, "op", "pause", now,
        )
        .unwrap();
        assert_eq!(receipt["revision"], json!(2));
        let (frozen, head) = set_head(&connection, "joint-1").unwrap().unwrap();
        assert!(frozen && head.revision == 2 && head.members.is_empty());
        // An already-true change answers the stored revision, not a duplicate.
        let replay = mutate_set(
            &connection, Path::new("/none"), "joint-1", 2, "freeze", None, "op", "pause", now,
        )
        .unwrap();
        assert_eq!(replay["revision"], json!(2));
        // A stale revision is refused whole and names the current one.
        let stale = mutate_set(
            &connection, Path::new("/none"), "joint-1", 1, "unfreeze", None, "op", "go", now,
        )
        .unwrap_err();
        assert!(
            format!("{stale:#}").contains("at revision 2, not 1"),
            "stale refusal names the current revision: {stale:#}"
        );
        // Binding to the frozen set is allowed; claims on it are what freeze.
        let bound = bind(
            &connection, Path::new("/none"), "joint-1", 2, "w", Some("driver"), Some("s1"), 60,
            "op", "bind", now,
        )
        .unwrap();
        assert_eq!(bound["revision"], json!(3));
        assert_eq!(bound["status"], json!("active"));
        // The identical bind replays the stored receipt.
        let again = bind(
            &connection, Path::new("/none"), "joint-1", 3, "w", Some("driver"), Some("s1"), 60,
            "op", "bind", now,
        )
        .unwrap();
        assert_eq!(again["bindingID"], bound["bindingID"]);
        // A second lane for the same agent never inherits the first binding's
        // set: it is refused at bind time, and would be refused at claim time.
        let clash = bind(
            &connection, Path::new("/none"), "joint-1", 3, "w", Some("driver"), Some("s2"), 60,
            "op", "bind", now,
        )
        .unwrap_err();
        assert!(
            format!("{clash:#}").contains("already bound"),
            "one lane holds one binding: {clash:#}"
        );
        // The authorized exit ends taking and is itself a revision.
        let exit = revoke(
            &connection, Path::new("/none"), "joint-1", 3, "w", Some("driver"), Some("s1"), "op",
            "done", now,
        )
        .unwrap();
        assert_eq!(exit["revision"], json!(4));
        assert_eq!(exit["status"], json!("revoked"));
        // Revoking again answers the stored revocation, not a second exit.
        let exit_again = revoke(
            &connection, Path::new("/none"), "joint-1", 4, "w", Some("driver"), Some("s1"), "op",
            "done", now,
        )
        .unwrap();
        assert_eq!(exit_again["bindingID"], exit["bindingID"]);
        // A revoked triple releases nothing further; only a rebind re-arms it.
        let released = release_binding(
            &connection, "joint-1", "w", Some("driver"), Some("s1"), "bye", now,
        )
        .unwrap_err();
        assert!(
            format!("{released:#}").contains("no live binding"),
            "release after revocation refuses: {released:#}"
        );
        // Rebinding the revoked triple re-arms it through a new audited
        // revision — never a silent resurrection.
        let rearmed = bind(
            &connection, Path::new("/none"), "joint-1", 4, "w", Some("driver"), Some("s1"), 60,
            "op", "again", now,
        )
        .unwrap();
        assert_eq!(rearmed["revision"], json!(5));
        assert_eq!(rearmed["kind"], json!("rebind"));
        assert_eq!(rearmed["status"], json!("active"));
        assert_ne!(rearmed["bindingID"], exit["bindingID"]);
        // Releasing the rearmed binding ends it cleanly, and rebinding after
        // a release re-arms the same way.
        let done = release_binding(
            &connection, "joint-1", "w", Some("driver"), Some("s1"), "bye", now,
        )
        .unwrap();
        assert_eq!(done["revision"], json!(6));
        assert_eq!(done["status"], json!("released"));
        let rearmed = bind(
            &connection, Path::new("/none"), "joint-1", 6, "w", Some("driver"), Some("s1"), 60,
            "op", "again", now,
        )
        .unwrap();
        assert_eq!(rearmed["revision"], json!(7));
        assert_eq!(rearmed["kind"], json!("rebind"));
        // Every change above sits in the hash-chained revision log.
        let report = crate::audit::verify_registry(&connection).unwrap();
        assert!(
            report.healthy,
            "audit verify stays healthy across revisions: {:?}",
            report.errors
        );
        let revisions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM linked_set_revisions WHERE set_id='joint-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            revisions, 7,
            "create, freeze, bind, exit, rebind, release, rebind — no duplicates"
        );
    }

    #[test]
    fn unknown_set_and_bad_kind_refuse_without_writing() {
        let (_root, connection) = temp_registry();
        let now = crate::registry::now_ms();
        for refused in [
            mutate_set(
                &connection, Path::new("/none"), "missing", 1, "freeze", None, "op", "x", now,
            )
            .unwrap_err(),
            bind(
                &connection, Path::new("/none"), "missing", 1, "w", None, None, 60, "op", "x",
                now,
            )
            .unwrap_err(),
            revoke(
                &connection, Path::new("/none"), "missing", 1, "w", None, None, "op", "x", now,
            )
            .unwrap_err(),
            release_binding(&connection, "missing", "w", None, None, "x", now).unwrap_err(),
        ] {
            assert!(
                format!("{refused:#}").contains("no such selected set"),
                "unknown set refuses: {refused:#}"
            );
        }
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM linked_set_revisions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "refused writes leave no revision");
    }

    #[test]
    fn pairing_queries_see_live_and_retired_rows() {
        let (_root, connection) = temp_registry();
        connection
            .execute(
                "INSERT INTO linked_companions(id,board_a_id,board_a_registration,item_a_id,\
                 item_a_incarnation,board_b_id,board_b_registration,item_b_id,\
                 item_b_incarnation,created_by,created_at,retired) \
                 VALUES('lc-1','a','ra','t-1','ia','b','rb','t-2','ib','op',7,0)",
                [],
            )
            .unwrap();
        let live = live_pairing(&connection, "a", "t-1", "b", "t-2").unwrap().unwrap();
        assert_eq!(live.id, "lc-1");
        assert!(retired_pairing(&connection, "a", "t-1", "b", "t-2").unwrap().is_none());
        connection
            .execute(
                "UPDATE linked_companions SET retired=1,retired_by='op',retired_at=8,\
                 retire_reason='x' WHERE id='lc-1'",
                [],
            )
            .unwrap();
        assert!(live_pairing(&connection, "a", "t-1", "b", "t-2").unwrap().is_none());
        let retired = retired_pairing(&connection, "a", "t-1", "b", "t-2")
            .unwrap()
            .unwrap();
        assert_eq!(retired.id, "lc-1");
        // The reverse direction addresses the same stored row.
        assert!(live_pairing(&connection, "b", "t-2", "a", "t-1").unwrap().is_none());
    }

    #[test]
    fn evidence_sha_rule_takes_full_ids_and_refuses_short_and_moving() {
        let full40 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        assert_eq!(
            require_full_sha(full40, "the record's commit").unwrap(),
            full40
        );
        let upper = "DA39A3EE5E6B4B0D3255BFEF95601890AFD80709";
        assert_eq!(
            require_full_sha(upper, "the record's commit").unwrap(),
            full40,
            "object IDs lowercase on the way in"
        );
        let full64 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            require_full_sha(full64, "the record's commit").unwrap(),
            full64
        );
        for moving in ["latest", "HEAD", "head"] {
            let refused = require_full_sha(moving, "the record's commit").unwrap_err();
            assert!(
                format!("{refused:#}").contains("moving pointer"),
                "a moving pointer is refused as proof: {refused:#}"
            );
        }
        let short = require_full_sha("da39a3e", "the record's commit").unwrap_err();
        assert!(
            format!("{short:#}").contains("shortened"),
            "short hashes are refused, never expanded: {short:#}"
        );
        for bad in [
            "",
            "xyz",
            "main",
            "da39a3ee5e6b4b0d3255bfef95601890afd8070g",
        ] {
            let refused = require_full_sha(bad, "the record's commit").unwrap_err();
            assert!(
                format!("{refused:#}").contains("full 40- or 64-character"),
                "a non-ID is refused: {bad:?} got {refused:#}"
            );
        }
    }

    #[test]
    fn role_refusal_names_the_four_roles_and_quotes_what_was_declared() {
        for roles in [
            Vec::new(),
            vec!["baseline".to_owned(), "candidate".to_owned()],
            vec!["review".to_owned()],
        ] {
            let refused = role_refusal(&roles);
            for role in EVIDENCE_ROLES {
                assert!(
                    refused.contains(role),
                    "the refusal names all four roles: {refused}"
                );
            }
        }
        assert!(role_refusal(&[]).contains("none"), "no role quotes none");
        let two = role_refusal(&["baseline".to_owned(), "candidate".to_owned()]);
        assert!(
            two.contains("\"baseline\"") && two.contains("\"candidate\""),
            "two roles are both quoted: {two}"
        );
    }

    #[test]
    fn repo_identity_refuses_empty_and_credential_urls() {
        assert_eq!(
            require_repo_identity("acme/billing").unwrap(),
            "acme/billing"
        );
        assert_eq!(
            require_repo_identity("https://github.com/acme/billing").unwrap(),
            "https://github.com/acme/billing",
            "a bare URL without userinfo is a stable identity"
        );
        let empty = require_repo_identity("  ").unwrap_err();
        assert!(
            format!("{empty:#}").contains("missing"),
            "empty identity names the missing field: {empty:#}"
        );
        let creds = require_repo_identity("https://user:pass@github.com/acme/billing").unwrap_err();
        assert!(
            format!("{creds:#}").contains("credentials"),
            "userinfo URLs are refused before they land: {creds:#}"
        );
    }

    #[test]
    fn machine_paths_must_be_local_and_observed_at_epoch_millis() {
        assert_eq!(
            require_local_path("/tmp/checkout", "repo-path").unwrap(),
            "/tmp/checkout"
        );
        let remote = require_local_path("ssh://host/srv/repo", "repo-path").unwrap_err();
        assert!(
            format!("{remote:#}").contains("local filesystem"),
            "remote machine paths are out of scope: {remote:#}"
        );
        assert_eq!(require_observed_at("1790900000000").unwrap(), 1790900000000);
        for bad in ["0", "-7", "yesterday", ""] {
            assert!(
                require_observed_at(bad).is_err(),
                "observed time {bad:?} is refused"
            );
        }
    }

    fn fake_record(
        id: &str,
        role: &str,
        commit: &str,
        verification: &str,
        corrects: Option<&str>,
    ) -> ContributionRow {
        ContributionRow {
            id: id.to_owned(),
            board_id: "b".to_owned(),
            item_id: "t-1".to_owned(),
            deliverable: "d".to_owned(),
            kind: "code".to_owned(),
            repo: "acme/billing".to_owned(),
            commit_sha: commit.to_owned(),
            role: role.to_owned(),
            actor: "w".to_owned(),
            lane: "driver".to_owned(),
            session: "s1".to_owned(),
            host: "hax".to_owned(),
            worktree: "/work".to_owned(),
            branch: "wt/x".to_owned(),
            observed_at: 7,
            recorded_at: 8,
            evidence_refs: Vec::new(),
            note: String::new(),
            corrects: corrects.map(str::to_owned),
            sources: Vec::new(),
            mapping: String::new(),
            consumer_path: Vec::new(),
            consumes: String::new(),
            verify_run: String::new(),
            dep_kind: String::new(),
            verification: verification.to_owned(),
            verify_detail: String::new(),
            repo_path: String::new(),
            hop_paths: Vec::new(),
        }
    }

    #[test]
    fn superseded_receipts_count_for_nothing() {
        let records = vec![
            fake_record("le-1", "candidate", "a", "verified", None),
            fake_record("le-2", "candidate", "b", "verified", Some("le-1")),
        ];
        let current = current_records(&records);
        assert_eq!(
            current
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            vec!["le-2"],
            "the corrected receipt stays stored but counts for nothing"
        );
    }

    #[test]
    fn code_missing_names_each_gap_and_nothing_when_complete() {
        let empty = code_missing(&CodeEvidence::default(), "d");
        assert_eq!(
            empty.len(),
            4,
            "baseline, implementation, integration, candidate: {empty:?}"
        );
        let complete = CodeEvidence {
            baseline: true,
            implementations: vec!["c1".to_owned(), "c2".to_owned()],
            integrations: vec![("i".to_owned(), vec!["c1".to_owned(), "c2".to_owned()])],
            candidate: true,
            unverified: Vec::new(),
        };
        assert!(code_missing(&complete, "d").is_empty());
        let partial = CodeEvidence {
            integrations: vec![("i".to_owned(), vec!["c1".to_owned()])],
            ..complete_shallow()
        };
        let gaps = code_missing(&partial, "d");
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("c2") && gap.contains("unmapped")),
            "an integration that absorbs unmapped commits satisfies nothing: {gaps:?}"
        );
        let waiting = CodeEvidence {
            unverified: vec![
                "receipt le-9 (candidate c3) is unverified: no --repo-path".to_owned(),
            ],
            ..complete_shallow()
        };
        let gaps = code_missing(&waiting, "d");
        assert!(
            gaps.iter()
                .any(|gap| gap.contains("le-9") && gap.contains("satisfies nothing")),
            "unverified evidence reads as partial, never as joint: {gaps:?}"
        );
        let split = CodeEvidence {
            integrations: vec![
                ("i1".to_owned(), vec!["c1".to_owned()]),
                ("i2".to_owned(), vec!["c2".to_owned()]),
            ],
            ..complete_shallow()
        };
        assert!(
            code_missing(&split, "d").is_empty(),
            "coverage is the union across current integrations"
        );
    }

    fn complete_shallow() -> CodeEvidence {
        CodeEvidence {
            baseline: true,
            implementations: vec!["c1".to_owned(), "c2".to_owned()],
            integrations: Vec::new(),
            candidate: true,
            unverified: Vec::new(),
        }
    }

    #[test]
    fn binding_covers_only_the_bound_triple_and_its_members() {
        let (_root, connection) = temp_registry();
        connection
            .execute(
                "INSERT INTO linked_sets(id,created_by,created_at) VALUES('joint-u','op',10)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO linked_set_revisions(seq,set_id,revision,kind,author,reason,members,\
                 created_at,prev_hash,event_hash,payload) \
                 VALUES(1,'joint-u',1,'create','op','x','[{\"boardID\":\"b\",\"id\":\"t-1\"}]',11,\
                 'p','e','{}')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO linked_bindings(id,set_id,actor,lane,session,revision_checked,\
                 status,created_at,expires_at) \
                 VALUES('bb-1','joint-u','w','driver','s1',1,'active',12,9999999999999)",
                [],
            )
            .unwrap();
        let triple = normalize_triple("w", Some("driver"), Some("s1"));
        assert!(binding_covers(&connection, &triple, "b", "t-1", 100).unwrap());
        assert!(
            !binding_covers(&connection, &triple, "b", "t-2", 100).unwrap(),
            "membership outside the selected set is not covered"
        );
        let lookalike = normalize_triple("w", Some("other"), Some("s1"));
        assert!(
            !binding_covers(&connection, &lookalike, "b", "t-1", 100).unwrap(),
            "another lane's triple is not covered"
        );
    }
}
