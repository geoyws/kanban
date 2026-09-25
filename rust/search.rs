use crate::LIMIT_CEILING;
use crate::authz::AuthzContext;
use crate::model::{
    Rule, SearchIndexHealth, SearchIndexReport, SearchOptions, SearchReceipt, SearchResult,
    UnreadableBoard,
};
use crate::registry::now_ms;
use crate::store::SearchTagCache;
use anyhow::{Result, bail};
use rusqlite::{Connection, params, params_from_iter};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

pub const EMBEDDING_MODEL: &str = "kanban-semantic-lite-v1";
const EMBEDDING_DIMS: usize = 384;
const SNIPPET_CHARS: usize = 480;

#[derive(Debug)]
struct Document {
    seq: i64,
    source_kind: String,
    source_id: String,
    task_id: Option<String>,
    title: String,
    body: String,
    status: Option<String>,
    lane: Option<String>,
    tags: String,
    created_at: i64,
    updated_at: i64,
    archived: bool,
    source_hash: Option<String>,
    embedding_model: Option<String>,
    embedding: Option<Vec<u8>>,
    /// An event document's row, read with the document by the JOIN in
    /// [`load_documents`]: the event's own `task_id` and full `payload`, so
    /// authorizing it costs no second SELECT. `None` on both for every other
    /// source kind — and for an event document whose event row is gone, which
    /// is the stale index entry [`document_row_tags`] keeps dropping.
    event_task_id: Option<String>,
    event_payload: Option<String>,
}

fn source_hash(document: &Document) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for part in [
        document.source_kind.as_str(),
        document.source_id.as_str(),
        document.task_id.as_deref().unwrap_or(""),
        document.title.as_str(),
        document.body.as_str(),
        document.status.as_deref().unwrap_or(""),
        document.lane.as_deref().unwrap_or(""),
        document.tags.as_str(),
    ] {
        for byte in part.as_bytes().iter().chain(std::iter::once(&0_u8)) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    format!("{hash:016x}")
}

fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter_map(|word| {
            let word = word.to_lowercase();
            (word.len() > 1).then_some(word)
        })
        .collect()
}

fn stem(word: &str) -> &str {
    for suffix in [
        "ments", "ment", "ingly", "ing", "ation", "ions", "ion", "ers", "ed", "es", "s",
    ] {
        if word.len() > suffix.len() + 3 && word.ends_with(suffix) {
            return &word[..word.len() - suffix.len()];
        }
    }
    word
}

fn concept(word: &str) -> Option<&'static str> {
    match stem(word) {
        "deploy" | "release" | "publish" | "rollout" | "install" | "promot" => {
            Some("concept-release")
        }
        "handoff" | "resume" | "continu" | "successor" | "restart" | "context" => {
            Some("concept-handoff")
        }
        "auth" | "login" | "session" | "credential" | "password" | "sso" | "signin" | "sign" => {
            Some("concept-auth")
        }
        "archive" | "retention" | "prune" | "history" | "settled" | "settle" | "old"
        | "complete" | "hot" | "cold" => Some("concept-archive"),
        "stale" | "overdue" | "heartbeat" | "lease" | "abandon" | "check" => Some("concept-stale"),
        "attention" | "blocker" | "decision" | "approval" | "risk" => Some("concept-attention"),
        "search" | "find" | "retriev" | "query" | "rag" => Some("concept-search"),
        "sqlite" | "database" | "db" | "storage" | "ledger" => Some("concept-storage"),
        "task" | "work" | "card" | "todo" | "story" | "epic" => Some("concept-work"),
        _ => None,
    }
}

fn hash_feature(feature: &str) -> (usize, f32) {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in feature.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
    ((hash as usize) % EMBEDDING_DIMS, sign)
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let (index, sign) = hash_feature(feature);
    vector[index] += sign * weight;
}

pub(crate) fn embed(text: &str) -> Vec<f32> {
    let tokens = words(text);
    let mut vector = vec![0.0_f32; EMBEDDING_DIMS];
    for (index, token) in tokens.iter().enumerate() {
        add_feature(&mut vector, token, 1.0);
        add_feature(&mut vector, &format!("stem:{}", stem(token)), 0.8);
        if let Some(group) = concept(token) {
            add_feature(&mut vector, group, 2.4);
        }
        let characters = token.chars().collect::<Vec<_>>();
        for trigram in characters.windows(3) {
            add_feature(
                &mut vector,
                &format!("char:{}", trigram.iter().collect::<String>()),
                0.18,
            );
        }
        if let Some(next) = tokens.get(index + 1) {
            add_feature(&mut vector, &format!("pair:{token}:{next}"), 0.45);
        }
    }
    let magnitude = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if magnitude > 0.0 {
        for value in &mut vector {
            *value /= magnitude;
        }
    }
    vector
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    if left.len() != right.len() {
        return 0.0;
    }
    f64::from(
        left.iter()
            .zip(right)
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .max(0.0),
    )
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != EMBEDDING_DIMS * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

fn fts_query(query: &str) -> Option<String> {
    let mut seen = HashSet::new();
    let tokens = words(query)
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .take(12)
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then(|| tokens.join(" OR "))
}

fn document_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
    Ok(Document {
        seq: row.get(0)?,
        source_kind: row.get(1)?,
        source_id: row.get(2)?,
        task_id: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        status: row.get(6)?,
        lane: row.get(7)?,
        tags: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        archived: row.get::<_, i64>(11)? != 0,
        source_hash: row.get(12)?,
        embedding_model: row.get(13)?,
        embedding: row.get(14)?,
        event_task_id: row.get(15)?,
        event_payload: row.get(16)?,
    })
}

fn load_documents(connection: &Connection, options: &SearchOptions) -> Result<Vec<Document>> {
    // The event arm of [`document_row_tags`] authorizes each event document
    // against its event row's task and payload. Reading those two columns
    // here, in the same round trip as the document, removes the second
    // SELECT per event document without changing what the guard sees: a
    // document whose event row is gone joins to NULLs, which authorizes as
    // the same stale entry the re-SELECT path dropped. `source_id` is text,
    // so the join casts it; a non-sequence casts to zero, which matches no
    // event row and stays stale.
    let mut sql = String::from(
        "SELECT d.seq,d.source_kind,d.source_id,d.task_id,d.title,d.body,d.status,d.lane,d.tags,\
         d.created_at,d.updated_at,d.archived,d.source_hash,d.embedding_model,d.embedding,\
         e.task_id,e.payload \
         FROM search_documents d LEFT JOIN events e \
         ON d.source_kind='event' AND e.seq=CAST(d.source_id AS INTEGER) WHERE 1=1",
    );
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !options.include_archived {
        sql.push_str(" AND d.archived=0");
    }
    for (column, value) in [
        ("source_kind", options.source.as_ref()),
        ("status", options.status.as_ref()),
        ("lane", options.lane.as_ref()),
    ] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND d.{column}=?"));
            values.push(Box::new(value.clone()));
        }
    }
    for tag in &options.tags {
        sql.push_str(" AND instr(' ' || d.tags || ' ',' ' || ? || ' ')>0");
        values.push(Box::new(tag.clone()));
    }
    if let Some(after) = options.after {
        sql.push_str(" AND d.updated_at>=?");
        values.push(Box::new(after));
    }
    if let Some(before) = options.before {
        sql.push_str(" AND d.updated_at<=?");
        values.push(Box::new(before));
    }
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params_from_iter(values.iter().map(|value| value.as_ref())),
        document_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// The FTS5 membership set restricted to rows the caller may read.
///
/// The unenforced path keeps the whole-index MATCH in [`fts_match_rows`]:
/// every row is readable there, so its scores stay exactly what the
/// unfiltered code produced. Under enforcement the MATCH itself must not see
/// denied rows: running it over the whole index with a per-row bm25 and
/// dropping denied rowids afterwards materialises every denied match, so
/// latency grows with the number of denied rows holding a guessed prefix.
/// The MATCH is therefore restricted to the permitted seqs — denied rows
/// are never materialised, no denied content or bm25 is ever read. The FTS
/// table is external-content (`content='search_documents'`,
/// `content_rowid='seq'`), so its `rowid` IS the document `seq` and the
/// restriction needs no mapping.
///
/// A pure read throughout, by necessity: CLI search opens the board
/// `SQLITE_OPEN_READ_ONLY` with `PRAGMA query_only`
/// ([`crate::db::open_board_readonly`]), so a TEMP table — a write — is
/// refused there. Nothing persists anywhere, so pooled or reused
/// connections and concurrent searches cannot leak one request's permitted
/// set into another: every call binds its own seqs and leaves no state.
///
/// Only membership is served from here: the strengths still come from
/// [`permitted_bm25_scores`], so `lexicalScore`, `score`, order, truncation
/// and snippets are byte-identical with the filter-afterwards path.
fn fts_match_permitted(
    connection: &Connection,
    query: &str,
    permitted: &HashSet<i64>,
) -> Result<HashSet<i64>> {
    let mut matched = HashSet::new();
    if permitted.is_empty() {
        return Ok(matched);
    }
    let Some(query) = fts_query(query) else {
        return Ok(matched);
    };
    // Bound parameters keep the restriction a read, but the variable-number
    // limit predates the search schema on some builds (999), so a permitted
    // set larger than one chunk takes the single whole-index rowid walk with
    // the permitted intersect in Rust instead: repeating the MATCH walk per
    // chunk would cost more than the one walk whose bm25 this skips anyway.
    // Either way no bm25 is computed and no denied row is materialised.
    const CHUNK: usize = 900;
    if permitted.len() <= CHUNK {
        let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(permitted.len() + 1);
        values.push(Box::new(query));
        let mut placeholders = String::new();
        for (index, seq) in permitted.iter().enumerate() {
            if index > 0 {
                placeholders.push(',');
            }
            placeholders.push('?');
            values.push(Box::new(*seq));
        }
        let sql = format!(
            "SELECT rowid FROM search_fts WHERE search_fts MATCH ? AND rowid IN ({placeholders})"
        );
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            |row| row.get::<_, i64>(0),
        )?;
        for row in rows {
            matched.insert(row?);
        }
        return Ok(matched);
    }
    let mut statement =
        connection.prepare("SELECT rowid FROM search_fts WHERE search_fts MATCH ?")?;
    let rows = statement.query_map([query], |row| row.get::<_, i64>(0))?;
    for row in rows {
        let seq = row?;
        if permitted.contains(&seq) {
            matched.insert(seq);
        }
    }
    Ok(matched)
}

/// The raw FTS5 match strengths for one query: every indexed row the MATCH
/// accepts, with its bm25 made non-negative. Only the unenforced path uses
/// this, through [`lexical_scores`]: every row is readable there, so the
/// whole-index strengths are exactly what the unfiltered code served. Under
/// enforcement [`search`] takes membership from [`fts_match_permitted`]
/// instead and the strengths from [`permitted_bm25_scores`], because these
/// fold whole-index statistics a denied row would otherwise leak through.
fn fts_match_rows(connection: &Connection, query: &str) -> Result<Vec<(i64, f64)>> {
    let Some(query) = fts_query(query) else {
        return Ok(Vec::new());
    };
    let mut statement = connection.prepare(
        "SELECT rowid,bm25(search_fts,8.0,2.0,4.0) FROM search_fts WHERE search_fts MATCH ?",
    )?;
    let rows = statement.query_map([query], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
    })?;
    let mut ranked = Vec::new();
    for row in rows {
        let (seq, rank) = row?;
        ranked.push((seq, (-rank).max(0.0)));
    }
    Ok(ranked)
}

/// The FTS5 lexical scores for one query, normalised to the strongest match.
///
/// `permitted` is the set of index rows the caller may read, decided BEFORE
/// this runs: denied rows are dropped before the divisor is computed, so a
/// document the caller cannot read cannot move a readable hit's
/// `lexicalScore` (and through it, `score`) — the prefix oracle `t-e9c0127a`
/// closes. `exact` and `semantic` need no such filter: both are per-document
/// functions of the served row and the query text, so they already depend on
/// permitted documents only. `None` is the unenforced path, where every row
/// is readable and the divisor is taken over the same full match set as
/// before, leaving unmanaged boards byte-identical.
///
/// Under enforcement this is NOT the served path — [`search`] uses FTS only
/// for membership there and [`permitted_bm25_scores`] for the strengths —
/// but it stays for the unenforced boards, whose scores must remain exactly
/// what the unfiltered code produced.
fn lexical_scores(
    connection: &Connection,
    query: &str,
    permitted: Option<&HashSet<i64>>,
) -> Result<HashMap<i64, f64>> {
    let mut ranked = fts_match_rows(connection, query)?;
    if let Some(permitted) = permitted {
        ranked.retain(|(seq, _)| permitted.contains(seq));
    }
    let strongest = ranked.iter().map(|(_, rank)| *rank).fold(0.0_f64, f64::max);
    let scores = ranked
        .into_iter()
        .map(|(seq, rank)| {
            (
                seq,
                if strongest > 0.0 {
                    rank / strongest
                } else {
                    0.0
                },
            )
        })
        .collect();
    Ok(scores)
}

/// BM25 strengths recomputed over the permitted candidate set only
/// (`t-e9c0127a` M1).
///
/// Filtering only the normalisation divisor is not enough: FTS5's per-row
/// bm25 folds whole-index statistics — the per-term document frequency, the
/// row count, the average length — into every strength, so a denied row
/// holding a guessed prefix still moves the non-max scores and possibly the
/// order. The served strengths are therefore recomputed here, as a function
/// of permitted documents only:
///
/// ```text
/// score(d) = SUM_t idf(t) * tf_w(t,d)*(K1+1) / (tf_w(t,d) + K1*(1-B+B*len(d)/avgdl))
/// idf(t)   = ln(1 + (N - df(t) + 0.5) / (df(t) + 0.5))
/// tf_w     = 8*title_hits + 2*body_hits + 4*tags_hits
/// ```
///
/// with `N` the permitted candidate count, `df` and `avgdl` ranging over the
/// permitted candidates alone, and `K1`/`B` FTS5's own 1.2/0.75. Term hits
/// are prefix hits over `words` tokens stemmed with the existing `stem`, the
/// same tokenization the query path uses, and the 8/2/4 title/body/tags
/// weights match the `bm25(search_fts,8.0,2.0,4.0)` call — so the managed
/// ranking keeps the same shape with denied-independent values. Only rows in
/// `matched` (the FTS membership set, already permitted-filtered) are
/// scored; the map is normalised to the strongest of them, like
/// [`lexical_scores`].
fn permitted_bm25_scores(
    documents: &[Document],
    matched: &HashSet<i64>,
    query: &str,
) -> HashMap<i64, f64> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    const WEIGHTS: [f64; 3] = [8.0, 2.0, 4.0];
    // The query's prefix terms, stemmed like the indexed text: the same
    // dedup-and-twelve cap `fts_query` applies, so scoring sees the terms
    // matching saw.
    let mut seen = HashSet::new();
    let terms: Vec<String> = words(query)
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .take(12)
        .map(|token| stem(&token).to_owned())
        .collect();
    if terms.is_empty() || documents.is_empty() {
        return HashMap::new();
    }
    // One pass over the permitted corpus: per-document weighted hit counts
    // per term plus the document frequencies. Only stack-local counts are
    // kept, never the tokens.
    let mut stats: Vec<(i64, u64, Vec<[u32; 3]>)> = Vec::with_capacity(documents.len());
    let mut document_frequency = vec![0_u64; terms.len()];
    let mut total_length = 0_u64;
    for document in documents {
        let mut hits = vec![[0_u32; 3]; terms.len()];
        let mut length = 0_u64;
        for (field, text) in [
            (0, document.title.as_str()),
            (1, document.body.as_str()),
            (2, document.tags.as_str()),
        ] {
            for token in words(text) {
                length += 1;
                let stemmed = stem(&token);
                for (index, term) in terms.iter().enumerate() {
                    if stemmed.starts_with(term.as_str()) {
                        hits[index][field] += 1;
                    }
                }
            }
        }
        for (index, term_hits) in hits.iter().enumerate() {
            if term_hits.iter().any(|count| *count > 0) {
                document_frequency[index] += 1;
            }
        }
        total_length += length;
        stats.push((document.seq, length, hits));
    }
    let count = documents.len() as f64;
    let average_length = total_length as f64 / count;
    let inverse_frequency: Vec<f64> = document_frequency
        .iter()
        .map(|frequency| ((count - *frequency as f64 + 0.5) / (*frequency as f64 + 0.5) + 1.0).ln())
        .collect();
    // Score the FTS-matched rows only; the corpus statistics above already
    // range over every permitted candidate, denied rows nowhere in sight.
    let mut scores = HashMap::with_capacity(matched.len());
    for (seq, length, hits) in &stats {
        if !matched.contains(seq) {
            continue;
        }
        let relative = if average_length > 0.0 {
            *length as f64 / average_length
        } else {
            1.0
        };
        let mut score = 0.0;
        for (index, term_hits) in hits.iter().enumerate() {
            let weighted: f64 = term_hits
                .iter()
                .enumerate()
                .map(|(field, count)| WEIGHTS[field] * *count as f64)
                .sum();
            score += inverse_frequency[index] * weighted * (K1 + 1.0)
                / (weighted + K1 * (1.0 - B + B * relative));
        }
        scores.insert(*seq, score);
    }
    let strongest = scores.values().copied().fold(0.0_f64, f64::max);
    if strongest > 0.0 {
        for score in scores.values_mut() {
            *score /= strongest;
        }
    }
    scores
}

fn literal_exact_score(document: &Document, query: &str, query_words: &[String]) -> f64 {
    let query = query.to_lowercase();
    let source_id = document.source_id.to_lowercase();
    let title = document.title.to_lowercase();
    let body = document.body.to_lowercase();
    if source_id == query {
        return 1.0;
    }
    if !query.is_empty() {
        if title == query {
            return 0.99;
        }
        if title.starts_with(&query) {
            return 0.98;
        }
        if title.contains(&query) {
            return 0.95;
        }
        if body.contains(&query) {
            return 0.9;
        }
    }
    let haystack = format!("{source_id}\n{title}\n{body}");
    if query_words.is_empty() {
        return 0.0;
    }
    query_words
        .iter()
        .filter(|word| haystack.contains(word.as_str()))
        .count() as f64
        / query_words.len() as f64
        * 0.7
}

fn exact_score(document: &Document, query: &str, query_words: &[String]) -> f64 {
    let query = query.to_lowercase();
    let source_id = document.source_id.to_lowercase();
    let title = document.title.to_lowercase();
    let body = format!("{}\n{}", document.body, document.tags).to_lowercase();
    let haystack = format!("{source_id}\n{title}\n{body}");
    if source_id == query {
        return 1.0;
    }
    if !query.is_empty() {
        if title == query {
            return 0.99;
        }
        if title.starts_with(&query) {
            return 0.98;
        }
        if title.contains(&query) {
            return 0.95;
        }
        if body.contains(&query) {
            return 0.9;
        }
    }
    if query_words.is_empty() {
        return 0.0;
    }
    query_words
        .iter()
        .filter(|word| haystack.contains(word.as_str()))
        .count() as f64
        / query_words.len() as f64
        * 0.7
}

fn canonical_generated_id_query(query: &str) -> bool {
    let query = query.trim();
    let Some((prefix, suffix)) = query.split_once('-') else {
        return false;
    };
    matches!(
        prefix,
        "t" | "e" | "s" | "sp" | "d" | "sr" | "a" | "h" | "sub" | "r"
    ) && suffix.len() == 8
        && suffix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn result_is_eligible(query: &str, exact: f64, lexical: f64, semantic: f64) -> bool {
    if canonical_generated_id_query(query) {
        exact >= 0.9
    } else {
        exact > 0.0 || lexical > 0.0 || semantic >= 0.18
    }
}

fn snippet(document: &Document, query_words: &[String]) -> String {
    let text = format!("{} — {}", document.title.trim(), document.body.trim());
    let lower = text.to_lowercase();
    let start = query_words
        .iter()
        .filter_map(|word| lower.find(word))
        .min()
        .unwrap_or(0)
        .saturating_sub(100);
    let mut value = text
        .chars()
        .skip(start)
        .take(SNIPPET_CHARS)
        .collect::<String>();
    if start > 0 {
        value.insert(0, '…');
    }
    if text.chars().count() > start + SNIPPET_CHARS {
        value.push('…');
    }
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The REAL tags of one indexed document's source row, read from the source
/// tables at materialisation time.
///
/// `search_documents.tags` is a PROJECTED COPY, written by the triggers in
/// `BOARD_V13` and rewritten when the source changes. Authorizing against
/// that copy would make a stale index a bypass: a board restored from an
/// older snapshot, an index that predates a retag, or a `search-rebuild` that
/// has not run yet all leave a row whose indexed tags are narrower than the
/// tags it now carries — and the guard would then hand over a row the caller
/// may no longer see. So the copy is used for RANKING (which is all it is
/// good for) and never for the decision.
///
/// Task-linked source kinds derive authorization tags from their task tags
/// (see search_source_rows). An attention document also carries its own
/// attention tags, so both are required here. Rules live in the registry and
/// sprints are board-only rows; neither carries board tags, so its containing
/// scope is the whole authorization check.
///
/// A NON-EVENT document whose task no longer exists is usually a stale index
/// entry with no source row left to authorize against, so it yields the tag
/// that can never be satisfied — it is dropped rather than trusted. The
/// exception is a document whose source row outlives the task: since
/// `BOARD_V34` the sitrep, handoff, deployment and attention links keep the
/// removed task's id, and the index authorizes those documents against the
/// task's last-known removal tags — the same union the listings serve them
/// through — so the index neither serves a task-less document to a denied
/// caller nor drops what the owner may still read. A removed task with no
/// removal record still fails closed with the stale-entry tag. Event
/// documents are the older exception: the event row outlives the task, and
/// the tails authorize it against the task's last-known removal tags, so the
/// index does the same through `cached_event_authorization_tags` rather
/// than dropping what the tails serve.
fn document_row_tags(
    connection: &Connection,
    document: &Document,
    cache: &mut SearchTagCache,
) -> Result<Vec<String>> {
    // An event document is authorized as the event it indexes, not as its
    // task: an event ABOUT an attention row carries that row's id, kind,
    // tags and choices in the indexed payload, so the task's tags alone
    // would hand a denied row to any caller who can read the task (ACC-14).
    // [`crate::store::cached_event_authorization_tags`] reads the same union
    // the event tails filter on, against the task and payload the document
    // row already carries — no second SELECT, and a document whose event row
    // is gone preloads no payload and stays stale.
    if document.source_kind == "event" {
        return crate::store::cached_event_authorization_tags(
            connection,
            &document.source_id,
            document
                .event_payload
                .clone()
                .map(|payload| (document.event_task_id.clone(), payload)),
            cache,
        );
    }
    let mut tags = Vec::new();
    if let Some(task_id) = document.task_id.as_deref() {
        if !cache.task_exists(connection, task_id)? {
            match cache.removed_task_final_tags(connection, task_id)? {
                Some(removed) => tags.extend(removed),
                None => return Ok(vec![STALE_INDEX_TAG.to_owned()]),
            }
        } else {
            tags.extend(cache.task_tags(connection, task_id)?);
        }
    }
    if document.source_kind == "attention" {
        // Defence in depth: the delete trigger removes the document with the
        // row, so no route reaches this today, but a stale index entry (a
        // partial restore, a rebuilt migration) must not authorize against
        // the empty tag set of a row that no longer exists.
        if !cache.attention_exists(connection, document.source_id.as_str())? {
            return Ok(vec![STALE_INDEX_TAG.to_owned()]);
        }
        tags.extend(cache.attention_tags(connection, document.source_id.as_str())?);
    }
    tags.sort();
    tags.dedup();
    Ok(tags)
}

/// The tag a stale index entry is authorized against. `*` is not a legal tag
/// slug (ADR-033's vocabulary reserves it for the board wildcard atom and
/// `ScopeTuple::from_atoms` will not build a `tag:` from it), so no grant can
/// name it and no caller can satisfy it. A document pointing at a row that no
/// longer exists is therefore always dropped.
pub(crate) const STALE_INDEX_TAG: &str = "*";

/// Rank and materialise search hits.
///
/// `authz` is the store's context, passed down rather than re-derived: search
/// is the surface where the row is reached through an INDEX, so the guard has
/// to be applied where the result is built and against the row's real tags.
/// See [`document_row_tags`].
pub fn search(
    connection: &Connection,
    board: &str,
    options: &SearchOptions,
    authz: &AuthzContext,
) -> Result<Vec<SearchResult>> {
    if options.query.trim().is_empty() {
        bail!("search query is required");
    }
    // The band the CLI states in `search_options`, restated here because the
    // MCP and serve adapters build a `SearchOptions` without going through
    // it. `limit` only truncates the ranked results, so the ceiling is a typo
    // guard rather than a memory bound.
    if options.limit == 0 || options.limit > LIMIT_CEILING as usize {
        bail!("search limit must be between 1 and {LIMIT_CEILING}");
    }
    if options.max_chars < 256 || options.max_chars > 100_000 {
        bail!("search max chars must be between 256 and 100000");
    }
    // Board scope first: no read on this board, no hits from it at all. The
    // board is the context's, taken from the board file's own name — never the
    // `board` display label above, which is whatever the caller passed.
    authz.check_read(&[])?;
    // One consistent view for the document scan, the FTS membership probe
    // and the per-document authorization reads: without it a concurrent
    // write could move a row between the three. A no-op inside an open
    // scope, released at the end of the request.
    let snapshot = crate::store::ReadSnapshot::open(connection)?;
    let query_words = words(&options.query);
    let query_vector = embed(&options.query);
    // Authorization before scoring (`t-e9c0127a`): the permitted candidate
    // set is decided first, and served `lexicalScore` and `score` are a
    // function of permitted documents only. The tag sets behind the decision
    // are memoized per request, and each event document already carries its
    // event row — authorizing the whole corpus no longer costs a SELECT per
    // event. Unenforced boards skip the decision entirely and keep the FTS5
    // strengths, so their scores and order are exactly what the unfiltered
    // code produced.
    let mut tag_cache = SearchTagCache::default();
    let mut permitted = HashSet::new();
    let mut candidates = Vec::new();
    for document in load_documents(connection, options)? {
        if authz.is_enforcing() {
            if !authz.permits_read(&document_row_tags(connection, &document, &mut tag_cache)?) {
                continue;
            }
            permitted.insert(document.seq);
        }
        candidates.push(document);
    }
    // Under enforcement FTS decides only WHICH rows match, and the MATCH
    // itself is restricted to the permitted seqs (`t-5f7daa1b`): denied rows
    // are never materialised, so a guessed prefix they hold costs no MATCH
    // time. The strengths come from [`permitted_bm25_scores`], whose N, df
    // and avgdl range over the permitted candidates alone — FTS5's
    // whole-index statistics would otherwise leak a denied row's text
    // through the non-max scores and the order (M1).
    let lexical = if authz.is_enforcing() {
        let matched = fts_match_permitted(connection, &options.query, &permitted)?;
        permitted_bm25_scores(&candidates, &matched, &options.query)
    } else {
        lexical_scores(connection, &options.query, None)?
    };
    let mut results = Vec::new();
    for document in candidates {
        let hash = source_hash(&document);
        let vector = if document.source_hash.as_deref() == Some(hash.as_str())
            && document.embedding_model.as_deref() == Some(EMBEDDING_MODEL)
        {
            document.embedding.as_deref().and_then(decode)
        } else {
            None
        }
        .unwrap_or_else(|| {
            embed(&format!(
                "{} {} {} {} {}",
                document.title,
                document.body,
                document.tags,
                document.status.as_deref().unwrap_or(""),
                document.lane.as_deref().unwrap_or("")
            ))
        });
        let exact = if canonical_generated_id_query(&options.query) {
            literal_exact_score(&document, &options.query, &query_words)
        } else {
            exact_score(&document, &options.query, &query_words)
        };
        let lexical = lexical.get(&document.seq).copied().unwrap_or(0.0);
        let semantic = cosine(&query_vector, &vector);
        if !result_is_eligible(&options.query, exact, lexical, semantic) {
            continue;
        }
        let score = if exact >= 1.0 {
            10.0 + lexical + semantic
        } else if exact >= 0.98 {
            8.0 + exact + lexical + semantic
        } else if exact >= 0.95 {
            6.0 + exact + lexical + semantic
        } else if exact >= 0.9 {
            4.0 + exact + lexical + semantic
        } else {
            exact * 0.24 + lexical * 0.42 + semantic * 0.34
        };
        let excerpt = snippet(&document, &query_words);
        results.push(SearchResult {
            board: board.to_owned(),
            source_kind: document.source_kind.clone(),
            source_id: document.source_id.clone(),
            task_id: document.task_id,
            title: document.title,
            snippet: excerpt,
            status: document.status,
            lane: document.lane,
            tags: document
                .tags
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            created_at: document.created_at,
            updated_at: document.updated_at,
            archived: document.archived,
            exact_score: exact,
            lexical_score: lexical,
            semantic_score: semantic,
            score,
            citation: format!(
                "kanban://{}/{}/{}",
                board, document.source_kind, document.source_id
            ),
        });
    }
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| left.citation.cmp(&right.citation))
    });
    results.truncate(options.limit);
    snapshot.close()?;
    Ok(results)
}

/// Rank the registry-owned rules document with the same semantic model. It does
/// not live in a board database, so exact and semantic ranking supplement the
/// board-local FTS indexes.
pub fn search_rules(rules: &[Rule], options: &SearchOptions) -> Vec<SearchResult> {
    if options
        .source
        .as_deref()
        .is_some_and(|source| source != "rule")
        || options.lane.is_some()
    {
        return Vec::new();
    }
    let query_words = words(&options.query);
    let query_vector = embed(&options.query);
    let lower_query = options.query.to_lowercase();
    let mut results = rules
        .iter()
        .filter(|rule| options.include_archived || !rule.archived)
        .filter(|rule| {
            options
                .tags
                .iter()
                .all(|tag| rule.tags.iter().any(|candidate| candidate == tag))
        })
        .filter(|rule| options.after.is_none_or(|after| rule.updated_at >= after))
        .filter(|rule| {
            options
                .before
                .is_none_or(|before| rule.updated_at <= before)
        })
        .filter(|rule| {
            options
                .status
                .as_deref()
                .is_none_or(|status| status == if rule.archived { "retired" } else { "active" })
        })
        .filter_map(|rule| {
            let body = rule.body.to_lowercase();
            let exact = if rule.id.to_lowercase() == lower_query {
                1.0
            } else if body.contains(&lower_query) {
                0.9
            } else if query_words.is_empty() {
                0.0
            } else {
                query_words
                    .iter()
                    .filter(|word| body.contains(word.as_str()))
                    .count() as f64
                    / query_words.len() as f64
                    * 0.7
            };
            let semantic = cosine(&query_vector, &embed(&rule.body));
            result_is_eligible(&options.query, exact, 0.0, semantic).then(|| SearchResult {
                board: "rules".to_owned(),
                source_kind: "rule".to_owned(),
                source_id: rule.id.clone(),
                task_id: None,
                title: rule.body.lines().next().unwrap_or("rule").to_owned(),
                snippet: rule
                    .body
                    .chars()
                    .take(SNIPPET_CHARS)
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                status: Some(if rule.archived { "retired" } else { "active" }.to_owned()),
                lane: None,
                tags: rule.tags.clone(),
                created_at: rule.created_at,
                updated_at: rule.updated_at,
                archived: rule.archived,
                exact_score: exact,
                lexical_score: 0.0,
                semantic_score: semantic,
                score: if exact >= 1.0 {
                    10.0 + semantic
                } else if exact >= 0.9 {
                    4.0 + exact + semantic
                } else {
                    exact * 0.4 + semantic * 0.6
                },
                citation: format!("kanban://rules/rule/{}", rule.id),
            })
        })
        .collect::<Vec<_>>();
    results.sort_by(|left, right| right.score.total_cmp(&left.score));
    results.truncate(options.limit);
    results
}

pub fn bound_receipt(
    query: &str,
    boards: Vec<String>,
    missing_boards: Vec<String>,
    unreadable_boards: Vec<UnreadableBoard>,
    mut results: Vec<SearchResult>,
    limit: usize,
    max_chars: usize,
) -> SearchReceipt {
    results.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.citation.cmp(&right.citation))
    });
    let initially = results.len();
    results.truncate(limit);
    let mut chars = 0;
    let mut kept = Vec::new();
    for mut result in results {
        let fixed = result.title.chars().count() + result.citation.chars().count() + 80;
        if chars + fixed >= max_chars {
            break;
        }
        let available = max_chars - chars - fixed;
        if result.snippet.chars().count() > available {
            result.snippet = result
                .snippet
                .chars()
                .take(available.saturating_sub(1))
                .collect();
            result.snippet.push('…');
        }
        chars += fixed + result.snippet.chars().count();
        kept.push(result);
    }
    let truncated = kept.len() < initially;
    SearchReceipt {
        query: query.to_owned(),
        embedding_model: EMBEDDING_MODEL.to_owned(),
        boards,
        missing_boards,
        unreadable_boards,
        results: kept,
        result_chars: chars,
        truncated,
        generated_at: now_ms(),
    }
}

fn document_text(document: &Document) -> String {
    format!(
        "{} {} {} {} {}",
        document.title,
        document.body,
        document.tags,
        document.status.as_deref().unwrap_or(""),
        document.lane.as_deref().unwrap_or("")
    )
}

/// Compute one document's vector and persist it with its source hash and the
/// current model. Shared by the explicit rebuild and the incremental write
/// paths, so a source mutation and `search-rebuild` can never drift apart.
fn embed_document(connection: &Connection, document: &Document) -> Result<()> {
    let vector = embed(&document_text(document));
    connection.execute(
        "UPDATE search_documents SET source_hash=?,embedding_model=?,embedding=? WHERE seq=?",
        params![
            source_hash(document),
            EMBEDDING_MODEL,
            encode(&vector),
            document.seq
        ],
    )?;
    Ok(())
}

/// Embed every `search_documents` row whose vector is missing or was computed
/// by a different model. The incremental write paths call this after a source
/// mutation so a newly triggered document never sits unembedded; it is
/// idempotent, so it only ever does work for rows that lack a current vector.
/// Returns how many rows were embedded.
pub(crate) fn embed_missing(connection: &Connection) -> Result<i64> {
    let documents: Vec<Document> = {
        // The trailing NULLs stand in for the event preload [`load_documents`]
        // JOINs in: the write path only needs the embedding inputs, never an
        // event row for authorization.
        let mut statement = connection.prepare(
            "SELECT seq,source_kind,source_id,task_id,title,body,status,lane,tags,\
                    created_at,updated_at,archived,source_hash,embedding_model,embedding,\
                    NULL,NULL \
             FROM search_documents \
             WHERE embedding IS NULL OR embedding_model IS NULL OR source_hash IS NULL \
                OR embedding_model != ?1",
        )?;
        statement
            .query_map([EMBEDDING_MODEL], document_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut embedded = 0_i64;
    for document in documents {
        let hash = source_hash(&document);
        // A row selected for a NULL marker may still hold a current vector
        // (e.g. a model name that drifted while the hash stayed current).
        // Re-embedding it is harmless and keeps `embed_missing` idempotent.
        if document.embedding.is_none()
            || document.embedding_model.as_deref() != Some(EMBEDDING_MODEL)
            || document.source_hash.as_deref() != Some(hash.as_str())
            || document.embedding.as_deref().and_then(decode).is_none()
        {
            embed_document(connection, &document)?;
            embedded += 1;
        }
    }
    Ok(embedded)
}

pub fn rebuild(connection: &mut Connection, board: &str, actor: &str) -> Result<SearchIndexReport> {
    let actor = actor.trim();
    if actor.is_empty() {
        bail!("actor is required");
    }
    let started = Instant::now();
    let transaction = connection.transaction()?;
    transaction.execute("DELETE FROM search_documents", [])?;
    transaction.execute(
        "INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived) \
         SELECT * FROM search_source_rows",
        [],
    )?;
    transaction.execute(
        "INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived) \
         SELECT * FROM search_deployment_event_rows",
        [],
    )?;
    let documents = load_documents(
        &transaction,
        &SearchOptions {
            query: "rebuild".to_owned(),
            source: None,
            status: None,
            tags: Vec::new(),
            lane: None,
            after: None,
            before: None,
            include_archived: true,
            limit: 1,
            max_chars: 256,
        },
    )?;
    let mut embedded = 0_i64;
    for document in &documents {
        embed_document(&transaction, document)?;
        embedded += 1;
    }
    crate::store::event_at(
        &transaction,
        None,
        "search_rebuilt",
        Some(actor),
        json!({"documents":documents.len(),"embeddingModel":EMBEDDING_MODEL}),
        now_ms(),
    )?;
    transaction.commit()?;
    Ok(SearchIndexReport {
        board: board.to_owned(),
        documents: documents.len() as i64,
        embedded,
        embedding_model: EMBEDDING_MODEL.to_owned(),
        duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    })
}

pub fn health(connection: &Connection) -> Result<SearchIndexHealth> {
    let source_rows = connection.query_row(
        "SELECT (SELECT count(*) FROM search_source_rows) + \
                (SELECT count(*) FROM search_deployment_event_rows)",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let documents = connection.query_row("SELECT count(*) FROM search_documents", [], |row| {
        row.get::<_, i64>(0)
    })?;
    let fts_rows = connection.query_row("SELECT count(*) FROM search_fts_docsize", [], |row| {
        row.get::<_, i64>(0)
    })?;
    let indexed = load_documents(
        connection,
        &SearchOptions {
            query: "health".to_owned(),
            source: None,
            status: None,
            tags: Vec::new(),
            lane: None,
            after: None,
            before: None,
            include_archived: true,
            limit: 1,
            max_chars: 256,
        },
    )?;
    let missing_embeddings = indexed
        .iter()
        .filter(|document| document.embedding.is_none())
        .count() as i64;
    let stale_embeddings = indexed
        .iter()
        .filter(|document| {
            let hash = source_hash(document);
            document.embedding.is_some()
                && (document.embedding_model.as_deref() != Some(EMBEDDING_MODEL)
                    || document.source_hash.as_deref() != Some(hash.as_str())
                    || document.embedding.as_deref().and_then(decode).is_none())
        })
        .count() as i64;
    let mut unhealthy_because = Vec::new();
    if source_rows != documents {
        unhealthy_because.push(format!(
            "{documents} search documents for {source_rows} source rows; \
             run `kb search-rebuild --project NAME --as ACTOR`"
        ));
    }
    if documents != fts_rows {
        unhealthy_because.push(format!(
            "{documents} search documents for {fts_rows} FTS rows; \
             run `kb search-rebuild --project NAME --as ACTOR`"
        ));
    }
    if missing_embeddings > 0 {
        unhealthy_because.push(format!(
            "{missing_embeddings} of {documents} documents have no embedding; \
             run `kb search-rebuild --project NAME --as ACTOR`"
        ));
    }
    if stale_embeddings > 0 {
        unhealthy_because.push(format!(
            "{stale_embeddings} stale embeddings; \
             run `kb search-rebuild --project NAME --as ACTOR`"
        ));
    }
    Ok(SearchIndexHealth {
        healthy: unhealthy_because.is_empty(),
        source_rows,
        documents,
        fts_rows,
        missing_embeddings,
        stale_embeddings,
        embedding_model: EMBEDDING_MODEL.to_owned(),
        unhealthy_because,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// The restricted membership probe agrees with the filter-afterwards set
    /// on every branch (`t-5f7daa1b`): the `rowid IN` path for small
    /// permitted sets, the rowid walk with the Rust intersect past the
    /// variable-number chunk size, the empty permitted set, and a query with
    /// no indexable tokens. The large-set branch is unreachable through the
    /// existing process tests, so without this a wrong branch there — a
    /// dropped match, a widened one — would slip through them all.
    #[test]
    fn restricted_match_agrees_with_filter_afterwards_on_both_branches() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE VIRTUAL TABLE search_fts USING fts5(\
                 title, body, tags, \
                 tokenize='porter unicode61 remove_diacritics 2', prefix='2 3')",
            )
            .unwrap();
        // 1200 documents: even rows carry the `harbourq` prefix family the
        // probe query guesses, odd rows do not.
        {
            let mut insert = connection
                .prepare("INSERT INTO search_fts(rowid,title,body,tags) VALUES(?,?,?,?)")
                .unwrap();
            for seq in 1..=1200_i64 {
                let body = if seq % 2 == 0 {
                    format!("a short vault note harbourq{seq}vlt")
                } else {
                    format!("an unrelated manifest entry number {seq}")
                };
                insert
                    .execute(rusqlite::params![seq, format!("note {seq}"), body, ""])
                    .unwrap();
            }
        }
        let full: HashSet<i64> = fts_match_rows(&connection, "harbourq")
            .unwrap()
            .into_iter()
            .map(|(seq, _)| seq)
            .collect();
        assert_eq!(
            full.len(),
            600,
            "the fixture must match exactly the even rows"
        );
        // Small permitted set: only even rows 2..=20 are readable.
        let permitted: HashSet<i64> = (1..=20_i64).filter(|seq| seq % 2 == 0).collect();
        let restricted = fts_match_permitted(&connection, "harbourq", &permitted).unwrap();
        let expected: HashSet<i64> = full.intersection(&permitted).copied().collect();
        assert_eq!(restricted, expected);
        assert_eq!(restricted.len(), 10);
        // Denied rows are excluded even when they are the strongest matches.
        let denied_only: HashSet<i64> = (1..=20_i64).filter(|seq| seq % 2 == 1).collect();
        assert!(
            fts_match_permitted(&connection, "harbourq", &denied_only)
                .unwrap()
                .is_empty()
        );
        // Empty permitted set and tokenless query short-circuit to empty.
        assert!(
            fts_match_permitted(&connection, "harbourq", &HashSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(
            fts_match_permitted(&connection, "!!!", &permitted)
                .unwrap()
                .is_empty()
        );
        // Large permitted set: every row is readable, past the chunk size, so
        // the rowid walk with the Rust intersect answers.
        let all: HashSet<i64> = (1..=1200_i64).collect();
        let restricted = fts_match_permitted(&connection, "harbourq", &all).unwrap();
        assert_eq!(restricted, full);
        // Repeated calls agree: the probe binds its own seqs and leaves no
        // per-connection state behind.
        let again = fts_match_permitted(&connection, "harbourq", &permitted).unwrap();
        assert_eq!(again, expected);
    }

    #[test]
    fn domain_paraphrases_share_semantic_signal() {
        assert!(
            cosine(
                &embed("release to production"),
                &embed("deploy the live build")
            ) > 0.2
        );
        assert!(
            cosine(
                &embed("resume from the handoff"),
                &embed("continue successor work")
            ) > 0.2
        );
    }

    #[test]
    fn encoded_vectors_have_a_fixed_portable_shape() {
        let vector = embed("SQLite retrieval");
        assert_eq!(vector.len(), EMBEDDING_DIMS);
        assert_eq!(decode(&encode(&vector)), Some(vector));
    }

    #[test]
    fn canonical_generated_id_queries_are_recognized_by_the_documented_shape() {
        for prefix in ["t", "e", "s", "sp", "d", "sr", "a", "h", "sub", "r"] {
            assert!(
                canonical_generated_id_query(&format!("{prefix}-1234abcd")),
                "{prefix}"
            );
        }
        assert!(canonical_generated_id_query(" t-1234abcd "));
        assert!(!canonical_generated_id_query("t-1234abc"));
        assert!(!canonical_generated_id_query("t-1234abcg"));
        assert!(!canonical_generated_id_query("t-1234ABCD"));
        assert!(!canonical_generated_id_query("foo-bar"));
        assert!(!canonical_generated_id_query("sub-12345678-extra"));
        assert!(canonical_generated_id_query("r-1234abcd"));
    }

    #[test]
    fn canonical_generated_id_queries_keep_only_literal_hits() {
        assert!(result_is_eligible("sub-deadbeef", 0.95, 0.99, 0.99));
        assert!(result_is_eligible("sub-deadbeef", 0.9, 0.0, 0.0));
        assert!(!result_is_eligible("sub-deadbeef", 0.7, 0.99, 0.99));
        assert!(!result_is_eligible("sub-deadbeef", 0.0, 0.99, 0.99));
        assert!(!result_is_eligible("sub-deadbeef", 0.0, 0.3, 0.99));
        assert!(result_is_eligible(
            "keep old completed items",
            0.0,
            0.0,
            0.19
        ));
        assert!(result_is_eligible(
            "keep-old-completed-items",
            0.0,
            0.0,
            0.19
        ));
    }

    #[test]
    fn literal_exact_score_ignores_tag_only_canonical_id_collisions() {
        let tagged = Document {
            seq: 1,
            source_kind: "task".to_owned(),
            source_id: "t-tagged".to_owned(),
            task_id: Some("t-tagged".to_owned()),
            title: "Tagged collision".to_owned(),
            body: "No literal match here.".to_owned(),
            status: None,
            lane: None,
            tags: "sub-deadbeef".to_owned(),
            created_at: 0,
            updated_at: 0,
            archived: false,
            source_hash: None,
            embedding_model: None,
            embedding: None,
            event_task_id: None,
            event_payload: None,
        };
        let literal = Document {
            seq: 2,
            source_kind: "task".to_owned(),
            source_id: "t-literal".to_owned(),
            task_id: Some("t-literal".to_owned()),
            title: "Literal source body hit".to_owned(),
            body: "Keep sub-deadbeef in the source body.".to_owned(),
            status: None,
            lane: None,
            tags: "irrelevant".to_owned(),
            created_at: 0,
            updated_at: 0,
            archived: false,
            source_hash: None,
            embedding_model: None,
            embedding: None,
            event_task_id: None,
            event_payload: None,
        };
        let query_words = words("sub-deadbeef");
        assert_eq!(
            literal_exact_score(&tagged, "sub-deadbeef", &query_words),
            0.0
        );
        assert!(literal_exact_score(&literal, "sub-deadbeef", &query_words) >= 0.9);
    }

    #[test]
    fn exact_score_still_counts_tag_tokens_for_natural_language_queries() {
        let tagged = Document {
            seq: 1,
            source_kind: "task".to_owned(),
            source_id: "t-tagged".to_owned(),
            task_id: Some("t-tagged".to_owned()),
            title: "Tagged collision".to_owned(),
            body: "No literal match here.".to_owned(),
            status: None,
            lane: None,
            tags: "release ops".to_owned(),
            created_at: 0,
            updated_at: 0,
            archived: false,
            source_hash: None,
            embedding_model: None,
            embedding: None,
            event_task_id: None,
            event_payload: None,
        };
        let query_words = words("release ops");
        assert!(exact_score(&tagged, "release ops", &query_words) > 0.0);
    }
}
