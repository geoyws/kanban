//! Compiled-binary E2E: archive, deployments, retired workspaces, completion
//! gates, sprints, model restriction, complaints, claim routing and deploy tiers.
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
fn compiled_binary_archives_settled_history_without_deleting_it() {
    let fixture = Fixture::new("archive");
    fixture.ok_json(&fixture.main, &["init", "--name", "Archive", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Old closed work",
            "--id",
            "t-old",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-old",
            "durable evidence",
            "--as",
            "worker",
            "--kind",
            "evidence",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-old", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-old",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "worker",
            "--state",
            "done",
            "--summary",
            "finished",
            "--intent",
            "close it",
            "--next-action",
            "none",
            "--json",
        ],
    );
    let attention = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Review old work",
            "--as",
            "worker",
            "--kind",
            "review",
            "--task",
            "t-old",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            attention["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--note",
            "accepted",
            "--json",
        ],
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-old'",
            [],
        )
        .unwrap();

    let preview = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--dry-run",
            "--json",
        ],
    );
    assert_eq!(preview["dryRun"], true);
    assert_eq!(preview["tasks"], 1);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"])[0]["id"],
        "t-old",
        "a dry run changed the board"
    );

    let archived = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(archived["tasks"], 1);
    assert_eq!(archived["notes"], 1);
    assert_eq!(archived["checkpoints"], 1);
    assert_eq!(archived["attention"], 1);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        json!([])
    );
    let all = fixture.ok_json(&fixture.main, &["task", "list", "--all", "--json"]);
    assert_eq!(all[0]["id"], "t-old");
    assert_eq!(all[0]["archived"], true);
    assert!(all[0]["archivedAt"].as_i64().is_some());

    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--status", "resolved", "--json"]
        ),
        json!([])
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "list",
                "--status",
                "resolved",
                "--all",
                "--json"
            ]
        )[0]["archived"],
        true
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["events", "--task", "t-old", "--json"]),
        json!([])
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["events", "--task", "t-old", "--all", "--json"]
            )
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["archived"] == true)
    );

    let rewrite = fixture.run(
        &fixture.main,
        &[
            "task", "move", "t-old", "todo", "--as", "operator", "--json",
        ],
    );
    assert!(
        !rewrite.status.success(),
        "archived history was silently reactivated"
    );
    assert!(
        String::from_utf8_lossy(&rewrite.stderr).contains("archived history"),
        "the refusal did not explain the archival boundary"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true,
        "archival changed immutable audit history"
    );

    let database = Connection::open(&board).unwrap();
    let index_sql: String = database
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_tasks_status_priority'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(index_sql.contains("WHERE archived=0"));
}

#[test]
fn compiled_binary_tracks_verified_deployments_and_self_archives_only_non_current_history() {
    let fixture = Fixture::new("deployment-archive");
    fixture.ok_json(&fixture.main, &["init", "--name", "Deployments", "--json"]);

    let start = |operation: &str, commit: &str| {
        fixture.ok_json(
            &fixture.main,
            &[
                "deploy",
                "start",
                "--repo",
                "geoyws/kanban",
                "--commit",
                commit,
                "--tier",
                "@_p",
                "--environment",
                "production",
                "--host",
                "hax",
                "--url",
                "https://kb.geoy.ws",
                "--operation-id",
                operation,
                "--as",
                "codex@e2e",
                "--json",
            ],
        )
    };
    let first = start("deploy-e2e-1", "1111111111111111111111111111111111111111");
    let first_id = first["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &first_id,
            "--token",
            first["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "1111111111111111111111111111111111111111",
            "--receipt",
            "served first",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );

    let current = start("deploy-e2e-2", "2222222222222222222222222222222222222222");
    let current_id = current["id"].as_str().unwrap().to_owned();
    let mismatch = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &current_id,
            "--token",
            current["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "3333333333333333333333333333333333333333",
            "--receipt",
            "mismatch",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    assert!(
        !mismatch.status.success(),
        "a mismatching served commit was accepted"
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &current_id,
            "--token",
            current["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            "2222222222222222222222222222222222222222",
            "--receipt",
            "served current",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    let replay = start("deploy-e2e-2", "2222222222222222222222222222222222222222");
    assert_eq!(replay["id"], current_id);
    assert_eq!(replay["idempotentReplay"], true);
    assert_eq!(replay["capabilityToken"], current["capabilityToken"]);

    let active = start(
        "deploy-e2e-active",
        "4444444444444444444444444444444444444444",
    );
    let active_id = active["id"].as_str().unwrap().to_owned();
    let failed = start(
        "deploy-e2e-failed",
        "5555555555555555555555555555555555555555",
    );
    let failed_id = failed["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &failed_id,
            "--token",
            failed["capabilityToken"].as_str().unwrap(),
            "--result",
            "failed",
            "--phase",
            "publish",
            "--receipt",
            "registry refused",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );

    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let database = Connection::open(&board).unwrap();
    database.execute("UPDATE deployments SET created_at=1,updated_at=1,completed_at=CASE WHEN status='started' THEN NULL ELSE 1 END", []).unwrap();
    database
        .execute(
            "UPDATE deployments SET created_at=2,updated_at=2,completed_at=2 WHERE id=?",
            [&current_id],
        )
        .unwrap();

    let projected = fixture.ok_json(&fixture.main, &["deploy", "current", "--json"]);
    assert_eq!(projected.as_array().unwrap().len(), 1);
    assert_eq!(projected[0]["id"], current_id);

    let archived = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(
        archived["deployments"], 2,
        "only superseded success and old failure should leave hot storage"
    );
    let hot = fixture.ok_json(&fixture.main, &["deploy", "list", "--json"]);
    assert_eq!(hot.as_array().unwrap().len(), 2);
    assert!(
        hot.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == current_id && row["archived"] == false)
    );
    assert!(
        hot.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == active_id && row["status"] == "started")
    );
    let all = fixture.ok_json(&fixture.main, &["deploy", "list", "--all", "--json"]);
    assert_eq!(all.as_array().unwrap().len(), 4);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == first_id && row["archived"] == true)
    );
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == failed_id && row["archived"] == true)
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["search", &first_id, "--json"])["results"],
        json!([]),
        "archived deployment documents leaked into the hot search corpus"
    );
    let cold_search = fixture.ok_json(&fixture.main, &["search", &first_id, "--all", "--json"]);
    assert!(
        cold_search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["title"] == format!("deployment: {first_id}") && row["archived"] == true
            })
    );
    let hot_search = fixture.ok_json(&fixture.main, &["search", &current_id, "--json"]);
    assert!(hot_search["results"].as_array().unwrap().iter().any(|row| {
        row["title"] == format!("deployment: {current_id}") && row["archived"] == false
    }));

    let repeated = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(
        repeated["deployments"], 0,
        "the self-archive sweep must be idempotent"
    );
    let index_sql: String = database.query_row(
        "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_deployments_hot_target'",
        [], |row| row.get(0),
    ).unwrap();
    assert!(index_sql.contains("WHERE archived=0"));
}

/// The two retained legacy images px `t-15121f7e` has to restore: one known
/// by its Docker config/image ID, one by the manifest digest a registry
/// served. Distinct values, so a test that compared the wrong pair would say
/// so.
const RECOVERY_API_IMAGE_ID: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";

const RECOVERY_WEB_MANIFEST: &str =
    "sha256:2222222222222222222222222222222222222222222222222222222222222222";

const RECOVERY_WEB_IMAGE_ID: &str =
    "sha256:3333333333333333333333333333333333333333333333333333333333333333";

const RECOVERY_CHECKOUT: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// Open one artifact-identity attempt for two roles, with the deployer's own
/// checkout recorded beside — never as — the build commit.
fn start_recovery_deployment(fixture: &Fixture, environment: &str) -> Value {
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/legacy-stack",
            "--artifact",
            &format!("api=docker-image-id:{RECOVERY_API_IMAGE_ID}"),
            "--artifact",
            &format!("web=oci-manifest-digest:{RECOVERY_WEB_MANIFEST}"),
            "--build-commit",
            "unknown",
            "--deployer-checkout",
            RECOVERY_CHECKOUT,
            "--tier",
            "@_p",
            "--environment",
            environment,
            "--host",
            "hax",
            "--url",
            "https://legacy.geoy.ws",
            "--as",
            "codex@e2e",
            "--json",
        ],
    )
}

/// `deploy finish` for a recovery attempt, with whatever `--observed` tokens
/// the caller wants to try.
fn finish_recovery_deployment<'a>(
    fixture: &'a Fixture,
    id: &'a str,
    token: &'a str,
    observed: &[&'a str],
) -> Output {
    let mut args = vec![
        "deploy",
        "finish",
        id,
        "--token",
        token,
        "--result",
        "succeeded",
        "--phase",
        "verification",
        "--receipt",
        "pulled both images on the tier and read their identities",
        "--as",
        "codex@e2e",
        "--json",
    ];
    for token in observed {
        args.push("--observed");
        args.push(token);
    }
    fixture.run(&fixture.main, &args)
}

/// The whole point of ADR-043, through the compiled binary: an image whose
/// build commit nobody knows can be recorded as a verified release of the
/// ARTIFACT, with the missing provenance stated in words rather than filled
/// in with a plausible SHA.
#[test]
fn a_recovery_deploy_records_typed_artifact_identities_and_an_unknown_build_commit() {
    let fixture = Fixture::new("deploy-recovery");
    fixture.ok_json(&fixture.main, &["init", "--name", "RECOVERY", "--json"]);
    let started = start_recovery_deployment(&fixture, "production");
    let id = started["id"].as_str().unwrap().to_owned();
    let token = started["capabilityToken"].as_str().unwrap().to_owned();

    assert_eq!(started["identityMode"], "artifact");
    assert_eq!(started["buildCommit"], "unknown");
    assert_eq!(
        started["buildCommitLabel"],
        "build commit unknown - recovered by artifact identity"
    );
    // The deployer's checkout is recorded and is NOT the build commit: the
    // whole failure mode this row exists to prevent is one being read as the
    // other.
    assert_eq!(started["deployerCheckout"], RECOVERY_CHECKOUT);
    assert_ne!(started["buildCommit"], started["deployerCheckout"]);
    assert_eq!(
        started["artifacts"],
        json!([
            {"role": "api", "kind": "docker-image-id",
             "expected": RECOVERY_API_IMAGE_ID, "observed": null},
            {"role": "web", "kind": "oci-manifest-digest",
             "expected": RECOVERY_WEB_MANIFEST, "observed": null},
        ])
    );

    let finished = finish_recovery_deployment(
        &fixture,
        &id,
        &token,
        &[
            &format!("api=docker-image-id:{RECOVERY_API_IMAGE_ID}"),
            &format!("web=oci-manifest-digest:{RECOVERY_WEB_MANIFEST}"),
        ],
    );
    assert!(
        finished.status.success(),
        "a matching recovery finish was refused: {}",
        String::from_utf8_lossy(&finished.stderr)
    );

    // Every projection a reader takes says the same thing.
    for row in [
        fixture.ok_json(&fixture.main, &["deploy", "show", &id, "--json"]),
        fixture.ok_json(&fixture.main, &["deploy", "current", "--json"])[0].clone(),
        fixture.ok_json(&fixture.main, &["deploy", "list", "--json"])[0].clone(),
    ] {
        assert_eq!(row["id"], id.as_str());
        assert_eq!(row["status"], "succeeded");
        assert_eq!(row["identityMode"], "artifact");
        assert_eq!(row["buildCommit"], "unknown");
        assert_eq!(
            row["buildCommitLabel"],
            "build commit unknown - recovered by artifact identity"
        );
        // No Git commit was proved, so none is presented as served.
        assert_eq!(row["servedCommit"], Value::Null);
        assert_eq!(
            row["artifacts"],
            json!([
                {"role": "api", "kind": "docker-image-id",
                 "expected": RECOVERY_API_IMAGE_ID, "observed": RECOVERY_API_IMAGE_ID},
                {"role": "web", "kind": "oci-manifest-digest",
                 "expected": RECOVERY_WEB_MANIFEST, "observed": RECOVERY_WEB_MANIFEST},
            ])
        );
    }

    // The literal is in storage too, so nothing reconstructs it on read and
    // no row carries a SHA nobody proved.
    let board = board_path_for_project(&fixture, &fixture.main, "RECOVERY");
    let (identity_mode, commit, deployer_checkout): (String, String, String) =
        Connection::open(&board)
            .unwrap()
            .query_row(
                "SELECT identity_mode,commit_sha,deployer_checkout FROM deployments WHERE id=?",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(identity_mode, "artifact");
    assert_eq!(commit, "unknown");
    assert_eq!(deployer_checkout, RECOVERY_CHECKOUT);

    // And the schema publishes the flags an adapter needs to reach this path.
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let flags = |operation: &str| {
        schema["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == operation)
            .unwrap_or_else(|| panic!("{operation} is not published"))["flags"]
            .as_array()
            .unwrap()
            .clone()
    };
    for (operation, flag, kind) in [
        ("deploy start", "artifact", "list"),
        ("deploy start", "build-commit", "value"),
        ("deploy start", "deployer-checkout", "value"),
        ("deploy finish", "observed", "list"),
    ] {
        let published = flags(operation)
            .into_iter()
            .find(|row| row["name"] == flag)
            .unwrap_or_else(|| panic!("{operation} does not publish --{flag}"));
        assert_eq!(published["kind"], kind, "{operation} --{flag}");
    }
}

/// Each way an observation can fail to be the expected identity, refused by
/// name with both values, through the compiled binary.
#[test]
fn a_recovery_finish_refuses_a_missing_role_a_kind_mismatch_and_a_value_mismatch_by_name() {
    let fixture = Fixture::new("deploy-recovery-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "REFUSE", "--json"]);
    let started = start_recovery_deployment(&fixture, "production");
    let id = started["id"].as_str().unwrap().to_owned();
    let token = started["capabilityToken"].as_str().unwrap().to_owned();
    let api = format!("api=docker-image-id:{RECOVERY_API_IMAGE_ID}");
    let web = format!("web=oci-manifest-digest:{RECOVERY_WEB_MANIFEST}");

    // 1. A role the attempt expects, and nothing observed for it.
    assert_eq!(
        refusal_object(&finish_recovery_deployment(&fixture, &id, &token, &[&api])),
        format!(
            "deployment {id} expects artifact role web (oci-manifest-digest \
             {RECOVERY_WEB_MANIFEST}) and --observed named no identity for it"
        )
    );

    // 2. A role the attempt does not expect.
    assert_eq!(
        refusal_object(&finish_recovery_deployment(
            &fixture,
            &id,
            &token,
            &[
                &api,
                &web,
                &format!("worker=docker-image-id:{RECOVERY_WEB_IMAGE_ID}"),
            ],
        )),
        format!(
            "deployment {id} does not expect artifact role worker; \
             its expected roles are api, web"
        )
    );

    // 3. The config ID of the web image offered where the manifest digest
    //    was expected: the two kinds are different numbers of different
    //    things, so this is refused on the kind and never compared.
    assert_eq!(
        refusal_object(&finish_recovery_deployment(
            &fixture,
            &id,
            &token,
            &[
                &api,
                &format!("web=docker-image-id:{RECOVERY_WEB_IMAGE_ID}")
            ],
        )),
        format!(
            "deployment {id} artifact role web expects oci-manifest-digest \
             {RECOVERY_WEB_MANIFEST} but --observed offered docker-image-id \
             {RECOVERY_WEB_IMAGE_ID}; a docker-image-id and an oci-manifest-digest are \
             never compared to each other"
        )
    );

    // 4. The right kind, the wrong image.
    assert_eq!(
        refusal_object(&finish_recovery_deployment(
            &fixture,
            &id,
            &token,
            &[
                &format!("api=docker-image-id:{RECOVERY_WEB_IMAGE_ID}"),
                &web,
            ],
        )),
        format!(
            "deployment {id} artifact role api expects docker-image-id \
             {RECOVERY_API_IMAGE_ID} but --observed offered docker-image-id \
             {RECOVERY_WEB_IMAGE_ID}"
        )
    );

    // Four refusals later the attempt is still open and unproved: nothing a
    // refused finish touched became a release.
    let row = fixture.ok_json(&fixture.main, &["deploy", "show", &id, "--json"]);
    assert_eq!(row["status"], "started");
    assert_eq!(row["artifacts"][0]["observed"], Value::Null);
    assert_eq!(row["artifacts"][1]["observed"], Value::Null);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["deploy", "current", "--json"]),
        json!([]),
        "a refused finish must not publish a current release"
    );

    // And the same attempt still finishes when the identities do match, so
    // the refusals above are about the observation and not about the row.
    assert!(
        finish_recovery_deployment(&fixture, &id, &token, &[&api, &web])
            .status
            .success()
    );
}

/// One mode per attempt, named: neither mode accepts the other's proof, at
/// `start` or at `finish`.
#[test]
fn artifact_mode_and_git_mode_refuse_each_others_flags() {
    let fixture = Fixture::new("deploy-identity-modes");
    fixture.ok_json(&fixture.main, &["init", "--name", "MODES", "--json"]);
    let full_sha = "1111111111111111111111111111111111111111";
    let artifact = format!("api=docker-image-id:{RECOVERY_API_IMAGE_ID}");
    let start = |extra: &[&str]| {
        let mut args = vec![
            "deploy",
            "start",
            "--repo",
            "geoyws/legacy-stack",
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://legacy.geoy.ws",
            "--as",
            "codex@e2e",
            "--json",
        ];
        args.extend_from_slice(extra);
        fixture.run(&fixture.main, &args)
    };

    assert_eq!(
        refusal_object(&start(&["--commit", full_sha, "--artifact", &artifact])),
        "deploy: --commit names a Git-mode attempt and --artifact names an \
         artifact-identity attempt; one mode per attempt, so pass one or the other"
    );
    assert_eq!(
        refusal_object(&start(&["--commit", full_sha, "--build-commit", "unknown"])),
        "deploy: --build-commit \"unknown\" belongs to artifact mode; the Git path names \
         its commit with --commit FULL_SHA"
    );
    assert_eq!(
        refusal_object(&start(&["--artifact", &artifact])),
        "deploy: artifact mode requires --build-commit unknown, so a missing build commit \
         is stated rather than defaulted"
    );
    assert_eq!(
        refusal_object(&start(&[
            "--artifact",
            &artifact,
            "--build-commit",
            full_sha
        ])),
        format!(
            "deploy: --build-commit {full_sha} is a full Git commit, so this build's \
             provenance is known; use the Git mode with --commit {full_sha} instead of \
             --artifact"
        )
    );
    assert_eq!(
        refusal_object(&start(&[])),
        "deploy start requires --commit FULL_SHA (the verified Git path) or --artifact \
         ROLE=KIND:VALUE with --build-commit unknown (the artifact-identity recovery path)"
    );

    // At finish: a digest-only success may not be dressed as a verified Git
    // commit, and a Git attempt may not be proved by a digest.
    let recovery = start_recovery_deployment(&fixture, "recovery");
    let recovery_id = recovery["id"].as_str().unwrap().to_owned();
    let served = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &recovery_id,
            "--token",
            recovery["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            full_sha,
            "--receipt",
            "the bundle was served",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&served),
        format!(
            "deployment {recovery_id} was started in artifact-identity mode, where no Git \
             commit was proved; --served-commit cannot be recorded for it — verify it with \
             --observed ROLE=KIND:VALUE for every expected role"
        )
    );

    let git = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/legacy-stack",
            "--commit",
            full_sha,
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://legacy.geoy.ws",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    let git_id = git["id"].as_str().unwrap().to_owned();
    let observed = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            &git_id,
            "--token",
            git["capabilityToken"].as_str().unwrap(),
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--served-commit",
            full_sha,
            "--observed",
            &artifact,
            "--receipt",
            "the bundle was served",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&observed),
        format!(
            "deployment {git_id} was started in Git mode; --observed belongs to \
             artifact-identity mode — verify it with --served-commit FULL_SHA"
        )
    );
}

/// The existing path, unchanged: a Git-mode attempt still proves itself with
/// its served commit, still refuses a mismatching one, and carries no
/// artifact identity at all.
#[test]
fn the_git_deploy_path_is_unchanged_by_artifact_mode() {
    let fixture = Fixture::new("deploy-git-unchanged");
    fixture.ok_json(&fixture.main, &["init", "--name", "GITPATH", "--json"]);
    let full_sha = "1111111111111111111111111111111111111111";
    let started = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            full_sha,
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--operation-id",
            "git-path-1",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    let id = started["id"].as_str().unwrap().to_owned();
    let token = started["capabilityToken"].as_str().unwrap().to_owned();
    assert_eq!(started["identityMode"], "git");
    assert_eq!(started["buildCommit"], full_sha);
    assert_eq!(started["buildCommitLabel"], full_sha);
    assert_eq!(started["deployerCheckout"], Value::Null);
    assert_eq!(started["artifacts"], json!([]));

    let finish = |served: &str| {
        fixture.run(
            &fixture.main,
            &[
                "deploy",
                "finish",
                &id,
                "--token",
                &token,
                "--result",
                "succeeded",
                "--phase",
                "verification",
                "--served-commit",
                served,
                "--receipt",
                "the served bundle carried the exact release",
                "--as",
                "codex@e2e",
                "--json",
            ],
        )
    };
    assert_eq!(
        refusal_object(&finish("2222222222222222222222222222222222222222")),
        "served commit must exactly match the requested deployment commit"
    );
    assert!(finish(full_sha).status.success());

    let row = fixture.ok_json(&fixture.main, &["deploy", "show", &id, "--json"]);
    assert_eq!(row["status"], "succeeded");
    assert_eq!(row["identityMode"], "git");
    assert_eq!(row["buildCommit"], full_sha);
    assert_eq!(row["servedCommit"], full_sha);
    assert_eq!(
        row["artifacts"],
        json!([]),
        "a Git attempt carries no artifact identity"
    );
    // The idempotent replay still answers with the same attempt, identity
    // included, rather than treating the new mode fields as a difference.
    let replay = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            full_sha,
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--operation-id",
            "git-path-1",
            "--as",
            "codex@e2e",
            "--json",
        ],
    );
    assert_eq!(replay["id"], id.as_str());
    assert_eq!(replay["idempotentReplay"], true);
    assert_eq!(replay["identityMode"], "git");
}

#[test]
fn compiled_binary_lists_retired_rootless_boards_once_in_workspace_list_all() {
    let fixture = Fixture::new("rootless-retire-list");
    let rootless = fixture.root.join("rootless");
    fs::create_dir_all(&rootless).unwrap();

    let created = fixture.ok_json(
        &rootless,
        &["init", "--name", "ROOTLESS", "--rootless", "--json"],
    );
    let rootless_path = created["boardPath"].as_str().unwrap().to_owned();

    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ROOTLESS",
            "--as",
            "geoyws",
            "--note",
            "retire rootless board",
            "--json",
        ],
    );
    assert_eq!(retired["archived"], true);

    let active_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ROOTLESS"),
        "the retired rootless board leaked into the default inventory: {active_list}"
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "ROOTLESS")
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        1,
        "retired rootless board duplicated in all inventory"
    );
    let row = rows[0];
    assert_eq!(row["archived"], true);
    assert_eq!(row["rootless"], true);
    assert_eq!(row["rootPath"], "");
    assert_eq!(row["boardPath"], rootless_path);

    let restored = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "unretire",
            "ROOTLESS",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(restored["archived"], false);
    assert!(
        restored["workspaceRoots"].as_array().unwrap().is_empty(),
        "unretiring a rootless board should not fabricate roots"
    );

    let restored_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        restored_list
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "ROOTLESS" && row["rootless"] == true),
        "unretiring the rootless board did not restore the active inventory: {restored_list}"
    );
}

#[test]
fn compiled_binary_keeps_rootless_retired_boards_visible_after_previous_detach_history() {
    let fixture = Fixture::new("rootless-retire-history");
    let project = fixture.root.join("project");
    fs::create_dir_all(&project).unwrap();

    let created = fixture.ok_json(&project, &["init", "--name", "ROOTLESS-HISTORY", "--json"]);
    let board_path = created["boardPath"].as_str().unwrap().to_owned();
    let root_path = created["workspaceRoots"][0]
        .as_str()
        .expect("rootless history root")
        .to_owned();

    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &root_path,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ROOTLESS-HISTORY",
            "--as",
            "geoyws",
            "--note",
            "retire after detach",
            "--json",
        ],
    );
    assert_eq!(retired["archived"], true);
    assert!(
        retired["workspaceRoots"].as_array().unwrap().is_empty(),
        "retiring a rootless board must not invent roots"
    );

    let active_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ROOTLESS-HISTORY"),
        "the retired rootless board leaked into the default inventory"
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let rootless_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["boardPath"] == board_path && row["rootless"] == true)
        .collect::<Vec<_>>();
    assert_eq!(
        rootless_rows.len(),
        1,
        "retired rootless board with prior history disappeared from all inventory"
    );
    let row = rootless_rows[0];
    assert_eq!(row["archived"], true);
    assert_eq!(row["rootPath"], "");
    assert_eq!(row["boardPath"], board_path);
}

#[test]
fn compiled_binary_keeps_same_name_retired_boards_distinct_by_path() {
    let fixture = Fixture::new("retired-same-name");
    let first = fixture.root.join("first");
    let second = fixture.root.join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();

    let first_created = fixture.ok_json(&first, &["init", "--name", "SAME", "--json"]);
    let first_path = first_created["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "SAME",
            "--as",
            "geoyws",
            "--note",
            "retire first same-name board",
            "--json",
        ],
    );

    let second_created = fixture.ok_json(&second, &["init", "--name", "SAME", "--json"]);
    let second_path = second_created["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "SAME",
            "--as",
            "geoyws",
            "--note",
            "retire second same-name board",
            "--json",
        ],
    );

    let all_list = fixture.ok_json(&fixture.main, &["workspace", "list", "--all", "--json"]);
    let same_name_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "SAME")
        .collect::<Vec<_>>();
    assert_eq!(
        same_name_rows.len(),
        2,
        "same-name retired boards were deduped"
    );
    let mut paths = same_name_rows
        .iter()
        .map(|row| row["boardPath"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    paths.sort();
    let mut expected = vec![first_path, second_path];
    expected.sort();
    assert_eq!(paths, expected);
    assert!(
        same_name_rows.iter().all(|row| row["archived"] == true),
        "retired same-name boards must remain archived in the all inventory"
    );
}

#[test]
fn the_mcp_server_rejects_retired_direct_board_paths_over_stdio() {
    let fixture = Fixture::new("mcp-retired-db");
    let active = fixture.root.join("active");
    let retired = fixture.root.join("retired");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&retired).unwrap();

    fixture.ok_json(&active, &["init", "--name", "MCP", "--json"]);
    let retired_board = fixture.ok_json(&retired, &["init", "--name", "RETIRED", "--json"]);
    let retired_path = retired_board["boardPath"].as_str().unwrap().to_owned();
    fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "retire",
            "RETIRED",
            "--as",
            "geoyws",
            "--note",
            "retire MCP board",
            "--json",
        ],
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let refused = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "task_list", "arguments": { "db": retired_path } }
    }));
    assert_eq!(refused["result"]["isError"], true);
    let text = refused["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("retire MCP board"), "{text}");

    session.finish();
}

#[test]
fn hax_registry_requires_rule_retirement_before_retiring_a_named_board() {
    let fixture = Fixture::new("active-rule-selector-retirement");
    let px = fixture.root.join("px");
    let kanban = fixture.root.join("kanban");
    fs::create_dir_all(&px).unwrap();
    fs::create_dir_all(&kanban).unwrap();
    fixture.ok_json(&px, &["init", "--name", "px", "--json"]);
    fixture.ok_json(&kanban, &["init", "--name", "kanban", "--json"]);
    let only = fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "PX host rule.",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let except = fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "All hosts except PX.",
            "--except-board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.root,
        &[
            "rule",
            "add",
            "Kanban host rule.",
            "--board",
            "kanban",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let only_id = only["id"].as_str().unwrap();
    let except_id = except["id"].as_str().unwrap();
    let mut blocker_ids = [only_id, except_id];
    blocker_ids.sort_unstable();

    let refused = fixture.run(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "px",
            "--as",
            "geoyws",
            "--note",
            "split host registries",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "workspace retirement left active named selectors behind"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("ONLY:px"), "{stderr}");
    assert!(stderr.contains("EXCEPT:px"), "{stderr}");
    assert!(
        stderr.contains(&format!(
            "blocking rule IDs: {}, {}",
            blocker_ids[0], blocker_ids[1]
        )),
        "{stderr}"
    );
    assert!(stderr.contains("update or retire those rules"), "{stderr}");
    let still_active = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        still_active
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == false),
        "refused retirement mutated the board"
    );
    assert!(
        fixture
            .ok_json(
                &fixture.root,
                &[
                    "events",
                    "--registry",
                    "--kind",
                    "workspace_retired",
                    "--json",
                ],
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "refused retirement appended an audit event"
    );

    for id in [only_id, except_id] {
        fixture.ok_json(
            &fixture.root,
            &["rule", "retire", id, "--as", "geoyws", "--json"],
        );
    }
    let retired_board = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "px",
            "--as",
            "geoyws",
            "--note",
            "split host registries",
            "--json",
        ],
    );
    assert_eq!(retired_board["archived"], true);

    let all_rules = fixture.ok_json(
        &fixture.root,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(all_rules.as_array().unwrap().iter().any(|rule| {
        rule["id"] == only["id"]
            && rule["archived"] == true
            && rule["body"] == "PX host rule."
            && rule["tags"] == json!(["ONLY:px"])
    }));
    let shown = fixture.ok_json(&fixture.root, &["rule", "show", only_id, "--json"]);
    assert_eq!(shown["body"], "PX host rule.");
    assert_eq!(shown["tags"], json!(["ONLY:px"]));
    let rule_events = fixture.ok_json(&fixture.root, &["events", "--rule", only_id, "--json"]);
    assert!(
        rule_events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "rule_retired")
    );

    let all_workspaces = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert!(
        all_workspaces
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == true)
    );
    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--all", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
    assert_eq!(doctor["activeRuleSelectors"]["healthy"], true);
    assert!(
        doctor["activeRuleSelectors"]["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        doctor["projects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "px" && row["archived"] == true)
    );
}

#[test]
fn doctor_reports_stale_active_selectors_without_blocking_rule_history() {
    let fixture = Fixture::new("doctor-active-rule-selectors");
    fixture.ok_json(&fixture.main, &["init", "--name", "px", "--json"]);
    let rule = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Recoverable rule.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let rule_id = rule["id"].as_str().unwrap();
    {
        let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
        registry
            .execute(
                "UPDATE rules SET tags='[\"ONLY:unum\",\"ONLY:unum\"]' WHERE id=?",
                [rule_id],
            )
            .unwrap();
    }

    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor certified a stale active selector"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert_eq!(report["healthy"], false);
    assert_eq!(report["activeRuleSelectors"]["healthy"], false);
    assert_eq!(
        report["activeRuleSelectors"]["errors"],
        json!([{
            "ruleId": rule_id,
            "selector": "ONLY:unum",
            "activeBoardCount": 0
        }])
    );

    let listed = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(listed.as_array().unwrap().iter().any(|row| {
        row["id"] == rule["id"] && row["tags"] == json!(["ONLY:unum", "ONLY:unum"])
    }));
    let shown = fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"]);
    assert_eq!(shown["tags"], json!(["ONLY:unum", "ONLY:unum"]));

    let update = fixture.run(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--body",
            "An edit must not preserve a stale active selector.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !update.status.success(),
        "an active-rule edit retained a stale selector"
    );
    let stderr = String::from_utf8_lossy(&update.stderr);
    assert!(stderr.contains(rule_id), "{stderr}");
    assert!(stderr.contains("selector ONLY:unum"), "{stderr}");
    assert!(stderr.contains("found 0"), "{stderr}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"])["body"],
        "Recoverable rule.",
        "refused update changed the active rule"
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", rule_id, "--as", "geoyws", "--json"],
    );
    let recovered = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(recovered["healthy"], true, "{recovered}");
    assert_eq!(recovered["activeRuleSelectors"]["healthy"], true);
}

#[test]
fn retiring_and_unretiring_a_workspace_hides_it_by_default_and_rolls_back_conflicts() {
    let fixture = Fixture::new("workspace-retire-unretire");
    let alpha = fixture.root.join("alpha");
    let alpha_spare = fixture.root.join("alpha-spare");
    let beta = fixture.root.join("beta");
    let alpha_nested = alpha.join("nested");
    fs::create_dir_all(&alpha).unwrap();
    fs::create_dir_all(&alpha_spare).unwrap();
    fs::create_dir_all(&beta).unwrap();
    fs::create_dir_all(&alpha_nested).unwrap();
    let alpha_spare = alpha_spare.canonicalize().unwrap();

    let alpha_registered = fixture.ok_json(&alpha, &["init", "--name", "ALPHA", "--json"]);
    let alpha_root = alpha_registered["workspaceRoots"][0]
        .as_str()
        .expect("alpha root")
        .to_owned();
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "attach",
            "--to",
            "ALPHA",
            "--workspace",
            alpha_spare.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            alpha_spare.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &alpha,
        &[
            "task",
            "add",
            "Retired needle 77",
            "--id",
            "t-retired-77",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(&beta, &["init", "--name", "BETA", "--json"]);
    fixture.ok_json(
        &beta,
        &[
            "task",
            "add",
            "BETA wrong-board sentinel",
            "--id",
            "t-retired-77",
            "--json",
        ],
    );
    let retirement_note = "moved-to-hig";

    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ALPHA",
            "--as",
            "geoyws",
            "--note",
            retirement_note,
            "--json",
        ],
    );
    assert_eq!(retired["name"], "ALPHA");
    assert_eq!(retired["archivedBy"], "geoyws");
    assert_eq!(retired["archivedNote"], retirement_note);
    assert_eq!(retired["workspaceRoots"], json!([alpha_root.clone()]));
    assert!(retired["archivedAt"].as_i64().is_some());
    let retired_path = retired["boardPath"].as_str().unwrap().to_owned();

    let with_env = |key: &str, value: &str, args: &[&str]| -> Output {
        fixture
            .command(&fixture.root)
            .env(key, value)
            .args(args)
            .output()
            .unwrap()
    };

    let direct_write = fixture.run(
        &fixture.root,
        &[
            "task",
            "add",
            "Blocked by retirement",
            "--db",
            &retired_path,
            "--json",
        ],
    );
    assert!(
        !direct_write.status.success(),
        "a retired board path still answered writes"
    );
    let direct_write_stderr = String::from_utf8_lossy(&direct_write.stderr).into_owned();
    assert!(
        direct_write_stderr.contains(retirement_note),
        "{direct_write_stderr}"
    );

    let direct_watch = fixture.run(
        &fixture.root,
        &["watch", "--db", &retired_path, "--limit", "0", "--json"],
    );
    assert!(
        !direct_watch.status.success(),
        "a retired board path still answered watch"
    );
    let direct_watch_stderr = String::from_utf8_lossy(&direct_watch.stderr).into_owned();
    assert!(
        direct_watch_stderr.contains(retirement_note),
        "{direct_watch_stderr}"
    );

    let env_list = with_env("KANBAN_DB", &retired_path, &["task", "list", "--json"]);
    assert!(
        !env_list.status.success(),
        "KANBAN_DB still answered from a retired board"
    );
    let env_list_stderr = String::from_utf8_lossy(&env_list.stderr).into_owned();
    assert!(
        env_list_stderr.contains(retirement_note),
        "{env_list_stderr}"
    );

    let explicit_workspace = fixture.run(
        &beta,
        &[
            "task",
            "show",
            "t-retired-77",
            "--workspace",
            &alpha_root,
            "--json",
        ],
    );
    let env_project = fixture
        .command(&beta)
        .env("KANBAN_PROJECT", "ALPHA")
        .args(["task", "show", "t-retired-77", "--json"])
        .output()
        .unwrap();
    let readonly_workspace = fixture.run(
        &beta,
        &["subscription", "list", "--workspace", &alpha_root, "--json"],
    );
    for (label, selector, output) in [
        (
            "explicit --workspace",
            alpha_root.as_str(),
            explicit_workspace,
        ),
        ("KANBAN_PROJECT", "ALPHA", env_project),
        (
            "read-only subscription --workspace",
            alpha_root.as_str(),
            readonly_workspace,
        ),
    ] {
        assert!(
            !output.status.success(),
            "{label} fell through to the active BETA board"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(selector),
            "{label} did not identify {selector}: {stderr}"
        );
        assert!(
            stderr.contains(retirement_note),
            "{label} omitted the recorded retirement note: {stderr}"
        );
        // The refusal is the only thing on stdout: no answer from BETA rides
        // along with it.
        assert!(
            refusal_object(&output).contains(selector),
            "{label} returned a wrong-board answer while refusing: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    let active_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        active_list
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default workspace list"
    );
    let all_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    let retired_rows = all_list
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["boardPath"] == retired_path && row["rootless"] == true)
        .collect::<Vec<_>>();
    assert_eq!(
        retired_rows.len(),
        0,
        "retired rooted board still gained a rootless summary row"
    );
    let retired_row = all_list
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["boardPath"] == retired_path && row["rootPath"] == alpha_root)
        .expect("archived ALPHA retirement row");
    assert_eq!(retired_row["archived"], true);
    assert_eq!(retired_row["archivedBy"], "geoyws");
    assert_eq!(retired_row["archivedNote"], retirement_note);
    assert_eq!(retired_row["rootless"], false);

    let dashboard = fixture.ok_json(&fixture.root, &["dashboard", "--json"]);
    assert!(
        dashboard
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default dashboard"
    );
    let dashboard_all = fixture.ok_json(&fixture.root, &["dashboard", "--all", "--json"]);
    let dashboard_alpha = dashboard_all
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ALPHA")
        .expect("archived ALPHA dashboard row");
    assert_eq!(dashboard_alpha["archived"], true);
    assert_eq!(dashboard_alpha["archivedNote"], retirement_note);

    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--json"]);
    assert!(
        doctor["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "ALPHA"),
        "retired board leaked into the default doctor report"
    );
    let doctor_all = fixture.ok_json(&fixture.root, &["doctor", "--all", "--json"]);
    let doctor_alpha = doctor_all["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "ALPHA")
        .expect("archived ALPHA doctor row");
    assert_eq!(doctor_alpha["archived"], true);
    assert_eq!(doctor_alpha["archivedBy"], "geoyws");
    assert_eq!(doctor_alpha["archivedNote"], retirement_note);

    let search_default = fixture.ok_json(
        &fixture.root,
        &["search", "Retired needle 77", "--all-boards", "--json"],
    );
    assert!(
        search_default["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["board"] != "ALPHA"),
        "default all-board search inspected a retired board"
    );
    let rebuilt_default = fixture.ok_json(
        &fixture.root,
        &["search-rebuild", "--all-boards", "--as", "geoyws", "--json"],
    );
    assert!(
        rebuilt_default["reports"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["board"] != "ALPHA"),
        "default all-board search rebuild inspected a retired board"
    );
    let search_all = fixture.ok_json(
        &fixture.root,
        &[
            "search",
            "Retired needle 77",
            "--all-boards",
            "--all",
            "--json",
        ],
    );
    let search_alpha = search_all["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["board"] == "ALPHA")
        .expect("archived ALPHA search result");
    assert_eq!(search_alpha["board"], "ALPHA");

    let denied_name = fixture.run(
        &beta,
        &[
            "task",
            "add",
            "Blocked by retirement",
            "--id",
            "t-retired-write",
            "--project",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !denied_name.status.success(),
        "a retired board name was still writable"
    );
    assert!(
        String::from_utf8_lossy(&denied_name.stderr).contains(retirement_note),
        "{}",
        String::from_utf8_lossy(&denied_name.stderr)
    );
    let denied_name_stderr = String::from_utf8_lossy(&denied_name.stderr);
    assert!(denied_name_stderr.contains("ALPHA"), "{denied_name_stderr}");
    assert!(refusal_object(&denied_name).contains("ALPHA"));
    let beta_tasks = fixture.ok_json(&beta, &["task", "list", "--json"]);
    assert_eq!(beta_tasks.as_array().unwrap().len(), 1);
    assert_eq!(beta_tasks[0]["id"], "t-retired-77");
    assert_eq!(beta_tasks[0]["title"], "BETA wrong-board sentinel");

    let denied_root = fixture.run(&alpha_nested, &["task", "show", "t-retired-77", "--json"]);
    assert!(
        !denied_root.status.success(),
        "a retired root still resolved"
    );
    assert!(
        String::from_utf8_lossy(&denied_root.stderr).contains(retirement_note),
        "{}",
        String::from_utf8_lossy(&denied_root.stderr)
    );
    let denied_root_stderr = String::from_utf8_lossy(&denied_root.stderr);
    assert!(denied_root_stderr.contains("ALPHA"), "{denied_root_stderr}");
    assert!(refusal_object(&denied_root).contains("ALPHA"));

    fixture.ok_json(
        &beta,
        &[
            "workspace",
            "attach",
            "--to",
            "BETA",
            "--workspace",
            &alpha_root,
            "--json",
        ],
    );
    let conflict = fixture.run(
        &fixture.root,
        &["workspace", "unretire", "ALPHA", "--as", "geoyws", "--json"],
    );
    assert!(
        !conflict.status.success(),
        "a conflicting unretire was accepted"
    );
    let conflict_stderr = String::from_utf8_lossy(&conflict.stderr).into_owned();
    assert!(
        conflict_stderr.contains("cannot be unretired"),
        "{conflict_stderr}"
    );
    let failed_unretire_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_unretired",
            "--json",
        ],
    );
    assert!(
        failed_unretire_events.as_array().unwrap().is_empty(),
        "a failed unretire wrote an audit event"
    );

    fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &alpha_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let restored = fixture.ok_json(
        &fixture.root,
        &["workspace", "unretire", "ALPHA", "--as", "geoyws", "--json"],
    );
    assert_eq!(restored["name"], "ALPHA");
    assert_eq!(restored["workspaceRoots"], json!([alpha_root.clone()]));
    assert!(restored.get("archivedAt").is_none());
    assert!(restored.get("archivedBy").is_none());
    assert!(restored.get("archivedNote").is_none());

    let restored_list = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        restored_list
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "ALPHA" && row["archived"] == false),
        "unretire did not restore the default workspace list"
    );
    let restored_search = fixture.ok_json(
        &fixture.root,
        &["search", "Retired needle 77", "--all-boards", "--json"],
    );
    assert!(
        restored_search["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["board"] == "ALPHA" && row["archived"] == false),
        "unretire did not restore all-board search access"
    );
    assert_eq!(
        fixture.ok_json(&alpha_nested, &["task", "show", "t-retired-77", "--json"])["id"],
        "t-retired-77",
        "unretire did not restore the retired workspace path"
    );

    let retired_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_retired",
            "--json",
        ],
    );
    assert_eq!(retired_events.as_array().unwrap().len(), 1);
    assert_eq!(retired_events[0]["actor"], "geoyws");
    assert!(
        !retired_events[0]["payload"]["retirementId"]
            .as_str()
            .expect("retirement id")
            .is_empty()
    );
    assert_eq!(
        retired_events[0]["payload"]["archivedNote"],
        retirement_note
    );
    let unretired_events = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_unretired",
            "--json",
        ],
    );
    assert_eq!(unretired_events.as_array().unwrap().len(), 1);
    assert_eq!(unretired_events[0]["actor"], "geoyws");
    assert_eq!(
        retired_events[0]["payload"]["retirementId"],
        unretired_events[0]["payload"]["retirementId"]
    );
    assert_eq!(
        unretired_events[0]["payload"]["restoredRoots"],
        json!([alpha_root])
    );
}

#[test]
fn retired_direct_db_refuses_when_registry_is_corrupt_or_stale_but_external_db_without_registry_still_works()
 {
    let fixture = Fixture::new("retired-direct-db-boundary");
    let managed = fixture.root.join("managed");
    let workspace = managed.join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    let created = fixture.ok_json(&workspace, &["init", "--name", "ALPHA", "--json"]);
    assert_eq!(created["name"], "ALPHA");
    let retired = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "retire",
            "ALPHA",
            "--as",
            "geoyws",
            "--note",
            "moved-to-hig",
            "--json",
        ],
    );
    let retired_path = retired["boardPath"].as_str().unwrap().to_owned();
    let registry_source = fixture.data.join("registry.db");

    let corrupt_root = fixture.root.join("corrupt-data");
    fs::create_dir_all(&corrupt_root).unwrap();
    let corrupt_registry = corrupt_root.join("registry.db");
    fs::copy(&registry_source, &corrupt_registry).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&corrupt_registry)
        .unwrap()
        .set_len(32)
        .unwrap();

    let corrupt_flag = fixture
        .command_with_data_dir(&fixture.root, &corrupt_root)
        .args(["task", "list", "--db", &retired_path, "--json"])
        .output()
        .unwrap();
    assert!(
        !corrupt_flag.status.success(),
        "a corrupt registry still allowed a retired direct board path"
    );

    let corrupt_env = fixture
        .command_with_data_dir(&fixture.root, &corrupt_root)
        .env("KANBAN_DB", &retired_path)
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        !corrupt_env.status.success(),
        "a corrupt registry still allowed KANBAN_DB on a retired board"
    );

    let stale_root = fixture.root.join("stale-data");
    fs::create_dir_all(&stale_root).unwrap();
    let stale_registry = stale_root.join("registry.db");
    fs::copy(&registry_source, &stale_registry).unwrap();
    let connection = Connection::open(&stale_registry).unwrap();
    connection.execute_batch("PRAGMA user_version=11;").unwrap();

    let stale_flag = fixture
        .command_with_data_dir(&fixture.root, &stale_root)
        .args(["task", "list", "--db", &retired_path, "--json"])
        .output()
        .unwrap();
    assert!(
        !stale_flag.status.success(),
        "a stale registry still allowed a retired direct board path"
    );

    let stale_env = fixture
        .command_with_data_dir(&fixture.root, &stale_root)
        .env("KANBAN_DB", &retired_path)
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    assert!(
        !stale_env.status.success(),
        "a stale registry still allowed KANBAN_DB on a retired board"
    );

    let external_root = fixture.root.join("external-data");
    fs::create_dir_all(&external_root).unwrap();
    let external_db = fixture.root.join("external.db");
    let external_added = fixture
        .command_with_data_dir(&fixture.root, &external_root)
        .args([
            "task",
            "add",
            "External control task",
            "--id",
            "t-external",
            "--db",
            external_db.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        external_added.status.success(),
        "a truly external direct board file should stay usable without a registry"
    );
    let external_added_json: Value = serde_json::from_slice(&external_added.stdout).unwrap();
    assert_eq!(external_added_json["id"], "t-external");

    let external_list = fixture
        .command_with_data_dir(&fixture.root, &external_root)
        .args([
            "task",
            "list",
            "--db",
            external_db.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        external_list.status.success(),
        "a truly external direct board file should stay usable without a registry"
    );
    let external_list_json: Value = serde_json::from_slice(&external_list.stdout).unwrap();
    assert!(
        external_list_json
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == "t-external"),
        "the external control board lost its task after a successful list"
    );
}

#[test]
fn a_listing_says_whether_each_task_is_held_and_by_whom() {
    // Measured 2026-09-04 across eight boards: `task list --status
    // in_progress` carried no claim key at all, so 32 leased tasks read as
    // free and "no such claim exists anywhere" was reported twice before
    // `task show` on the same ids turned up live leases. A listing that
    // structurally cannot show a lease sits on the surface an agent uses to
    // decide whether work is free to take, and its absence renders as an
    // answer. Every row now says whether it is held; --with-claims says by
    // whom, in the shape `task show` already emits.
    let fixture = Fixture::new("listed-claims");
    fixture.ok_json(&fixture.main, &["init", "--name", "HELD", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Held", "--id", "t-held", "--json"],
    );
    // The free task carries an assignee, which is an inviting wrong answer
    // to "who holds it": on the px board a task read assignee=driver-3 while
    // the lease sat elsewhere. Assignee is intent; the claim is possession.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Wanted",
            "--id",
            "t-free",
            "--assignee",
            "driver-3",
            "--json",
        ],
    );
    let lease = fixture.ok_json(
        &fixture.main,
        &["claim", "t-held", "--as", "driver-2", "--json"],
    );
    assert_eq!(lease["agentID"], "driver-2");

    let row = |rows: &Value, id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} is not in {rows}"))
            .clone()
    };

    // The question as it is naturally asked: which in-progress work is held?
    let in_progress = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--status", "in_progress", "--json"],
    );
    let held = row(&in_progress, "t-held");
    assert_eq!(held["claimed"], true, "{held}");
    assert!(
        held.get("claim").is_none(),
        "the full summary is opt-in through --with-claims: {held}"
    );

    let with_claims = fixture.ok_json(&fixture.main, &["task", "list", "--with-claims", "--json"]);
    let held = row(&with_claims, "t-held");
    assert_eq!(held["claimed"], true, "{held}");
    assert_eq!(held["claim"]["agentID"], "driver-2", "{held}");
    assert_eq!(held["claim"]["taskID"], "t-held", "{held}");
    assert_eq!(held["claim"]["expiresAt"], lease["expiresAt"], "{held}");
    assert_eq!(held["claim"]["claimedAt"], lease["claimedAt"], "{held}");
    assert!(
        held["claim"].get("leaseToken").is_none(),
        "a listing never carries the token that authorizes writes: {held}"
    );
    let free = row(&with_claims, "t-free");
    assert_eq!(free["claimed"], false, "{free}");
    assert_eq!(free["claim"], Value::Null, "{free}");
    assert_eq!(
        free["assignee"], "driver-3",
        "assignee is intent and stays its own field: {free}"
    );

    // `claimed` is a row key like any other, so it projects.
    let projected = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--fields", "claimed,id", "--json"],
    );
    let mut seen = Vec::new();
    for row in projected.as_array().unwrap() {
        let keys = row.as_object().unwrap().keys().collect::<Vec<_>>();
        assert_eq!(keys, ["claimed", "id"], "exactly the keys asked for");
        seen.push((
            row["id"].as_str().unwrap().to_owned(),
            row["claimed"].as_bool().unwrap(),
        ));
    }
    seen.sort();
    assert_eq!(
        seen,
        [("t-free".to_owned(), false), ("t-held".to_owned(), true)]
    );

    // The wrong field names for the holder are refused where a name is typed,
    // naming the keys that exist. Reading `claim.actor` off the JSON would
    // yield null on every task including live ones, indistinguishable from
    // "no holder"; the binary cannot refuse a missing-key read, so it refuses
    // the name at the only place it sees one.
    for wrong in ["actor", "claim.actor"] {
        let refused = fixture.run(
            &fixture.main,
            &["task", "list", "--with-claims", "--fields", wrong, "--json"],
        );
        assert!(!refused.status.success(), "--fields {wrong} was accepted");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(wrong), "{stderr}");
        assert!(stderr.contains("claimed, claim"), "{stderr}");
    }
    // Without the flag `claim` is not on the row, and the refusal says which
    // flag puts it there rather than listing keys that omit it (ADR-008).
    let refused = fixture.run(
        &fixture.main,
        &["task", "list", "--fields", "id,claim", "--json"],
    );
    assert!(
        !refused.status.success(),
        "--fields claim was accepted without --with-claims"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--with-claims"), "{stderr}");

    // Releasing the lease is visible on the next listing; nothing is cached.
    let released = fixture.run(
        &fixture.main,
        &[
            "release",
            "t-held",
            "--lease",
            lease["leaseToken"].as_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        released.status.success(),
        "{}",
        String::from_utf8_lossy(&released.stderr)
    );
    let after = fixture.ok_json(&fixture.main, &["task", "list", "--with-claims", "--json"]);
    let held = row(&after, "t-held");
    assert_eq!(held["claimed"], false, "{held}");
    assert_eq!(held["claim"], Value::Null, "{held}");

    // --help is where a caller who never read the docs learns the flag, and
    // the one place to say which key is the holder and that assignee is not.
    let help = fixture.run(&fixture.main, &["task", "list", "--help"]);
    let usage = String::from_utf8_lossy(&help.stdout);
    assert!(usage.contains("--with-claims"), "{usage}");
    assert!(usage.contains("claim.agentID"), "{usage}");
}

/// Where this board's SQLite file lives, for the raw counts below.
fn completion_gate_board_path(fixture: &Fixture) -> PathBuf {
    fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .into()
}

/// Every event on the board, archived history included. A refused completion
/// gate must leave this number exactly where it was: a partial write that
/// appends a receipt for work it then refuses is what these tests are here to
/// catch.
fn completion_gate_event_count(board: &Path) -> i64 {
    Connection::open(board)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .unwrap()
}

/// A completion-gate refusal, checked for the four facts an agent needs in
/// order to have a next move: the row it wanted to work, the prerequisite that
/// is not done, that prerequisite's status, and the row the edge was declared
/// on -- an ancestor when the gate was inherited, the row itself when it was
/// not. Wording is not pinned; the identifiers are.
fn completion_gate_refusal(
    output: &Output,
    affected: &str,
    prerequisite: &str,
    prerequisite_status: &str,
    source: &str,
) -> String {
    let error = refusal_object(output);
    for value in [affected, prerequisite, prerequisite_status, source] {
        assert!(
            error.contains(value),
            "a completion-gate refusal that does not name {value} loses which gate blocked the \
             row: {error}"
        );
    }
    error
}

/// Sorted dependency ids, so an assertion about which direct dependencies a
/// row carries does not also pin the order they happen to be read back in.
/// Accepts both shapes the surfaces emit: bare ids from `task list
/// --with-relations`, full rows from `task show`.
fn dependency_ids(dependencies: &Value) -> Vec<String> {
    let mut ids = dependencies
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| match dependency {
            Value::String(id) => id.clone(),
            row => row["id"].as_str().unwrap().to_owned(),
        })
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// A prerequisite declared on an epic gates every row beneath it, and the
/// blocker a reader is handed says which epic that was. Two levels of plan,
/// each with its own prerequisite, plus two declared on the leaf: the leaf
/// reports all four, attributed, while an unrelated task stays workable.
#[test]
fn completion_gates_report_direct_and_inherited_blockers_on_every_relation_surface() {
    let fixture = Fixture::new("completion-gate-surfaces");
    fixture.ok_json(&fixture.main, &["init", "--name", "GATES", "--json"]);

    // Assigned away so the prerequisites are not themselves candidates, and
    // created b before a so prerequisite-id order is distinguishable from
    // creation order.
    for (id, title) in [
        ("t-root-prereq", "Root prerequisite"),
        ("t-phase-prereq", "Phase prerequisite"),
        ("t-direct-b", "Direct prerequisite B"),
        ("t-direct-a", "Direct prerequisite A"),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                title,
                "--id",
                id,
                "--assignee",
                "maintainer",
                "--priority",
                "9",
                "--json",
            ],
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Root plan",
            "--id",
            "e-root",
            "--type",
            "epic",
            "--depends-on",
            "t-root-prereq",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Phase",
            "--id",
            "e-phase",
            "--type",
            "epic",
            "--parent",
            "e-root",
            "--depends-on",
            "t-phase-prereq",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Leaf with direct and inherited gates",
            "--id",
            "t-leaf",
            "--parent",
            "e-phase",
            "--priority",
            "0",
            "--depends-on",
            "t-direct-b",
            "--depends-on",
            "t-direct-a",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Leaf with inherited gates only",
            "--id",
            "t-inherited-only",
            "--parent",
            "e-phase",
            "--priority",
            "0",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Independent work",
            "--id",
            "t-free",
            "--priority",
            "1",
            "--json",
        ],
    );

    // Nearest owner first, prerequisite id within an owner: the leaf's own
    // two, then the phase's, then the root plan's.
    let expected = json!([
        {
            "sourceTaskID": "t-leaf",
            "prerequisiteID": "t-direct-a",
            "prerequisiteTitle": "Direct prerequisite A",
            "prerequisiteStatus": "todo"
        },
        {
            "sourceTaskID": "t-leaf",
            "prerequisiteID": "t-direct-b",
            "prerequisiteTitle": "Direct prerequisite B",
            "prerequisiteStatus": "todo"
        },
        {
            "sourceTaskID": "e-phase",
            "prerequisiteID": "t-phase-prereq",
            "prerequisiteTitle": "Phase prerequisite",
            "prerequisiteStatus": "todo"
        },
        {
            "sourceTaskID": "e-root",
            "prerequisiteID": "t-root-prereq",
            "prerequisiteTitle": "Root prerequisite",
            "prerequisiteStatus": "todo"
        }
    ]);

    // One computed answer, identical on all three surfaces, and the direct
    // dependency output beside it unchanged: an inherited gate is reported,
    // never folded into the row's own declared edges.
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-leaf", "--json"]);
    assert_eq!(shown["blockingGates"], expected);
    assert_eq!(
        dependency_ids(&shown["dependencies"]),
        ["t-direct-a", "t-direct-b"]
    );
    let context = fixture.ok_json(&fixture.main, &["context", "t-leaf", "--json"]);
    assert_eq!(context["task"]["id"], "t-leaf");
    assert_eq!(context["blockingGates"], expected);

    // The packet a human or a resuming agent actually reads, not only its
    // machine form: both the full render and the compact one an agent gets
    // when the budget is tight must name every unmet prerequisite and the
    // ancestor whose gate it is, or a reader sees an unstartable row with no
    // stated reason. Identifiers are pinned; prose is not.
    let rendered = fixture.run(&fixture.main, &["context", "t-leaf"]);
    assert!(rendered.status.success());
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    for named in [
        "t-direct-a",
        "t-direct-b",
        "t-phase-prereq",
        "e-phase",
        "t-root-prereq",
        "e-root",
    ] {
        assert!(
            rendered.contains(named),
            "the rendered context packet never names {named}: {rendered}"
        );
    }

    // The compact packet only appears when the full one overruns the budget,
    // so give the board a real sitrep to carry the packet past it: the point
    // under test is what compaction keeps, and it must keep the gates.
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Lane status while the leaf waits on its prerequisites, written long \
             enough that the full cold-start packet no longer fits the smallest \
             budget a resuming agent can ask for, which is the only way to reach \
             the compact rendering from the command line at all, and deliberately \
             saying nothing about which rows are gated so the compact gate line \
             cannot pass on this text alone.",
            "--as",
            "operator",
            "--lane",
            "driver",
            "--task",
            "t-leaf",
            "--json",
        ],
    );
    let compact = fixture.run(&fixture.main, &["context", "t-leaf", "--max-chars", "1000"]);
    assert!(compact.status.success());
    let compact = String::from_utf8(compact.stdout).unwrap();
    assert!(
        compact.contains("# Kanban cold-start context (compact)"),
        "the budget did not produce the compact packet, so its gate line went \
         unchecked: {compact}"
    );
    for named in [
        "t-direct-a",
        "t-direct-b",
        "t-phase-prereq",
        "e-phase",
        "t-root-prereq",
        "e-root",
    ] {
        assert!(
            compact.contains(named),
            "the compact packet an agent resumes on drops {named}: {compact}"
        );
    }

    // And the other half of the same contract: a row with nothing unmet says
    // so, so "no gates" is distinguishable from "gates not rendered".
    let free_rendered = fixture.run(&fixture.main, &["context", "t-free"]);
    assert!(free_rendered.status.success());
    let free_rendered = String::from_utf8(free_rendered.stdout).unwrap();
    for unrelated in ["t-direct-a", "t-phase-prereq", "t-root-prereq"] {
        assert!(
            !free_rendered.contains(unrelated),
            "an ungated row was rendered as blocked by {unrelated}: {free_rendered}"
        );
    }
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--with-relations", "--json"],
    );
    let listed_leaf = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-leaf")
        .unwrap();
    assert_eq!(listed_leaf["blockingGates"], expected);
    assert_eq!(
        dependency_ids(&listed_leaf["dependencies"]),
        ["t-direct-a", "t-direct-b"]
    );

    // A row with nothing unmet says so with an empty array, which is the only
    // way a reader can tell "not gated" from "not computed".
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-free", "--json"])["blockingGates"],
        json!([])
    );

    // The key rides on the relations flag exactly as dependencies does, so a
    // caller naming it without the flag is told which flag adds it.
    let missing_flag = fixture.run(
        &fixture.main,
        &["task", "list", "--fields", "id,blockingGates", "--json"],
    );
    assert!(
        !missing_flag.status.success(),
        "--fields blockingGates was accepted without --with-relations"
    );
    let missing_flag_error = String::from_utf8_lossy(&missing_flag.stderr);
    assert!(
        missing_flag_error.contains("blockingGates"),
        "{missing_flag_error}"
    );
    assert!(
        missing_flag_error.contains("--with-relations"),
        "{missing_flag_error}"
    );
    let projected = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "list",
            "--with-relations",
            "--fields",
            "id,dependencies,blockingGates",
            "--json",
        ],
    );
    let projected_leaf = projected
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "t-leaf")
        .unwrap();
    assert_eq!(projected_leaf["blockingGates"], expected);
    assert_eq!(
        dependency_ids(&projected_leaf["dependencies"]),
        ["t-direct-a", "t-direct-b"]
    );

    // Inspection and the scheduler agree, and both withhold the leaf whose
    // only gates are inherited -- it carries no dependency edge of its own.
    let candidates = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "--candidates",
            "--as",
            "driver",
            "--limit",
            "20",
            "--json",
        ],
    );
    let candidate_ids = candidates
        .as_array()
        .unwrap()
        .iter()
        .map(|candidate| candidate["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(candidate_ids, ["t-free"]);
    let named = fixture.run(
        &fixture.main,
        &["claim", "t-inherited-only", "--as", "driver", "--json"],
    );
    let error = completion_gate_refusal(
        &named,
        "t-inherited-only",
        "t-phase-prereq",
        "todo",
        "e-phase",
    );
    assert!(
        error.contains("t-root-prereq") && error.contains("e-root"),
        "the outer plan's gate was dropped from the refusal: {error}"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "driver", "--json"]
        )["taskID"],
        "t-free",
        "the scheduler handed out gated work"
    );

    // The operator surface says how much of the board is waiting on a
    // prerequisite, as its own number: the two gated leaves, while the status
    // counts beside it stay raw -- a gated row still holds the status it
    // holds, so silently moving it out of `todo` would leave the board's own
    // totals not adding up.
    let all_rows = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    let todo_rows = all_rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|task| task["status"] == "todo")
        .count();
    let dashboard = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let board_row = dashboard
        .as_array()
        .unwrap()
        .iter()
        .find(|project| project["taskCounts"].is_object())
        .unwrap_or_else(|| panic!("the dashboard reported no board: {dashboard}"));
    assert_eq!(
        board_row["gatedTasks"], 2,
        "the dashboard lost the count of rows held by a completion gate \
         (t-leaf, t-inherited-only): {board_row}"
    );
    assert_eq!(
        board_row["taskCounts"]["todo"], todo_rows,
        "the status counts were filtered by the gate instead of staying raw: {board_row}"
    );
    assert_eq!(
        board_row["totalTasks"],
        all_rows.as_array().unwrap().len(),
        "{board_row}"
    );
}

/// What counts as satisfied: done, and nothing else. A cancelled prerequisite
/// will never complete, an archived done one still happened, and reopening a
/// completed prerequisite re-gates future work without rewriting the history
/// already recorded against it.
#[test]
fn completion_gates_track_prerequisite_lifecycle_and_cleared_dependencies() {
    let fixture = Fixture::new("completion-gate-lifecycle");
    fixture.ok_json(&fixture.main, &["init", "--name", "LIFECYCLE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Foundation",
            "--id",
            "t-prerequisite",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Dependent work",
            "--id",
            "t-gated",
            "--depends-on",
            "t-prerequisite",
            "--json",
        ],
    );
    let gate_status = |expected: &str| {
        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "show", "t-gated", "--json"])["blockingGates"]
                [0]["prerequisiteStatus"],
            expected
        );
    };
    gate_status("todo");

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-prerequisite",
            "cancelled",
            "--as",
            "operator",
            "--json",
        ],
    );
    gate_status("cancelled");
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &["claim", "t-gated", "--as", "worker", "--json"],
        ),
        "t-gated",
        "t-prerequisite",
        "cancelled",
        "t-gated",
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-prerequisite",
            "done",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-gated", "--json"])["blockingGates"],
        json!([])
    );
    let first = fixture.ok_json(
        &fixture.main,
        &["claim", "t-gated", "--as", "worker", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "release",
            "t-gated",
            "--lease",
            first["leaseToken"].as_str().unwrap(),
            "--json",
        ],
    );

    // Reopened: new work is gated again, and the claim already recorded stays
    // in the ledger -- a gate refuses the future, it does not edit the past.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-prerequisite",
            "todo",
            "--as",
            "operator",
            "--json",
        ],
    );
    gate_status("todo");
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &["claim", "t-gated", "--as", "worker", "--json"],
        ),
        "t-gated",
        "t-prerequisite",
        "todo",
        "t-gated",
    );
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &[
                    "events",
                    "--task",
                    "t-gated",
                    "--kind",
                    "task_claimed",
                    "--json",
                ]
            )
            .as_array()
            .unwrap()
            .len(),
        1,
        "the completed claim history was disturbed by a later gate"
    );

    // Archived done still satisfies: cold storage is where completed work
    // goes, not a way of un-completing it.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-prerequisite",
            "done",
            "--as",
            "operator",
            "--json",
        ],
    );
    let board = completion_gate_board_path(&fixture);
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-prerequisite'",
            [],
        )
        .unwrap();
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "archive",
                "--older-than-days",
                "1",
                "--as",
                "operator",
                "--json",
            ],
        )["tasks"],
        1
    );
    let after_archive = fixture.ok_json(
        &fixture.main,
        &["claim", "t-gated", "--as", "worker", "--json"],
    );
    assert_eq!(after_archive["taskID"], "t-gated");
    fixture.ok_json(
        &fixture.main,
        &[
            "release",
            "t-gated",
            "--lease",
            after_archive["leaseToken"].as_str().unwrap(),
            "--json",
        ],
    );

    // Authoring is the dependency surface that already exists, so withdrawing
    // a gate is --clear-dependencies and nothing new.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "New prerequisite",
            "--id",
            "t-new-prerequisite",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Clearable work",
            "--id",
            "t-clearable",
            "--depends-on",
            "t-new-prerequisite",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-clearable", "--json"])["blockingGates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-clearable",
            "--as",
            "operator",
            "--clear-dependencies",
            "--json",
        ],
    );
    let cleared = fixture.ok_json(&fixture.main, &["task", "show", "t-clearable", "--json"]);
    assert_eq!(
        dependency_ids(&cleared["dependencies"]),
        Vec::<String>::new()
    );
    assert_eq!(cleared["blockingGates"], json!([]));
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-clearable", "--as", "worker", "--json"]
        )["taskID"],
        "t-clearable"
    );
}

/// Accepting a handoff mints a lease, so it meets the same gate claim does --
/// otherwise the route past a completion gate is to hand the task to yourself.
/// The pending handoff survives the refusal: the correspondence is still
/// waiting for whoever finishes the prerequisite.
#[test]
fn completion_gates_apply_to_lease_taking_handoff_acceptance() {
    let fixture = Fixture::new("completion-gate-handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "HANDOFF", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Handoff prerequisite",
            "--id",
            "t-handoff-prerequisite",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Handoff plan",
            "--id",
            "e-handoff",
            "--type",
            "epic",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Handed-off work",
            "--id",
            "t-handoff",
            "--parent",
            "e-handoff",
            "--json",
        ],
    );
    let outgoing = fixture.ok_json(
        &fixture.main,
        &["claim", "t-handoff", "--as", "outgoing", "--json"],
    );
    // The gate arrives on the plan while the work is already held, which is
    // how a board really acquires one: somebody discovers the ordering later.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-handoff",
            "--as",
            "operator",
            "--depends-on",
            "t-handoff-prerequisite",
            "--json",
        ],
    );
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-handoff",
            "--lease",
            outgoing["leaseToken"].as_str().unwrap(),
            "--as",
            "outgoing",
            "--to",
            "incoming",
            "--summary",
            "Paused once the plan grew a prerequisite",
            "--intent",
            "Continue only after it completes",
            "--next-action",
            "Finish t-handoff-prerequisite",
            "--json",
        ],
    );
    let board = completion_gate_board_path(&fixture);
    let before = completion_gate_event_count(&board);
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &[
                "handoff",
                "accept",
                handoff["id"].as_str().unwrap(),
                "--as",
                "incoming",
                "--json",
            ],
        ),
        "t-handoff",
        "t-handoff-prerequisite",
        "todo",
        "e-handoff",
    );
    assert_eq!(
        completion_gate_event_count(&board),
        before,
        "a refused acceptance still wrote to the ledger"
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["handoff", "list", "--status", "pending", "--json"]
            )
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == handoff["id"]),
        "the refused handoff was consumed"
    );
    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-handoff", "--json"]);
    assert!(
        task["claim"].is_null(),
        "a refused acceptance minted a lease"
    );
    assert_eq!(task["status"], "todo");
    assert_eq!(
        task["blockingGates"][0]["sourceTaskID"], "e-handoff",
        "{task}"
    );

    // And the correspondence really is only waiting: once the prerequisite
    // completes, the same handoff id accepts, mints the incoming agent's
    // lease and puts the row into work. Without this, a gate that refused
    // every acceptance for gated ancestry -- forever, prerequisite or not --
    // would read exactly like the refusal above.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-handoff-prerequisite",
            "done",
            "--as",
            "operator",
            "--json",
        ],
    );
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            handoff["id"].as_str().unwrap(),
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert_eq!(accepted["handoff"]["status"], "accepted", "{accepted}");
    assert_eq!(accepted["claim"]["agentID"], "incoming", "{accepted}");
    assert!(
        accepted["claim"]["leaseToken"]
            .as_str()
            .is_some_and(|token| !token.is_empty()),
        "acceptance minted no lease for the incoming agent: {accepted}"
    );
    let resumed = fixture.ok_json(&fixture.main, &["task", "show", "t-handoff", "--json"]);
    assert_eq!(resumed["status"], "in_progress", "{resumed}");
    assert_eq!(resumed["blockingGates"], json!([]), "{resumed}");
}

/// The gate is about doing the work, not only about being handed it: the
/// work-bearing statuses are refused on the verb that creates a row and the
/// verb that moves one, --force included, while the administrative statuses
/// stay available so a board can still record what is true.
#[test]
fn completion_gates_refuse_work_bearing_task_add_and_move_states() {
    let fixture = Fixture::new("completion-gate-task-states");
    fixture.ok_json(&fixture.main, &["init", "--name", "TASK-STATES", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "State prerequisite",
            "--id",
            "t-state-prerequisite",
            "--json",
        ],
    );
    let board = completion_gate_board_path(&fixture);

    for status in ["in_progress", "review", "done"] {
        let id = format!("t-add-{status}");
        let before = completion_gate_event_count(&board);
        completion_gate_refusal(
            &fixture.run(
                &fixture.main,
                &[
                    "task",
                    "add",
                    "Premature work",
                    "--id",
                    &id,
                    "--status",
                    status,
                    "--depends-on",
                    "t-state-prerequisite",
                    "--json",
                ],
            ),
            &id,
            "t-state-prerequisite",
            "todo",
            &id,
        );
        assert_eq!(completion_gate_event_count(&board), before);
        assert!(
            !fixture
                .run(&fixture.main, &["task", "show", &id, "--json"])
                .status
                .success(),
            "{id} was created by a refused add"
        );
    }

    for (id, target, force) in [
        ("t-move-progress", "in_progress", false),
        ("t-move-review", "review", false),
        ("t-move-done", "done", false),
        ("t-move-forced", "done", true),
    ] {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "Movable work",
                "--id",
                id,
                "--depends-on",
                "t-state-prerequisite",
                "--json",
            ],
        );
        let before = completion_gate_event_count(&board);
        let mut args = vec!["task", "move", id, target, "--as", "operator"];
        if force {
            args.push("--force");
        }
        args.push("--json");
        completion_gate_refusal(
            &fixture.run(&fixture.main, &args),
            id,
            "t-state-prerequisite",
            "todo",
            id,
        );
        assert_eq!(completion_gate_event_count(&board), before);
        let unchanged = fixture.ok_json(&fixture.main, &["task", "show", id, "--json"]);
        assert_eq!(unchanged["status"], "todo", "{id} moved anyway");
        assert!(
            unchanged["completedAt"].is_null(),
            "{id} was stamped complete"
        );
    }

    for status in ["blocked", "cancelled"] {
        let id = format!("t-admin-{status}");
        assert_eq!(
            fixture.ok_json(
                &fixture.main,
                &[
                    "task",
                    "add",
                    "Administrative state",
                    "--id",
                    &id,
                    "--status",
                    status,
                    "--depends-on",
                    "t-state-prerequisite",
                    "--json",
                ],
            )["status"],
            status
        );
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Administrative move",
            "--id",
            "t-admin-move",
            "--depends-on",
            "t-state-prerequisite",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "move",
                "t-admin-move",
                "blocked",
                "--as",
                "operator",
                "--json",
            ],
        )["status"],
        "blocked"
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "note",
            "t-admin-move",
            "Waiting on the prerequisite",
            "--as",
            "operator",
            "--json",
        ],
    );
}

/// A story's gate walks through planning before any work exists, so the
/// completion gate starts where the work does: planning to ready is allowed on
/// a gated story, and every step past it is refused with the projection and
/// the metadata left where the last legal step put them.
#[test]
fn completion_gates_stop_a_story_at_the_boundary_between_planning_and_work() {
    let fixture = Fixture::new("completion-gate-story");
    fixture.ok_json(&fixture.main, &["init", "--name", "STORY-GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Story prerequisite",
            "--id",
            "t-story-prerequisite",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Gated story",
            "--id",
            "s-gated",
            "--type",
            "story",
            "--depends-on",
            "t-story-prerequisite",
            "--json",
        ],
    );

    let planned = fixture.ok_json(
        &fixture.main,
        &["story", "advance", "s-gated", "--as", "driver", "--json"],
    );
    assert_eq!(planned["from"], "planning");
    assert_eq!(planned["to"], "ready");

    let board = completion_gate_board_path(&fixture);
    let before_work = completion_gate_event_count(&board);
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &["story", "advance", "s-gated", "--as", "driver", "--json"],
        ),
        "s-gated",
        "t-story-prerequisite",
        "todo",
        "s-gated",
    );
    assert_eq!(completion_gate_event_count(&board), before_work);
    let ready = fixture.ok_json(&fixture.main, &["task", "show", "s-gated", "--json"]);
    assert_eq!(ready["metadata"]["workflowStatus"], "ready");
    assert_eq!(ready["status"], "todo");

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-story-prerequisite",
            "done",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["story", "advance", "s-gated", "--as", "driver", "--json"]
        )["to"],
        "in-progress"
    );

    // Reopened mid-flight: the next step is refused and the story keeps the
    // state it legitimately reached rather than being rolled back.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-story-prerequisite",
            "todo",
            "--as",
            "operator",
            "--json",
        ],
    );
    let before_testing = completion_gate_event_count(&board);
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &["story", "advance", "s-gated", "--as", "driver", "--json"],
        ),
        "s-gated",
        "t-story-prerequisite",
        "todo",
        "s-gated",
    );
    assert_eq!(completion_gate_event_count(&board), before_testing);
    let working = fixture.ok_json(&fixture.main, &["task", "show", "s-gated", "--json"]);
    assert_eq!(working["metadata"]["workflowStatus"], "in-progress");
    assert_eq!(working["status"], "in_progress");
}

/// A gate that appears after the lease was taken does not seize the lease --
/// nothing is torn out from under a working agent. It refuses the next
/// heartbeat and the next progress or completion checkpoint, and leaves the
/// two honest exits open: checkpoint blocked, or release.
#[test]
fn completion_gates_refuse_further_progress_without_revoking_a_live_lease() {
    let fixture = Fixture::new("completion-gate-lease-progress");
    fixture.ok_json(&fixture.main, &["init", "--name", "LEASE-GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Lease prerequisite",
            "--id",
            "t-lease-prerequisite",
            "--status",
            "done",
            "--json",
        ],
    );
    let claim_gated = |id: &str| -> String {
        fixture.ok_json(
            &fixture.main,
            &[
                "task",
                "add",
                "Leased work",
                "--id",
                id,
                "--depends-on",
                "t-lease-prerequisite",
                "--json",
            ],
        );
        fixture.ok_json(&fixture.main, &["claim", id, "--as", "worker", "--json"])["leaseToken"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let heartbeat_token = claim_gated("t-heartbeat");
    let continue_token = claim_gated("t-continue");
    let done_token = claim_gated("t-done");
    let blocked_token = claim_gated("t-blocked");

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-lease-prerequisite",
            "todo",
            "--as",
            "operator",
            "--json",
        ],
    );
    for id in ["t-heartbeat", "t-continue", "t-done", "t-blocked"] {
        let held = fixture.ok_json(&fixture.main, &["task", "show", id, "--json"]);
        assert_eq!(
            held["claim"]["agentID"], "worker",
            "the reopened prerequisite revoked the live lease on {id}"
        );
        assert_eq!(held["status"], "in_progress");
    }

    let board = completion_gate_board_path(&fixture);
    let lease_before =
        fixture.ok_json(&fixture.main, &["task", "show", "t-heartbeat", "--json"])["claim"].clone();
    let before_heartbeat = completion_gate_event_count(&board);
    completion_gate_refusal(
        &fixture.run(
            &fixture.main,
            &[
                "heartbeat",
                "t-heartbeat",
                "--lease",
                &heartbeat_token,
                "--json",
            ],
        ),
        "t-heartbeat",
        "t-lease-prerequisite",
        "todo",
        "t-heartbeat",
    );
    assert_eq!(completion_gate_event_count(&board), before_heartbeat);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-heartbeat", "--json"])["claim"],
        lease_before,
        "a refused renewal still moved the lease"
    );

    for (id, token, state) in [
        ("t-continue", &continue_token, "continue"),
        ("t-done", &done_token, "done"),
    ] {
        let before = completion_gate_event_count(&board);
        completion_gate_refusal(
            &fixture.run(
                &fixture.main,
                &[
                    "checkpoint",
                    id,
                    "--lease",
                    token,
                    "--as",
                    "worker",
                    "--summary",
                    "A prerequisite appeared mid-flight",
                    "--intent",
                    "Do not cross the gate",
                    "--next-action",
                    "Finish t-lease-prerequisite",
                    "--state",
                    state,
                    "--json",
                ],
            ),
            id,
            "t-lease-prerequisite",
            "todo",
            id,
        );
        assert_eq!(completion_gate_event_count(&board), before);
        let task = fixture.ok_json(&fixture.main, &["task", "show", id, "--json"]);
        assert_eq!(task["status"], "in_progress", "{id} was moved by a refusal");
        assert!(task["completedAt"].is_null(), "{id} was stamped complete");
        assert_eq!(task["claim"]["agentID"], "worker", "{id} lost its lease");
        assert!(
            fixture.ok_json(&fixture.main, &["context", id, "--json"])["checkpoints"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{id} persisted a checkpoint the gate refused"
        );
    }

    // The way out stays open, and it is the one that records why.
    let blocked = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-blocked",
            "--lease",
            &blocked_token,
            "--as",
            "worker",
            "--summary",
            "Blocked on a new prerequisite",
            "--intent",
            "Preserve the work for whoever resumes",
            "--next-action",
            "Finish t-lease-prerequisite",
            "--state",
            "blocked",
            "--json",
        ],
    );
    assert_eq!(blocked["state"], "blocked");
    let blocked_task = fixture.ok_json(&fixture.main, &["task", "show", "t-blocked", "--json"]);
    assert_eq!(blocked_task["status"], "blocked");
    assert!(blocked_task["claim"].is_null());

    fixture.ok_json(
        &fixture.main,
        &[
            "release",
            "t-heartbeat",
            "--lease",
            &heartbeat_token,
            "--json",
        ],
    );
    let released = fixture.ok_json(&fixture.main, &["task", "show", "t-heartbeat", "--json"]);
    assert_eq!(released["status"], "todo");
    assert!(released["claim"].is_null());
}

/// Rule `g-74e8d80c`, measured twice on one board on 2026-09-17: a lane
/// stops, names George as the next step, raises no card, and the work waits
/// on a person who was never told while the lane's queue reads empty. The
/// write is refused instead, and the refusal quotes the clause that tripped
/// it and names the card that would reach him.
#[test]
fn a_blocked_checkpoint_that_parks_work_on_the_owner_needs_a_card_first() {
    let fixture = Fixture::new("owner-gate-checkpoint");
    fixture.ok_json(&fixture.main, &["init", "--name", "OWNERGATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Bootstrap the lane",
            "--id",
            "t-park",
            "--json",
        ],
    );
    let token = fixture.ok_json(
        &fixture.main,
        &["claim", "t-park", "--as", "worker", "--json"],
    )["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let blocked = |fixture: &Fixture, token: &str, next: &str| {
        fixture.run(
            &fixture.main,
            &[
                "checkpoint",
                "t-park",
                "--lease",
                token,
                "--as",
                "worker",
                "--state",
                "blocked",
                "--summary",
                "The bootstrap needs a live session",
                "--intent",
                "Preserve the work for whoever resumes",
                "--next-action",
                next,
                "--json",
            ],
        )
    };

    let refusal = refusal_object(&blocked(&fixture, &token, "George re-logins to bootstrap"));
    assert!(
        refusal.contains("\"George re-logins to bootstrap\""),
        "the refusal must quote the clause it matched: {refusal}"
    );
    assert!(
        refusal.contains("kb attention raise") && refusal.contains("--task t-park"),
        "the refusal must name the card that reaches him: {refusal}"
    );

    // A clause that carries its own double quote must not break the sentence
    // in half: one line, and exactly the two quoted spans the sentence owns --
    // the echoed clause and the `<the ask>` placeholder.
    let quoted = refusal_object(&blocked(
        &fixture,
        &token,
        "George runs the \"bootstrap\" script",
    ));
    assert!(
        quoted.contains("\"George runs the 'bootstrap' script\""),
        "the echoed clause must keep the sentence's own quoting intact: {quoted}"
    );
    assert_eq!(quoted.lines().count(), 1, "the refusal split: {quoted}");
    assert_eq!(
        quoted.matches('"').count(),
        4,
        "the refusal has more quoted spans than it owns: {quoted}"
    );
    // Refused means nothing landed: the holder still holds the row, and there
    // is no checkpoint claiming the work stopped.
    let task = fixture.ok_json(&fixture.main, &["task", "show", "t-park", "--json"]);
    assert_eq!(task["status"], "in_progress");
    assert_eq!(task["claim"]["agentID"], "worker");
    assert!(
        fixture.ok_json(&fixture.main, &["context", "t-park", "--json"])["checkpoints"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused checkpoint was persisted"
    );

    // With the card raised, the same write is the right one and goes through.
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Re-login so the lane can bootstrap",
            "--as",
            "worker",
            "--kind",
            "blocking",
            "--task",
            "t-park",
            "--json",
        ],
    );
    let written = blocked(&fixture, &token, "George re-logins to bootstrap");
    assert!(
        written.status.success(),
        "a blocked checkpoint with an open card was refused: {}",
        String::from_utf8_lossy(&written.stderr)
    );
    let written: Value = serde_json::from_slice(&written.stdout).unwrap();
    assert_eq!(written["state"], "blocked");
    assert_eq!(written["nextAction"], "George re-logins to bootstrap");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-park", "--json"])["status"],
        "blocked"
    );
}

/// The gate reads what a record *assigns*, not what it mentions. A stop that
/// parks work on nobody, one that cites a past decision of George's while
/// handing the next step to a lane, and one that hands it to the operator's
/// own LANE address are all ordinary blocked checkpoints: refusing them would
/// make the honest record the expensive one to write.
#[test]
fn a_blocked_checkpoint_that_assigns_the_owner_nothing_is_written_without_a_card() {
    let fixture = Fixture::new("owner-gate-false-positive");
    fixture.ok_json(&fixture.main, &["init", "--name", "OWNERGATE2", "--json"]);
    for (id, summary, next) in [
        (
            "t-nobody",
            "The upstream crate has not published the fix",
            "Re-check crates.io on Monday",
        ),
        (
            "t-cites",
            "Kept the retry path per George's 2026-09-01 decision",
            "driver-2 runs the suite against the retry path",
        ),
        // The lane this board spells `@:geoyws/kanban/driver` is a machine
        // address, and the next step belonging to that lane is the work being
        // assigned to a worker -- not parked on a person.
        (
            "t-lane",
            "The suite has not run since the rebase",
            "@:geoyws/kanban/driver runs the suite",
        ),
    ] {
        fixture.ok_json(&fixture.main, &["task", "add", id, "--id", id, "--json"]);
        let token = fixture.ok_json(&fixture.main, &["claim", id, "--as", "worker", "--json"])
            ["leaseToken"]
            .as_str()
            .unwrap()
            .to_owned();
        let written = fixture.run(
            &fixture.main,
            &[
                "checkpoint",
                id,
                "--lease",
                &token,
                "--as",
                "worker",
                "--state",
                "blocked",
                "--summary",
                summary,
                "--intent",
                "Preserve the work for whoever resumes",
                "--next-action",
                next,
                "--json",
            ],
        );
        assert!(
            written.status.success(),
            "{id} was refused a blocked checkpoint that parks nothing on the owner: {}",
            String::from_utf8_lossy(&written.stderr)
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["task", "show", id, "--json"])["status"],
            "blocked"
        );
    }
}

/// A handoff has no `blocked` state; its `--blocker` list is where it says
/// what stops the work, so that is the half the same gate reads.
#[test]
fn a_handoff_blocker_that_parks_work_on_the_owner_needs_a_card_first() {
    let fixture = Fixture::new("owner-gate-handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "OWNERGATE3", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Hand the lane over",
            "--id",
            "t-hand",
            "--json",
        ],
    );
    let token = fixture.ok_json(
        &fixture.main,
        &["claim", "t-hand", "--as", "worker", "--json"],
    )["leaseToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let handoff = |fixture: &Fixture, token: &str| {
        fixture.run(
            &fixture.main,
            &[
                "handoff",
                "create",
                "t-hand",
                "--lease",
                token,
                "--as",
                "worker",
                "--summary",
                "The lane is out of session",
                "--intent",
                "Whoever picks this up needs the credential",
                "--next-action",
                "Resume once the credential is live",
                "--blocker",
                "waiting on George for the credential",
                "--json",
            ],
        )
    };

    let refusal = refusal_object(&handoff(&fixture, &token));
    assert!(
        refusal.contains("\"waiting on George for the credential\""),
        "the refusal must quote the blocker it matched: {refusal}"
    );
    assert!(
        refusal.contains("kb attention raise") && refusal.contains("--task t-hand"),
        "the refusal must name the card that reaches him: {refusal}"
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["handoff", "list", "--task", "t-hand", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused handoff was persisted"
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "The credential expired; only you can refresh it",
            "--as",
            "worker",
            "--kind",
            "blocking",
            "--task",
            "t-hand",
            "--json",
        ],
    );
    let written = handoff(&fixture, &token);
    assert!(
        written.status.success(),
        "a handoff whose blocker has an open card was refused: {}",
        String::from_utf8_lossy(&written.stderr)
    );
    let written: Value = serde_json::from_slice(&written.stdout).unwrap();
    assert_eq!(written["status"], "pending");
    assert_eq!(
        written["blockers"][0],
        "waiting on George for the credential"
    );
}

/// Inheritance makes the graph two-edged, so a gate can close a loop the
/// dependency-only guard cannot see: an epic waiting on a task inside its own
/// subtree gates that task on itself forever. Both ways of writing that loop
/// are refused, and a refusal leaves no half-written edge behind.
#[test]
fn completion_gate_graph_rejects_self_and_mixed_parent_dependency_cycles() {
    let fixture = Fixture::new("completion-gate-cycles");
    fixture.ok_json(&fixture.main, &["init", "--name", "CYCLES", "--json"]);
    let board = completion_gate_board_path(&fixture);

    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Self", "--id", "t-self", "--json"],
    );
    let before_self = completion_gate_event_count(&board);
    let self_cycle = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-self",
            "--as",
            "operator",
            "--depends-on",
            "t-self",
            "--json",
        ],
    );
    // The pre-existing self guard: refused, and its wording is its own -- what
    // this test owns is that it survives and writes nothing.
    refusal_object(&self_cycle);
    assert_eq!(completion_gate_event_count(&board), before_self);
    assert_eq!(
        dependency_ids(
            &fixture.ok_json(&fixture.main, &["task", "show", "t-self", "--json"])["dependencies"]
        ),
        Vec::<String>::new()
    );

    // What an epic declares is inherited by everything under it, so depending
    // on one of its own descendants is that epic depending on itself.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Parent plan",
            "--id",
            "e-parent",
            "--type",
            "epic",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Inside work",
            "--id",
            "t-inside",
            "--parent",
            "e-parent",
            "--json",
        ],
    );
    let before_subtree = completion_gate_event_count(&board);
    let subtree_cycle = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "e-parent",
            "--as",
            "operator",
            "--depends-on",
            "t-inside",
            "--json",
        ],
    );
    let subtree_error = refusal_object(&subtree_cycle);
    assert!(
        subtree_error.contains("e-parent") && subtree_error.contains("t-inside"),
        "a mixed parent/dependency cycle must name both ends: {subtree_error}"
    );
    assert_eq!(completion_gate_event_count(&board), before_subtree);
    assert_eq!(
        dependency_ids(
            &fixture.ok_json(&fixture.main, &["task", "show", "e-parent", "--json"])["dependencies"]
        ),
        Vec::<String>::new()
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-inside", "--json"])["parentID"],
        "e-parent",
        "the refused dependency change moved the tree"
    );

    // Written the other way round: re-parenting an existing prerequisite under
    // the plan that waits on it closes the same loop.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "External prerequisite",
            "--id",
            "t-external",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Gated plan",
            "--id",
            "e-gated",
            "--type",
            "epic",
            "--depends-on",
            "t-external",
            "--json",
        ],
    );
    let before_parent = completion_gate_event_count(&board);
    let parent_cycle = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-external",
            "--as",
            "operator",
            "--parent",
            "e-gated",
            "--json",
        ],
    );
    let parent_error = refusal_object(&parent_cycle);
    assert!(
        parent_error.contains("e-gated") && parent_error.contains("t-external"),
        "{parent_error}"
    );
    assert_eq!(completion_gate_event_count(&board), before_parent);
    assert!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-external", "--json"])["parentID"]
            .is_null(),
        "the refused re-parent still landed"
    );
    assert_eq!(
        dependency_ids(
            &fixture.ok_json(&fixture.main, &["task", "show", "e-gated", "--json"])["dependencies"]
        ),
        ["t-external"]
    );
}

#[test]
fn sprint_cli_enforces_scope_version_proof_carry_and_atomic_writes() {
    let fixture = Fixture::new("sprint-core-contract");
    fixture.ok_json(&fixture.main, &["init", "--name", "SPRINT", "--json"]);
    let banner = fixture.run(&fixture.main, &["--version"]);
    assert!(
        banner.status.success(),
        "global --version was consumed by sprint arguments"
    );

    let create = |id: &str, version: &str| {
        fixture.ok_json(
            &fixture.main,
            &[
                "sprint",
                "new",
                id,
                "--id",
                id,
                "--target-version",
                version,
                "--start",
                "0",
                "--end",
                "4102444800000",
                "--as",
                "operator",
                "--json",
            ],
        )
    };
    create("sp-current", "1.2.3");
    create("sp-next", "1.2.4");
    let bad_id = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "new",
            "bad",
            "--id",
            "not-a-sprint",
            "--target-version",
            "1.0.0",
            "--start",
            "0",
            "--end",
            "1",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&bad_id).contains("must start with sp-"));
    let unplanned = fixture.run(
        &fixture.main,
        &["sprint", "start", "sp-next", "--as", "operator", "--json"],
    );
    assert!(refusal_object(&unplanned).contains("no recorded goal and criteria"));
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-next",
            "--body",
            "Reserved emergency sprint\nShip nothing unless needed",
            "--empty-scope",
            "--as",
            "operator",
            "--json",
        ],
    );

    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Loose", "--id", "t-loose", "--json"],
    );
    let plain = fixture.run(&fixture.main, &["context", "t-loose"]);
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains("## Sprint"));

    let add_refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Must roll back",
            "--id",
            "t-rolledback",
            "--sprint",
            "sp-missing",
            "--json",
        ],
    );
    refusal_object(&add_refused);
    refusal_object(&fixture.run(&fixture.main, &["task", "show", "t-rolledback", "--json"]));

    let update_refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-loose",
            "--as",
            "operator",
            "--title",
            "Changed",
            "--sprint",
            "sp-missing",
            "--json",
        ],
    );
    refusal_object(&update_refused);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-loose", "--json"])["title"],
        "Loose"
    );

    fixture.ok_json(
        &fixture.main,
        &["task", "add", "In sprint", "--id", "t-in", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-current",
            "--body",
            "Ship 1.2.3\nAcceptance",
            "--candidate",
            "t-in",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-current",
            "--as",
            "operator",
            "--json",
        ],
    );
    let dash = fixture.ok_json(&fixture.main, &["dashboard", "--json"]);
    let sprint = &dash[0]["currentSprint"];
    assert_eq!(sprint["targetVersion"], "1.2.3");
    assert_eq!(sprint["open"], 1);
    assert_eq!(sprint["done"], 0);
    assert_eq!(sprint["goal"], "Ship 1.2.3");
    assert!(sprint["daysRemaining"].as_i64().unwrap() > 0);

    let direct = fixture.run(
        &fixture.main,
        &["claim", "t-loose", "--as", "worker", "--json"],
    );
    assert!(refusal_object(&direct).contains("--any-sprint"));

    let outgoing = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-loose",
            "--as",
            "worker",
            "--any-sprint",
            "--json",
        ],
    );
    let lease = outgoing["leaseToken"].as_str().unwrap();
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-loose",
            "--lease",
            lease,
            "--as",
            "worker",
            "--summary",
            "cross-sprint handoff",
            "--intent",
            "continue loose work",
            "--next-action",
            "accept explicitly",
            "--reason",
            "manual",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap();
    let handoff_refused = fixture.run(
        &fixture.main,
        &["handoff", "accept", handoff_id, "--as", "next", "--json"],
    );
    assert!(refusal_object(&handoff_refused).contains("--any-sprint"));
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            handoff_id,
            "--as",
            "next",
            "--any-sprint",
            "--json",
        ],
    );
    assert_eq!(accepted["claim"]["taskID"], "t-loose");

    let started = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "geoyws/kanban",
            "--commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--tier",
            "@_p",
            "--environment",
            "production",
            "--host",
            "hax",
            "--url",
            "https://kb.geoy.ws",
            "--sprint",
            "sp-current",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(started["sprintID"], "sp-current");
    assert_eq!(started["targetVersion"], "1.2.3");
    let deployment = started["id"].as_str().unwrap();
    let token = started["capabilityToken"].as_str().unwrap();
    let wrong = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            deployment,
            "--token",
            token,
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--receipt",
            "observed running tier",
            "--served-commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--served-version",
            "9.9.9",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&wrong).contains("target version 1.2.3"));
    fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "finish",
            deployment,
            "--token",
            token,
            "--result",
            "succeeded",
            "--phase",
            "verification",
            "--receipt",
            "observed running tier",
            "--served-commit",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--served-version",
            "1.2.3",
            "--as",
            "operator",
            "--json",
        ],
    );

    let no_carry = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-current",
            "--deployment",
            deployment,
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&no_carry).contains("--carry-to"));
    let closed = fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-current",
            "--deployment",
            deployment,
            "--carry-to",
            "sp-next",
            "--carry-note",
            "unfinished work is explicitly deferred",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(closed["status"], "closed");
    let next = fixture.ok_json(&fixture.main, &["sprint", "show", "sp-next", "--json"]);
    assert!(
        next["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == "t-in")
    );
}

fn sprint_fixture_new(fixture: &Fixture, id: &str, version: &str) {
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "new",
            id,
            "--id",
            id,
            "--target-version",
            version,
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );
}

/// Record a deployment receipt made entirely inside the fixture. This is
/// deliberately labelled as simulated local test data: no live claim or
/// external tier is involved in these process-level acceptance journeys.
fn sprint_fixture_deployment(
    fixture: &Fixture,
    sprint_id: &str,
    version: &str,
    result: &str,
    phase: &str,
) -> String {
    let started = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "fixture/local",
            "--commit",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--tier",
            "@_bdt",
            "--environment",
            "local-test",
            "--host",
            "geoywsMBP",
            "--url",
            "https://fixture.invalid",
            "--sprint",
            sprint_id,
            "--as",
            "operator",
            "--json",
        ],
    );
    let id = started["id"].as_str().unwrap().to_owned();
    let token = started["capabilityToken"].as_str().unwrap();
    let mut args = vec![
        "deploy",
        "finish",
        &id,
        "--token",
        token,
        "--result",
        result,
        "--phase",
        phase,
        "--receipt",
        "simulated local test data; no live claim or external deployment",
    ];
    if result == "succeeded" && phase == "verification" {
        args.extend([
            "--served-commit",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--served-version",
            version,
        ]);
    }
    args.extend(["--as", "operator", "--json"]);
    fixture.ok_json(&fixture.main, &args);
    id
}

#[test]
fn sprint_v26_board_migrates_then_completes_a_proof_gated_lifecycle() {
    let fixture = Fixture::new("sprint-v26-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "SPRINT-V26", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Legacy v26 work",
            "--id",
            "t-v26-legacy",
            "--json",
        ],
    );
    let board = board_path_for_project(&fixture, &fixture.main, "SPRINT-V26");
    {
        let connection = Connection::open(&board).unwrap();
        remove_v27_sprint_schema(&connection);
        connection.execute_batch("PRAGMA user_version=26;").unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT title FROM tasks WHERE id='t-v26-legacy'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "Legacy v26 work"
        );
    }

    sprint_fixture_new(&fixture, "sp-migrated", "26.1.0");
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-migrated",
            "--body",
            "Ship the migrated board
Legacy row remains durable",
            "--candidate",
            "t-v26-legacy",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-migrated",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-v26-legacy",
            "done",
            "--as",
            "operator",
            "--json",
        ],
    );
    let proof = sprint_fixture_deployment(
        &fixture,
        "sp-migrated",
        "26.1.0",
        "succeeded",
        "verification",
    );
    let closed = fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-migrated",
            "--deployment",
            &proof,
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(closed["status"], "closed");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-v26-legacy", "--json"])["title"],
        "Legacy v26 work"
    );
    let audit = fixture.ok_json(&fixture.main, &["audit", "verify", "--json"]);
    assert_eq!(audit["healthy"], true);
    assert_eq!(audit["boards"][0]["audit"]["healthy"], true);
}

#[test]
fn sprint_parent_epic_scope_preserves_explicit_descendants_and_audits_detach() {
    let fixture = Fixture::new("sprint-parent-scope");
    fixture.ok_json(&fixture.main, &["init", "--name", "SPRINT-SCOPE", "--json"]);
    sprint_fixture_new(&fixture, "sp-release", "2.0.0");
    sprint_fixture_new(&fixture, "sp-other", "2.1.0");
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Release epic",
            "--id",
            "e-release",
            "--type",
            "epic",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Release story",
            "--id",
            "s-release",
            "--type",
            "story",
            "--parent",
            "e-release",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Inherited task",
            "--id",
            "t-inherited",
            "--parent",
            "s-release",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Already planned elsewhere",
            "--id",
            "t-other",
            "--parent",
            "s-release",
            "--sprint",
            "sp-other",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-release",
            "--body",
            "Ship release 2.0
Epic subtree is the scope",
            "--parent-epic",
            "e-release",
            "--as",
            "operator",
            "--json",
        ],
    );

    let release = fixture.ok_json(&fixture.main, &["sprint", "show", "sp-release", "--json"]);
    let release_ids = release["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(release_ids, ["e-release", "s-release", "t-inherited"]);
    let other = fixture.ok_json(&fixture.main, &["sprint", "show", "sp-other", "--json"]);
    assert_eq!(other["tasks"][0]["id"], "t-other");

    for task_id in ["e-release", "s-release", "t-inherited"] {
        let events = fixture.ok_json(
            &fixture.main,
            &[
                "events",
                "--task",
                task_id,
                "--kind",
                "task_sprint_changed",
                "--json",
            ],
        );
        assert_eq!(
            events[0]["payload"]["oldSprintID"],
            Value::Null,
            "{task_id}: {events}"
        );
        assert_eq!(
            events[0]["payload"]["newSprintID"], "sp-release",
            "{task_id}: {events}"
        );
    }

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-inherited",
            "--as",
            "operator",
            "--clear-sprint",
            "--json",
        ],
    );
    let detached = fixture.ok_json(
        &fixture.main,
        &[
            "events",
            "--task",
            "t-inherited",
            "--kind",
            "task_sprint_changed",
            "--json",
        ],
    );
    assert_eq!(detached[0]["payload"]["oldSprintID"], "sp-release");
    assert_eq!(detached[0]["payload"]["newSprintID"], Value::Null);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["sprint", "show", "sp-other", "--json"])["tasks"][0]["id"],
        "t-other"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true
    );
}

#[test]
fn sprint_claim_boundary_filters_scheduler_and_records_only_explicit_overrides() {
    let fixture = Fixture::new("sprint-claim-boundary");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SPRINT-CLAIMS", "--json"],
    );
    sprint_fixture_new(&fixture, "sp-current-boundary", "3.0.0");
    sprint_fixture_new(&fixture, "sp-other-boundary", "3.1.0");
    for (id, sprint) in [
        ("t-current-boundary", Some("sp-current-boundary")),
        ("t-other-boundary", Some("sp-other-boundary")),
        ("t-unattached-boundary", None),
    ] {
        let mut args = vec!["task", "add", id, "--id", id];
        if let Some(sprint) = sprint {
            args.extend(["--sprint", sprint]);
        }
        args.push("--json");
        fixture.ok_json(&fixture.main, &args);
    }
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-current-boundary",
            "--body",
            "Ship current scope
Only attached work is claimable",
            "--candidate",
            "t-current-boundary",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-current-boundary",
            "--as",
            "operator",
            "--json",
        ],
    );

    let candidates = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "--candidates",
            "--as",
            "candidate-reader",
            "--json",
        ],
    );
    assert_eq!(candidates.as_array().unwrap().len(), 1, "{candidates}");
    assert_eq!(candidates[0]["id"], "t-current-boundary");
    let next = fixture.ok_json(
        &fixture.main,
        &["claim", "--next", "--as", "next-worker", "--json"],
    );
    assert_eq!(next["taskID"], "t-current-boundary");
    let implicit_event = fixture.ok_json(
        &fixture.main,
        &[
            "events",
            "--task",
            "t-current-boundary",
            "--kind",
            "task_claimed",
            "--json",
        ],
    );
    assert!(
        implicit_event[0]["payload"].get("sprintOverride").is_none(),
        "an implicit current-sprint selection was falsely recorded as an override: {implicit_event}"
    );

    let refused = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-other-boundary",
            "--as",
            "direct-worker",
            "--json",
        ],
    );
    assert!(refusal_object(&refused).contains("not in sprint sp-current-boundary"));
    let named = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-other-boundary",
            "--as",
            "named-worker",
            "--sprint",
            "sp-other-boundary",
            "--json",
        ],
    );
    assert_eq!(named["taskID"], "t-other-boundary");
    let any = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-unattached-boundary",
            "--as",
            "any-worker",
            "--any-sprint",
            "--json",
        ],
    );
    assert_eq!(any["taskID"], "t-unattached-boundary");
    let named_event = fixture.ok_json(
        &fixture.main,
        &[
            "events",
            "--task",
            "t-other-boundary",
            "--kind",
            "task_claimed",
            "--json",
        ],
    );
    assert_eq!(
        named_event[0]["payload"]["sprintOverride"],
        "sp-other-boundary"
    );
    let any_event = fixture.ok_json(
        &fixture.main,
        &[
            "events",
            "--task",
            "t-unattached-boundary",
            "--kind",
            "task_claimed",
            "--json",
        ],
    );
    assert_eq!(any_event[0]["payload"]["sprintOverride"], "any");

    let legacy = Fixture::new("claim-without-current-sprint");
    legacy.ok_json(
        &legacy.main,
        &["init", "--name", "NO-CURRENT-SPRINT", "--json"],
    );
    legacy.ok_json(
        &legacy.main,
        &[
            "task",
            "add",
            "Legacy claim",
            "--id",
            "t-legacy-claim",
            "--json",
        ],
    );
    let legacy_candidates = legacy.ok_json(
        &legacy.main,
        &["claim", "--candidates", "--as", "legacy-worker", "--json"],
    );
    assert_eq!(legacy_candidates[0]["id"], "t-legacy-claim");
    legacy.ok_json(
        &legacy.main,
        &["claim", "--next", "--as", "legacy-worker", "--json"],
    );
    let legacy_event = legacy.ok_json(
        &legacy.main,
        &[
            "events",
            "--task",
            "t-legacy-claim",
            "--kind",
            "task_claimed",
            "--json",
        ],
    );
    assert!(
        legacy_event[0]["payload"].get("sprintOverride").is_none(),
        "{legacy_event}"
    );
}

#[test]
fn sprint_current_boundary_requires_deliberate_scope_and_enriches_only_attached_context() {
    let fixture = Fixture::new("sprint-context-boundary");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SPRINT-CONTEXT", "--json"],
    );
    sprint_fixture_new(&fixture, "sp-context-current", "4.0.0");
    sprint_fixture_new(&fixture, "sp-context-next", "4.1.0");
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Attached", "--id", "t-context-in", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Unattached",
            "--id",
            "t-context-out",
            "--json",
        ],
    );

    let missing_scope = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-context-next",
            "--body",
            "Next goal
Scope must be deliberate",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&missing_scope).contains(
        "requires --candidate, --parent-epic, existing explicit scope, or --empty-scope"
    ));
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-context-current",
            "--body",
            "Exact context goal
Attached context names the release",
            "--candidate",
            "t-context-in",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-context-current",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-context-next",
            "--body",
            "Next goal
Deliberately empty",
            "--empty-scope",
            "--as",
            "operator",
            "--json",
        ],
    );
    let second_start = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-context-next",
            "--as",
            "operator",
            "--json",
        ],
    );
    let second_error = refusal_object(&second_start);
    assert!(
        second_error.contains("sp-context-current") && second_error.contains("sp-context-next"),
        "{second_error}"
    );

    let context = fixture.ok_json(&fixture.main, &["context", "t-context-in", "--json"]);
    assert_eq!(context["sprint"]["sprintID"], "sp-context-current");
    assert_eq!(context["sprint"]["targetVersion"], "4.0.0");
    assert_eq!(context["sprint"]["goal"], "Exact context goal");
    let text = fixture.run(&fixture.main, &["context", "t-context-in"]);
    assert!(text.status.success());
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(
        text.contains(r#"sp-context-current "sp-context-current" v4.0.0 · current"#),
        "{text}"
    );
    assert!(text.contains("Goal: Exact context goal"), "{text}");

    let unattached = fixture.ok_json(&fixture.main, &["context", "t-context-out", "--json"]);
    assert!(unattached.get("sprint").is_none(), "{unattached}");
    let unattached_text = fixture.run(&fixture.main, &["context", "t-context-out"]);
    assert!(unattached_text.status.success());
    assert!(!String::from_utf8_lossy(&unattached_text.stdout).contains("## Sprint"));
}

#[test]
fn sprint_close_rejects_every_unqualified_proof_and_durably_records_carry() {
    let fixture = Fixture::new("sprint-close-gates");
    fixture.ok_json(&fixture.main, &["init", "--name", "SPRINT-CLOSE", "--json"]);
    sprint_fixture_new(&fixture, "sp-close-current", "5.0.0");
    sprint_fixture_new(&fixture, "sp-close-carry", "5.1.0");
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unfinished", "--id", "t-carry", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "plan",
            "sp-close-current",
            "--body",
            "Ship exact 5.0.0
Only verified serving closes",
            "--candidate",
            "t-carry",
            "--as",
            "operator",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "start",
            "sp-close-current",
            "--as",
            "operator",
            "--json",
        ],
    );

    let absent = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-close-current",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&absent).contains("requires --deployment"));

    let failed = sprint_fixture_deployment(
        &fixture,
        "sp-close-current",
        "5.0.0",
        "failed",
        "verification",
    );
    let failed_close = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-close-current",
            "--deployment",
            &failed,
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&failed_close).contains("failed (verification)"));

    let build = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "fixture/local",
            "--commit",
            "cccccccccccccccccccccccccccccccccccccccc",
            "--tier",
            "@_bdt",
            "--environment",
            "local-test",
            "--host",
            "geoywsMBP",
            "--url",
            "https://fixture.invalid",
            "--sprint",
            "sp-close-current",
            "--as",
            "operator",
            "--json",
        ],
    );
    let build_id = build["id"].as_str().unwrap();
    let build_token = build["capabilityToken"].as_str().unwrap();
    let non_verification = fixture.run(
        &fixture.main,
        &[
            "deploy",
            "finish",
            build_id,
            "--token",
            build_token,
            "--result",
            "succeeded",
            "--phase",
            "build",
            "--receipt",
            "simulated local test data; no live claim or external deployment",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&non_verification),
        "a succeeded deployment requires --phase verification"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["deploy", "show", build_id, "--json"])["status"],
        "started",
        "the refused non-verification result finalized the deployment"
    );
    let other = sprint_fixture_deployment(
        &fixture,
        "sp-close-carry",
        "5.1.0",
        "succeeded",
        "verification",
    );
    let other_close = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-close-current",
            "--deployment",
            &other,
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&other_close).contains("bound to this sprint and served version 5.0.0"));

    let proof = sprint_fixture_deployment(
        &fixture,
        "sp-close-current",
        "5.0.0",
        "succeeded",
        "verification",
    );
    let unnamed_note = fixture.run(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-close-current",
            "--deployment",
            &proof,
            "--carry-to",
            "sp-close-carry",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(refusal_object(&unnamed_note).contains("carry-over note"));
    let note = "Carry t-carry after simulated local verification";
    let closed = fixture.ok_json(
        &fixture.main,
        &[
            "sprint",
            "close",
            "sp-close-current",
            "--deployment",
            &proof,
            "--carry-to",
            "sp-close-carry",
            "--carry-note",
            note,
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(closed["status"], "closed");
    assert_eq!(closed["closedByDeployment"], proof);
    let destination = fixture.ok_json(
        &fixture.main,
        &["sprint", "show", "sp-close-carry", "--json"],
    );
    assert_eq!(destination["tasks"][0]["id"], "t-carry");
    assert_eq!(destination["tasks"][0]["status"], "todo");
    let carry_event = fixture.ok_json(
        &fixture.main,
        &[
            "events",
            "--task",
            "t-carry",
            "--kind",
            "task_sprint_changed",
            "--json",
        ],
    );
    assert_eq!(carry_event[0]["payload"]["oldSprintID"], "sp-close-current");
    assert_eq!(carry_event[0]["payload"]["newSprintID"], "sp-close-carry");
    assert_eq!(carry_event[0]["payload"]["carryNote"], note);
    let close_event = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "sprint_closed", "--json"],
    );
    assert_eq!(close_event[0]["payload"]["sprintID"], "sp-close-current");
    assert_eq!(close_event[0]["payload"]["deploymentID"], proof);
    assert_eq!(close_event[0]["payload"]["targetVersion"], "5.0.0");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        true
    );
}

/// The refusal a restricted task gives, and the claim that gets through.
///
/// George's ask, in his words: "only a subset of models, or one model, can
/// claim this task. Blender must only be used by Astra." What makes that
/// scheduling rather than documentation is that the refusal happens on the
/// claim path, beside `driver_only` and `assignee` — so this also pins where
/// in the order it sits: a driver-only restricted row refuses driver-only
/// first, and an assigned restricted row refuses the model first.
#[test]
fn model_restricted_task_refuses_a_claim_without_or_outside_its_allow_list() {
    let fixture = Fixture::new("model-claim");
    fixture.ok_json(&fixture.main, &["init", "--name", "MODEL", "--json"]);
    // Duplicates collapse and the set reads back sorted, so the wire shape
    // cannot depend on the order the flags arrived in.
    let added = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "render the scene",
            "--id",
            "t-blender",
            "--allowed-model",
            "Blender",
            "--allowed-model",
            "Astra",
            "--allowed-model",
            "Blender",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(added["allowedModels"], json!(["Astra", "Blender"]));

    let no_model = fixture.run(
        &fixture.main,
        &["claim", "t-blender", "--as", "d1", "--json"],
    );
    assert_eq!(
        refusal_object(&no_model),
        "task t-blender is restricted to models [Astra, Blender]; \
         pass --model with one of them to claim it"
    );
    let wrong = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-blender",
            "--as",
            "d1",
            "--model",
            "Kimi",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&wrong),
        "task t-blender is restricted to models [Astra, Blender]; model Kimi may not claim it"
    );
    // Case is not folded: there is no master file to fold toward.
    let cased = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-blender",
            "--as",
            "d1",
            "--model",
            "astra",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&cased),
        "task t-blender is restricted to models [Astra, Blender]; model astra may not claim it"
    );
    let malformed = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-blender",
            "--as",
            "d1",
            "--model",
            "bad name",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&malformed),
        "model name bad name is not a usable name: \
         it must match ^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$"
    );

    let claimed = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-blender",
            "--as",
            "d1",
            "--model",
            "Astra",
            "--json",
        ],
    );
    assert_eq!(claimed["model"], "Astra");
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-blender", "--json"]);
    assert_eq!(shown["claim"]["model"], "Astra");
    assert_eq!(shown["allowedModels"], json!(["Astra", "Blender"]));

    // The declared model rides on the ledger only when it was declared.
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "t-blender", "--json"]);
    let claimed_payload = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "task_claimed")
        .expect("a task_claimed event")["payload"]
        .clone();
    assert_eq!(claimed_payload["model"], "Astra");

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "open work",
            "--id",
            "t-open",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-open", "--as", "d2", "--json"]);
    let open_events = fixture.ok_json(&fixture.main, &["events", "--task", "t-open", "--json"]);
    let open_payload = open_events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "task_claimed")
        .expect("a task_claimed event")["payload"]
        .clone();
    assert!(
        open_payload.get("model").is_none(),
        "an unrestricted claim that declared no model carried one anyway: {open_payload}"
    );

    // Order: driver-only is refused BEFORE the model, and the model BEFORE
    // the assignee. Either pair would otherwise silently swap when the
    // refusals moved.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "driver work",
            "--id",
            "t-dr",
            "--driver-only",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let driver_first = fixture.run(&fixture.main, &["claim", "t-dr", "--as", "d3", "--json"]);
    assert_eq!(refusal_object(&driver_first), "task t-dr is driver-only");
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "assigned work",
            "--id",
            "t-as",
            "--assignee",
            "someone-else",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let model_before_assignee =
        fixture.run(&fixture.main, &["claim", "t-as", "--as", "d3", "--json"]);
    assert_eq!(
        refusal_object(&model_before_assignee),
        "task t-as is restricted to models [Astra]; pass --model with one of them to claim it"
    );
}

/// The scheduler must not offer work the claim path would refuse.
///
/// `--candidates` inspects the same pool `--next` hands out, so a restricted
/// row that is invisible to one must be invisible to the other, and an
/// unrestricted row must be unaffected by `--model` either way.
#[test]
fn claim_next_and_candidates_skip_restricted_rows_unless_the_model_matches() {
    let fixture = Fixture::new("model-candidates");
    fixture.ok_json(&fixture.main, &["init", "--name", "POOL", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "blender work",
            "--id",
            "t-restricted",
            "--priority",
            "0",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "anyone work",
            "--id",
            "t-free",
            "--priority",
            "1",
            "--as",
            "geoyws",
            "--json",
        ],
    );

    let ids = |args: &[&str]| -> Vec<String> {
        fixture
            .ok_json(&fixture.main, args)
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        ids(&["claim", "--candidates", "--as", "d1", "--json"]),
        vec!["t-free".to_owned()],
        "a restricted row was offered to a caller that named no model"
    );
    assert_eq!(
        ids(&[
            "claim",
            "--candidates",
            "--as",
            "d1",
            "--model",
            "Kimi",
            "--json"
        ]),
        vec!["t-free".to_owned()],
        "a restricted row was offered to a model outside its list"
    );
    assert_eq!(
        ids(&[
            "claim",
            "--candidates",
            "--as",
            "d1",
            "--model",
            "Astra",
            "--json"
        ]),
        vec!["t-restricted".to_owned(), "t-free".to_owned()],
        "the matching model was not offered the restricted row, or lost the free one"
    );

    // `--next` hands out what `--candidates` showed: the free row for a
    // caller with no model, and the higher-priority restricted row for Astra.
    let without = fixture.ok_json(&fixture.main, &["claim", "--next", "--as", "d1", "--json"]);
    assert_eq!(without["taskID"], "t-free");
    assert!(without["model"].is_null());
    fixture.ok_json(
        &fixture.main,
        &[
            "release",
            "t-free",
            "--lease",
            without["leaseToken"].as_str().unwrap(),
        ],
    );
    let with = fixture.ok_json(
        &fixture.main,
        &[
            "claim", "--next", "--as", "d1", "--model", "Astra", "--json",
        ],
    );
    assert_eq!(with["taskID"], "t-restricted");
    assert_eq!(with["model"], "Astra");
}

/// Accepting a handoff mints a lease, so it answers to the same rule.
#[test]
fn handoff_accept_honours_the_task_model_allow_list() {
    let fixture = Fixture::new("model-handoff");
    fixture.ok_json(&fixture.main, &["init", "--name", "HANDOFF", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "blender work",
            "--id",
            "t-h",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &[
            "claim", "t-h", "--as", "outgoing", "--model", "Astra", "--json",
        ],
    );
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-h",
            "--lease",
            &token,
            "--as",
            "outgoing",
            "--summary",
            "half rendered",
            "--intent",
            "finish the render",
            "--next-action",
            "render frames 40-90",
            "--reason",
            "session_end",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap().to_owned();

    let refused = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&refused),
        "task t-h is restricted to models [Astra]; pass --model with one of them to claim it"
    );
    let wrong = fixture.run(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            "incoming",
            "--model",
            "Kimi",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&wrong),
        "task t-h is restricted to models [Astra]; model Kimi may not claim it"
    );

    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            &handoff_id,
            "--as",
            "incoming",
            "--model",
            "Astra",
            "--json",
        ],
    );
    assert_eq!(accepted["claim"]["model"], "Astra");
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "t-h", "--json"]);
    let payload = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "handoff_accepted")
        .expect("a handoff_accepted event")["payload"]
        .clone();
    assert_eq!(payload["model"], "Astra");
}

/// The allow-list is replaced wholesale or emptied, never merged.
#[test]
fn task_update_replaces_or_clears_the_allow_list_and_refuses_both_flags() {
    let fixture = Fixture::new("model-update");
    fixture.ok_json(&fixture.main, &["init", "--name", "UPDATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "scene",
            "--id",
            "t-u",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "other", "--id", "t-other", "--as", "geoyws", "--json",
        ],
    );

    let both = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-u",
            "--allowed-model",
            "Kimi",
            "--clear-allowed-models",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&both),
        "--allowed-model and --clear-allowed-models are mutually exclusive"
    );
    let malformed = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-u",
            "--allowed-model",
            "-leading",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&malformed),
        "model name -leading is not a usable name: \
         it must match ^[A-Za-z0-9][A-Za-z0-9._:/-]{0,63}$"
    );
    // The refused update left the row exactly as it was.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-u", "--json"])["allowedModels"],
        json!(["Astra"])
    );

    let replaced = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-u",
            "--allowed-model",
            "zeta",
            "--allowed-model",
            "Kimi",
            "--allowed-model",
            "zeta",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        replaced["allowedModels"],
        json!(["Kimi", "zeta"]),
        "the update merged instead of replacing, or lost the sort"
    );

    // Server-side filter: only rows whose list holds the name come back.
    let filtered = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--allowed-model", "Kimi", "--json"],
    );
    assert_eq!(
        filtered
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["t-u"]
    );
    assert!(
        fixture
            .ok_json(
                &fixture.main,
                &["task", "list", "--allowed-model", "Astra", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "the replaced model still filters to the row"
    );

    let cleared = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-u",
            "--clear-allowed-models",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(cleared["allowedModels"], json!([]));
    // And an unrestricted row is claimable by anyone again.
    fixture.ok_json(&fixture.main, &["claim", "t-u", "--as", "anyone", "--json"]);

    let changed = fixture
        .ok_json(&fixture.main, &["events", "--task", "t-u", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "task_updated")
        .map(|event| event["payload"]["changed"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        changed,
        vec![json!(["allowedModels"]), json!(["allowedModels"])],
        "task_updated did not name the field that moved"
    );

    // A no-op replace says nothing moved.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-other",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "t-other",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let other_changed = fixture
        .ok_json(&fixture.main, &["events", "--task", "t-other", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "task_updated")
        .map(|event| event["payload"]["changed"].clone())
        .collect::<Vec<_>>();
    // Newest first, as `events` lists: the second update moved nothing.
    assert_eq!(other_changed, vec![json!([]), json!(["allowedModels"])]);
}

/// A board written before the feature opens, migrates, and reads as
/// unrestricted — with its existing claim's model absent rather than invented.
#[test]
fn a_v29_board_gains_task_models_and_claim_model_and_existing_claims_read_null() {
    let fixture = Fixture::new("model-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "OLD", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "older work",
            "--id",
            "t-old",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture.ok_json(&fixture.main, &["claim", "t-old", "--as", "d1", "--json"]);
    let board = board_path_for_project(&fixture, &fixture.main, "OLD");

    {
        let connection = Connection::open(&board).unwrap();
        remove_v30_model_restriction_schema(&connection);
        connection.execute_batch("PRAGMA user_version=29;").unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name='task_models'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0,
            "the fixture did not actually reach v29"
        );
    }

    let shown = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "show",
            "t-old",
            "--db",
            board.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(shown["allowedModels"], json!([]));
    assert!(
        shown["claim"]["model"].is_null(),
        "a pre-migration claim invented a model: {}",
        shown["claim"]
    );

    let connection = Connection::open(&board).unwrap();
    // An ordinary open of a registered board stops at the pre-CROSS schema;
    // the CROSS step is the owner's, taken by `init` (ADR-056 §5).
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        38
    );
    // The table, its index, and the column are all there.
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM task_models", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='index' \
                 AND name='idx_task_models_model'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    // The rebuilt claims table kept the row it copied, its lease included.
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM task_claims WHERE task_id='t-old' AND model IS NULL",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type='index' \
                 AND name='idx_task_claims_expiry'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    drop(connection);

    // And the migrated board restricts exactly as a fresh one does.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "new work",
            "--id",
            "t-new",
            "--allowed-model",
            "Astra",
            "--db",
            board.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let refused = fixture.run(
        &fixture.main,
        &[
            "claim",
            "t-new",
            "--as",
            "d2",
            "--db",
            board.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        refusal_object(&refused),
        "task t-new is restricted to models [Astra]; pass --model with one of them to claim it"
    );
}

/// The MCP tool schemas follow the surface table, so a client reads the
/// allow-list as an array and the claim's model as a string (ADR-010).
#[test]
fn mcp_schema_exposes_allowed_model_array_and_claim_model_string() {
    let fixture = Fixture::new("model-mcp");
    fixture.ok_json(&fixture.main, &["init", "--name", "MCPMODEL", "--json"]);

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let kind = |operation: &str, flag: &str| -> String {
        schema["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == operation)
            .unwrap_or_else(|| panic!("no operation named {operation}"))["flags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == flag)
            .unwrap_or_else(|| panic!("{operation} does not accept --{flag}"))["kind"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(kind("task add", "allowed-model"), "list");
    assert_eq!(kind("task update", "allowed-model"), "list");
    assert_eq!(kind("task update", "clear-allowed-models"), "boolean");
    // Scalar where one name is asked about or declared.
    assert_eq!(kind("task list", "allowed-model"), "value");
    assert_eq!(kind("claim", "model"), "value");
    assert_eq!(kind("handoff accept", "model"), "value");

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap().clone();
    let tool = |name: &str| -> Value {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("no tool named {name}: {listed}"))
            .clone()
    };
    let add = tool("task_add");
    assert_eq!(
        add["inputSchema"]["properties"]["allowed-model"]["type"],
        "array"
    );
    assert_eq!(
        add["inputSchema"]["properties"]["allowed-model"]["items"]["type"],
        "string"
    );
    let update = tool("task_update");
    assert_eq!(
        update["inputSchema"]["properties"]["allowed-model"]["type"],
        "array"
    );
    assert_eq!(
        update["inputSchema"]["properties"]["clear-allowed-models"]["type"],
        "boolean"
    );
    assert_eq!(
        tool("task_list")["inputSchema"]["properties"]["allowed-model"]["type"],
        "string"
    );
    assert_eq!(
        tool("claim")["inputSchema"]["properties"]["model"]["type"],
        "string"
    );
    assert_eq!(
        tool("handoff_accept")["inputSchema"]["properties"]["model"]["type"],
        "string"
    );

    // And the tool actually restricts through the MCP path, not only the CLI.
    let called = session.ask(json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "task_add", "arguments": {
            "title": "mcp scene", "id": "t-mcp", "as": "geoyws",
            "allowed-model": ["Astra", "Blender"]
        }}
    }));
    assert_eq!(
        called["result"]["isError"],
        json!(false),
        "task_add through MCP failed: {called}"
    );
    let refused = session.ask(json!({
        "jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": { "name": "claim", "arguments": { "id": "t-mcp", "as": "d1" }}
    }));
    let text = refused["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains(
            "task t-mcp is restricted to models [Astra, Blender]; \
             pass --model with one of them to claim it"
        ),
        "{refused}"
    );
}

/// COMPLAINT-01: the sixth kind raises, lists and shows through the existing
/// attention verbs, with no new table, verb or adapter.
#[test]
fn complaint_kind_raise_list_show_round_trip() {
    let fixture = Fixture::new("complaint-round-trip");
    fixture.ok_json(&fixture.main, &["init", "--name", "COMPLAINT", "--json"]);
    let raised = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "returns arrive broken",
            "--as",
            "lane@driver-2",
            "--kind",
            "complaint",
            "--json",
        ],
    );
    assert_eq!(raised["kind"], "complaint");
    assert_eq!(raised["status"], "open");
    assert_eq!(raised["raisedBy"], "lane@driver-2");
    let id = raised["id"].as_str().unwrap().to_owned();

    let shown = fixture.ok_json(&fixture.main, &["attention", "show", &id, "--json"]);
    assert_eq!(shown, raised);

    let filtered = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--kind", "complaint", "--json"],
    );
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0], raised);

    // The unfiltered listing carries the complaint alongside the other kinds,
    // and each kind filter still selects only its own rows.
    let blocking = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "the manual contradicts the field set",
            "--as",
            "lane@driver-2",
            "--kind",
            "blocking",
            "--json",
        ],
    );
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let blocking_only = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--kind", "blocking", "--json"],
    );
    assert_eq!(blocking_only.as_array().unwrap().len(), 1);
    assert_eq!(blocking_only[0]["id"], blocking["id"]);

    // The raise wrote the same envelope as every other kind, with the new
    // value as the only difference.
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_raised", "--json"],
    );
    let complaint_event = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["payload"]["attentionID"] == json!(id))
        .expect("the complaint raise left no attention_raised event");
    assert_eq!(complaint_event["payload"]["kind"], "complaint");
}

/// COMPLAINT-02: the five legacy kinds behave exactly as before the sixth
/// landed — same flags, same sentences, same JSON keys, same events.
#[test]
fn five_legacy_kinds_unchanged_after_complaint_lands() {
    let fixture = Fixture::new("legacy-kinds");
    fixture.ok_json(&fixture.main, &["init", "--name", "LEGACY", "--json"]);
    let mut raised = Vec::new();
    for kind in ["blocking", "decision", "approval", "review", "risk"] {
        let row = fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                &format!("a {kind} item"),
                "--as",
                "lane@driver-2",
                "--kind",
                kind,
                "--json",
            ],
        );
        assert_eq!(row["kind"], kind);
        assert_eq!(row["status"], "open");
        raised.push(row);
    }
    for row in &raised {
        let kind = row["kind"].as_str().unwrap();
        let filtered = fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--kind", kind, "--json"],
        );
        assert_eq!(filtered.as_array().unwrap().len(), 1);
        assert_eq!(filtered[0]["id"], row["id"]);
        let shown = fixture.ok_json(
            &fixture.main,
            &["attention", "show", row["id"].as_str().unwrap(), "--json"],
        );
        assert_eq!(shown, *row);
    }
    // Resolving and reopening a legacy row still settles and restores it.
    let id = raised[0]["id"].as_str().unwrap().to_owned();
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "lane@driver-2",
            "--choice",
            "approve",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            &id,
            "--as",
            "lane@driver-2",
            "--note",
            "still needs George",
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
}

/// COMPLAINT-03: an unknown kind is refused with the sentence naming all six
/// values, and the refusal writes nothing — no row, no event.
#[test]
fn unknown_attention_kind_refusal_names_all_six() {
    let fixture = Fixture::new("complaint-refusal");
    fixture.ok_json(&fixture.main, &["init", "--name", "REFUSAL", "--json"]);
    let before = attention_and_chain(&fixture);
    let expected = "invalid attention kind grievance; expected blocking, decision, approval, review, risk, or complaint";
    assert_eq!(
        attention_refusal(
            &fixture,
            &["raise", "x", "--as", "lane@driver-2", "--kind", "grievance"]
        ),
        expected
    );
    assert_eq!(
        attention_refusal(&fixture, &["list", "--kind", "grievance"]),
        expected
    );
    assert_eq!(
        attention_and_chain(&fixture),
        before,
        "a refused kind wrote a row or an event"
    );
}

/// COMPLAINT-04: a V32 board seeded with one open row of each legacy kind
/// migrates forward losslessly, accepts a fresh complaint, and re-opens
/// without migrating anything further.
#[test]
fn complaint_migration_carries_five_kind_board_forward() {
    let fixture = Fixture::new("complaint-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "MIGRATE", "--json"]);
    let mut seeded = Vec::new();
    for kind in ["blocking", "decision", "approval", "review", "risk"] {
        seeded.push(fixture.ok_json(
            &fixture.main,
            &[
                "attention",
                "raise",
                &format!("seeded {kind} body"),
                "--as",
                "lane@driver-2",
                "--kind",
                kind,
                "--json",
            ],
        ));
    }
    let board = board_path_for_project(&fixture, &fixture.main, "MIGRATE");
    // Rewind the declaration to the V32 five-kind CHECK without touching any
    // row, and lower user_version: the next open must migrate forward. The
    // V32 text is derived from the live declaration rather than restated, so
    // this fixture cannot drift from the CHECK the ladder actually ships.
    {
        let connection = Connection::open(&board).unwrap();
        let sql: String = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='attention'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("'complaint'"),
            "expected the migrated declaration"
        );
        let v32 = sql.replace(",'complaint'", "").replacen(
            "CREATE TABLE attention (",
            "CREATE TABLE attention_v32_rewind (",
            1,
        );
        assert!(!v32.contains("complaint"), "the rewind left the new value");
        assert!(
            v32.starts_with("CREATE TABLE attention_v32_rewind ("),
            "the rewind did not retarget the declaration"
        );
        let columns: Vec<String> = connection
            .prepare("SELECT name FROM pragma_table_info('attention') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let list = columns.join(",");
        // The rename carries the table's triggers and indexes to the old name
        // and the drop takes them, so their declarations are read back first
        // and re-created after: the rewound board is a whole V32 shape, not a
        // table the V33 step cannot open.
        let attached: Vec<String> = connection
            .prepare(
                "SELECT sql FROM sqlite_master WHERE tbl_name='attention' AND sql IS NOT NULL AND type != 'table' ORDER BY type, name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let restore = attached.join(";");
        // The swap builds aside and drops the live table rather than renaming
        // it: a rename rewrites every stored reference to the old name — the
        // view's attention arm and `attention_tags`' foreign key — while the
        // aside name has no referrers to rewrite. The rename pragma is set as
        // its own statement, which is the form this connection honours, so
        // the final rename back to `attention` neither rewrites nor validates
        // references: the view and the foreign key keep naming `attention`
        // and land on the rebuilt table untouched.
        connection
            .execute_batch("PRAGMA legacy_alter_table=ON;")
            .unwrap();
        connection
            .execute_batch(&format!(
                "{v32};\
                 INSERT INTO attention_v32_rewind({list}) SELECT {list} FROM attention;\
                 DROP TABLE attention;\
                 ALTER TABLE attention_v32_rewind RENAME TO attention;\
                 {restore};"
            ))
            .unwrap();
        connection
            .execute_batch("PRAGMA legacy_alter_table=OFF;PRAGMA user_version=32;")
            .unwrap();
        let stale: Vec<(String, String)> = connection
            .prepare("SELECT type, name FROM sqlite_master WHERE sql LIKE '%attention_v32_rewind%'")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(stale.is_empty(), "rewind left stale references: {stale:?}");
    }
    let listed = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 5);
    for row in &seeded {
        let kind = row["kind"].as_str().unwrap();
        let filtered = fixture.ok_json(
            &fixture.main,
            &["attention", "list", "--kind", kind, "--json"],
        );
        assert_eq!(filtered.as_array().unwrap().len(), 1);
        for key in ["kind", "body", "raisedBy", "status", "createdAt"] {
            assert_eq!(filtered[0][key], row[key], "{kind} lost {key} in migration");
        }
    }
    let migrated: i64 = Connection::open(&board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(migrated, 38, "the board did not migrate forward");

    // The migrated board takes a fresh complaint, and only under its kind.
    let complaint = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "a fresh complaint",
            "--as",
            "lane@driver-2",
            "--kind",
            "complaint",
            "--json",
        ],
    );
    assert_eq!(complaint["kind"], "complaint");
    let complaints = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--kind", "complaint", "--json"],
    );
    assert_eq!(complaints.as_array().unwrap().len(), 1);
    assert_eq!(complaints[0]["id"], complaint["id"]);

    // Re-opening migrates nothing further and changes nothing.
    let first = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    let second = fixture.ok_json(&fixture.main, &["attention", "list", "--json"]);
    assert_eq!(first, second);
    let again: i64 = Connection::open(&board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(again, 38);
}

/// COMPLAINT-05: a complaint resolves, refuses, and reopens exactly like any
/// other kind — including the raise/resolve asymmetry.
#[test]
fn complaint_resolve_reopen_matches_other_kinds() {
    let fixture = Fixture::new("complaint-settle");
    fixture.ok_json(&fixture.main, &["init", "--name", "SETTLE", "--json"]);
    let raised = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "returns arrive broken",
            "--as",
            "lane@driver-2",
            "--kind",
            "complaint",
            "--json",
        ],
    );
    let id = raised["id"].as_str().unwrap().to_owned();
    let before = attention_and_chain(&fixture);

    // A third party who is neither the operator nor the raiser is refused,
    // and the row stays open with no new event.
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve",
                &id,
                "--as",
                "claude/driver-3",
                "--choice",
                "approve",
                "--note",
                "Probe"
            ]
        ),
        format!(
            "attention {id} was raised by lane@driver-2; only geoyws or that same raiser may resolve it — \
             use attention update to correct it without closing George's queue"
        )
    );
    assert_eq!(attention_and_chain(&fixture), before);

    // The raiser settles it through the same composed decision.
    let settled = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "lane@driver-2",
            "--choice",
            "approve",
            "--note",
            "confirmed with the vendor",
            "--json",
        ],
    );
    assert_eq!(settled["status"], "resolved");
    assert_eq!(settled["resolvedBy"], "lane@driver-2");
    assert_eq!(settled["decision"]["choice"], "approve");
    assert_eq!(settled["decision"]["outcome"], "approve");
    let resolved_events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_resolved", "--json"],
    );
    assert_eq!(resolved_events.as_array().unwrap().len(), 1);
    assert_eq!(resolved_events[0]["payload"]["kind"], "complaint");

    // Settling twice is refused: the row is history, not a queue entry.
    assert_eq!(
        attention_refusal(
            &fixture,
            &[
                "resolve",
                &id,
                "--as",
                "lane@driver-2",
                "--choice",
                "approve"
            ]
        ),
        format!(
            "attention {id} was already resolved by lane@driver-2 — it is history, not a queue entry"
        )
    );

    // The operator reopens it like any other kind.
    let reopened = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "reopen",
            &id,
            "--as",
            "geoyws",
            "--note",
            "the vendor answered; re-check",
            "--json",
        ],
    );
    assert_eq!(reopened["status"], "open");
    assert_eq!(reopened["reopenedBy"], "geoyws");
    let reopened_events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_reopened", "--json"],
    );
    assert_eq!(reopened_events.as_array().unwrap().len(), 1);
    // The reopened envelope keeps its baseline keys — no kind field — and
    // still names the row it reopened and who had settled it.
    assert_eq!(reopened_events[0]["payload"]["attentionID"], json!(id));
    assert_eq!(reopened_events[0]["payload"]["resolvedBy"], "lane@driver-2");
}

/// COMPLAINT-06: a complaint row carries exactly the keys a legacy row
/// carries — the new kind value changes no JSON shape.
#[test]
fn complaint_json_keys_match_legacy_kind_shapes() {
    let fixture = Fixture::new("complaint-keys");
    fixture.ok_json(&fixture.main, &["init", "--name", "KEYS", "--json"]);
    let complaint = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "returns arrive broken",
            "--as",
            "lane@driver-2",
            "--kind",
            "complaint",
            "--json",
        ],
    );
    let blocking = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "the manual contradicts the field set",
            "--as",
            "lane@driver-2",
            "--kind",
            "blocking",
            "--json",
        ],
    );
    let keys = |value: &Value| {
        let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        keys
    };
    assert_eq!(keys(&complaint), keys(&blocking));
    let shown_complaint = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "show",
            complaint["id"].as_str().unwrap(),
            "--json",
        ],
    );
    let shown_blocking = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "show",
            blocking["id"].as_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(keys(&shown_complaint), keys(&shown_blocking));
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--kind", "attention_raised", "--json"],
    );
    let payloads: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| &event["payload"])
        .collect();
    assert_eq!(payloads.len(), 2);
    assert_eq!(keys(payloads[0]), keys(payloads[1]));
    assert!(
        payloads
            .iter()
            .any(|payload| payload["kind"] == "complaint")
    );
    assert!(payloads.iter().any(|payload| payload["kind"] == "blocking"));
}

/// COMPLAINT-07: `schema --json` publishes the sixth value on both `--kind`
/// flags, and the operation set gains no tool and renames none.
#[test]
fn generated_surface_publishes_complaint_without_new_tool() {
    let fixture = Fixture::new("complaint-schema");
    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();
    let operation = |name: &str| -> Value {
        operations
            .iter()
            .find(|operation| operation["name"] == json!(name))
            .unwrap_or_else(|| panic!("no {name} operation"))
            .clone()
    };
    let kinds = |name: &str| -> Value {
        operation(name)["flags"]
            .as_array()
            .unwrap()
            .iter()
            .find(|flag| flag["name"] == json!("kind"))
            .unwrap_or_else(|| panic!("{name} has no --kind"))
            .get("values")
            .unwrap_or_else(|| panic!("{name} --kind publishes no values"))
            .clone()
    };
    let expected = json!([
        "blocking",
        "decision",
        "approval",
        "review",
        "risk",
        "complaint"
    ]);
    assert_eq!(kinds("attention raise"), expected);
    assert_eq!(kinds("attention list"), expected);
    let attention: Vec<&str> = operations
        .iter()
        .filter_map(|operation| operation["name"].as_str())
        .filter(|name| name.starts_with("attention "))
        .collect();
    assert_eq!(
        attention,
        [
            "attention raise",
            "attention list",
            "attention show",
            "attention update",
            "attention resolve",
            "attention check",
            "attention reopen"
        ]
    );
}

/// Raise one checked row and resolve it with the given check key, returning
/// its id. The definition texts avoid every agreed diagnosis marker so the
/// rows settle; the explanation carries a sentinel the report must never
/// print.
fn raise_resolved_check(
    fixture: &Fixture,
    cwd: &std::path::Path,
    body: &str,
    about: &str,
    kind: Option<&str>,
    answered_key: &str,
) -> String {
    let mut raise = vec![
        "attention".to_owned(),
        "raise".to_owned(),
        body.to_owned(),
        "--as".to_owned(),
        "claude@driver".to_owned(),
    ];
    if let Some(kind) = kind {
        raise.push("--kind".to_owned());
        raise.push(kind.to_owned());
    }
    raise.extend(
        [
            "--check",
            "Where does the one check answer live after a resolve?",
            "--check-choice",
            "alpha=Alpha option",
            "--check-choice",
            "beta=Beta option",
            "--check-answer",
            "alpha",
            "--check-explain",
            "REPORT_SENTINEL rust/store.rs records answered, correct and answeredAt as columns.",
            "--check-about",
            about,
            "--json",
        ]
        .iter()
        .map(|flag| flag.to_string()),
    );
    let raised = fixture.ok_json(cwd, &raise.iter().map(String::as_str).collect::<Vec<_>>());
    let id = raised["id"].as_str().unwrap().to_owned();
    fixture.ok_json(
        cwd,
        &[
            "attention",
            "resolve",
            &id,
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--check-answered",
            answered_key,
            "--json",
        ],
    );
    id
}

#[test]
fn att_list_check_report_groups_worst_first_with_adr037_caps() {
    let fixture = Fixture::new("check-report");
    fixture.ok_json(&fixture.main, &["init", "--name", "ACCRPT", "--json"]);
    // (about, resolved rows, correct answers, kind).
    for (about, total, correct, kind) in [
        ("src/store.rs", 5, 1, None),
        ("@@edge", 4, 3, None),
        ("FAST_FLAG", 3, 3, Some("risk")),
        ("src/trunc.rs", 6, 5, None),
    ] {
        for index in 0..total {
            raise_resolved_check(
                &fixture,
                &fixture.main,
                &format!("Comprehension for {about} row {index}."),
                about,
                kind,
                if index < correct { "alpha" } else { "beta" },
            );
        }
    }
    // An open checked row and resolved/open rows with no check contribute
    // nothing to the report.
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Still waiting on its answer.",
            "--as",
            "claude@driver",
            "--check",
            "Where does the one check answer live after a resolve?",
            "--check-choice",
            "alpha=Alpha option",
            "--check-choice",
            "beta=Beta option",
            "--check-answer",
            "alpha",
            "--check-explain",
            "REPORT_SENTINEL rust/store.rs records the open row.",
            "--check-about",
            "src/open.rs",
            "--json",
        ],
    );
    let plain = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "No check rides along.",
            "--as",
            "claude@driver",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "resolve",
            plain["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--choice",
            "approve",
            "--json",
        ],
    );

    // The table prints the five fixed columns worst first: 80, 25, 16, 0.
    let table = fixture.run(&fixture.main, &["attention", "list", "--check-report"]);
    assert!(table.status.success());
    let stdout = String::from_utf8_lossy(&table.stdout);
    assert_eq!(
        stdout.as_ref(),
        "about answered correct missed miss-rate\n\
         src/store.rs 5 1 4 80\n\
         @@edge 4 3 1 25\n\
         src/trunc.rs 6 5 1 16\n\
         FAST_FLAG 3 3 0 0\n",
        "the report table: {stdout}"
    );
    assert!(
        !stdout.contains("REPORT_SENTINEL"),
        "the table carries aggregate data only: {stdout}"
    );

    // `--json` returns the same groups with exactly the five keys — no
    // raiser, actor, answer-key, explanation or choice-label field.
    let report = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--check-report", "--json"],
    );
    let groups = report.as_array().expect("the report groups");
    assert_eq!(
        report,
        json!([
            {"about": "src/store.rs", "answered": 5, "correct": 1, "missed": 4, "missRate": 80},
            {"about": "@@edge", "answered": 4, "correct": 3, "missed": 1, "missRate": 25},
            {"about": "src/trunc.rs", "answered": 6, "correct": 5, "missed": 1, "missRate": 16},
            {"about": "FAST_FLAG", "answered": 3, "correct": 3, "missed": 0, "missRate": 0},
        ])
    );
    for group in groups {
        let mut keys = group
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(keys, ["about", "answered", "correct", "missRate", "missed"]);
    }
    assert!(
        !report.to_string().contains("REPORT_SENTINEL"),
        "the JSON report carries aggregate data only"
    );

    // `--limit 2` keeps the first two groups in worst-first order, exit zero.
    let two = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--limit",
            "2",
            "--json",
        ],
    );
    assert_eq!(two.as_array().unwrap().len(), 2);
    assert_eq!(two[0]["about"], "src/store.rs");
    assert_eq!(two[1]["about"], "@@edge");

    // The existing limit law refuses a negative or over-ceiling `--limit`.
    for limit in ["-1", "1000001"] {
        let refused = fixture.run(
            &fixture.main,
            &[
                "attention",
                "list",
                "--check-report",
                "--limit",
                limit,
                "--json",
            ],
        );
        assert!(
            refusal_object(&refused).contains("--limit"),
            "limit {limit} must meet the existing limit refusal"
        );
    }

    // `--status` is refused naming the conflict: the report fixes status to
    // resolved. `--kind` still narrows the resolved set.
    let status = refusal_object(&fixture.run(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--status",
            "open",
            "--json",
        ],
    ));
    assert!(status.contains("--status"), "{status}");
    assert!(status.contains("--check-report"), "{status}");
    let risk = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--kind",
            "risk",
            "--json",
        ],
    );
    assert_eq!(
        risk,
        json!([
            {"about": "FAST_FLAG", "answered": 3, "correct": 3, "missed": 0, "missRate": 0},
        ])
    );

    // The row-shape flags are refused naming the report's fixed columns.
    for shape in [["--fields", "id"], ["--no-body", ""]] {
        let mut args = vec!["attention", "list", "--check-report", shape[0]];
        if !shape[1].is_empty() {
            args.push(shape[1]);
        }
        args.push("--json");
        let refused = refusal_object(&fixture.run(&fixture.main, &args));
        assert!(refused.contains(shape[0]), "{refused}");
        assert!(
            refused.contains("about, answered, correct, missed, miss-rate"),
            "{refused}"
        );
    }

    // Past 100 subjects with no `--limit`, the listing refuses naming it
    // (ADR-037); an explicit `--limit` takes the first N worst-first,
    // silently with exit zero.
    for index in 0..101 {
        raise_resolved_check(
            &fixture,
            &fixture.main,
            &format!("Bench comprehension row {index}."),
            &format!("bench/{index}.rs"),
            None,
            "beta",
        );
    }
    let capped = fixture.run(&fixture.main, &["attention", "list", "--check-report"]);
    assert!(!capped.status.success());
    let message = String::from_utf8_lossy(&capped.stderr).into_owned();
    assert!(
        message.contains("found more than 100 check subjects"),
        "{message}"
    );
    assert!(message.contains("--limit"), "{message}");
    let all = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--limit",
            "200",
            "--json",
        ],
    );
    assert_eq!(all.as_array().unwrap().len(), 105);
    let hundred = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--limit",
            "100",
            "--json",
        ],
    );
    assert_eq!(hundred.as_array().unwrap().len(), 100);

    // A board with no resolved checked rows prints its header with zero
    // groups — an empty array under `--json` — and exits zero.
    let vacant = Fixture::new("check-report-empty");
    vacant.ok_json(&vacant.main, &["init", "--name", "VACANT", "--json"]);
    let header = vacant.run(&vacant.main, &["attention", "list", "--check-report"]);
    assert!(header.status.success());
    assert_eq!(
        String::from_utf8_lossy(&header.stdout).as_ref(),
        "about answered correct missed miss-rate\n"
    );
    assert_eq!(
        vacant.ok_json(
            &vacant.main,
            &["attention", "list", "--check-report", "--json"]
        ),
        json!([])
    );
}

#[test]
fn att_list_check_report_fans_out_across_boards() {
    let fixture = Fixture::new("check-report-boards");
    fixture.ok_json(&fixture.main, &["init", "--name", "RPTA", "--json"]);
    fixture.ok_json(&fixture.worktree, &["init", "--name", "RPTB", "--json"]);
    raise_resolved_check(
        &fixture,
        &fixture.main,
        "First board comprehension.",
        "src/store.rs",
        None,
        "beta",
    );
    raise_resolved_check(
        &fixture,
        &fixture.worktree,
        "Second board comprehension.",
        "@@edge",
        None,
        "alpha",
    );
    // A plain report stays on its own board.
    let single = fixture.ok_json(
        &fixture.main,
        &["attention", "list", "--check-report", "--json"],
    );
    assert_eq!(
        single,
        json!([
            {"about": "src/store.rs", "answered": 1, "correct": 0, "missed": 1, "missRate": 100},
        ])
    );
    // `--all-boards` merges every readable board's resolved checked rows
    // through the same grouping, worst first.
    let merged = fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--all-boards",
            "--json",
        ],
    );
    assert_eq!(
        merged,
        json!([
            {"about": "src/store.rs", "answered": 1, "correct": 0, "missed": 1, "missRate": 100},
            {"about": "@@edge", "answered": 1, "correct": 1, "missed": 0, "missRate": 0},
        ])
    );
    // A board selector beside `--all-boards` is refused, as the search
    // listing refuses it.
    let refused = refusal_object(&fixture.run(
        &fixture.main,
        &[
            "attention",
            "list",
            "--check-report",
            "--all-boards",
            "--project",
            "RPTA",
            "--json",
        ],
    ));
    assert!(refused.contains("--all-boards"), "{refused}");
}

#[test]
fn compiled_binary_accepts_a_handoff_on_a_removed_task_and_archives_it() {
    let fixture = Fixture::new("handoff-accept-orphan");
    fixture.ok_json(&fixture.main, &["init", "--name", "Orphan", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Doomed work",
            "--id",
            "t-doomed",
            "--as",
            "owner",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-doomed", "--as", "owner", "--json"],
    );
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-doomed",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "owner",
            "--summary",
            "doomed summary",
            "--intent",
            "doomed intent",
            "--next-action",
            "doomed next",
            "--reason",
            "manual",
            "--json",
        ],
    );
    let handoff_id = handoff["id"].as_str().unwrap().to_owned();
    let removed = fixture.run(
        &fixture.main,
        &["task", "remove", "t-doomed", "--as", "owner"],
    );
    assert!(
        removed.status.success(),
        "removing the handoff's task failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    // The owner accepts the orphaned handoff as an acknowledgement: it
    // succeeds, and no lease is minted on the missing task.
    let accepted = fixture.ok_json(
        &fixture.main,
        &["handoff", "accept", &handoff_id, "--as", "owner", "--json"],
    );
    assert_eq!(accepted["handoff"]["status"], "accepted");
    assert_eq!(
        accepted["handoff"]["acceptedBy"],
        json!("owner"),
        "the acknowledgement lost its acceptor: {accepted}"
    );
    assert_eq!(
        accepted["claim"],
        Value::Null,
        "accepting a removed task minted a lease on a missing task: {accepted}"
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let claims: i64 = Connection::open(&board)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM task_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claims, 0, "accepting a removed task left a lease behind");
    // Once old, the archive sweep files the orphaned acknowledgement away.
    Connection::open(&board)
        .unwrap()
        .execute(
            "UPDATE handoffs SET created_at=1,accepted_at=1 WHERE id=?",
            [&handoff_id],
        )
        .unwrap();
    let swept = fixture.ok_json(
        &fixture.main,
        &[
            "archive",
            "--older-than-days",
            "1",
            "--as",
            "system@archive",
            "--json",
        ],
    );
    assert_eq!(
        swept["handoffs"], 1,
        "the sweep left the orphan pending: {swept}"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["handoff", "list", "--json"]),
        json!([])
    );
    let all = fixture.ok_json(&fixture.main, &["handoff", "list", "--all", "--json"]);
    assert_eq!(all[0]["id"], json!(handoff_id));
    assert_eq!(all[0]["archived"], true);
}

#[test]
fn compiled_binary_hides_an_orphan_deployment_and_doctor_reports_it() {
    let fixture = Fixture::new("orphan-deployment");
    fixture.ok_json(&fixture.main, &["init", "--name", "OrphanDeploy", "--json"]);
    let healthy = fixture.ok_json(
        &fixture.main,
        &[
            "deploy",
            "start",
            "--repo",
            "kanban",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--tier",
            "@_bdt",
            "--environment",
            "branch-dev-testing",
            "--host",
            "geoywsMBP",
            "--url",
            "http://localhost:9999",
            "--as",
            "owner",
            "--json",
        ],
    );
    let healthy_id = healthy["id"].as_str().unwrap().to_owned();
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    // A partial restore leaves a deployment naming a task with neither a live
    // row nor a removal record — a link no writer path creates.
    Connection::open(&board)
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             INSERT INTO deployments(id,task_id,repo,commit_sha,tier,environment,host,url,status,actor,capability_token,created_at,updated_at)
               VALUES('d-orphan','t-vanished','kanban','0123456789abcdef0123456789abcdef01234567','@_bdt','branch-dev-testing','geoywsMBP','http://localhost:9999','started','owner','token-orphan',1,1);",
        )
        .unwrap();
    // Outside enforcement every row stays visible, so the listing succeeds
    // and still shows both attempts; hiding the orphan under enforcement is
    // pinned at store level by
    // `managed_deployment_listing_hides_an_orphan_link_and_doctor_reports_it`.
    let listed = fixture.ok_json(&fixture.main, &["deploy", "list", "--json"]);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == healthy_id),
        "the listing lost its healthy attempt beside the orphan: {listed}"
    );
    assert!(
        listed.to_string().contains("t-vanished"),
        "the direct estate hid a row it must keep visible: {listed}"
    );
    // Doctor reports the dangling link and refuses to certify the board.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        !checked.status.success(),
        "doctor certified a board with a dangling task link"
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let links = report["projects"][0]["orphanedTaskLinks"].clone();
    assert!(
        links
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap()
                == "deployments row d-orphan references missing task t-vanished"),
        "doctor did not report the dangling task link: {links}"
    );
}

#[test]
fn compiled_binary_doctor_reports_a_nulled_row_from_a_reused_live_task_id() {
    let fixture = Fixture::new("reused-task-link");
    fixture.ok_json(&fixture.main, &["init", "--name", "ReusedLink", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/secret", "--as", "seed", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "the reused secret task",
            "--id",
            "t-reused",
            "--tag",
            "geoyws/secret",
            "--as",
            "seed",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "reusedlink body on the removed secret task",
            "--as",
            "seed",
            "--lane",
            "driver-1",
            "--repo",
            "/tmp/reused-link-probe",
            "--branch",
            "main",
            "--head",
            "0000000000000000000000000000000000000000",
            "--dirty",
            "clean",
            "--task",
            "t-reused",
            "--json",
        ],
    );
    let removed = fixture.run(
        &fixture.main,
        &["task", "remove", "t-reused", "--as", "seed"],
    );
    assert!(
        removed.status.success(),
        "could not remove the secret task: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    // What the reuse refusal now forbids, planted the way history left it: a
    // pre-V34 removal nulled the sitrep link, and the id was re-added before
    // the refusal existed, so the row is NULL-linked while its creation event
    // names a removed-but-live-again id. The stale search documents go first:
    // the delete trigger re-embeds surviving rows, so any re-INSERT would
    // otherwise collide with the history index on its own documents.
    let planted = Connection::open(&board).unwrap();
    planted
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
             UPDATE sitreps SET task_id=NULL WHERE task_id='t-reused';
             DELETE FROM search_documents WHERE task_id='t-reused';
             INSERT INTO tasks(id,type,title,status,created_at,updated_at)
               VALUES('t-reused','task','reused incarnation','todo',1,1);",
        )
        .unwrap();
    let sitrep_id: String = planted
        .query_row("SELECT id FROM sitreps", [], |row| row.get(0))
        .unwrap();
    drop(planted);
    // The plant is raw SQL, so it bypasses the inline embedding writer and
    // leaves unembedded documents behind; rebuilding restores the "otherwise
    // healthy" premise without touching the residual itself.
    fixture.ok_json(&fixture.main, &["search-rebuild", "--as", "seed", "--json"]);
    // The old key stays quiet — the id has a removal record — and the new key
    // names the row for the owner to review. The residual is advisory: no verb
    // can clear it, so it does not affect `healthy` or the exit code.
    let checked = fixture.run(&fixture.main, &["doctor", "--json"]);
    assert!(
        checked.status.success(),
        "doctor failed on an otherwise healthy board over the advisory reused-task residual: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let report: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert!(
        report["healthy"].as_bool().unwrap(),
        "doctor marked an otherwise healthy board unhealthy over the advisory residual: {report}"
    );
    assert!(
        report["projects"][0]["orphanedTaskLinks"]
            .as_array()
            .unwrap()
            .is_empty(),
        "the reused id was reported as an orphan: {}",
        report["projects"][0]["orphanedTaskLinks"]
    );
    let links = report["projects"][0]["reusedTaskLinks"].clone();
    assert!(
        links
            .as_array()
            .unwrap()
            .iter()
            .any(|line| line.as_str().unwrap()
                == format!("sitreps row {sitrep_id} nulled from reused task t-reused")),
        "doctor did not report the nulled row from the reused task id: {links}"
    );
}

/// A fresh board holding one `todo` row `t-lane` assigned to `assignee`, with
/// no `lane` column, so no `--lane`/`--role` filter removes it
/// (docs/specs/claim-routing.md §4).
fn claim_routing_board(label: &str, assignee: &str) -> Fixture {
    let fixture = Fixture::new(label);
    fixture.ok_json(&fixture.main, &["init", "--name", "ROUTE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Sweep the logs",
            "--id",
            "t-lane",
            "--assignee",
            assignee,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    fixture
}

fn claim_candidate_ids(fixture: &Fixture, agent: &str, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["claim", "--candidates", "--as", agent];
    args.extend_from_slice(extra);
    args.push("--json");
    fixture
        .ok_json(&fixture.main, &args)
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The board a refused claim leaves behind: still `todo`, assignee unchanged,
/// no lease and no `task_claimed` event.
fn assert_claim_wrote_nothing(fixture: &Fixture, assignee: &str) {
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-lane", "--json"]);
    assert_eq!(
        shown["status"], "todo",
        "a refused claim moved the row: {shown}"
    );
    assert_eq!(
        shown["assignee"], assignee,
        "a refused claim retargeted the row: {shown}"
    );
    assert!(
        shown["claim"].is_null(),
        "a refused claim left a lease: {shown}"
    );
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "t-lane", "--json"]);
    assert!(
        !events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "task_claimed"),
        "a refused claim appended task_claimed: {events}"
    );
}

/// CLAIM-01: bare, harness and typed spellings of one lane are one worker, in
/// both directions, and lane-word lookalikes are not.
#[test]
fn claim_routing_treats_bare_harness_and_typed_spellings_as_one_lane() {
    let lane = vec!["t-lane".to_owned()];
    let typed = claim_routing_board("claim-routing-typed", "@:px/px/driver-2");
    for agent in [
        "driver-2",
        "claude@driver-2",
        "codex@driver-2",
        "@:px/px/driver-2",
    ] {
        assert_eq!(
            claim_candidate_ids(&typed, agent, &[]),
            lane,
            "{agent} did not see its own lane's row assigned to @:px/px/driver-2"
        );
    }
    for agent in [
        "claude@driver-20",
        "claude@driver-02",
        "driver-two",
        "claude@driverless",
    ] {
        assert!(
            claim_candidate_ids(&typed, agent, &[]).is_empty(),
            "{agent} is not lane driver-2 but saw its row"
        );
    }
    let harness = claim_routing_board("claim-routing-harness", "claude@driver-2");
    for agent in ["driver-2", "codex@driver-2", "@:kanban/kanban/driver-2"] {
        assert_eq!(
            claim_candidate_ids(&harness, agent, &[]),
            lane,
            "{agent} did not see its own lane's row assigned to claude@driver-2"
        );
    }
    let trunk = claim_routing_board("claim-routing-trunk", "driver");
    assert_eq!(claim_candidate_ids(&trunk, "codex@driver", &[]), lane);
    assert!(claim_candidate_ids(&trunk, "codex@driver-2", &[]).is_empty());
}

/// CLAIM-02: where either side names no lane, only the byte-identical string
/// is the same worker.
#[test]
fn claim_routing_falls_back_to_exact_strings_without_a_lane() {
    let lane = vec!["t-lane".to_owned()];
    for assignee in [
        "geoyws",
        "superdriver",
        "a@b@driver-2",
        "@:px/px/superdriver",
    ] {
        let fixture = claim_routing_board("claim-routing-exact", assignee);
        assert_eq!(claim_candidate_ids(&fixture, assignee, &[]), lane);
        for agent in ["driver-2", "claude@driver-2", "@:px/px/driver-2", "Geoyws"] {
            assert!(
                claim_candidate_ids(&fixture, agent, &[]).is_empty(),
                "{agent} saw a row assigned to the lane-less {assignee}"
            );
        }
        let refused = fixture.run(
            &fixture.main,
            &["claim", "t-lane", "--as", "claude@driver-2", "--json"],
        );
        assert_eq!(
            refusal_object(&refused),
            format!("task t-lane is assigned to {assignee}")
        );
        assert_claim_wrote_nothing(&fixture, assignee);
    }
    let lane_side = claim_routing_board("claim-routing-lane-vs-name", "driver-2");
    assert!(claim_candidate_ids(&lane_side, "geoyws", &[]).is_empty());
}

/// CLAIM-03: two typed forms for the same lane word in different estates are
/// different workers.
#[test]
fn claim_routing_refuses_a_typed_lane_from_another_board() {
    let fixture = claim_routing_board("claim-routing-estate", "@:px/px/driver-2");
    for agent in [
        "@:other/kanban/driver-2",
        "@:px/kanban/driver-2",
        "@:other/px/driver-2",
    ] {
        assert!(
            claim_candidate_ids(&fixture, agent, &[]).is_empty(),
            "{agent} saw another estate's row"
        );
        let refused = fixture.run(&fixture.main, &["claim", "t-lane", "--as", agent, "--json"]);
        assert_eq!(
            refusal_object(&refused),
            "task t-lane is assigned to @:px/px/driver-2"
        );
    }
    assert_claim_wrote_nothing(&fixture, "@:px/px/driver-2");
}

/// CLAIM-04: the measured case — a lane asking in its harness spelling sees and
/// is handed its lane's row.
#[test]
fn claim_candidates_show_same_lane_rows_to_the_callers_own_lane() {
    let fixture = claim_routing_board("claim-routing-candidates", "@:px/px/driver-2");
    assert_eq!(
        claim_candidate_ids(&fixture, "claude@driver-2", &[]),
        vec!["t-lane".to_owned()]
    );
    let next = fixture.ok_json(
        &fixture.main,
        &["claim", "--next", "--as", "claude@driver-2", "--json"],
    );
    assert_eq!(next["taskID"], "t-lane");
}

/// CLAIM-05: a named claim takes a same-lane row, and the model check still
/// refuses first.
#[test]
fn named_claim_takes_a_same_lane_row() {
    let bare = claim_routing_board("claim-routing-named-bare", "@:px/px/driver-2");
    let claimed = bare.ok_json(
        &bare.main,
        &["claim", "t-lane", "--as", "driver-2", "--json"],
    );
    assert_eq!(claimed["taskID"], "t-lane");

    let fixture = Fixture::new("claim-routing-named-order");
    fixture.ok_json(&fixture.main, &["init", "--name", "ROUTE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "restricted",
            "--id",
            "t-lane",
            "--assignee",
            "@:px/px/driver-2",
            "--allowed-model",
            "Astra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let model_first = fixture.run(
        &fixture.main,
        &["claim", "t-lane", "--as", "claude@driver-3", "--json"],
    );
    assert_eq!(
        refusal_object(&model_first),
        "task t-lane is restricted to models [Astra]; pass --model with one of them to claim it"
    );
    let claimed = fixture.ok_json(
        &fixture.main,
        &[
            "claim",
            "t-lane",
            "--as",
            "claude@driver-2",
            "--model",
            "Astra",
            "--json",
        ],
    );
    assert_eq!(claimed["taskID"], "t-lane");
}

/// CLAIM-06: a different lane, or a lane-less caller, is refused in the
/// existing words and the board is untouched.
#[test]
fn named_claim_refuses_a_different_lane_in_the_existing_words() {
    let fixture = claim_routing_board("claim-routing-other-lane", "@:px/px/driver-2");
    for agent in ["claude@driver-3", "codex@driver", "driver-3", "geoyws"] {
        let refused = fixture.run(&fixture.main, &["claim", "t-lane", "--as", agent, "--json"]);
        assert_eq!(
            refusal_object(&refused),
            "task t-lane is assigned to @:px/px/driver-2",
            "{agent} was not refused in the existing words"
        );
        assert!(
            claim_candidate_ids(&fixture, agent, &[]).is_empty(),
            "{agent} was offered another lane's row"
        );
        let next = fixture.run(&fixture.main, &["claim", "--next", "--as", agent, "--json"]);
        assert!(
            !String::from_utf8_lossy(&next.stdout).contains("t-lane"),
            "{agent} was handed another lane's row by --next"
        );
    }
    assert_claim_wrote_nothing(&fixture, "@:px/px/driver-2");
}

/// CLAIM-07: `--allow-reassign` still bypasses the assignee gate for every
/// spelling, on both paths.
#[test]
fn allow_reassign_still_bypasses_every_assignee_spelling() {
    for assignee in ["@:px/px/driver-2", "claude@driver-2", "superdriver"] {
        let fixture = claim_routing_board("claim-routing-reassign", assignee);
        assert_eq!(
            claim_candidate_ids(&fixture, "claude@driver-3", &["--allow-reassign"]),
            vec!["t-lane".to_owned()],
            "--allow-reassign did not offer the row assigned to {assignee}"
        );
        let claimed = fixture.ok_json(
            &fixture.main,
            &[
                "claim",
                "t-lane",
                "--as",
                "claude@driver-3",
                "--allow-reassign",
                "--json",
            ],
        );
        assert_eq!(claimed["taskID"], "t-lane");
    }
}

/// CLAIM-08: a successful same-lane claim stores the caller's own string,
/// byte-for-byte, not a canonical spelling.
#[test]
fn successful_claim_stores_the_caller_string_verbatim() {
    let fixture = claim_routing_board("claim-routing-verbatim", "@:px/px/driver-2");
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-lane", "--as", "claude@driver-2", "--json"],
    );
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-lane", "--json"]);
    assert_eq!(shown["assignee"], "claude@driver-2");
    assert_eq!(shown["status"], "in_progress");
}

/// One board named `name` in its own fixture, and a `deploy start` against it
/// (docs/specs/deploy.md §4 `START`).
fn deploy_tier_board(test: &str, name: &str) -> Fixture {
    let fixture = Fixture::new(&format!("deploy-{test}-{}", name.to_lowercase()));
    fixture.ok_json(&fixture.main, &["init", "--name", name, "--json"]);
    fixture
}

fn deploy_tier_start(fixture: &Fixture, tier: &str, host: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "deploy",
        "start",
        "--repo",
        "geoyws/example",
        "--commit",
        "1111111111111111111111111111111111111111",
        "--tier",
        tier,
        "--environment",
        "env",
        "--host",
        host,
        "--url",
        "https://x",
        "--as",
        "e2e",
    ];
    args.extend_from_slice(extra);
    args.push("--json");
    fixture.run(&fixture.main, &args)
}

fn deploy_tier_accepted(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "deploy start was refused: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn deploy_tier_attempt_count(fixture: &Fixture) -> usize {
    fixture
        .ok_json(&fixture.main, &["deploy", "list", "--all", "--json"])
        .as_array()
        .unwrap()
        .len()
}

/// DEPLOY-05, DEPLOY-07, DEPLOY-08: a Unum or geoyws board records the dev
/// tiers on `hax`; a replay still returns before the pairing check; the board
/// schema does not move.
#[test]
fn deploy_start_accepts_dev_tiers_on_hax_for_unum_and_geoyws_boards() {
    for board in ["kanban", "acies", "unum", "unum-web"] {
        let fixture = deploy_tier_board("hax-ok", board);
        for tier in ["@_bdt", "@_bd"] {
            let started = deploy_tier_accepted(&deploy_tier_start(&fixture, tier, "hax", &[]));
            let attempt = &started;
            assert_eq!(attempt["host"], "hax", "{board} {tier}: {started}");
            assert_eq!(attempt["tier"], tier, "{board} {tier}: {started}");
        }
    }

    let fixture = deploy_tier_board("hax-ok", "kanban");
    // `init` takes the owner's open, so the board is already past the CROSS
    // step; the property below is that the deploy attempts do not move it
    // from wherever `init` left it (ADR-056 §5).
    let before = Connection::open(board_path_for_project(&fixture, &fixture.main, "kanban"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    let first = deploy_tier_accepted(&deploy_tier_start(
        &fixture,
        "@_bdt",
        "hax",
        &["--operation-id", "op-1"],
    ));
    let again = deploy_tier_accepted(&deploy_tier_start(
        &fixture,
        "@_bdt",
        "hax",
        &["--operation-id", "op-1"],
    ));
    assert_eq!(again["idempotentReplay"], true, "{again}");
    assert_eq!(again["id"], first["id"]);
    assert_eq!(deploy_tier_attempt_count(&fixture), 1);

    let board = board_path_for_project(&fixture, &fixture.main, "kanban");
    let schema = Connection::open(board)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .unwrap();
    assert_eq!(
        schema, before,
        "a dev-tier attempt on hax moved the board schema"
    );
}

/// DEPLOY-06: an IFCA board, and a board no estate claims, keep the dev tiers
/// off `hax`, in exactly the words the slice states, and write nothing.
#[test]
fn deploy_start_refuses_dev_tiers_on_hax_for_ifca_and_unmapped_boards() {
    for (board, why) in [
        ("px", "is in estate ifca"),
        ("prjx-root", "is in estate ifca"),
        ("TIERHOST", "maps to no estate"),
    ] {
        let fixture = deploy_tier_board("hax-no", board);
        for tier in ["@_bdt", "@_bd"] {
            assert_eq!(
                refusal_object(&deploy_tier_start(&fixture, tier, "hax", &[])),
                format!(
                    "tier {tier} on host hax is a dev tier for the unum and geoyws estates only; board {board} {why}, so deploy it from geoywsMBP (or geoywsMBA)"
                )
            );
        }
        assert_eq!(
            deploy_tier_attempt_count(&fixture),
            0,
            "a refused dev-tier start on hax wrote an attempt for {board}"
        );
    }
    // Byte-exact host: another spelling of hax is an ordinary Hetzner host.
    let fixture = deploy_tier_board("hax-no", "kanban");
    assert_eq!(
        refusal_object(&deploy_tier_start(&fixture, "@_bdt", "HAX", &[])),
        "tier @_bdt is an MBP tier (canonical row \"@_bdt -> geoywsMBP\"), but host is HAX; deploy it from geoywsMBP (or geoywsMBA)"
    );
}

/// DEPLOY-02, DEPLOY-03: every other pairing refusal is byte-identical to the
/// baseline, for a board the hax exception would otherwise favour.
#[test]
fn deploy_start_keeps_the_mbp_tier_refusal_off_hax_in_the_same_words() {
    let fixture = deploy_tier_board("same-words", "kanban");
    for tier in ["@_bdt", "@_bd"] {
        assert_eq!(
            refusal_object(&deploy_tier_start(&fixture, tier, "hig", &[])),
            format!(
                "tier {tier} is an MBP tier (canonical row \"{tier} -> geoywsMBP\"), but host is hig; deploy it from geoywsMBP (or geoywsMBA)"
            )
        );
    }
    for host in ["geoywsMBP", "geoywsMBA"] {
        assert_eq!(
            refusal_object(&deploy_tier_start(&fixture, "@_p", host, &[])),
            format!(
                "tier @_p is a Hetzner tier (canonical row \"@_p -> Hetzner host\"), but host is {host}; deploy it from a Hetzner host (e.g. hax or hig)"
            )
        );
    }
    assert_eq!(deploy_tier_attempt_count(&fixture), 0);
}

/// DEPLOY-01, DEPLOY-04: an IFCA board still records the dev tiers on the MBP
/// and the Hetzner tiers on hax.
#[test]
fn deploy_start_keeps_mbp_and_hetzner_pairings_for_an_ifca_board() {
    let fixture = deploy_tier_board("ifca-pairs", "px");
    for (tier, host) in [
        ("@_bdt", "geoywsMBP"),
        ("@_bd", "geoywsMBA"),
        ("@_p", "hax"),
        ("@_uat", "hig"),
    ] {
        let started = deploy_tier_accepted(&deploy_tier_start(&fixture, tier, host, &[]));
        assert_eq!(started["host"], host, "{tier} {host}");
    }
    assert_eq!(deploy_tier_attempt_count(&fixture), 4);
}
