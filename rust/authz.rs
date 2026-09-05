//! The central authorization guard (ADR-038 clause 5).
//!
//! One decision — allow, or the single generic denial — for every read and
//! write of a board row, computed from an already-minted authority map and an
//! explicit enforcement state. This is the only place the clause-5 all-of-tag
//! and both-scopes rules live; the surfaces that will enforce them
//! (t-90903ebe) call in rather than re-derive them.
//!
//! The guard is pure: it reads no filesystem, no registry, and no enforcement
//! state. The caller performs the single enforcement read
//! ([`crate::routing::enforcement_state`]) and mints the authority
//! ([`crate::policy::Registry::principal_authority`] or
//! [`crate::policy::authority`]), then hands both in. Taking the enforcement
//! state as an argument keeps the guard testable and keeps the read with the
//! caller, while the required (non-optional) parameter makes it structurally
//! impossible to call the guard without stating an enforcement state.
//!
//! The estate is `direct` today. Outside [`Enforcement::Managed`] the guard is
//! a no-op that permits everything, so every existing single-user caller keeps
//! behaving exactly as it does now; `direct` and `prepared` estates are
//! unchanged.

use crate::policy::{Capability, ScopeTuple, satisfies};
use crate::routing::Enforcement;
use anyhow::{Result, bail};
use std::collections::HashMap;

/// The one refusal a denied access produces. Byte-identical whether the row is
/// invisible to the caller or absent from the board, and carrying no tag,
/// board, or row detail, so a denial is not an existence oracle. This is the
/// same generic wording [`crate::policy`] uses for every non-enumerating
/// denial; there is deliberately no second string.
const DENIED_OR_NOT_FOUND: &str = "denied or not found";

/// Decide whether a caller may read one row: it must satisfy `read` at
/// `{board:ID}` and at `{board:ID, tag:SLUG}` for every tag the row carries.
/// An untagged row needs only the board scope.
///
/// `authority` is the caller's already-minted authority map — the value
/// [`crate::policy::authority`] produces from active grants, or that
/// [`crate::policy::Registry::principal_authority`] returns. It is never
/// reconstructed here from a username, actor string, or selector, because
/// those cannot produce authority.
///
/// Returns `Ok(())` when permitted, and the generic
/// [`DENIED_OR_NOT_FOUND`] otherwise. Outside [`Enforcement::Managed`] this
/// always permits.
pub fn check_read(
    enforcement: Enforcement,
    authority: &HashMap<ScopeTuple, Capability>,
    board_id: &str,
    tags: &[String],
) -> Result<()> {
    if !enforcement.is_managed() {
        return Ok(());
    }
    check_scopes(authority, board_id, tags, Capability::Read)
}

/// Decide whether a caller may write one row. A write must satisfy the
/// read-style all-of-tag check at `write` against **both** the old tag set and
/// the resulting tag set, so a retag is permitted only to a caller who could
/// see the row before *and* after the change.
///
/// The arguments are the same shape as [`check_read`]: the minted authority
/// and an explicit enforcement state, never a caller-supplied identity claim.
pub fn check_write(
    enforcement: Enforcement,
    authority: &HashMap<ScopeTuple, Capability>,
    board_id: &str,
    old_tags: &[String],
    resulting_tags: &[String],
) -> Result<()> {
    if !enforcement.is_managed() {
        return Ok(());
    }
    check_scopes(authority, board_id, old_tags, Capability::Write)?;
    check_scopes(authority, board_id, resulting_tags, Capability::Write)
}

/// The all-of-tag requirement for one tag set at one capability: board scope
/// first, then every tag. The `{board, *}` wildcard is honoured exactly once,
/// inside [`satisfies`] in the lattice, not re-derived here — this loop only
/// asks `satisfies` about `{board:ID, tag:SLUG}`, which is precisely the one
/// tuple the wildcard satisfier rule crosses.
fn check_scopes(
    authority: &HashMap<ScopeTuple, Capability>,
    board_id: &str,
    tags: &[String],
    capability: Capability,
) -> Result<()> {
    if !satisfies(
        authority,
        &ScopeTuple::Board {
            board_id: board_id.to_owned(),
        },
        capability,
    ) {
        bail!(DENIED_OR_NOT_FOUND);
    }
    for tag in tags {
        if !satisfies(
            authority,
            &ScopeTuple::BoardTag {
                board_id: board_id.to_owned(),
                tag: tag.to_owned(),
            },
            capability,
        ) {
            bail!(DENIED_OR_NOT_FOUND);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Capability, ScopeTuple, authority};
    use crate::routing::Enforcement;

    /// The immutable per-board UUID (ADR-032) the scope atoms key on.
    const BOARD: &str = "a1b2c3d4-e5f6-4789-abcd-ef0123456789";

    fn grants(pairs: &[(ScopeTuple, Capability)]) -> HashMap<ScopeTuple, Capability> {
        authority(pairs.iter().cloned())
    }

    fn board() -> ScopeTuple {
        ScopeTuple::Board {
            board_id: BOARD.to_owned(),
        }
    }

    fn tag(slug: &str) -> ScopeTuple {
        ScopeTuple::BoardTag {
            board_id: BOARD.to_owned(),
            tag: slug.to_owned(),
        }
    }

    fn wildcard() -> ScopeTuple {
        ScopeTuple::BoardWildcard {
            board_id: BOARD.to_owned(),
        }
    }

    #[test]
    fn untagged_row_needs_only_board_scope() {
        let board_read = grants(&[(board(), Capability::Read)]);
        assert!(
            check_read(Enforcement::Managed, &board_read, BOARD, &[]).is_ok(),
            "board read alone must permit an untagged row"
        );
        let none = grants(&[]);
        assert_eq!(
            check_read(Enforcement::Managed, &none, BOARD, &[])
                .unwrap_err()
                .to_string(),
            DENIED_OR_NOT_FOUND,
            "a caller without board scope must be denied an untagged row"
        );
    }

    #[test]
    fn one_tag_row_permitted_when_tag_held() {
        let a = grants(&[
            (board(), Capability::Read),
            (tag("alpha"), Capability::Read),
        ]);
        assert!(
            check_read(Enforcement::Managed, &a, BOARD, &["alpha".to_owned()]).is_ok(),
            "board plus the single tag must permit a one-tag row"
        );
    }

    #[test]
    fn three_tag_row_denied_to_caller_holding_two() {
        let two = grants(&[
            (board(), Capability::Read),
            (tag("alpha"), Capability::Read),
            (tag("beta"), Capability::Read),
        ]);
        assert!(
            check_read(
                Enforcement::Managed,
                &two,
                BOARD,
                &["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()],
            )
            .is_err(),
            "holding two of three tags must not permit the row"
        );
        let three = grants(&[
            (board(), Capability::Read),
            (tag("alpha"), Capability::Read),
            (tag("beta"), Capability::Read),
            (tag("gamma"), Capability::Read),
        ]);
        assert!(
            check_read(
                Enforcement::Managed,
                &three,
                BOARD,
                &["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()],
            )
            .is_ok(),
            "holding all three tags must permit the row"
        );
    }

    #[test]
    fn wildcard_permits_tag_never_granted_individually() {
        let a = grants(&[(board(), Capability::Read), (wildcard(), Capability::Read)]);
        assert!(
            check_read(Enforcement::Managed, &a, BOARD, &["gamma".to_owned()]).is_ok(),
            "{{board, *}} read must satisfy a tag the caller was never granted individually"
        );
    }

    #[test]
    fn retag_denied_when_only_old_scope_held() {
        let old_only = grants(&[
            (board(), Capability::Write),
            (tag("alpha"), Capability::Write),
        ]);
        assert!(
            check_write(
                Enforcement::Managed,
                &old_only,
                BOARD,
                &["alpha".to_owned()],
                &["beta".to_owned()],
            )
            .is_err(),
            "a caller who could see only the old tag must not retag"
        );
    }

    #[test]
    fn retag_denied_when_only_resulting_scope_held() {
        let resulting_only = grants(&[
            (board(), Capability::Write),
            (tag("beta"), Capability::Write),
        ]);
        assert!(
            check_write(
                Enforcement::Managed,
                &resulting_only,
                BOARD,
                &["alpha".to_owned()],
                &["beta".to_owned()],
            )
            .is_err(),
            "a caller who could see only the resulting tag must not retag"
        );
    }

    #[test]
    fn retag_permitted_when_both_scopes_held() {
        let both = grants(&[
            (board(), Capability::Write),
            (tag("alpha"), Capability::Write),
            (tag("beta"), Capability::Write),
        ]);
        assert!(
            check_write(
                Enforcement::Managed,
                &both,
                BOARD,
                &["alpha".to_owned()],
                &["beta".to_owned()],
            )
            .is_ok(),
            "a caller who could see the row before and after must be able to retag"
        );
    }

    #[test]
    fn read_insufficient_for_write() {
        let read_only = grants(&[
            (board(), Capability::Read),
            (tag("alpha"), Capability::Read),
            (tag("beta"), Capability::Read),
        ]);
        assert!(
            check_write(
                Enforcement::Managed,
                &read_only,
                BOARD,
                &["alpha".to_owned()],
                &["beta".to_owned()],
            )
            .is_err(),
            "read authority must not satisfy a write check"
        );
    }

    #[test]
    fn direct_and_prepared_permit_what_managed_denies() {
        let none = grants(&[]);
        let tags = ["alpha".to_owned()];
        // The Managed case this names: empty authority denies a read and a
        // write of the same row.
        assert!(check_read(Enforcement::Managed, &none, BOARD, &tags).is_err());
        assert!(
            check_write(
                Enforcement::Managed,
                &none,
                BOARD,
                &tags,
                &["beta".to_owned()],
            )
            .is_err()
        );
        for enforcement in [Enforcement::Direct, Enforcement::Prepared] {
            assert!(
                check_read(enforcement, &none, BOARD, &tags).is_ok(),
                "{enforcement:?} must permit what Managed denies"
            );
            assert!(
                check_write(enforcement, &none, BOARD, &tags, &["beta".to_owned()]).is_ok(),
                "{enforcement:?} must permit what Managed denies"
            );
        }
    }

    #[test]
    fn denial_byte_identical_for_invisible_and_absent_row() {
        // Invisible: the caller sees the board but not the row's tag.
        let sees_board = grants(&[(board(), Capability::Read)]);
        let invisible = check_read(
            Enforcement::Managed,
            &sees_board,
            BOARD,
            &["secret".to_owned()],
        )
        .unwrap_err()
        .to_string();
        // Absent: the caller holds no board scope at all, so the row is not
        // there to them.
        let none = grants(&[]);
        let absent = check_read(Enforcement::Managed, &none, BOARD, &["secret".to_owned()])
            .unwrap_err()
            .to_string();
        assert_eq!(
            invisible, DENIED_OR_NOT_FOUND,
            "an invisible-row denial must be the generic string"
        );
        assert_eq!(
            absent, DENIED_OR_NOT_FOUND,
            "an absent-row denial must be the generic string"
        );
        assert_eq!(
            invisible, absent,
            "the two denials must be byte-identical, carrying no row detail"
        );
    }
}
