//! Compiled-binary E2E: `--limit` caps and ceilings, and enum-argument refusals.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

#[test]
fn a_negative_limit_is_refused_rather_than_read_as_no_limit() {
    // SQLite reads LIMIT -1 as *no limit*, so a caller who explicitly bounded
    // a listing got every row of it back and reported success -- the same
    // shape as a --max-chars that is accepted and ignored.
    let fixture = Fixture::new("limit");
    fixture.ok_json(&fixture.main, &["init", "--name", "LIMIT", "--json"]);
    for index in 0..4 {
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                &format!("item {index}"),
                "--as",
                "agent",
                "--kind",
                "risk",
                "--json",
            ],
        );
    }

    // A bound is honoured, and zero means zero.
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--limit", "2", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--limit", "0", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a limit of zero asks for nothing"
    );

    // A negative is refused on every command that takes the flag.
    for command in [
        vec!["attention", "list", "--limit", "-1", "--json"],
        vec!["events", "--limit", "-1", "--json"],
    ] {
        let refused = fixture.run(&fixture.main, &command);
        assert!(
            !refused.status.success(),
            "{command:?} accepted a negative limit"
        );
        let error = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(error.contains("--limit"), "{error}");
        assert!(error.contains("-1"), "{error}");
    }
}

/// One capped listing, for the ADR-037 property below. Adding a listing is
/// one row here; the test refuses to pass until every `--limit`-taking
/// operation the binary publishes has one.
struct CappedListing {
    label: &'static str,
    /// The listing as invoked, without `--limit` or `--json`.
    argv: &'static [&'static str],
    /// The default it must not pass off as the whole.
    default: usize,
    /// Where the rows sit in the reply: the bare array, or this key of an object.
    rows: Option<&'static str>,
    /// Stand up whatever the rows hang off; what it returns is handed to `seed`.
    prepare: fn(&Fixture) -> String,
    /// Add one more row the listing would return.
    seed: fn(&Fixture, &str, usize),
}

/// One open attention item, so a resolve reaches its own validation instead
/// of "not found".
fn seed_open_attention(fixture: &Fixture) -> String {
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "needs a verdict",
            "--as",
            "geoyws",
            "--json",
        ],
    )["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn seed_nothing(_: &Fixture) -> String {
    String::new()
}

fn seed_task(fixture: &Fixture) -> String {
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "capped history", "--id", "t-1", "--json"],
    );
    String::new()
}

fn seed_task_and_lease(fixture: &Fixture) -> String {
    seed_task(fixture);
    fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "agent", "--json"])["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn seed_task_and_board_path(fixture: &Fixture) -> String {
    seed_task(fixture);
    board_path_for_project(fixture, &fixture.main, "CAPPED")
        .to_str()
        .unwrap()
        .to_owned()
}

const CAPPED_LISTINGS: &[CappedListing] = &[
    CappedListing {
        label: "sprints",
        argv: &["sprint", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "sprint",
                    "new",
                    &format!("cap probe {index}"),
                    "--target-version",
                    "0.3.0",
                    "--start",
                    "0",
                    "--end",
                    "4102444800000",
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "events",
        argv: &["events"],
        default: 50,
        rows: None,
        prepare: seed_task,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "note",
                    "t-1",
                    &format!("event {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "registry-events",
        argv: &["events", "--registry"],
        default: 50,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "rule",
                    "add",
                    &format!("rule {index}"),
                    "--as",
                    "geo",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "sitrep-list",
        argv: &["sitrep", "list"],
        default: 20,
        rows: None,
        prepare: seed_nothing,
        // One lane each: a lane keeps only its ten newest current, and the
        // rest are archived out of the default view.
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "sitrep",
                    "post",
                    &format!("sitrep {index}"),
                    "--as",
                    "agent",
                    "--lane",
                    &format!("lane-{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "attention-list",
        argv: &["attention", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "attention",
                    "raise",
                    &format!("item {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "deploy-list",
        argv: &["deploy", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "deploy",
                    "start",
                    "--repo",
                    "geoyws/kanban",
                    "--commit",
                    "1111111111111111111111111111111111111111",
                    "--tier",
                    "@_p",
                    "--environment",
                    "production",
                    "--host",
                    "hax",
                    "--url",
                    "https://kb.geoy.ws",
                    "--operation-id",
                    &format!("op-{index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "claim-candidates",
        argv: &["claim", "--candidates", "--as", "agent"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    &format!("candidate {index}"),
                    "--id",
                    &format!("t-c{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "search",
        argv: &["search", "quokka", "--source", "task"],
        default: 10,
        rows: Some("results"),
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    &format!("quokka {index}"),
                    "--id",
                    &format!("t-q{index}"),
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "handoff-list",
        argv: &["handoff", "list"],
        default: 100,
        rows: None,
        prepare: seed_nothing,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "handoff",
                    "create",
                    "--as",
                    "agent",
                    "--to",
                    "@:team/project/driver-2",
                    "--summary",
                    &format!("handoff {index}"),
                    "--intent",
                    "carry on",
                    "--next-action",
                    "resume",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-notes",
        argv: &["task", "show", "t-1"],
        default: 100,
        rows: Some("notes"),
        prepare: seed_task,
        seed: |fixture, _, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "note",
                    "t-1",
                    &format!("note {index}"),
                    "--as",
                    "agent",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-checkpoints",
        argv: &["task", "show", "t-1"],
        default: 20,
        rows: Some("checkpoints"),
        prepare: seed_task_and_lease,
        seed: |fixture, lease, index| {
            fixture.ok_json(
                &fixture.main,
                &[
                    "checkpoint",
                    "t-1",
                    "--lease",
                    lease,
                    "--as",
                    "agent",
                    "--summary",
                    &format!("checkpoint {index}"),
                    "--intent",
                    "carry on",
                    "--next-action",
                    "resume",
                    "--json",
                ],
            );
        },
    },
    CappedListing {
        label: "task-show-handoffs",
        argv: &["task", "show", "t-1"],
        default: 100,
        rows: Some("handoffs"),
        prepare: seed_task_and_board_path,
        // Written straight into the table: every task handoff the CLI creates
        // also writes a checkpoint, so a hundred of them through the binary
        // would trip the twenty-checkpoint cap first and this cap could never
        // be observed on its own. The rows are real; only the checkpoint
        // side-effect is skipped.
        seed: |_, board, index| {
            Connection::open(board)
                .unwrap()
                .execute(
                    "INSERT INTO handoffs(id,task_id,checkpoint_seq,reason,status,from_agent,\
                     summary,intent,next_action,created_at) \
                     VALUES(?,'t-1',NULL,'manual','pending','agent',?,'carry on','resume',?)",
                    params![
                        format!("h-{index:08}"),
                        format!("handoff {index}"),
                        index as i64
                    ],
                )
                .unwrap();
        },
    },
    CappedListing {
        label: "access-audit",
        argv: &["access", "audit"],
        default: 50,
        rows: None,
        prepare: seed_nothing,
        // A refused `access` attempt appends exactly one denied access-audit
        // row and no policy event (clause 4), so one denied grant per index
        // seeds exactly one row of what `access audit` lists.
        seed: |fixture, _, _index| {
            let _ = fixture.run(
                &fixture.main,
                &[
                    "access",
                    "grant",
                    "--principal",
                    "p-deadbeef",
                    "--capability",
                    "read",
                    "--scope",
                    "registry",
                    "--as",
                    "geoyws",
                    "--reason",
                    "seed",
                    "--json",
                ],
            );
        },
    },
];

/// ADR-037: a capped listing that would exceed its default without `--limit`
/// refuses and names the flag; exactly the default is complete and answered;
/// an explicit `--limit` is honoured as-is.
///
/// Read bottom-up: the refusal is asserted on a board holding one row more
/// than the default, which is the one case every day of the silent-cap bug
/// answered with a full-looking page and exit 0. A test that only seeded
/// under the cap would have passed throughout.
#[test]
fn every_capped_listing_refuses_a_default_it_would_exceed_and_answers_one_it_meets() {
    // Enumerated from the surface the binary publishes, so a listing that
    // grows `--limit` later has to join the table or fail here. `watch` is
    // one exception: its `--limit` sizes a batch, and it computes and
    // reports truncation itself. `worker list` is the other, by spec: it
    // answers at most its default with `"truncated": true` when more exist
    // (docs/specs/identity.md IDENT-15), which the refusal table below
    // cannot express. Its bound is proven instead by
    // `worker_list_is_bounded_and_the_manifest_carries_the_worker_tools` in
    // tests/worker_identity_e2e.rs: 501 workers list as 50 rows plus
    // `"truncated": true`, and `--limit 501` is refused naming 500.
    let fixture = Fixture::new("capped-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "SURFACE", "--json"]);
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    for operation in schema["operations"].as_array().unwrap() {
        let name = operation["name"].as_str().unwrap();
        let takes_limit = operation["flags"]
            .as_array()
            .unwrap()
            .iter()
            .any(|flag| flag["name"] == "limit");
        if !takes_limit || name == "watch" || name == "worker list" {
            continue;
        }
        let words = name.split(' ').collect::<Vec<_>>();
        assert!(
            CAPPED_LISTINGS
                .iter()
                .any(|listing| listing.argv.starts_with(&words)),
            "`{name}` takes --limit but no CAPPED_LISTINGS row proves it refuses its default"
        );
    }
    drop(fixture);

    for listing in CAPPED_LISTINGS {
        let fixture = Fixture::new(&format!("capped-{}", listing.label));
        fixture.ok_json(&fixture.main, &["init", "--name", "CAPPED", "--json"]);
        let context = (listing.prepare)(&fixture);
        let rows = |value: &Value| -> Vec<Value> {
            match listing.rows {
                Some(key) => value[key].as_array(),
                None => value.as_array(),
            }
            .unwrap_or_else(|| {
                panic!(
                    "{}: no rows at {:?} in {value}",
                    listing.label, listing.rows
                )
            })
            .clone()
        };
        let with_limit = |limit: usize| -> Vec<Value> {
            let mut argv = listing.argv.to_vec();
            let limit = limit.to_string();
            argv.extend(["--limit", &limit, "--json"]);
            rows(&fixture.ok_json(&fixture.main, &argv))
        };
        let mut bare = listing.argv.to_vec();
        bare.push("--json");

        // Whatever `init` and `prepare` already wrote counts toward the cap.
        let baseline = with_limit(listing.default + 1).len();
        assert!(
            baseline <= listing.default,
            "{}: the board starts past the cap ({baseline})",
            listing.label
        );
        for index in baseline..listing.default {
            (listing.seed)(&fixture, &context, index);
        }

        // Exactly the default, no extra row: complete, and answered as such.
        // This is the false refusal a count-equals-limit check would commit.
        let complete = fixture.run(&fixture.main, &bare);
        assert!(
            complete.status.success(),
            "{}: exactly {} rows were refused as if more existed\nstderr: {}",
            listing.label,
            listing.default,
            String::from_utf8_lossy(&complete.stderr)
        );
        assert_eq!(
            rows(&serde_json::from_slice(&complete.stdout).unwrap()).len(),
            listing.default,
            "{}: a complete page at the cap was cut",
            listing.label
        );

        // One past it, no --limit: refuse, naming the cap and the flag.
        (listing.seed)(&fixture, &context, listing.default);
        let refused = fixture.run(&fixture.main, &bare);
        let message = refusal_object(&refused);
        assert!(
            message.contains("--limit"),
            "{}: the refusal does not name its fix: {message}",
            listing.label
        );
        assert!(
            message.contains(&listing.default.to_string()),
            "{}: the refusal does not name the cap it stopped at: {message}",
            listing.label
        );
        let plain = fixture.run(&fixture.main, listing.argv);
        assert!(
            !plain.status.success(),
            "{}: without --json the same listing answered",
            listing.label
        );
        assert!(
            String::from_utf8_lossy(&plain.stderr).contains("--limit"),
            "{}: the plain refusal does not name --limit",
            listing.label
        );

        // An explicit bound is honoured as stated, hit or not, with no marker.
        assert_eq!(
            with_limit(listing.default).len(),
            listing.default,
            "{}: --limit at the cap did not return exactly the cap",
            listing.label
        );
        assert_eq!(
            with_limit(listing.default + 1).len(),
            listing.default + 1,
            "{}: --limit above the cap did not reach the row past it",
            listing.label
        );
    }
}

/// The ceiling is 1,000,000 on every surface that takes `--limit`, and it is
/// the same refusal on each.
///
/// Read bottom-up: the row past the ceiling is refused with one wording, and
/// the ceiling itself answers. Before t-5d38449a `search` stopped at 100,
/// `watch` at 1000 and every other listing had no ceiling at all, so an agent
/// asking for a board's whole history had to know which surface it was on.
#[test]
fn every_limit_surface_accepts_one_million_and_refuses_one_more() {
    let fixture = Fixture::new("limit-ceiling");
    fixture.ok_json(&fixture.main, &["init", "--name", "CEILING", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "ceiling probe",
            "--id",
            "t-ceiling",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "needs a verdict",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "still going",
            "--as",
            "agent",
            "--lane",
            "driver",
            "--json",
        ],
    );

    // `watch` is here without `--follow`: it drains the backlog and exits, so
    // the batch size is exercised without the loop.
    let surfaces: [&[&str]; 6] = [
        &["events"],
        &["search", "ceiling"],
        &["attention", "list"],
        &["sitrep", "list"],
        &["watch", "--cursor", "0"],
        &["deploy", "list"],
    ];
    for surface in surfaces {
        let mut accepted = surface.to_vec();
        accepted.extend(["--limit", "1000000", "--json"]);
        let answered = fixture.run(&fixture.main, &accepted);
        assert!(
            answered.status.success(),
            "{surface:?} refused the ceiling itself\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&answered.stdout),
            String::from_utf8_lossy(&answered.stderr)
        );

        let mut refused_argv = surface.to_vec();
        refused_argv.extend(["--limit", "1000001", "--json"]);
        let refused = fixture.run(&fixture.main, &refused_argv);
        assert!(
            !refused.status.success(),
            "{surface:?} accepted a limit past the ceiling"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
        assert!(
            stderr.contains(&over_ceiling_refusal("1000001")),
            "{surface:?} refused with its own wording instead of the shared one: {stderr}"
        );
    }
}

/// `ev --limit N` that cut names the cut on stderr; stdout and the exit
/// status are exactly what ADR-037 §3 promised.
///
/// Read bottom-up: with five events on the board, `--limit 5` and `--limit 6`
/// say nothing, because a page that reached the end of history is not a
/// truncated one — the notice is driven by the row that came back, not by the
/// count matching the flag.
#[test]
fn ev_with_an_explicit_limit_that_cuts_names_the_cut_on_stderr_and_keeps_stdout_exact() {
    const NOTICE: &str =
        "events: showing 3 of more than 3; pass --limit above 3 for the rest (ceiling 1000000)";
    let fixture = Fixture::new("ev-explicit-cut");
    fixture.ok_json(&fixture.main, &["init", "--name", "EVCUT", "--json"]);
    // `init` writes one event and each `task add` writes one more: five.
    for index in 0..4 {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                &format!("row {index}"),
                "--id",
                &format!("t-{index}"),
                "--json",
            ],
        );
    }
    let total = fixture
        .ok_json(&fixture.main, &["events", "--limit", "1000", "--json"])
        .as_array()
        .unwrap()
        .len();
    assert_eq!(total, 5, "the seed did not leave exactly five events");

    let cut = fixture.run(&fixture.main, &["events", "--limit", "3", "--json"]);
    assert!(
        cut.status.success(),
        "a cut page exited non-zero: {}",
        String::from_utf8_lossy(&cut.stderr)
    );
    let rows: Value = serde_json::from_slice(&cut.stdout).unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        3,
        "stdout carried something other than the three rows asked for: {rows}"
    );
    let cut_stderr = String::from_utf8_lossy(&cut.stderr).to_string();
    assert!(
        cut_stderr.contains(NOTICE),
        "the cut was not named on stderr: {cut_stderr}"
    );
    assert!(
        !String::from_utf8_lossy(&cut.stdout).contains("showing 3 of more than 3"),
        "the notice leaked into the parsed payload"
    );
    assert_eq!(
        cut_stderr
            .lines()
            .filter(|line| line.contains("showing"))
            .count(),
        1,
        "the cut was named more than once: {cut_stderr}"
    );

    // The text form is the same contract: rows on stdout, notice on stderr.
    let cut_text = fixture.run(&fixture.main, &["events", "--limit", "3"]);
    assert!(cut_text.status.success());
    assert!(
        String::from_utf8_lossy(&cut_text.stderr).contains(NOTICE),
        "the text form did not name the cut"
    );
    assert!(
        !String::from_utf8_lossy(&cut_text.stdout).contains("showing 3 of more than 3"),
        "the notice reached the text payload"
    );

    // Exactly the whole history, and one more than it: nothing was cut, so
    // nothing is said.
    for limit in ["5", "6"] {
        let whole = fixture.run(&fixture.main, &["events", "--limit", limit, "--json"]);
        assert!(whole.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&whole.stdout)
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            5,
            "--limit {limit} did not return the whole history"
        );
        let stderr = String::from_utf8_lossy(&whole.stderr).to_string();
        assert!(
            stderr.is_empty(),
            "--limit {limit} cut nothing and said something anyway: {stderr}"
        );
    }
}

/// Raising the ceiling changed no default and softened no refusal.
///
/// Read bottom-up: each case here is a refusal that a "limits are bigger now"
/// change is most likely to have relaxed by accident — the ADR-037 default
/// refusals, verbatim, and the unknown-flag refusal on a listing that takes
/// no `--limit` at all.
#[test]
fn defaults_and_default_refusals_are_unchanged_by_the_ceiling() {
    let fixture = Fixture::new("ceiling-defaults");
    fixture.ok_json(&fixture.main, &["init", "--name", "DEFAULTS", "--json"]);

    // One lane each, because a lane keeps only its ten newest current.
    for index in 0..21 {
        fixture.ok_json(
            &fixture.main,
            &[
                "sitrep",
                "post",
                &format!("sitrep {index}"),
                "--as",
                "agent",
                "--lane",
                &format!("lane-{index}"),
                "--json",
            ],
        );
    }
    let sitreps = fixture.run(&fixture.main, &["sitrep", "list", "--json"]);
    assert_eq!(
        refusal_object(&sitreps),
        "found more than 20 sitreps and no --limit was given — a page cut at the default would \
         read as the whole; pass --limit N, above 20 to see more or exactly 20 to take the \
         first 20 knowingly",
        "the sitrep default refusal changed wording"
    );
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["sitrep", "list", "--limit", "20", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        20,
        "the sitrep default is no longer 20"
    );

    // `init` and the sitreps have already written events; top up to 51 so the
    // events default of 50 is the thing that is exceeded.
    let mut events = fixture
        .ok_json(&fixture.main, &["events", "--limit", "1000", "--json"])
        .as_array()
        .unwrap()
        .len();
    let mut index = 0;
    while events < 51 {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                &format!("filler {index}"),
                "--id",
                &format!("t-fill-{index}"),
                "--json",
            ],
        );
        index += 1;
        events = fixture
            .ok_json(&fixture.main, &["events", "--limit", "1000", "--json"])
            .as_array()
            .unwrap()
            .len();
    }
    let bare_events = fixture.run(&fixture.main, &["events", "--json"]);
    assert_eq!(
        refusal_object(&bare_events),
        "found more than 50 events and no --limit was given — a page cut at the default would \
         read as the whole; pass --limit N, above 50 to see more or exactly 50 to take the \
         first 50 knowingly",
        "the events default refusal changed wording"
    );

    // `task list` never took `--limit`, and a bigger ceiling is no reason for
    // it to start: the refusal still names what it does take.
    let task_list = fixture.run(&fixture.main, &["task", "list", "--limit", "5"]);
    assert!(!task_list.status.success(), "task list accepted --limit");
    let stderr = String::from_utf8_lossy(&task_list.stderr).to_string();
    assert!(stderr.contains("unknown flag --limit"), "{stderr}");
    assert!(stderr.contains("accepted here:"), "{stderr}");
    assert!(stderr.contains("--with-claims"), "{stderr}");
}

/// A follow that fetches nothing per poll would sit there reporting nothing,
/// so `--follow --limit 0` is refused before the loop starts. The ceiling
/// moved the top of the band; the bottom of it is unchanged.
#[test]
fn watch_follow_still_refuses_a_zero_limit() {
    let fixture = Fixture::new("watch-zero-follow");
    fixture.ok_json(&fixture.main, &["init", "--name", "ZEROFOLLOW", "--json"]);
    let refused = fixture.run(
        &fixture.main,
        &[
            "watch", "--cursor", "0", "--follow", "--limit", "0", "--json",
        ],
    );
    assert_eq!(
        refusal_object(&refused),
        "--follow requires --limit to be at least 1"
    );

    // Without `--follow`, zero is a legitimate "ask for nothing".
    let drained = fixture.run(
        &fixture.main,
        &["watch", "--cursor", "0", "--limit", "0", "--json"],
    );
    assert!(
        drained.status.success(),
        "a non-follow zero limit was refused: {}",
        String::from_utf8_lossy(&drained.stderr)
    );
}

/// One enum-valued argument, for the ADR-008 property below. Adding one is a
/// row here; the test refuses to pass until every argument the binary
/// publishes with a closed set has one.
struct EnumArgument {
    label: &'static str,
    /// The `schema --json` operation name the argument belongs to.
    operation: &'static str,
    /// The flag name (without `--`) or the positional name.
    argument: &'static str,
    /// Whether `argument` is a positional (`task move`'s `status`).
    positional: bool,
    /// Stand up whatever the command needs to reach the enum validation; the
    /// returned string substitutes `@ctx@` in `argv`.
    prepare: fn(&Fixture) -> String,
    /// A full invocation whose enum argument is the `@bogus@` token. The test
    /// swaps in a deliberately bad value, so the only refusal is the enum.
    argv: &'static [&'static str],
}

fn seed_story(fixture: &Fixture) -> String {
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "gated story",
            "--type",
            "story",
            "--id",
            "s-1",
            "--json",
        ],
    );
    String::new()
}

const ENUM_ARGUMENTS: &[EnumArgument] = &[
    EnumArgument {
        label: "checkpoint-state",
        operation: "checkpoint",
        argument: "state",
        positional: false,
        prepare: seed_task_and_lease,
        argv: &[
            "checkpoint",
            "t-1",
            "--lease",
            "@ctx@",
            "--as",
            "agent",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--state",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "task-add-type",
        operation: "task add",
        argument: "type",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "add", "titled", "--type", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-add-status",
        operation: "task add",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "add", "titled", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-list-status",
        operation: "task list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["task", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "task-move-status",
        operation: "task move",
        argument: "status",
        positional: true,
        prepare: seed_nothing,
        argv: &["task", "move", "t-1", "@bogus@", "--as", "agent", "--json"],
    },
    EnumArgument {
        label: "note-kind",
        operation: "note",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "note", "t-1", "body", "--as", "agent", "--kind", "@bogus@", "--json",
        ],
    },
    EnumArgument {
        label: "attention-raise-kind",
        operation: "attention raise",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "attention",
            "raise",
            "needs a look",
            "--as",
            "agent",
            "--kind",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "sprint-list-status",
        operation: "sprint list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["sprint", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "attention-list-kind",
        operation: "attention list",
        argument: "kind",
        positional: false,
        prepare: seed_nothing,
        argv: &["attention", "list", "--kind", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "attention-list-status",
        operation: "attention list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["attention", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-start-tier",
        operation: "deploy start",
        argument: "tier",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy",
            "start",
            "--repo",
            "r",
            "--commit",
            // A real commit, because the identity is resolved before the
            // board is opened: a malformed one would be refused by
            // `DeployIdentity::parse` and this row would prove nothing about
            // --tier.
            "1111111111111111111111111111111111111111",
            "--tier",
            "@bogus@",
            "--environment",
            "e",
            "--host",
            "h",
            "--url",
            "u",
            "--as",
            "agent",
            "--json",
        ],
    },
    EnumArgument {
        label: "deploy-list-tier",
        operation: "deploy list",
        argument: "tier",
        positional: false,
        prepare: seed_nothing,
        argv: &["deploy", "list", "--tier", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-list-status",
        operation: "deploy list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["deploy", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "deploy-finish-result",
        operation: "deploy finish",
        argument: "result",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy", "finish", "d-1", "--token", "x", "--result", "@bogus@", "--phase", "build",
            "--as", "agent", "--json",
        ],
    },
    EnumArgument {
        label: "deploy-finish-phase",
        operation: "deploy finish",
        argument: "phase",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "deploy",
            "finish",
            "d-1",
            "--token",
            "x",
            "--result",
            "succeeded",
            "--phase",
            "@bogus@",
            "--as",
            "agent",
            "--json",
        ],
    },
    EnumArgument {
        label: "handoff-create-reason",
        operation: "handoff create",
        argument: "reason",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "handoff",
            "create",
            "--as",
            "agent",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--reason",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "handoff-list-status",
        operation: "handoff list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["handoff", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "subscription-add-relation",
        operation: "subscription add",
        argument: "relation",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--relation",
            "@bogus@:id",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-add-prior-status",
        operation: "subscription add",
        argument: "prior-status",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--prior-status",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-add-current-status",
        operation: "subscription add",
        argument: "current-status",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "subscription",
            "add",
            "--consumer",
            "c",
            "--action",
            "a",
            "--timeout-ms",
            "100",
            "--max-retries",
            "1",
            "--rate-per-minute",
            "60",
            "--max-concurrency",
            "1",
            "--as",
            "agent",
            "--current-status",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "subscription-list-status",
        operation: "subscription list",
        argument: "status",
        positional: false,
        prepare: seed_nothing,
        argv: &["subscription", "list", "--status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "story-advance-to",
        operation: "story advance",
        argument: "to",
        positional: false,
        prepare: seed_story,
        argv: &[
            "story", "advance", "s-1", "--as", "agent", "--to", "@bogus@", "--json",
        ],
    },
    EnumArgument {
        label: "watch-relation",
        operation: "watch",
        argument: "relation",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--relation", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "watch-prior-status",
        operation: "watch",
        argument: "prior-status",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--prior-status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "watch-current-status",
        operation: "watch",
        argument: "current-status",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--current-status", "@bogus@", "--json"],
    },
    EnumArgument {
        label: "access-grant-capability",
        operation: "access grant",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "grant",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--as",
            "geoyws",
            "--reason",
            "r",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-revoke-capability",
        operation: "access revoke",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "revoke",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--as",
            "geoyws",
            "--reason",
            "r",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-explain-capability",
        operation: "access explain",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &[
            "access",
            "explain",
            "--principal",
            "p",
            "--capability",
            "@bogus@",
            "--scope",
            "registry",
            "--json",
        ],
    },
    EnumArgument {
        label: "attention-resolve-outcome",
        operation: "attention resolve",
        argument: "outcome",
        positional: false,
        prepare: seed_open_attention,
        argv: &[
            "attention",
            "resolve",
            "@ctx@",
            "--as",
            "geoyws",
            "--choice",
            "custom",
            "--note",
            "a note the custom answer requires",
            "--outcome",
            "@bogus@",
            "--json",
        ],
    },
    EnumArgument {
        label: "access-audit-capability",
        operation: "access audit",
        argument: "capability",
        positional: false,
        prepare: seed_nothing,
        argv: &["access", "audit", "--capability", "@bogus@", "--json"],
    },
    EnumArgument {
        // The steer slice's `--note-kind` filter (WATCH-05): validated before
        // the stream starts, so a bogus kind is refused without blocking.
        label: "watch-note-kind",
        operation: "watch",
        argument: "note-kind",
        positional: false,
        prepare: seed_nothing,
        argv: &["watch", "--note-kind", "@bogus@", "--cursor", "0", "--json"],
    },
];

/// ADR-008: an enum-valued refusal names the whole set it accepts, so the
/// caller reads the exit status and the fix in one message. The values are
/// read back out of `schema --json`, which is the same surface an adapter
/// validates against (ADR-010), so a set the manifest advertises but the
/// refusal omits fails here — and an argument the binary publishes with a
/// closed set but no table row fails the completeness check above the loop.
///
/// Read bottom-up: the refusal is asserted against a deliberately bad value,
/// which is the one input every silent "invalid X" of the old shape answered
/// with no hint of what would work.
#[test]
fn every_enum_argument_refusal_names_the_whole_set() {
    let fixture = Fixture::new("enum-surface");
    fixture.ok_json(&fixture.main, &["init", "--name", "ENUM", "--json"]);
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);

    // (operation, argument, positional) -> the accepted values, projected from
    // the manifest. This is the "join the table or fail" guard: every argument
    // the binary publishes with a closed set must have a row, and every row
    // must be published, so the two cannot drift in either direction.
    let mut schema_values: BTreeMap<(String, String, bool), Vec<String>> = BTreeMap::new();
    for operation in schema["operations"].as_array().unwrap() {
        let name = operation["name"].as_str().unwrap().to_owned();
        for flag in operation["flags"].as_array().unwrap() {
            if let Some(values) = flag.get("values").and_then(Value::as_array) {
                schema_values.insert(
                    (
                        name.clone(),
                        flag["name"].as_str().unwrap().to_owned(),
                        false,
                    ),
                    values
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect(),
                );
            }
        }
        if let Some(positionals) = operation.get("positionalValues").and_then(Value::as_object) {
            for (positional, values) in positionals {
                schema_values.insert(
                    (name.clone(), positional.clone(), true),
                    values
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_owned())
                        .collect(),
                );
            }
        }
    }
    for arg in ENUM_ARGUMENTS {
        let key = (
            arg.operation.to_owned(),
            arg.argument.to_owned(),
            arg.positional,
        );
        assert!(
            schema_values.contains_key(&key),
            "{}: no schema --json row publishes values for {}.{}",
            arg.label,
            arg.operation,
            arg.argument
        );
        let values = schema_values.get(&key).unwrap();
        assert!(
            !values.is_empty(),
            "{}: schema --json publishes an empty set",
            arg.label
        );
    }
    for key in schema_values.keys() {
        let (operation, argument, positional) = key;
        assert!(
            ENUM_ARGUMENTS.iter().any(|arg| {
                arg.operation == operation
                    && arg.argument == argument
                    && arg.positional == *positional
            }),
            "schema --json publishes values for {operation} {argument} but no ENUM_ARGUMENTS row proves its refusal names them"
        );
    }
    drop(fixture);

    for arg in ENUM_ARGUMENTS {
        let fixture = Fixture::new(&format!("enum-{}", arg.label));
        fixture.ok_json(&fixture.main, &["init", "--name", "ENUM", "--json"]);
        let context = (arg.prepare)(&fixture);
        let bogus = "bogus-value";
        let argv: Vec<String> = arg
            .argv
            .iter()
            .map(|token| match *token {
                "@ctx@" => context.clone(),
                "@bogus@" => bogus.to_owned(),
                _ => (*token).to_owned(),
            })
            .collect();
        let borrowed = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let refused = fixture.run(&fixture.main, &borrowed);
        let message = refusal_object(&refused);
        let values = schema_values
            .get(&(
                arg.operation.to_owned(),
                arg.argument.to_owned(),
                arg.positional,
            ))
            .unwrap();
        for value in values {
            assert!(
                message.contains(value.as_str()),
                "{}: the refusal omits {value:?}: {message}",
                arg.label
            );
        }
    }
}
