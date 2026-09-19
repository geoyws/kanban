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

use crate::model::{
    Attention, Checkpoint, ClaimSummary, DeadLetterCode, DeploymentAttempt, Event, RuleSummary,
    SearchResult, Sitrep, Sprint, Subscription, SubscriptionPosition, Task, TaskNote,
    UnreadableBoard,
};
use crate::registry::Registry;
use crate::serve::{
    DECIDED_ROWS, DECIDED_SCAN, DETAIL_ROWS, LANE_UPDATE_ROWS, OPEN_ATTENTION_ROWS,
    PREVIEW_BODY_CHARS, SEARCH_LIMIT, excerpt, lane_groups, markdown, project_named, projects,
    search_receipt, sort_decided_queue, sort_open_queue, task_attention_count,
};
use crate::store::Store;
use serde::Serialize;

/// Why a projection could not answer.
///
/// The split is the whole of the status mapping: one variant is the store's
/// own non-enumerating denial and becomes `404` with the generic sentence, and
/// the other keeps its cause here — where a log or a test can read it — while
/// the route answers a sentence that names nothing.
// `Debug` so a refusal can be printed where it is read: a log line, and a
// test that expects a projection to answer and has to say what came back
// instead.
#[derive(Debug)]
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

    /// The same listing over a richer row, with the envelope untouched.
    ///
    /// Typesetting a body is not free, so the rows are mapped after the
    /// bound has already dropped the over-fetched one: the reader pays for
    /// what is served and nothing else.
    fn map<U>(self, convert: impl FnMut(T) -> U) -> Listing<U> {
        Listing {
            items: self.items.into_iter().map(convert).collect(),
            returned: self.returned,
            limit: self.limit,
            truncated: self.truncated,
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
///
/// Three fields carry agent-authored prose already typeset by [`markdown`],
/// for the reason [`AttentionCard::body_html`] does: the page renders those
/// bytes, and a second renderer in the client would be a second sanitiser
/// (SPA-41). The server-rendered page typeset the task body, every note and
/// every open attention row, so the projection that replaces it carries the
/// same three.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetail {
    board: String,
    task: Task,
    /// The task's own body, typeset. `None` when the row has no body.
    body_html: Option<String>,
    /// `Store::get_claim`, converted. Never [`crate::model::Claim`], which
    /// carries the lease token (SPA-09).
    claim: Option<ClaimSummary>,
    open_attention: Vec<RenderedAttention>,
    notes: Listing<RenderedNote>,
    checkpoints: Listing<Checkpoint>,
    events: Listing<Event>,
}

/// One note and its body as the page shows it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedNote {
    note: TaskNote,
    body_html: String,
}

/// One attention row and its body as the page shows it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedAttention {
    attention: Attention,
    body_html: String,
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
        body_html: task.body.as_deref().map(markdown),
        task,
        claim: claim.as_ref().map(ClaimSummary::from),
        open_attention: open_attention
            .into_iter()
            .map(|attention| RenderedAttention {
                body_html: markdown(&attention.body),
                attention,
            })
            .collect(),
        notes: Listing::capped(notes, DETAIL_ROWS).map(|note| RenderedNote {
            body_html: markdown(&note.body),
            note,
        }),
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

// --- releases and subscriptions ---

/// How many started attempts one board contributes to the index, as
/// `/deployments` has always asked for them.
const ACTIVE_DEPLOYMENT_ROWS: i64 = 100;

/// How many failed, and separately how many abandoned, attempts one board
/// contributes. The two statuses are two calls, so this bound is per board
/// per status and the envelope says so.
const FAILED_DEPLOYMENT_ROWS: i64 = 30;

/// One attempt paired with the board it was recorded on.
///
/// The board is not a column of the attempt — a board holds its own rows,
/// and the name only exists once they are merged across boards.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardDeployment {
    board: String,
    deployment: DeploymentAttempt,
}

/// The deployment matrix: three groups with three different caps.
///
/// Three envelopes rather than one, because one envelope would have to pick
/// a number and be wrong about the other two: `current` cannot be cut at
/// all, `active` is cut at a hundred per board and `failures` at thirty per
/// board per status.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentIndex {
    current: Listing<BoardDeployment>,
    active: Listing<BoardDeployment>,
    failures: Listing<BoardDeployment>,
}

/// One attempt and the board it belongs to.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentDetail {
    board: String,
    deployment: DeploymentAttempt,
}

/// One subscription, where it has actually got to, and what refused it.
///
/// The head travels with the position because the two are only meaningful
/// together: "acked through seq 8" says nothing until you know whether the
/// board is at seq 8 or seq 800. The codes travel with the dead-letter count
/// for the same reason — a count without them is a number nobody can act on.
///
/// **No lease token, structurally** (SPA-09): `position.leased` is how many
/// deliveries are held, and the token that holds them is not a field of
/// anything serialised here.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionView {
    board: String,
    subscription: Subscription,
    position: SubscriptionPosition,
    dead_letter_codes: Vec<DeadLetterCode>,
    head_event_seq: i64,
}

/// Cut an over-fetched page back to its bound, recording that it was cut.
///
/// Every bounded call below asks for one row past the bound it will return,
/// so `truncated` is observed rather than inferred from a row count that
/// happens to equal the bound (ADR-037 §1).
fn cut_to(rows: &mut Vec<DeploymentAttempt>, limit: i64, truncated: &mut bool) {
    if i64::try_from(rows.len()).unwrap_or(i64::MAX) > limit {
        *truncated = true;
        rows.truncate(usize::try_from(limit.max(0)).unwrap_or(usize::MAX));
    }
}

/// Verified current releases, what is in flight, and what went wrong.
///
/// The three groups and their orders are the `/deployments` page's own:
/// current releases by repository, tier, environment and board, and both
/// attempt lists newest first.
pub fn deployments() -> Projected<DeploymentIndex> {
    let mut current = Vec::new();
    let mut active = Vec::new();
    let mut failures = Vec::new();
    let mut active_truncated = false;
    let mut failures_truncated = false;
    for (project, store) in projects()? {
        let pair = |deployment| BoardDeployment {
            board: project.name.clone(),
            deployment,
        };
        current.extend(store.current_deployments()?.into_iter().map(pair));
        let mut started =
            store.deployments(Some("started"), None, false, ACTIVE_DEPLOYMENT_ROWS + 1)?;
        cut_to(&mut started, ACTIVE_DEPLOYMENT_ROWS, &mut active_truncated);
        active.extend(started.into_iter().map(pair));
        for status in ["failed", "abandoned"] {
            let mut rows =
                store.deployments(Some(status), None, false, FAILED_DEPLOYMENT_ROWS + 1)?;
            cut_to(&mut rows, FAILED_DEPLOYMENT_ROWS, &mut failures_truncated);
            failures.extend(rows.into_iter().map(pair));
        }
    }
    current.sort_by(|a, b| {
        (
            &a.deployment.repo,
            &a.deployment.tier,
            &a.deployment.environment,
            &a.board,
        )
            .cmp(&(
                &b.deployment.repo,
                &b.deployment.tier,
                &b.deployment.environment,
                &b.board,
            ))
    });
    active.sort_by_key(|row| std::cmp::Reverse(row.deployment.created_at));
    failures.sort_by_key(|row| std::cmp::Reverse(row.deployment.created_at));
    Ok(DeploymentIndex {
        current: Listing::complete(current),
        active: Listing::merged(active, ACTIVE_DEPLOYMENT_ROWS, active_truncated),
        failures: Listing::merged(failures, FAILED_DEPLOYMENT_ROWS, failures_truncated),
    })
}

/// One attempt, its identity mode and what its finish observed.
pub fn deployment(project_name: &str, id: &str) -> Projected<DeploymentDetail> {
    let (project, store) = project_named(project_name).map_err(|_| Refusal::DeniedOrNotFound)?;
    // An attempt that is invisible and one that is absent answer the same
    // way, which is the answer `require_deployment` already gives.
    let deployment = store
        .require_deployment(id)
        .map_err(|_| Refusal::DeniedOrNotFound)?;
    Ok(DeploymentDetail {
        board: project.name,
        deployment,
    })
}

/// Every subscription on every readable board, with its position.
///
/// Two store calls per board, both outside the row loop, exactly as the page
/// makes them: the grouped delivery projection plus the head it is measured
/// against. Paused rows are read whatever the client's filter says, so a
/// hidden row can be counted and offered rather than reading as "nothing
/// exists" — the filter is the client's display choice, not a read bound.
///
/// Neither call takes a limit, so the listing cannot be cut.
pub fn subscriptions() -> Projected<Listing<SubscriptionView>> {
    let mut items = Vec::new();
    for (project, store) in projects()? {
        let positions = store.subscription_positions()?;
        for subscription in store.subscriptions(None, None, true)? {
            items.push(SubscriptionView {
                board: project.name.clone(),
                position: positions.position(&subscription.id),
                dead_letter_codes: positions.dead_letter_codes(&subscription.id).to_vec(),
                head_event_seq: positions.head_event_seq,
                subscription,
            });
        }
    }
    items.sort_by(|a, b| {
        (&a.board, a.subscription.created_at, &a.subscription.id).cmp(&(
            &b.board,
            b.subscription.created_at,
            &b.subscription.id,
        ))
    });
    Ok(Listing::complete(items))
}

// --- sprints and plans ---

/// One sprint with the two counts its card shows and its goal, typeset.
///
/// `sprint_summary` (`rust/serve.rs`) renders exactly these values, from
/// exactly this pair of store calls, so a card on the page and a card on
/// the wire cannot disagree about how much of a sprint is still open.
///
/// `goal_html` is the sprint body through [`markdown`] — the same renderer
/// and the same sanitiser the served card used, because a second markdown
/// implementation in the client would be a second sanitiser (SPA-41).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SprintCard {
    sprint: Sprint,
    open_tasks: i64,
    done_tasks: i64,
    goal_html: Option<String>,
}

/// One board's release boundary and everything either side of it.
///
/// `current` is `null` rather than absent when the board has none: a board
/// between sprints is a state the page names, and a missing key would leave
/// the client to decide whether it meant "none" or "not asked".
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardSprints {
    board: String,
    current: Option<SprintCard>,
    history: Vec<SprintCard>,
}

/// One sprint's goal, dates, visible scope and the deployment that closed it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SprintDetail {
    board: String,
    sprint: Sprint,
    open_tasks: i64,
    done_tasks: i64,
    /// The goal, typeset by [`markdown`], as on [`SprintCard`].
    goal_html: Option<String>,
    /// `Store::sprint_tasks` takes no bound, so this is the whole visible
    /// scope and needs no envelope to say so.
    tasks: Vec<Task>,
    closing_deployment: Option<DeploymentAttempt>,
}

/// One draft epic: the plan, what it holds back, and the plan itself.
///
/// Three fields beyond the row, each of them something the page renders and
/// the client must not derive:
///
/// - `body_html` is the plan body through [`markdown`], the one renderer and
///   the one sanitiser this crate has for agent-authored text.
/// - `children` are the rows the draft gates — the page lists them because
///   opening the plan is what releases them, so they are the plan's meaning
///   rather than a related listing.
/// - `open_attention` is the count the page badges, on the plan and on each
///   child alike.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanCard {
    board: String,
    task: Task,
    body_html: Option<String>,
    open_attention: usize,
    children: Vec<PlanChild>,
}

/// One row a draft plan holds back.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanChild {
    task: Task,
    open_attention: usize,
}

/// One sprint's card: the row and `Store::sprint_task_counts`' own pair.
fn sprint_card(store: &Store, sprint: Sprint) -> Projected<SprintCard> {
    let (open_tasks, done_tasks) = store.sprint_task_counts(&sprint.id)?;
    Ok(SprintCard {
        goal_html: sprint.body.as_deref().map(markdown),
        sprint,
        open_tasks,
        done_tasks,
    })
}

/// One board's sprint state, as `board_sprints_content` assembles it.
///
/// The history call is `Store::sprints(None, true, i64::MAX)` — the page's
/// own, bound included — and everything whose status is not `current` is
/// history, planned rows and abandoned ones with the closed ones. There is
/// no cap to report because the store call has none.
fn board_sprint_state(board: String, store: &Store) -> Projected<BoardSprints> {
    let current = match store.current_sprint()? {
        Some(sprint) => Some(sprint_card(store, sprint)?),
        None => None,
    };
    let mut history = Vec::new();
    for sprint in store.sprints(None, true, i64::MAX)? {
        if sprint.status != "current" {
            history.push(sprint_card(store, sprint)?);
        }
    }
    Ok(BoardSprints {
        board,
        current,
        history,
    })
}

/// Every readable board's release boundary and sprint history.
///
/// Uncapped by construction: the history call passes `i64::MAX` and the
/// board enumeration takes no bound, so `limit` is null and `truncated` is
/// false rather than a number nothing enforces.
pub fn sprints() -> Projected<Listing<BoardSprints>> {
    let mut boards = Vec::new();
    for (project, store) in projects()? {
        boards.push(board_sprint_state(project.name.clone(), &store)?);
    }
    Ok(Listing::complete(boards))
}

/// One named board's release boundary and sprint history.
pub fn board_sprints(name: &str) -> Projected<BoardSprints> {
    let (project, store) = project_named(name).map_err(|_| Refusal::DeniedOrNotFound)?;
    board_sprint_state(project.name.clone(), &store)
}

/// One sprint, its visible scope and the attempt that proved its version.
///
/// A sprint this principal may not read and a sprint that does not exist
/// answer the same way, which is the answer `require_sprint` already gives.
pub fn sprint(project_name: &str, id: &str) -> Projected<SprintDetail> {
    let (project, store) = project_named(project_name).map_err(|_| Refusal::DeniedOrNotFound)?;
    let sprint = store
        .require_sprint(id)
        .map_err(|_| Refusal::DeniedOrNotFound)?;
    let tasks = store.sprint_tasks(&sprint.id)?;
    let (open_tasks, done_tasks) = store.sprint_task_counts(&sprint.id)?;
    let closing_deployment = match &sprint.closed_by_deployment {
        Some(deployment_id) => Some(store.require_deployment(deployment_id)?),
        None => None,
    };
    Ok(SprintDetail {
        board: project.name,
        goal_html: sprint.body.as_deref().map(markdown),
        sprint,
        open_tasks,
        done_tasks,
        tasks,
        closing_deployment,
    })
}

/// Every draft epic on every readable board, with the work each one gates.
///
/// The filter is the page's and the write verb's: type `epic`, status
/// `draft`. `Store::list_tasks` takes no bound, so the listing is complete.
pub fn plans() -> Projected<Listing<PlanCard>> {
    let mut cards = Vec::new();
    for (project, store) in projects()? {
        let tasks = store.list_tasks(None, None, None, None, false)?;
        for plan in tasks
            .iter()
            .filter(|task| task.status == "draft" && task.task_type == "epic")
        {
            let mut children = Vec::new();
            for child in tasks
                .iter()
                .filter(|task| task.parent_id.as_deref() == Some(plan.id.as_str()))
            {
                children.push(PlanChild {
                    task: child.clone(),
                    open_attention: task_attention_count(&store, &child.id)?,
                });
            }
            cards.push(PlanCard {
                board: project.name.clone(),
                body_html: plan.body.as_deref().map(markdown),
                open_attention: task_attention_count(&store, &plan.id)?,
                task: plan.clone(),
                children,
            });
        }
    }
    Ok(Listing::complete(cards))
}

// --- search and previews ---

/// The bounded result set and the receipt that bounds it.
///
/// [`crate::model::SearchReceipt`] with its results promoted into the
/// listing, so a search answers the same envelope every other listing does
/// and a reader learns from one field whether the bound was reached.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    #[serde(flatten)]
    listing: Listing<SearchResult>,
    query: String,
    embedding_model: String,
    boards: Vec<String>,
    missing_boards: Vec<String>,
    unreadable_boards: Vec<UnreadableBoard>,
    result_chars: usize,
    generated_at: i64,
}

/// What a reference link answers on hover, discriminated by `kind`.
///
/// Exactly the four the page has: the payload field that matches `kind` is
/// present and the others are absent, rather than null, because a card that
/// is not about a deployment has no deployment to be null about.
///
/// Three fields beyond the records, each of them something the served
/// fragment showed and the client cannot derive:
///
/// - `body_html` is the task body's first [`PREVIEW_BODY_CHARS`] characters
///   typeset by [`markdown`], as `task_preview` typeset them; the client
///   renders those bytes rather than parsing agent prose a second time.
/// - `open_attention` is how many open items name the task, which is the
///   sentence "it is on Needs you" the preview ends on.
/// - `parent` and `about` are the references the fragment carried as links:
///   a task's parent, and the row an attention item was raised against.
///   They are what makes a preview nest, so they are resolved here exactly
///   as `task_reference` resolves them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    kind: String,
    board: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<Task>,
    #[serde(skip_serializing_if = "Option::is_none")]
    body_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_attention: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<TaskReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<Attention>,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<TaskReference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deployment: Option<DeploymentAttempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    board_counts: Option<BoardCounts>,
}

/// The four numbers a board's own preview prints.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoardCounts {
    open_attention: i64,
    todo: usize,
    in_progress: usize,
    tasks: usize,
}

impl Preview {
    /// One kind's card, with every other kind's payload absent.
    fn of(kind: &str, board: String) -> Self {
        Self {
            kind: kind.to_owned(),
            board,
            task: None,
            body_html: None,
            open_attention: None,
            parent: None,
            attention: None,
            about: None,
            deployment: None,
            board_counts: None,
        }
    }
}

/// How many rows the attention listing a preview reads is bounded to, as
/// `attention_preview` bounds it: no single-row getter exists and a hover is
/// not the reason to add one.
const PREVIEW_ATTENTION_ROWS: i64 = 500;

/// Hybrid search across every readable board, and the rules.
///
/// The whole retrieval is [`search_receipt`], shared with the page rather
/// than reimplemented: same boards, same ranking, same bound. `truncated` is
/// the receipt's own, promoted into the envelope rather than recomputed from
/// the rows that survived it.
pub fn search(query: &str) -> Projected<SearchPage> {
    let mut receipt = search_receipt(query)?;
    let results = std::mem::take(&mut receipt.results);
    let limit = i64::try_from(SEARCH_LIMIT).unwrap_or(i64::MAX);
    Ok(SearchPage {
        listing: Listing::merged(results, limit, receipt.truncated),
        query: receipt.query,
        embedding_model: receipt.embedding_model,
        boards: receipt.boards,
        missing_boards: receipt.missing_boards,
        unreadable_boards: receipt.unreadable_boards,
        result_chars: receipt.result_chars,
        generated_at: receipt.generated_at,
    })
}

/// One reference answered on hover: what the item is, in one glance.
///
/// A kind that is not one of the four, and `board` with an id that is not
/// the board's name, are the same refusal an unknown board gets: the served
/// fragment answered "Nothing to preview here.", which says the same thing
/// without naming anything.
pub fn preview(kind: &str, project_name: &str, id: &str) -> Projected<Preview> {
    let (record, store) = project_named(project_name).map_err(|_| Refusal::DeniedOrNotFound)?;
    let board = record.name;
    let reference = |task_id: &str| -> Option<TaskReference> {
        store.require_task(task_id).ok().map(|task| TaskReference {
            id: task.id,
            task_type: task.task_type,
            title: task.title,
        })
    };
    match kind {
        "task" => {
            let task = store
                .require_task(id)
                .map_err(|_| Refusal::DeniedOrNotFound)?;
            let open = store
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
            let mut card = Preview::of(kind, board);
            card.body_html = task
                .body
                .as_ref()
                .map(|body| markdown(&excerpt(body, PREVIEW_BODY_CHARS)));
            card.parent = task.parent_id.as_deref().and_then(reference);
            card.open_attention = Some(open);
            card.task = Some(task);
            Ok(card)
        }
        "attention" => {
            let item = store
                .attention(None, None, None, None, None, PREVIEW_ATTENTION_ROWS, false)?
                .into_iter()
                .find(|item| item.id == id)
                .ok_or(Refusal::DeniedOrNotFound)?;
            let mut card = Preview::of(kind, board);
            card.about = item.task_id.as_deref().and_then(reference);
            card.attention = Some(item);
            Ok(card)
        }
        "deployment" => {
            let row = store
                .require_deployment(id)
                .map_err(|_| Refusal::DeniedOrNotFound)?;
            let mut card = Preview::of(kind, board);
            card.deployment = Some(row);
            Ok(card)
        }
        "board" if id == board => {
            let tasks = store.list_tasks(None, None, None, None, false)?;
            let count = |status: &str| tasks.iter().filter(|task| task.status == status).count();
            let mut card = Preview::of(kind, board.clone());
            card.board_counts = Some(BoardCounts {
                open_attention: store.count_open_attention()?,
                todo: count("todo"),
                in_progress: count("in_progress"),
                tasks: tasks.len(),
            });
            Ok(card)
        }
        _ => Err(Refusal::DeniedOrNotFound),
    }
}
