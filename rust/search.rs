use crate::model::{
    Rule, SearchIndexHealth, SearchIndexReport, SearchOptions, SearchReceipt, SearchResult,
};
use crate::registry::now_ms;
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

fn load_documents(connection: &Connection, options: &SearchOptions) -> Result<Vec<Document>> {
    let mut sql = String::from(
        "SELECT seq,source_kind,source_id,task_id,title,body,status,lane,tags,\
         created_at,updated_at,archived,source_hash,embedding_model,embedding \
         FROM search_documents WHERE 1=1",
    );
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if !options.include_archived {
        sql.push_str(" AND archived=0");
    }
    for (column, value) in [
        ("source_kind", options.source.as_ref()),
        ("status", options.status.as_ref()),
        ("lane", options.lane.as_ref()),
    ] {
        if let Some(value) = value {
            sql.push_str(&format!(" AND {column}=?"));
            values.push(Box::new(value.clone()));
        }
    }
    for tag in &options.tags {
        sql.push_str(" AND instr(' ' || tags || ' ',' ' || ? || ' ')>0");
        values.push(Box::new(tag.clone()));
    }
    if let Some(after) = options.after {
        sql.push_str(" AND updated_at>=?");
        values.push(Box::new(after));
    }
    if let Some(before) = options.before {
        sql.push_str(" AND updated_at<=?");
        values.push(Box::new(before));
    }
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(
        params_from_iter(values.iter().map(|value| value.as_ref())),
        |row| {
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
            })
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn lexical_scores(connection: &Connection, query: &str) -> Result<HashMap<i64, f64>> {
    let Some(query) = fts_query(query) else {
        return Ok(HashMap::new());
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
        "t" | "e" | "s" | "d" | "sr" | "a" | "h" | "sub" | "r"
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

pub fn search(
    connection: &Connection,
    board: &str,
    options: &SearchOptions,
) -> Result<Vec<SearchResult>> {
    if options.query.trim().is_empty() {
        bail!("search query is required");
    }
    if options.limit == 0 || options.limit > 100 {
        bail!("search limit must be between 1 and 100");
    }
    if options.max_chars < 256 || options.max_chars > 100_000 {
        bail!("search max chars must be between 256 and 100000");
    }
    let query_words = words(&options.query);
    let query_vector = embed(&options.query);
    let lexical = lexical_scores(connection, &options.query)?;
    let mut results = Vec::new();
    for document in load_documents(connection, options)? {
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
        let exact = exact_score(&document, &options.query, &query_words);
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
        results: kept,
        result_chars: chars,
        truncated,
        generated_at: now_ms(),
    }
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
        let vector = embed(&format!(
            "{} {} {} {} {}",
            document.title,
            document.body,
            document.tags,
            document.status.as_deref().unwrap_or(""),
            document.lane.as_deref().unwrap_or("")
        ));
        transaction.execute(
            "UPDATE search_documents SET source_hash=?,embedding_model=?,embedding=? WHERE seq=?",
            params![
                source_hash(document),
                EMBEDDING_MODEL,
                encode(&vector),
                document.seq
            ],
        )?;
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
    Ok(SearchIndexHealth {
        healthy: source_rows == documents && documents == fts_rows,
        source_rows,
        documents,
        fts_rows,
        missing_embeddings,
        stale_embeddings,
        embedding_model: EMBEDDING_MODEL.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for prefix in ["t", "e", "s", "d", "sr", "a", "h", "sub", "r"] {
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
}
