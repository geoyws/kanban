//! Cross-board prerequisite identities (`CROSS-01`, `CROSS-02`, `CROSS-05`,
//! `CROSS-06`; ADR-056, ADR-057).
//!
//! Three things live here and nothing else: the strict `--depends-on-json`
//! shape, the identity of the board a dependency is being written ON (the
//! target), and the read-only, fail-closed lookup of the board a foreign
//! prerequisite lives on (the source). Gate enforcement and the qualified read
//! projections are their own slices; they consume [`resolve_source`] and the
//! pins `task_foreign_dependencies` stores.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

use crate::authz::AuthzContext;
use crate::model::{DependencyRef, board_id_from_path};

/// The refusal a foreign prerequisite gets when it cannot be pinned, whatever
/// the cause. One sentence for every cause, so it never confirms which.
pub const UNAVAILABLE_SENTENCE: &str = "cannot be used as a prerequisite: it is unavailable to this caller. \
     Nothing was written";

/// Parse `--depends-on-json`: a JSON array of objects carrying exactly the
/// string fields `boardID` and `id` (`CROSS-01`).
///
/// Parsed straight into the typed shape, never through an untyped value, so a
/// duplicated key inside one object refuses as ambiguous instead of silently
/// keeping the last one. `boardID` must be a board UUID in its canonical
/// lowercase hyphenated spelling — no name, path or other alias. `id` is
/// opaque: it is never split on `/`, `:` or anything else. An exact repeat of
/// an entry collapses, as a repeated `--depends-on` does; the result is a set.
pub fn parse_dependency_json(text: &str) -> Result<Vec<DependencyRef>> {
    let entries: Vec<DependencyRef> = serde_json::from_str(text).with_context(|| {
        "--depends-on-json must be a JSON array of {\"boardID\": \"UUID\", \"id\": \"ID\"} \
         objects with exactly those two string fields"
            .to_owned()
    })?;
    let mut set: Vec<DependencyRef> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.into_iter().enumerate() {
        if !is_canonical_uuid(&entry.board_id) {
            bail!(
                "--depends-on-json entry {index}: boardID must be a board UUID in canonical \
                 lowercase hyphenated form; board names and paths are not accepted"
            );
        }
        if entry.id.is_empty() {
            bail!("--depends-on-json entry {index}: id must not be empty");
        }
        if !set.contains(&entry) {
            set.push(entry);
        }
    }
    Ok(set)
}

pub(crate) fn is_canonical_uuid(text: &str) -> bool {
    Uuid::parse_str(text).is_ok_and(|uuid| uuid.hyphenated().to_string() == text)
}

/// The board a dependency is written on, as `--depends-on-json` needs it.
pub struct TargetBoard {
    /// The canonical registry root this board is registered under.
    pub root: PathBuf,
    /// The board's registered UUID, its file stem.
    pub board_id: String,
    /// Whether this board has taken the CROSS step and holds the registry's
    /// registration token, so it can store a foreign pin.
    pub cross_aware: bool,
}

/// What the board behind `connection` is for `--depends-on-json`, or a
/// refusal naming the fix.
///
/// A scratch `--db` file has no registered board UUID, so it has nothing a
/// `boardID` could name and the whole flag refuses there; local
/// `--depends-on` keeps working (`CROSS-01`). A file under a registry root
/// that the registry does not list as an active board refuses the same way:
/// an orphan is not a registered board either.
pub fn target_board(connection: &Connection) -> Result<TargetBoard> {
    const SCRATCH: &str = "--depends-on-json needs a registered board, and this is a scratch \
         --db file with no registered board UUID. Register it with `kanban init` or select a \
         registered board; local --depends-on still works here";
    let path = connection
        .path()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .context(SCRATCH)?;
    let Some(root) = crate::db::registry_root_of(&path)? else {
        bail!(SCRATCH);
    };
    let board_id = board_id_from_path(&path.to_string_lossy()).context(SCRATCH)?;
    let Some(registered) = registered_board(&root, &board_id)? else {
        bail!(SCRATCH);
    };
    let schema = crate::db::schema_version(connection)?;
    let file_token = crate::db::board_registration_token(connection)?;
    let cross_aware = schema == crate::db::BOARD_SCHEMA_VERSION
        && registered.token.is_some()
        && file_token == registered.token;
    Ok(TargetBoard {
        root,
        board_id,
        cross_aware,
    })
}

/// Refuse a foreign entry on a target that cannot hold a pin yet.
pub fn require_cross_aware(target: &TargetBoard) -> Result<()> {
    if target.cross_aware {
        return Ok(());
    }
    bail!(
        "this board has not taken its cross-board upgrade, so it cannot hold a prerequisite on \
         another board. Its owner upgrades it by running the same `kanban init --name NAME` \
         that registered it; local prerequisites, including JSON entries naming this board's \
         own UUID, still work"
    )
}

pub(crate) struct RegisteredBoard {
    pub(crate) path: PathBuf,
    pub(crate) token: Option<String>,
}

/// The active registry row whose board file stem is `board_id`, read through
/// the registry's read-only open. Exactly one, or none: two rows sharing a
/// stem would be an ambiguous alias, and ambiguity resolves to nothing.
pub(crate) fn registered_board(root: &Path, board_id: &str) -> Result<Option<RegisteredBoard>> {
    let registry = crate::db::open_registry_readonly(&root.join("registry.db"))?;
    let token_column = crate::db::registry_holds_registration_tokens(&registry)?;
    let sql = if token_column {
        "SELECT board_path,registration_token FROM boards WHERE archived=0"
    } else {
        "SELECT board_path,NULL FROM boards WHERE archived=0"
    };
    let mut statement = registry.prepare(sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut matches = rows
        .into_iter()
        .filter(|(path, _)| board_id_from_path(path).as_deref() == Some(board_id));
    let (Some((path, token)), None) = (matches.next(), matches.next()) else {
        return Ok(None);
    };
    Ok(Some(RegisteredBoard {
        path: PathBuf::from(path),
        token,
    }))
}

/// The pin of a foreign prerequisite this caller may read, as it stands now:
/// exactly what `task_foreign_dependencies` stores (`CROSS-02`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSource {
    pub board_id: String,
    /// The source board's registration incarnation (registry and file agree).
    pub registration: String,
    pub item_id: String,
    /// The source item's creation incarnation.
    pub incarnation: String,
}

/// Resolve `reference` on its source board under `root`, for `caller`.
///
/// Read-only and fail-closed (`CROSS-05`, `CROSS-06`, ADR-057 §4): the
/// registry and the source board are opened only through their read-only,
/// no-migration, no-sweep openers, so an old, newer or corrupt source stays
/// untouched. Every way the source can fail to be a readable, current,
/// registered, token-matching, audit-valid row this caller may read answers
/// `None` — one answer, so the caller learns nothing about which cause it
/// was. A board UUID from another registry is simply not in this one.
pub fn resolve_source(
    root: &Path,
    caller: &AuthzContext,
    reference: &DependencyRef,
) -> Option<ResolvedSource> {
    resolve_source_inner(root, caller, reference).ok().flatten()
}

fn resolve_source_inner(
    root: &Path,
    caller: &AuthzContext,
    reference: &DependencyRef,
) -> Result<Option<ResolvedSource>> {
    let registry_path = root.join("registry.db");
    if crate::db::stored_schema_version(&registry_path) != Some(crate::db::REGISTRY_SCHEMA_VERSION)
    {
        return Ok(None);
    }
    let Some(registered) = registered_board(root, &reference.board_id)? else {
        return Ok(None);
    };
    let Some(registration) = registered.token else {
        return Ok(None);
    };
    // The registered canonical file and nothing else: not a copy, not a
    // symlink alias of it, not a row whose path wandered out of this root.
    let expected = root
        .canonicalize()?
        .join("boards")
        .join(format!("{}.db", reference.board_id));
    if registered.path.canonicalize()? != expected {
        return Ok(None);
    }
    if crate::db::stored_schema_version(&expected) != Some(crate::db::BOARD_SCHEMA_VERSION) {
        return Ok(None);
    }
    let board = crate::db::open_board_readonly(&expected)?;
    if crate::db::board_registration_token(&board)?.as_deref() != Some(registration.as_str()) {
        return Ok(None);
    }
    if !crate::audit::verify_board(&board)?.healthy {
        return Ok(None);
    }
    // Authority before existence: the read check runs against the row's tags
    // (none for a missing row, so board scope alone decides), and only then
    // is the row's absence observed — by which point both answer `None`.
    let tags = {
        let mut statement =
            board.prepare("SELECT tag FROM task_tags WHERE task_id=? ORDER BY tag")?;
        statement
            .query_map([&reference.id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if caller
        .for_board(reference.board_id.clone())
        .check_read(&tags)
        .is_err()
    {
        return Ok(None);
    }
    let incarnation = board
        .query_row(
            "SELECT incarnation FROM tasks WHERE id=?",
            [&reference.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(incarnation) = incarnation else {
        return Ok(None);
    };
    Ok(Some(ResolvedSource {
        board_id: reference.board_id.clone(),
        registration,
        item_id: reference.id.clone(),
        incarnation,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOARD: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    #[test]
    fn json_set_is_exact_typed_and_alias_free() {
        let parsed =
            parse_dependency_json(&format!(r#"[{{"boardID":"{BOARD}","id":"a/b:c\"d"}}]"#))
                .unwrap();
        assert_eq!(
            parsed,
            [DependencyRef {
                board_id: BOARD.into(),
                id: "a/b:c\"d".into()
            }],
            "the id stays opaque: no delimiter is split"
        );
        assert!(parse_dependency_json("[]").unwrap().is_empty());
        // Repeats collapse into one set member, like repeated --depends-on.
        let twice =
            format!(r#"[{{"boardID":"{BOARD}","id":"x"}},{{"boardID":"{BOARD}","id":"x"}}]"#);
        assert_eq!(parse_dependency_json(&twice).unwrap().len(), 1);
        for refused in [
            "{}".to_owned(),
            r#"["t-1"]"#.to_owned(),
            format!(r#"[{{"boardID":"{BOARD}"}}]"#),
            format!(r#"[{{"boardID":"{BOARD}","id":"x","name":"y"}}]"#),
            format!(r#"[{{"boardID":"{BOARD}","id":7}}]"#),
            format!(r#"[{{"boardID":"{BOARD}","id":""}}]"#),
            r#"[{"boardID":"kanban","id":"x"}]"#.to_owned(),
            format!(r#"[{{"boardID":"{}","id":"x"}}]"#, BOARD.to_uppercase()),
            format!(r#"[{{"boardID":"{}","id":"x"}}]"#, BOARD.replace('-', "")),
            format!(r#"[{{"boardID":"{BOARD}","boardID":"{BOARD}","id":"x"}}]"#),
        ] {
            assert!(
                parse_dependency_json(&refused).is_err(),
                "{refused} must refuse"
            );
        }
    }
}
