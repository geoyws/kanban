//! The read-only JSON projection the operator SPA reads.
//!
//! **A projection, not a second read model** (ADR-048 §5). Every function
//! here answers with the rows a page arm of [`crate::serve`] already renders,
//! read through the same [`Store`] methods that arm calls, with the same
//! bounds. There is no SQL in this module, no connection, and no `&mut`
//! store method: a JSON surface that grew its own query would be a second
//! place authorization and readiness have to be right, and the second place
//! is always the one that is wrong. `docs/api/kanban-web.openapi.yaml` names
//! the methods per operation in `x-store-method`, and
//! `the_projection_reaches_nothing_but_the_store_and_the_registry` reads this
//! file back and holds the boundary (SPA-07).
//!
//! The shapes are the contract's, field for field; the structs below carry
//! `rename_all = "camelCase"` and nothing they do not need, because every
//! record schema in that document is `additionalProperties: false`.
//!
//! **No lease token, structurally** (SPA-09). A task's holder is served as
//! [`ClaimSummary`], which has no token field to forget to strip; the
//! conversion from [`crate::model::Claim`] is the model's own.

use serde::Serialize;

use crate::model::{
    Attention, Checkpoint, ClaimSummary, Event, RuleSummary, Sitrep, Task, TaskNote,
};
use crate::registry::Registry;
use crate::serve::{
    DECIDED_ROWS, DECIDED_SCAN, DETAIL_ROWS, LANE_UPDATE_ROWS, OPEN_ATTENTION_ROWS, lane_groups,
    markdown, project_named, projects, sort_decided_queue, sort_open_queue,
};

/// Why a projection could not answer.
///
/// The split is the whole of the status mapping: one variant is the store's
/// own non-enumerating denial and becomes `404` with the generic sentence, and
/// the other keeps its cause here — where a log or a test can read it — while
/// the route answers a sentence that names nothing.
pub enum Refusal {
    /// The board or the row named is unknown, retired, ambiguous, or invisible
    /// to this caller. One answer for all four, because a refusal that
    /// distinguishes them is an existence oracle (`rust/authz.rs:33`).
    DeniedOrNotFound,
    /// Anything else. Never rendered.
    Failed(anyhow::Error),
}

/// The store's single generic denial, as it is worded at its source
/// (`rust/authz.rs:33`, `rust/policy.rs:1157`).
///
/// Classifying on it rather than inventing a second refusal string is
/// deliberate: authorization is decided in the store, and the projection's job
/// is to carry that decision across the boundary with its status code intact
/// (SPA-08), not to re-derive it.
const DENIED_OR_NOT_FOUND: &str = "denied or not found";

impl From<anyhow::Error> for Refusal {
    fn from(error: anyhow::Error) -> Self {
        if error
            .chain()
            .any(|cause| cause.to_string().contains(DENIED_OR_NOT_FOUND))
        {
            return Self::DeniedOrNotFound;
        }
        Self::Failed(error)
    }
}

impl From<Refusal> for anyhow::Error {
    /// A page arm that shares a projection keeps its own error text: the
    /// generic sentence is the JSON route's answer, not the projection's
    /// value, so the HTML surface is unchanged by sharing the computation.
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::DeniedOrNotFound => anyhow::anyhow!(DENIED_OR_NOT_FOUND),
            Refusal::Failed(error) => error,
        }
    }
}

/// What a projection returns: the rows, or one of the two refusals.
pub type Projected<T> = std::result::Result<T, Refusal>;

/// A bounded listing that says whether it was cut (ADR-037 §4, option A).
///
/// `truncated` is observed and never inferred: a capped listing asks its store
/// method for one row past the bound it will return and reports whether that
/// row came back. A page that got `returned == limit` and had to guess would
/// be guessing about exactly the thing an operator plans around.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing<T> {
    items: Vec<T>,
    returned: usize,
    limit: Option<i64>,
    truncated: bool,
}

impl<T> Listing<T> {
    /// One store call, bounded: hand in the rows an over-fetch of `limit + 1`
    /// returned and the bound itself.
    fn capped(mut rows: Vec<T>, limit: i64) -> Self {
        let truncated = i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit;
        rows.truncate(usize::try_from(limit.max(0)).unwrap_or(usize::MAX));
        Self {
            returned: rows.len(),
            items: rows,
            limit: Some(limit),
            truncated,
        }
    }

    /// Rows merged from several bounded calls: the bound is per call, and the
    /// caller has already observed whether any one of them was cut.
    fn merged(rows: Vec<T>, limit: i64, truncated: bool) -> Self {
        Self {
            returned: rows.len(),
            items: rows,
            limit: Some(limit),
            truncated,
        }
    }

    /// A listing its store call cannot cut. `limit` is null and `truncated` is
    /// false by construction, not by luck.
    fn complete(rows: Vec<T>) -> Self {
        Self {
            returned: rows.len(),
            items: rows,
            limit: None,
            truncated: false,
        }
    }
}

/// One open or decided item paired with the board that raised it.
///
/// The board name is not a field of [`Attention`] — a board holds its own
/// rows, and the name only exists once they are merged across boards.
///
/// Two fields beyond the row itself, both of them things the mounted card
/// cannot derive and must not invent:
///
/// - `body_html` is the row's body typeset by [`markdown`], the one bounded
///   place in this crate with a parser in front of agent-authored text. The
///   deck renders those bytes; a second renderer in the client would be a
///   second sanitiser, and the second one is always the one that is wrong.
/// - `task` is the row's task as the card's meta sentence reads it — the
///   kind and the title, resolved through `Store::require_task` exactly as
///   `serve`'s own `task_reference` resolves it, and absent when the row
///   names no task or the task is gone.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionCard {
    board: String,
    attention: Attention,
    body_html: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<TaskReference>,
}

/// What a card says about the row it is about: enough for the sentence, and
/// nothing that is not in it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskReference {
    id: String,
    task_type: String,
    title: String,
}

/// One row of the board index, as `/boards` computes it.
///
/// The page renders these same values into a table, from this same function:
/// there is one computation of a board's counts on the served surface.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSummary {
    pub board: String,
    pub open_attention: i64,
    pub todo: usize,
    pub in_progress: usize,
    pub stale: usize,
    pub handoffs: i64,
    pub tasks: usize,
}

/// One board's rows and the rules that frame it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardDetail {
    board: String,
    roots: Vec<String>,
    tasks: Listing<BoardTaskRow>,
    rules: Vec<BoardRule>,
}

/// One row of a board's task list, with the count the board page reads it
/// with.
///
/// The count is not derivable from [`Task`]: it is the board's open attention
/// against that row, which is what the served page's row sentence ends on
/// (`attention_clause`, `rust/serve.rs`). A client that had the tasks and not
/// the counts would have to ask once per row, which is the fan-out a
/// projection exists to avoid.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardTaskRow {
    task: Task,
    open_attention: usize,
}

/// One applicable rule: the registry's own summary, plus the body the board
/// page folds open.
///
/// The summary's three derived fields stay the registry's — they are not
/// recomputed here — and `body` is the same rule read back through
/// `applicable_rules` under the identical filter, because the served page
/// shows the whole rule inside its fold and a page that only had the
/// headline would be a different page.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardRule {
    #[serde(flatten)]
    summary: RuleSummary,
    body: String,
}

/// One `(board, lane)` group and what that lane last said about itself.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneSummary {
    board: String,
    lane: String,
    updates: Vec<LaneUpdate>,
}

/// One sitrep as the lanes page reads it: the row, and its body typeset by
/// the server's one markdown renderer.
///
/// `bodyHtml` is here for the reason [`AttentionCard`]'s is: the page renders
/// agent-authored markdown, and a second renderer in the client would be a
/// second sanitiser (SPA-41).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaneUpdate {
    #[serde(flatten)]
    update: Sitrep,
    body_html: String,
}

/// One task, who holds it, and its trail.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    board: String,
    task: Task,
    /// `Store::get_claim`, converted. Never [`crate::model::Claim`], which
    /// carries the lease token (SPA-09).
    claim: Option<ClaimSummary>,
    open_attention: Vec<Attention>,
    notes: Listing<TaskNote>,
    checkpoints: Listing<Checkpoint>,
    events: Listing<Event>,
}

/// Every open attention item across every board, in the order the deck shows
/// it.
///
/// `Store::attention(Some("open"), …)` per active board, at the arm's own
/// per-board bound, then the arm's own ordering — priority first, then oldest,
/// then id, then board.
///
/// The boards' stores are kept across the merge because a card resolves its
/// own task reference, exactly as the page's own `open_attention` keeps them
/// for the same reason: the row carries a task id, and the sentence the card
/// reads carries the task's kind and title.
pub fn needs_you() -> Projected<Listing<AttentionCard>> {
    let mut items: Vec<(String, Attention)> = Vec::new();
    let mut stores = std::collections::BTreeMap::new();
    let mut truncated = false;
    for (project, store) in projects()? {
        let mut rows = store.attention(
            Some("open"),
            None,
            None,
            None,
            None,
            OPEN_ATTENTION_ROWS + 1,
            false,
        )?;
        if i64::try_from(rows.len()).unwrap_or(i64::MAX) > OPEN_ATTENTION_ROWS {
            truncated = true;
            rows.truncate(usize::try_from(OPEN_ATTENTION_ROWS).unwrap_or(usize::MAX));
        }
        for item in rows {
            items.push((project.name.clone(), item));
        }
        stores.insert(project.name.clone(), store);
    }
    sort_open_queue(&mut items);
    Ok(Listing::merged(
        attention_cards(items, &stores),
        OPEN_ATTENTION_ROWS,
        truncated,
    ))
}

/// One merged `(board, row)` list as the cards the deck and the decisions
/// room both render.
///
/// Shared rather than copied because the two queues differ only in which
/// rows they hold: the card is the same card, with the same one markdown
/// renderer in front of the body and the same task reference resolved
/// through the board's own store.
fn attention_cards(
    items: Vec<(String, Attention)>,
    stores: &std::collections::BTreeMap<String, crate::store::Store>,
) -> Vec<AttentionCard> {
    items
        .into_iter()
        .map(|(board, attention)| {
            let task = attention.task_id.as_deref().and_then(|id| {
                stores
                    .get(&board)
                    .and_then(|store| store.require_task(id).ok())
                    .map(|task| TaskReference {
                        id: task.id,
                        task_type: task.task_type,
                        title: task.title,
                    })
            });
            AttentionCard {
                board,
                body_html: markdown(&attention.body),
                attention,
                task,
            }
        })
        .collect()
}

/// Every readable board at a glance, most urgent first.
///
/// This is the board index's own computation, shared with the page rather than
/// copied: `boards` (`rust/serve.rs`) renders exactly these rows in exactly
/// this order. A retired board is not here and neither is one this principal
/// may not read, because both are absent from `projects`, and absence is the
/// whole denial (SPA-08).
pub fn board_summaries() -> Projected<Vec<BoardSummary>> {
    let mut rows = Vec::new();
    for (project, store) in projects()? {
        let tasks = store.list_tasks(None, None, None, None, false)?;
        let count = |status: &str| tasks.iter().filter(|task| task.status == status).count();
        // Counted, not fetched: a page used as a count saturates silently.
        // Only the most urgent row of each ranks the board, and neither is
        // serialised — the derived order is.
        let open_attention = store.count_open_attention()?;
        let pending_handoffs = store.count_pending_handoffs()?;
        let urgent_attention = store.attention(Some("open"), None, None, None, None, 1, false)?;
        let urgent_handoff = store.handoffs(None, Some("pending"), None, 1, false)?;
        let queued = tasks
            .iter()
            .filter(|task| task.status == "todo")
            .map(|task| (task.priority, task.created_at))
            .chain(
                urgent_attention
                    .iter()
                    .map(|item| (item.priority, item.created_at)),
            )
            .chain(
                urgent_handoff
                    .iter()
                    .map(|item| (item.priority, item.created_at)),
            )
            .collect::<Vec<_>>();
        let highest = queued
            .iter()
            .map(|(priority, _)| *priority)
            .min()
            .unwrap_or(i64::MAX);
        let oldest = queued
            .iter()
            .filter(|(priority, _)| *priority == highest)
            .map(|(_, created_at)| *created_at)
            .min()
            .unwrap_or(i64::MAX);
        let summary = BoardSummary {
            board: project.name.clone(),
            open_attention,
            todo: count("todo"),
            in_progress: count("in_progress"),
            stale: store.stale_tasks()?.len(),
            handoffs: pending_handoffs,
            tasks: tasks.len(),
        };
        rows.push((highest, oldest, project.name.clone(), summary));
    }
    rows.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    Ok(rows.into_iter().map(|(_, _, _, summary)| summary).collect())
}

/// The board index as the JSON surface serves it.
///
/// The listing enumerates every readable board and no store call here takes a
/// bound, so `limit` is null and `truncated` is false.
pub fn boards() -> Projected<Listing<BoardSummary>> {
    Ok(Listing::complete(board_summaries()?))
}

/// One board's rows and its applicable rules.
///
/// `Store::list_tasks(None, None, None, false)` takes no limit, so the tasks
/// envelope reports none. Each row carries the board's open attention
/// against it — `Store::attention` filtered to that task, which is the count
/// the served page's row sentence ends on.
///
/// The rules are registry state, not board state — the one documented
/// exception to `x-store-method`, recorded as `x-registry-method` on
/// `getBoard`. `applicable_rule_summaries` is the registry's own
/// [`RuleSummary`] projection of `applicable_rules`: the filter is identical
/// and the headline, `hasMore` and byte count are derived once, in the
/// registry, rather than a second time in the web layer. The bodies come
/// from `applicable_rules` under that same filter, because the page folds
/// the whole rule open and the summary does not carry it.
pub fn board(name: &str) -> Projected<BoardDetail> {
    let (project, store) = project_named(name).map_err(|_| Refusal::DeniedOrNotFound)?;
    let mut rows = Vec::new();
    for task in store.list_tasks(None, None, None, None, false)? {
        let open_attention = store
            .attention(
                Some("open"),
                None,
                Some(&task.id),
                None,
                None,
                OPEN_ATTENTION_ROWS,
                false,
            )?
            .len();
        rows.push(BoardTaskRow {
            task,
            open_attention,
        });
    }
    let registry = Registry::open()?;
    let mut bodies = registry
        .applicable_rules(Some(&project.name), None, None, false)?
        .into_iter()
        .map(|rule| (rule.id, rule.body))
        .collect::<std::collections::BTreeMap<_, _>>();
    let rules = registry
        .applicable_rule_summaries(Some(&project.name), None, None, false)?
        .into_iter()
        .map(|summary| {
            let body = bodies.remove(&summary.id).unwrap_or_default();
            BoardRule { summary, body }
        })
        .collect();
    Ok(BoardDetail {
        board: project.name,
        roots: project.workspace_roots,
        tasks: Listing::complete(rows),
        rules,
    })
}

/// What each lane last said about itself, most recently active first.
///
/// The grouping and the ordering are `lanes`' own, shared with the page.
pub fn lanes() -> Projected<Listing<LaneSummary>> {
    let (groups, truncated) = lane_groups(LANE_UPDATE_ROWS)?;
    let items = groups
        .into_iter()
        .map(|((board, lane), updates)| LaneSummary {
            board,
            lane,
            updates: updates
                .into_iter()
                .map(|update| LaneUpdate {
                    body_html: markdown(&update.body),
                    update,
                })
                .collect(),
        })
        .collect();
    Ok(Listing::merged(items, LANE_UPDATE_ROWS, truncated))
}

/// One task, its holder, its notes, checkpoints and audit trail.
///
/// Every list is the detail page's own bound, `DETAIL_ROWS`, over-fetched by
/// one row so the envelope's `truncated` is observed rather than guessed.
pub fn task(project_name: &str, id: &str) -> Projected<TaskDetail> {
    let (project, store) = project_named(project_name).map_err(|_| Refusal::DeniedOrNotFound)?;
    // A row that is invisible and a row that is absent answer the same way
    // here, which is the same answer `require_task` already gives.
    let task = store
        .require_task(id)
        .map_err(|_| Refusal::DeniedOrNotFound)?;
    let claim = store.get_claim(&task.id)?;
    let open_attention = store.attention(
        Some("open"),
        None,
        Some(&task.id),
        None,
        None,
        OPEN_ATTENTION_ROWS,
        false,
    )?;
    let notes = store.notes(&task.id, DETAIL_ROWS + 1)?;
    let checkpoints = store.checkpoints(&task.id, DETAIL_ROWS + 1)?;
    let events = store.events(Some(&task.id), None, DETAIL_ROWS + 1, true)?;
    Ok(TaskDetail {
        board: project.name,
        task,
        claim: claim.as_ref().map(ClaimSummary::from),
        open_attention,
        notes: Listing::capped(notes, DETAIL_ROWS),
        checkpoints: Listing::capped(checkpoints, DETAIL_ROWS),
        events: Listing::capped(events, DETAIL_ROWS),
    })
}

// --- the decisions room ---------------------------------------------------

/// The decisions room's answer: the merged rows, plus the per-board scan
/// that fed the merge.
///
/// Two bounds shape this page and a reader has to be able to tell them
/// apart, so both are on the wire: `limit` is the cut the merge is served
/// at, and `scanLimit` is how far each board was read before merging.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecidedListing {
    #[serde(flatten)]
    listing: Listing<AttentionCard>,
    scan_limit: i64,
}

/// What was decided, newest first, across every readable board.
///
/// `Store::recent_resolved_attention(DECIDED_SCAN)` per board, then the
/// decisions room's own ordering — newest `resolved_at`, then id, then board
/// — and the merge cut to `DECIDED_ROWS`. The ordering is
/// [`sort_decided_queue`], shared with the page rather than copied, so the
/// two surfaces cannot disagree about which decision is newest.
pub fn decided() -> Projected<DecidedListing> {
    let mut items: Vec<(String, Attention)> = Vec::new();
    let mut stores = std::collections::BTreeMap::new();
    for (project, store) in projects()? {
        let name = project.name.clone();
        for item in store.recent_resolved_attention(DECIDED_SCAN)? {
            items.push((name.clone(), item));
        }
        stores.insert(name, store);
    }
    sort_decided_queue(&mut items);
    // Cut here, on the merge, rather than by over-fetching: the bound is the
    // merged list's, and `truncated` is the observation that the merge held
    // more rows than the cut kept.
    let cards = attention_cards(items, &stores);
    Ok(DecidedListing {
        listing: Listing::capped(cards, DECIDED_ROWS),
        scan_limit: DECIDED_SCAN,
    })
}
