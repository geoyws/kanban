//! The managed-mode routing boundary (ADR-038 clauses 9 and 11, ADR-033).
//!
//! Under managed enforcement the broker owns the data and every operation —
//! CLI, MCP command, dispatcher, adapter, backup, restore, archive, search —
//! routes through the broker's Unix socket and is authenticated by ITS OWN
//! kernel peer UID (`SO_PEERCRED`), never by a caller-supplied value. This
//! module owns the client side of that boundary: the enforcement gate that
//! refuses the five selector bypasses by name, and the read of the
//! enforcement state those refusals key on.
//!
//! The broker's operation protocol — what it executes once it has
//! authenticated the peer and minted the [`crate::broker::PrincipalContext`] —
//! is the `access` command grammar (t-86eb4fb3) and is deliberately not here;
//! neither is the socket connection itself. What this slice owns is the
//! fail-closed refusal of every route around the broker, so that no managed
//! caller reaches a direct open, a repointed data root, or a selector the
//! broker would have to resolve.

use crate::registry::canonical_data_root;
use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// The five selector bypasses a managed caller is refused, each by name.
///
/// Every one is a way to route around the broker: a direct board file, a
/// repointed data root, a workspace path, a project name, or an environment
/// default. None is a silent downgrade to unmanaged behaviour — each is a
/// named refusal with a specific message and a non-zero exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectorBypass {
    /// `--db PATH`: open a board file directly, bypassing the broker.
    DirectDb,
    /// `KANBAN_DATA_DIR`: repoint the data root away from the managed estate.
    RootPath,
    /// `--workspace PATH`: resolve a project by filesystem path.
    Workspace,
    /// `--project NAME`: address a registered project by name.
    Project,
    /// `KANBAN_DB` / `KANBAN_PROJECT`: environment defaults for `--db`/`--project`.
    EnvironmentSelector,
}

impl SelectorBypass {
    /// The selector (or selectors) this bypass names, as the caller wrote it.
    pub fn as_str(self) -> &'static str {
        match self {
            SelectorBypass::DirectDb => "--db",
            SelectorBypass::RootPath => "KANBAN_DATA_DIR",
            SelectorBypass::Workspace => "--workspace",
            SelectorBypass::Project => "--project",
            SelectorBypass::EnvironmentSelector => "KANBAN_DB / KANBAN_PROJECT",
        }
    }

    /// The named refusal, specific to this bypass. A managed caller reads this
    /// exact message on stderr and exits non-zero; it is never a silent
    /// downgrade to a direct open. The selector name is interpolated from
    /// [`SelectorBypass::as_str`] so it has a single source.
    pub fn refusal(self) -> String {
        let reason = match self {
            SelectorBypass::DirectDb => {
                "the broker owns board data and opens it for you; a direct \
                 database path bypasses the broker"
            }
            SelectorBypass::RootPath => {
                "the broker owns the data root; the caller cannot repoint it"
            }
            SelectorBypass::Workspace => {
                "board selection is the broker's, not a caller-chosen filesystem path"
            }
            SelectorBypass::Project => {
                "board selection is the broker's, not a caller-chosen project name"
            }
            SelectorBypass::EnvironmentSelector => {
                "board selection is the broker's, not an environment default"
            }
        };
        format!("managed mode refuses {}: {reason}", self.as_str())
    }
}

/// The registry's enforcement state (ADR-038 clause 9). The one-way
/// `direct -> prepared -> managed` transition is what separates a single-user
/// tool with an honest audit label from a multi-user system with an
/// authenticated actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enforcement {
    Direct,
    Prepared,
    Managed,
}

impl Enforcement {
    /// Parse a stored enforcement state.
    ///
    /// `Direct` is the PERMISSIVE end of this enum -- it allows all five
    /// bypasses -- so an unrecognized value must never land there. A state
    /// this binary does not know is most likely one a NEWER binary wrote on a
    /// managed host, and per ADR-008 that ambiguity fails closed.
    pub fn parse(state: &str) -> Enforcement {
        match state {
            "managed" => Enforcement::Managed,
            "prepared" => Enforcement::Prepared,
            "direct" => Enforcement::Direct,
            _ => Enforcement::Managed,
        }
    }

    pub fn is_managed(self) -> bool {
        matches!(self, Enforcement::Managed)
    }
}

/// The named refusal for the first present bypass, or `None` when there is
/// nothing to refuse.
///
/// Under `direct` and `prepared` enforcement every selector is still honoured
/// — existing single-user behaviour is unchanged — so the gate is a no-op
/// there. Only `managed` refuses, and it refuses by name, never silently. This
/// is the pure core; the CLI passes the enforcement state it read and the
/// bypasses it collected.
pub fn refusal(state: Enforcement, bypasses: &[SelectorBypass]) -> Option<String> {
    if !state.is_managed() {
        return None;
    }
    bypasses.first().map(|bypass| bypass.refusal())
}

/// Read the enforcement state from the registry at the canonical data root.
///
/// The canonical root (`$XDG_DATA_HOME/kanban`, else `~/.local/share/kanban`)
/// deliberately ignores `KANBAN_DATA_DIR`, because that override is itself the
/// root-path bypass this gate exists to refuse: reading it would let a caller
/// repoint the estate and make a `managed` registry look `direct`.
///
/// The cases are distinguished with the open primitive itself, because
/// SQLite's read-only open reports an absent file and a permission-denied file
/// as the same `CANTOPEN` error.
pub fn enforcement_state() -> Result<Enforcement> {
    // The canonical root is deliberately not a parameter here: letting a
    // caller choose it is one of the five bypasses this gate exists to refuse.
    enforcement_state_at(&canonical_data_root()?)
}

/// The enforcement decision for one registry root, split out so every failure
/// mode of the read is reachable from a test. Only `enforcement_state` may
/// choose the root.
fn enforcement_state_at(root: &Path) -> Result<Enforcement> {
    let registry_path = root.join("registry.db");
    match fs::File::open(&registry_path) {
        Ok(_) => {}
        // No registry at all is a fresh, unmanaged install.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Enforcement::Direct),
        // One we may not even open is the broker-owned managed estate.
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            return Ok(Enforcement::Managed);
        }
        Err(error) => return Err(error.into()),
    }
    // ADR-008: an enforcement state we cannot READ is ambiguous, and ambiguity
    // fails closed. Mapping every error to `Direct` -- the PERMISSIVE end of
    // the enum -- would let a corrupt or locked registry on a managed host
    // silently re-open all five bypasses, the one direction this gate must
    // never fail in.
    //
    // A registry predating REGISTRY_V14 is the exception, and it is checked
    // with a bare `PRAGMA user_version` rather than `Registry::open_readonly_at`
    // on purpose: that helper REFUSES a pre-V14 registry outright, so routing
    // it through there would turn "you have not migrated yet" into a hard
    // refusal of every selector. An unmigrated estate is not ambiguous, it is
    // an unmanaged single-user one, and it must keep working.
    let connection = open_readonly_sqlite(&registry_path)
        .with_context(|| format!("read enforcement state from {}", registry_path.display()))?;
    let schema: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .with_context(|| format!("read enforcement state from {}", registry_path.display()))?;
    if schema < ENFORCEMENT_STATE_SCHEMA {
        return Ok(Enforcement::Direct);
    }
    let state: String = connection
        .query_row(
            "SELECT state FROM enforcement_state WHERE id=1",
            [],
            |row| row.get(0),
        )
        .with_context(|| format!("read enforcement state from {}", registry_path.display()))?;
    Ok(Enforcement::parse(&state))
}

/// The registry schema that introduced `enforcement_state`. Below this a
/// registry is legacy-unmanaged, not ambiguous.
const ENFORCEMENT_STATE_SCHEMA: i64 = 14;

/// Open the registry file read-only for the enforcement probe alone, without
/// the schema-version gate `Registry::open_readonly_at` applies.
fn open_readonly_sqlite(path: &Path) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;

    #[test]
    fn every_bypass_is_refused_by_name_in_managed_mode() {
        let all = [
            SelectorBypass::DirectDb,
            SelectorBypass::RootPath,
            SelectorBypass::Workspace,
            SelectorBypass::Project,
            SelectorBypass::EnvironmentSelector,
        ];
        for bypass in all {
            let message = refusal(Enforcement::Managed, &[bypass])
                .expect("a managed caller must be refused, not waved through");
            // The refusal names the specific bypass: the caller can act on it.
            assert!(
                message.contains(bypass.as_str()),
                "refusal for {bypass:?} must name {}: {message}",
                bypass.as_str()
            );
        }
    }

    #[test]
    fn unmanaged_states_do_not_refuse_any_bypass() {
        let all = [
            SelectorBypass::DirectDb,
            SelectorBypass::RootPath,
            SelectorBypass::Workspace,
            SelectorBypass::Project,
            SelectorBypass::EnvironmentSelector,
        ];
        for state in [Enforcement::Direct, Enforcement::Prepared] {
            for bypass in all {
                assert_eq!(
                    refusal(state, &[bypass]),
                    None,
                    "a {state:?} caller must keep single-user selector behaviour for {bypass:?}"
                );
            }
        }
    }

    #[test]
    fn refusal_names_only_the_first_present_bypass() {
        // A caller who passed both --db and --project reads the most specific
        // refusal first; it is the one that matters, and a refusal is its own
        // fix (ADR-008).
        let message = refusal(
            Enforcement::Managed,
            &[SelectorBypass::DirectDb, SelectorBypass::Project],
        )
        .expect("managed caller refused");
        assert!(message.contains("--db"), "{message}");
        assert!(!message.contains("--project"), "{message}");
    }

    #[test]
    fn enforcement_parse_distinguishes_the_three_states() {
        assert_eq!(Enforcement::parse("direct"), Enforcement::Direct);
        assert_eq!(Enforcement::parse("prepared"), Enforcement::Prepared);
        assert_eq!(Enforcement::parse("managed"), Enforcement::Managed);
        // `Direct` is the permissive end of this enum, so an unknown state
        // must NOT land there: a value this binary does not recognize is most
        // likely a newer binary's managed state, and ADR-008 fails closed.
        assert_eq!(Enforcement::parse("bogus"), Enforcement::Managed);
        assert!(Enforcement::parse("").is_managed());
        assert!(Enforcement::Managed.is_managed());
        assert!(!Enforcement::Prepared.is_managed());
        assert!(!Enforcement::Direct.is_managed());
    }

    /// Every failure mode of the enforcement read is a security decision,
    /// because `Direct` is what allows all five bypasses. Exactly two things
    /// may produce it: no registry at all, and a pre-`REGISTRY_V14` registry
    /// with no `enforcement_state` table. Anything else is ambiguous and must
    /// fail closed (ADR-008).
    ///
    /// This drives `enforcement_state_at` rather than asserting the primitives
    /// around it: an earlier version of this test checked only
    /// `open_readonly_at(..).enforcement_state().is_err()`, and reverting the
    /// decision to `Err(_) => Direct` still passed it. A test of a security
    /// decision has to call the code that decides.
    #[test]
    fn only_absent_or_pre_v14_registries_resolve_to_direct() {
        let root = std::env::temp_dir().join(format!(
            "kanban-routing-enforcement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();

        // 1. No registry at all: a fresh install, legitimately unmanaged.
        assert_eq!(enforcement_state_at(&root).unwrap(), Enforcement::Direct);

        // 2. A real V14+ registry reads its own stored state.
        let fresh = Registry::open_test_at(&root).unwrap();
        drop(fresh);
        assert_eq!(enforcement_state_at(&root).unwrap(), Enforcement::Direct);

        // 3. A managed estate is honoured.
        {
            let connection = rusqlite::Connection::open(root.join("registry.db")).unwrap();
            connection
                .execute_batch("UPDATE enforcement_state SET state='managed' WHERE id=1")
                .unwrap();
        }
        assert_eq!(enforcement_state_at(&root).unwrap(), Enforcement::Managed);

        // 4. An UNMIGRATED registry, still stamped below V14. This is the case
        // that must not fail closed: `Registry::open_readonly_at` refuses such
        // a file outright, so probing through it would turn "you have not
        // migrated yet" into a refusal of every selector for existing users.
        // The row still says `managed` here, which is exactly the trap -- the
        // schema stamp is what decides, so a stale row cannot lock anyone out.
        {
            let connection = rusqlite::Connection::open(root.join("registry.db")).unwrap();
            connection
                .execute_batch(&format!(
                    "PRAGMA user_version = {}",
                    ENFORCEMENT_STATE_SCHEMA - 1
                ))
                .unwrap();
        }
        assert_eq!(
            enforcement_state_at(&root).unwrap(),
            Enforcement::Direct,
            "an unmigrated registry is a legacy single-user estate, not an ambiguity"
        );

        // 5. Present but unreadable as SQLite. This is the case that used to
        // resolve `Direct` and hand back every bypass on a managed host.
        fs::write(root.join("registry.db"), b"not a database").unwrap();
        let ambiguous = enforcement_state_at(&root);
        assert!(
            ambiguous.is_err(),
            "a corrupt registry must fail closed, got {:?}",
            ambiguous.map(|state| state)
        );

        fs::remove_dir_all(&root).ok();
    }
}
