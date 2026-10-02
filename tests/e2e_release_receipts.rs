//! Compiled-process E2E for `scripts/hig-release.sh`: activation and
//! provenance receipts, format-version-1 packages, reactivation, and more
//! managed-destination and install-root guards.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

/// The activation receipt is the record that a release ACTIVATED, so it is
/// written after the proof. The remote installer wrote it before, numbered
/// with a sequence the store hands out once, and the rollback deleted it only
/// when this install had created the release directory - so an activation onto
/// a directory that was already there left a receipt for an activation that
/// never happened, and `kb`'s own release listing counted it. The local
/// installer already wrote after the proof; both paths are measured here so
/// they cannot drift apart again.
#[test]
fn hig_release_script_install_writes_no_activation_receipt_when_the_proof_fails() {
    let harness = ReleaseGuardHarness::new("hig-release-receipt-order");
    let commit_a = "0123456789abcdef0123456789abcdef0000da01";
    let commit_b = "0123456789abcdef0123456789abcdef0000db02";
    let first = harness.fixture.root.join("receipt-a");
    clone_release_package(&harness.package_dir, &first, commit_a);
    let second = harness.fixture.root.join("receipt-b");
    clone_release_package(&harness.package_dir, &second, commit_b);
    let release_id_a = release_id_from_package(&first);
    let release_id_b = release_id_from_package(&second);
    for target in ["hax", "hig"] {
        let hax_for = |package: &Path, commit: &str, label: &str| {
            if target == "hig" {
                install_matching_hax_package(&harness.hax_context(), package, commit, label)
            } else {
                harness.hax_install_root.clone()
            }
        };
        let install_root = harness.fixture.root.join(format!("receipt-{target}"));
        let bin_dir = harness.fixture.root.join(format!("receipt-bin-{target}"));
        let current_link = install_root.join("current");
        let sequence_path = install_root.join("releases/.activation-sequence");
        let receipt_b = install_root
            .join("releases")
            .join(format!("{release_id_b}.receipt.json"));

        // A and B both activate on a host with no unit, then the operator
        // rolls back to A: B's directory is retained, and its receipt is the
        // one thing a later activation of B must write for itself.
        let hax_a = hax_for(&first, commit_a, &format!("receipt-a-{target}"));
        let activated_a = harness.install_from(target, &first, &hax_a, &install_root, &bin_dir);
        assert!(
            activated_a.status.success(),
            "{target}: release A failed to install: {}",
            String::from_utf8_lossy(&activated_a.stderr)
        );
        let hax_b = hax_for(&second, commit_b, &format!("receipt-b-{target}"));
        let activated_b = harness.install_from(target, &second, &hax_b, &install_root, &bin_dir);
        assert!(
            activated_b.status.success(),
            "{target}: release B failed to install: {}",
            String::from_utf8_lossy(&activated_b.stderr)
        );
        let rolled_back = harness
            .command()
            .args([
                "rollback",
                target,
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            rolled_back.status.success(),
            "{target}: rollback to release A failed: {}",
            String::from_utf8_lossy(&rolled_back.stderr)
        );
        fs::remove_file(&receipt_b).unwrap();
        let sequence_before = fs::read_to_string(&sequence_path).unwrap();

        // B activates again onto the directory it left behind, and the
        // activation is refused after `current` already switched.
        let refused = harness
            .install_command(target, &second, &hax_b, &install_root, &bin_dir)
            .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
            .output()
            .unwrap();
        assert!(
            !refused.status.success(),
            "{target}: a refused activation went green\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert!(
            !receipt_b.exists(),
            "{target}: a refused activation left a receipt for an activation that never happened"
        );
        assert_eq!(
            fs::read_to_string(&sequence_path).unwrap(),
            sequence_before,
            "{target}: a refused activation burned an activation sequence number"
        );
        assert!(
            install_root.join("releases").join(&release_id_b).is_dir(),
            "{target}: the rollback deleted a release directory this install did not create"
        );
        assert_eq!(
            fs::read_link(&current_link).unwrap(),
            install_root.join("releases").join(&release_id_a),
            "{target}: current does not point at release A"
        );
    }
}

/// Cleanup failure must retain the old directory's index, survive a later
/// successful removal, and leave a proved activation and its summary intact.
#[test]
fn hig_release_script_prune_failure_keeps_index_and_committed_summary() {
    let harness = ReleaseGuardHarness::new("hig-release-prune-failure");
    write_executable(
        &harness.hostname_bin.parent().unwrap().join("rm"),
        r#"#!/bin/sh
set -eu
for path in "$@"; do
  if [ "$path" = "$FAKE_PRUNE_FAIL_DIR" ]; then
    printf 'injected release-directory removal failure: %s\n' "$path" >&2
    exit 9
  fi
done
command -p rm "$@"
"#,
    );
    let release_id = release_id_from_package(&harness.package_dir);
    for operation in ["hax", "hig", "rollback"] {
        let install_root = harness.fixture.root.join(format!("prune-{operation}"));
        let bin_dir = harness.fixture.root.join(format!("prune-bin-{operation}"));
        let releases = install_root.join("releases");
        let candidate = releases.join(&release_id);
        let candidate_receipt = releases.join(format!("{release_id}.receipt.json"));
        if operation == "rollback" {
            let staged = harness
                .install_command(
                    "hax",
                    &harness.package_dir,
                    &harness.hax_install_root,
                    &install_root,
                    &bin_dir,
                )
                .env("FAKE_PRUNE_FAIL_DIR", "")
                .output()
                .unwrap();
            assert!(
                staged.status.success(),
                "{}",
                String::from_utf8_lossy(&staged.stderr)
            );
            let mut receipt: Value =
                serde_json::from_slice(&fs::read(&candidate_receipt).unwrap()).unwrap();
            receipt["activationSequence"] = json!(12);
            fs::write(&candidate_receipt, serde_json::to_vec(&receipt).unwrap()).unwrap();
        }
        fs::create_dir_all(&releases).unwrap();
        let old_id = |sequence: u64| format!("{sequence:040x}-{sequence:064x}");
        // Retention only reads these old entries' identity and ordering;
        // the candidate is activated and validated as a complete package.
        for sequence in 1..=11 {
            let id = old_id(sequence);
            fs::create_dir(releases.join(&id)).unwrap();
            fs::write(releases.join(&id).join("retained"), b"keep until removed").unwrap();
            fs::write(
                releases.join(format!("{id}.receipt.json")),
                serde_json::to_vec(&json!({
                    "releaseId": id,
                    "activationSequence": sequence,
                    "installedAt": sequence,
                }))
                .unwrap(),
            )
            .unwrap();
        }
        fs::write(
            releases.join(".activation-sequence"),
            if operation == "rollback" {
                "12\n"
            } else {
                "11\n"
            },
        )
        .unwrap();
        let failed_dir = releases.join(old_id(2));
        let failed_receipt = releases.join(format!("{}.receipt.json", old_id(2)));
        let before = fs::read(&failed_receipt).unwrap();
        let mut command = if operation == "rollback" {
            let mut command = harness.command();
            // Reactivation here exercises the rollback's shared pruning path.
            command.args([
                "rollback",
                "hax",
                "--steps",
                "0",
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ]);
            command
        } else {
            harness.install_command(
                operation,
                &harness.package_dir,
                &harness.hax_install_root,
                &install_root,
                &bin_dir,
            )
        };
        let output = command
            .env("FAKE_PRUNE_FAIL_DIR", &failed_dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{operation}: pruning failure went green: {stderr}"
        );
        let summary: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!("{operation}: committed failure lost its structured summary: {error}\n{stderr}")
        });
        assert_eq!(summary["releaseDir"], candidate.to_str().unwrap());
        // The install summary carries its activation receipt; the rollback
        // summary carries the release it reactivated instead. Either way the
        // committed summary names the candidate it kept.
        if operation == "rollback" {
            assert_eq!(
                summary["releaseId"],
                json!(release_id),
                "{operation}: the committed summary does not name its release"
            );
        } else {
            assert_eq!(
                summary["receipt"],
                candidate_receipt.to_str().unwrap(),
                "{operation}: the committed summary does not carry its activation receipt"
            );
        }
        assert_release_view(&install_root, &bin_dir, &candidate);
        assert!(candidate.join("kanban").is_file());
        let receipt: Value =
            serde_json::from_slice(&fs::read(&candidate_receipt).unwrap()).unwrap();
        assert_eq!(receipt["releaseId"], release_id);
        assert_eq!(
            fs::read(&failed_receipt).unwrap(),
            before,
            "{operation}: failed directory lost its index"
        );
        assert_eq!(
            fs::read(failed_dir.join("retained")).unwrap(),
            b"keep until removed"
        );
        // A successful removal follows the failed one. It must not erase the
        // aggregate failure status or keep unrelated old entries needlessly.
        assert!(!releases.join(old_id(1)).exists());
        assert!(
            !releases
                .join(format!("{}.receipt.json", old_id(1)))
                .exists()
        );
        assert!(
            stderr.contains("injected release-directory removal failure"),
            "{stderr}"
        );
        assert!(
            stderr.contains("pruning older releases") && stderr.contains("failed"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("recovery failed"),
            "{operation}: committed activation was rolled back: {stderr}"
        );
    }
}

/// A release's identity is derived, never carried: the id is
/// `sourceCommit-manifestSha256`, and every activation field belongs to the
/// installer that writes it. Before the guard this pins, a package receipt
/// could carry a `releaseId` of its own and a manifest could carry
/// `activationSequence`, and the installer neither refused nor honoured them:
/// the activation receipt is `$receipt + {releaseId: ..., activationSequence:
/// ...}`, so the installer's fields silently won the merge. An operator
/// reading `jq -r .releaseId` off the package receipt was told one release
/// while the store kept another, and the store was one reordered `+` away
/// from honouring the forgery instead.
#[test]
fn hig_release_script_refuses_a_package_that_carries_the_release_identity_it_derives() {
    let harness = ReleaseGuardHarness::new("hig-release-carried-identity");
    let commit = |tag: u32| format!("0123456789abcdef0123456789abcdef{tag:08x}");
    let patch = |path: &PathBuf, key: &str, value: Value| {
        let mut document: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        document[key] = value;
        fs::write(path, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    };

    // An id that does not derive from the package's own fields.
    let named_id = harness.fixture.root.join("carries-a-release-id");
    clone_release_package(&harness.package_dir, &named_id, &commit(0xa1));
    patch(
        &named_id.with_extension("receipt.json"),
        "releaseId",
        json!(format!("{}-{}", commit(0xdead), "0".repeat(64))),
    );

    // An activation field on the receipt.
    let receipt_activation = harness.fixture.root.join("receipt-carries-activation");
    clone_release_package(&harness.package_dir, &receipt_activation, &commit(0xa2));
    patch(
        &receipt_activation.with_extension("receipt.json"),
        "activationSequence",
        json!(9999),
    );

    // An activation field on the manifest, with the receipt's manifest hash
    // repaired so the only thing wrong with the package is the carried field.
    let manifest_activation = harness.fixture.root.join("manifest-carries-activation");
    clone_release_package(&harness.package_dir, &manifest_activation, &commit(0xa3));
    patch(
        &manifest_activation.join("manifest.json"),
        "installedAt",
        json!(1),
    );
    patch(
        &manifest_activation.with_extension("receipt.json"),
        "manifestSha256",
        json!(file_sha256(&manifest_activation.join("manifest.json"))),
    );

    let forgeries = [
        ("a receipt naming a release it is not", &named_id, "receipt"),
        (
            "a receipt carrying an activation field",
            &receipt_activation,
            "receipt",
        ),
        (
            "a manifest carrying an activation field",
            &manifest_activation,
            "manifest",
        ),
    ];
    for target in ["hax", "hig"] {
        for (label, package, artifact) in forgeries {
            let artifact = match artifact {
                "receipt" => package.with_extension("receipt.json"),
                _ => package.join("manifest.json"),
            };
            let slug = package.file_name().unwrap().to_str().unwrap();
            let install_root = harness.fixture.root.join(format!("{slug}-{target}"));
            let bin_dir = harness.fixture.root.join(format!("{slug}-bin-{target}"));
            harness.assert_package_refused_without_mutation(
                target,
                package,
                &harness.hax_install_root,
                &install_root,
                &bin_dir,
                &format!(
                    "refusing {}: a release artifact must not carry the release identity the installer derives",
                    artifact.display()
                ),
                &[&install_root, &bin_dir, package],
            );
            assert!(
                fs::symlink_metadata(&install_root).is_err(),
                "{target}: {label} created an install root"
            );
        }
    }

    // The guard is about the claim, not the key: a receipt that names the
    // release it actually becomes still installs, so this refuses forgery
    // rather than banning provenance an operator may legitimately record.
    let truthful = harness.fixture.root.join("carries-its-own-id");
    clone_release_package(&harness.package_dir, &truthful, &commit(0xa4));
    let truthful_id = release_id_from_package(&truthful);
    patch(
        &truthful.with_extension("receipt.json"),
        "releaseId",
        json!(truthful_id),
    );
    let install_root = harness.fixture.root.join("carries-its-own-id-install");
    let bin_dir = harness.fixture.root.join("carries-its-own-id-bin");
    let installed = harness.install_from(
        "hax",
        &truthful,
        &harness.hax_install_root,
        &install_root,
        &bin_dir,
    );
    assert!(
        installed.status.success(),
        "a truthful releaseId was refused: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
    let meta: Value =
        serde_json::from_slice(&fs::read(installed["receipt"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(meta["releaseId"], json!(truthful_id));
}

/// One row of a provenance case: the label it reports under, and the single
/// edit it makes to a receipt a real packaging run wrote.
type ReceiptEdit = (&'static str, fn(&mut Value));

/// Provenance is only worth reading if a receipt cannot claim a build that
/// never happened, so every validator refuses the shapes no build could have
/// produced - a native build on a machine that is not the artifact's own
/// platform, a native build naming an image, a containerised build with no
/// digest-pinned image, a version probe claiming to have executed the
/// artifact on a machine that cannot execute it - and refuses a receipt
/// missing any of the six fields outright.
///
/// What it does NOT refuse is a hostname it does not recognise: `host` is a
/// record and never an authorization, which is the whole difference from the
/// v1 receipt that asserted the literal "hax" back. The accepted rows install
/// on both legs to prove the readable shapes really read, including the
/// `versionProbe: "native"` receipt that only a linux x86-64 build host can
/// write and that this machine can therefore only verify by reading one.
#[test]
fn hig_release_script_install_refuses_a_receipt_whose_provenance_could_not_have_been_produced() {
    let harness = ReleaseGuardHarness::new("hig-release-provenance-invariants");
    let forge = |tag: u32, label: &str, patch: &dyn Fn(&mut Value)| -> PathBuf {
        let package = harness.fixture.root.join(format!("package-{label}"));
        clone_release_package(
            &harness.package_dir,
            &package,
            &format!("0123456789abcdef0123456789abcdef{tag:08x}"),
        );
        let receipt_path = package.with_extension("receipt.json");
        let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        patch(&mut receipt);
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        package
    };

    let refusals: [ReceiptEdit; 18] = [
        ("missing-build-platform", |r| {
            r.as_object_mut().unwrap().remove("buildPlatform").unwrap();
        }),
        ("missing-artifact-platform", |r| {
            r.as_object_mut()
                .unwrap()
                .remove("artifactPlatform")
                .unwrap();
        }),
        ("missing-build-kind", |r| {
            r.as_object_mut().unwrap().remove("buildKind").unwrap();
        }),
        // The key is always present, so removing it is not the same edit as
        // setting it to null and must not be read as one.
        ("missing-builder-image", |r| {
            r.as_object_mut().unwrap().remove("builderImage").unwrap();
        }),
        ("missing-toolchain", |r| {
            r.as_object_mut().unwrap().remove("toolchain").unwrap();
        }),
        ("missing-version-probe", |r| {
            r.as_object_mut().unwrap().remove("versionProbe").unwrap();
        }),
        // A native build on a machine that is not linux x86-64 is not
        // representable: this receipt was written on darwin-arm64.
        ("native-on-a-foreign-platform", |r| {
            r["buildKind"] = json!("native");
            r["builderImage"] = json!(null);
        }),
        ("native-naming-an-image", |r| {
            r["buildKind"] = json!("native");
            r["buildPlatform"] = json!("linux-x86_64");
        }),
        ("container-without-an-image", |r| {
            r["builderImage"] = json!(null);
        }),
        ("container-with-a-tag", |r| {
            r["builderImage"] = json!("rust:1.90-bookworm");
        }),
        ("container-with-a-short-digest", |r| {
            r["builderImage"] = json!(format!("rust@sha256:{}", "a".repeat(63)));
        }),
        // The artifact platform is earned by measuring every packaged file,
        // so it is the one value a receipt may not simply state.
        ("foreign-artifact-platform", |r| {
            r["artifactPlatform"] = json!("darwin-arm64");
        }),
        ("unknown-build-kind", |r| {
            r["buildKind"] = json!("cross");
        }),
        ("unknown-version-probe", |r| {
            r["versionProbe"] = json!("guess");
        }),
        // A host that cannot execute the artifact cannot have produced a
        // native version string for it.
        ("native-probe-on-a-foreign-platform", |r| {
            r["versionProbe"] = json!("native");
        }),
        ("toolchain-with-an-extra-key", |r| {
            r["toolchain"]["linker"] = json!("lld");
        }),
        ("toolchain-with-an-empty-rustc", |r| {
            r["toolchain"]["rustc"] = json!("");
        }),
        ("host-that-is-not-a-hostname", |r| {
            r["host"] = json!("two words");
        }),
    ];
    for (index, (label, patch)) in refusals.iter().enumerate() {
        let package = forge(index as u32 + 1, label, patch);
        let install_root = harness.fixture.root.join(format!("install-{label}"));
        let bin_dir = harness.fixture.root.join(format!("bin-{label}"));
        harness.assert_package_refused_without_mutation(
            "hax",
            &package,
            &harness.hax_install_root,
            &install_root,
            &bin_dir,
            "package receipt is incomplete or mismatched",
            &[&install_root, &bin_dir],
        );
    }

    let accepted: [ReceiptEdit; 2] = [
        // A name this estate has never heard of, on a real release: the
        // record is kept and nothing is authorized by it.
        ("another-host", |r| {
            r["host"] = json!("geoywsMBP");
        }),
        // The shape a hax build writes, which this machine can read but
        // cannot produce: it is linux x86-64, so it builds natively, uses no
        // image, and asks the artifact itself for its version.
        ("a-native-linux-build", |r| {
            r["buildKind"] = json!("native");
            r["buildPlatform"] = json!("linux-x86_64");
            r["builderImage"] = json!(null);
            r["versionProbe"] = json!("native");
        }),
    ];
    for (index, (label, patch)) in accepted.iter().enumerate() {
        let package = forge(100 + index as u32, label, patch);
        let hax_root = harness.fixture.root.join(format!("hax-{label}"));
        let hax_bin = harness.fixture.root.join(format!("hax-bin-{label}"));
        let installed = harness.install_from("hax", &package, &hax_root, &hax_root, &hax_bin);
        assert!(
            installed.status.success(),
            "{label}: a readable v2 receipt was refused\nstderr: {}",
            String::from_utf8_lossy(&installed.stderr)
        );
        let receipt: Value =
            serde_json::from_slice(&fs::read(package.with_extension("receipt.json")).unwrap())
                .unwrap();
        let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
        let activation: Value =
            serde_json::from_slice(&fs::read(installed["receipt"].as_str().unwrap()).unwrap())
                .unwrap();
        assert_eq!(activation["formatVersion"], json!(2));
        for field in [
            "host",
            "buildPlatform",
            "artifactPlatform",
            "buildKind",
            "builderImage",
            "toolchain",
            "versionProbe",
        ] {
            assert_eq!(
                activation[field], receipt[field],
                "{label}: the activation receipt changed {field}"
            );
        }
        // The activation's own facts stay the installing machine's, which is
        // still hax however the package receipt names its build host.
        assert_eq!(activation["installerHost"], json!("hax"));
        let hig_root = harness.fixture.root.join(format!("hig-{label}"));
        let hig_bin = harness.fixture.root.join(format!("hig-bin-{label}"));
        let shipped = harness.install_from("hig", &package, &hax_root, &hig_root, &hig_bin);
        assert!(
            shipped.status.success(),
            "{label}: the remote leg refused a readable v2 receipt\nstderr: {}",
            String::from_utf8_lossy(&shipped.stderr)
        );
    }

    // The hax activation receipt is validated by its own copy of the same
    // rules, and a hig install is what reaches it.
    let activated = harness.hax_install_root.join("releases").join(format!(
        "{}.receipt.json",
        release_id_from_package(&harness.package_dir)
    ));
    let genuine = fs::read(&activated).unwrap();
    let activation_refusals: [ReceiptEdit; 3] = [
        ("no-build-kind", |r: &mut Value| {
            r.as_object_mut().unwrap().remove("buildKind").unwrap();
        }),
        ("a-tagged-image", |r: &mut Value| {
            r["builderImage"] = json!("rust:1.90-bookworm");
        }),
        ("a-native-probe-it-could-not-have-run", |r: &mut Value| {
            r["versionProbe"] = json!("native");
        }),
    ];
    for (label, patch) in activation_refusals {
        let mut receipt: Value = serde_json::from_slice(&genuine).unwrap();
        patch(&mut receipt);
        fs::write(&activated, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        let install_root = harness.fixture.root.join(format!("hig-activation-{label}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("hig-activation-bin-{label}"));
        harness.assert_refused_without_mutation(
            "hig",
            &install_root,
            &bin_dir,
            "hax activation receipt is incomplete or mismatched",
            &[&install_root, &bin_dir],
        );
    }
    fs::write(&activated, &genuine).unwrap();
}

/// A package built before 2026-09-10 carries a `formatVersion` 1 receipt, and
/// a v2 script refuses it on both legs rather than reading its unconditional
/// `"hax"` as provenance. Packages do not outlive the operation that builds
/// them, so the remedy is to rebuild - and refusing is what stops a receipt
/// whose `host` was a literal from being read as a measurement.
#[test]
fn hig_release_script_install_refuses_a_format_version_1_package_receipt() {
    let harness = ReleaseGuardHarness::new("hig-release-v1-package-receipt");
    let package = harness.fixture.root.join("package-v1");
    clone_release_package(
        &harness.package_dir,
        &package,
        "0123456789abcdef0123456789abcdef0000f101",
    );
    let receipt_path = package.with_extension("receipt.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    // Exactly what v1 wrote: the literal host, and not one provenance field.
    receipt["formatVersion"] = json!(1);
    receipt["host"] = json!("hax");
    for field in [
        "buildPlatform",
        "artifactPlatform",
        "buildKind",
        "builderImage",
        "toolchain",
        "versionProbe",
    ] {
        receipt.as_object_mut().unwrap().remove(field).unwrap();
    }
    fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("install-{target}"));
        let bin_dir = harness.fixture.root.join(format!("bin-{target}"));
        harness.assert_package_refused_without_mutation(
            target,
            &package,
            &harness.hax_install_root,
            &install_root,
            &bin_dir,
            "package receipt is incomplete or mismatched",
            &[&install_root, &bin_dir],
        );
    }
}

/// A release ACTIVATED under v1 stays in service: the readers that manage the
/// store never look at `formatVersion`, so a v1 activation receipt is still
/// ordered, retained and rolled back to. And nothing backfills it - a receipt
/// is write-once per release, and inventing `buildKind: "native"` for a
/// release nobody measured would be the invented provenance this whole change
/// removes.
#[test]
fn hig_release_script_rolls_back_to_a_release_activated_under_format_version_1() {
    let harness = ReleaseGuardHarness::new("hig-release-v1-activation-receipt");
    let install_root = harness.fixture.root.join("install");
    let bin_dir = harness.fixture.root.join("bin");
    let first = harness.install("hax", &install_root, &bin_dir);
    assert!(
        first.status.success(),
        "the first install failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_id = release_id_from_package(&harness.package_dir);

    let second_package = harness.fixture.root.join("package-second");
    clone_release_package(
        &harness.package_dir,
        &second_package,
        "0123456789abcdef0123456789abcdef0000f202",
    );
    let second = harness.install_from(
        "hax",
        &second_package,
        &harness.hax_install_root,
        &install_root,
        &bin_dir,
    );
    assert!(
        second.status.success(),
        "the second install failed: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let activated = install_root
        .join("releases")
        .join(format!("{first_id}.receipt.json"));
    let mut receipt: Value = serde_json::from_slice(&fs::read(&activated).unwrap()).unwrap();
    receipt["formatVersion"] = json!(1);
    receipt["host"] = json!("hax");
    for field in [
        "buildPlatform",
        "artifactPlatform",
        "buildKind",
        "builderImage",
        "toolchain",
        "versionProbe",
    ] {
        receipt.as_object_mut().unwrap().remove(field).unwrap();
    }
    let downgraded = serde_json::to_vec_pretty(&receipt).unwrap();
    fs::write(&activated, &downgraded).unwrap();

    let rolled = harness
        .command()
        .args([
            "rollback",
            "hax",
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
            "--steps",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        rolled.status.success(),
        "a release activated under v1 could not be rolled back to: {}",
        String::from_utf8_lossy(&rolled.stderr)
    );
    assert_eq!(
        fs::read_link(install_root.join("current")).unwrap(),
        install_root.join("releases").join(&first_id),
        "the rollback did not put the v1-activated release back in service"
    );
    assert_eq!(
        fs::read(&activated).unwrap(),
        downgraded,
        "the rollback backfilled provenance into a receipt that never carried any"
    );
}

/// `releases/<id>.receipt.json` is a managed destination, not a hint. The
/// installer writes it only when nothing is there (`[[ ! -f "$release_meta"
/// ]]`), so a receipt planted for a release that was never installed used to
/// be adopted whole: the install reported the planted file back as its own
/// receipt, and `release_entries` orders retention and rollback by the
/// `activationSequence` it reads, so a forged 9999 pinned the forgery newest
/// and pushed a real release out of the ten. The guard therefore checks the
/// receipt's identity against the release being installed and its sequence
/// against the counter this install root has actually issued.
#[test]
fn hig_release_script_refuses_a_planted_activation_receipt_at_the_release_receipt_path() {
    let harness = ReleaseGuardHarness::new("hig-release-planted-receipt");
    let release_id = release_id_from_package(&harness.package_dir);
    // The bytes a real activation of this very release wrote, so the second
    // forgery below is genuine in every field except the one under test.
    let genuine: Value = serde_json::from_slice(
        &fs::read(
            harness
                .hax_install_root
                .join("releases")
                .join(format!("{release_id}.receipt.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let mut resequenced = genuine.clone();
    resequenced["activationSequence"] = json!(9999);

    let forgeries = [
        // The bug's own reproduction: a matching id, a forged sequence, and
        // none of the provenance a receipt this installer wrote carries.
        (
            "bare",
            json!({
                "releaseId": release_id.clone(),
                "activationSequence": 9999,
                "installedAt": 1,
            }),
            true,
        ),
        // Every identity field genuine; only the sequence forged past
        // anything this install root ever issued.
        ("resequenced", resequenced, false),
    ];

    for target in ["hax", "hig"] {
        for (slug, forgery, identity_check) in &forgeries {
            let install_root = harness
                .fixture
                .root
                .join(format!("planted-receipt-{slug}-{target}"));
            let bin_dir = harness
                .fixture
                .root
                .join(format!("planted-receipt-{slug}-bin-{target}"));
            let releases = install_root.join("releases");
            fs::create_dir_all(&releases).unwrap();
            let planted = releases.join(format!("{release_id}.receipt.json"));
            let bytes = serde_json::to_vec_pretty(forgery).unwrap();
            fs::write(&planted, &bytes).unwrap();

            let refusal = if *identity_check {
                format!(
                    "refusing to install: {} does not name the release being installed ({release_id}); move it aside before installing",
                    planted.display()
                )
            } else {
                format!(
                    "refusing to install: {} carries activationSequence 9999, which is not an integer this install root has issued (its counter stands at 0); move it aside before installing",
                    planted.display()
                )
            };
            harness.assert_refused_without_mutation(
                target,
                &install_root,
                &bin_dir,
                &refusal,
                &[&install_root, &bin_dir],
            );

            assert_eq!(
                fs::read(&planted).unwrap(),
                bytes,
                "{target}: the {slug} receipt was rewritten instead of refused"
            );
            assert!(
                fs::symlink_metadata(releases.join(&release_id)).is_err(),
                "{target}: the {slug} forgery got a release directory"
            );
            assert!(
                fs::symlink_metadata(install_root.join("current")).is_err(),
                "{target}: the {slug} forgery was activated at current"
            );
            assert!(
                fs::symlink_metadata(releases.join(".activation-sequence")).is_err(),
                "{target}: the refused install issued an activation sequence"
            );
        }
    }
}

/// The other half of the guard above: a receipt this installer wrote must
/// pass it, or re-activating an already-installed release -- the ordinary
/// idempotent case, and the one every redeploy takes -- becomes a refusal.
/// The proof is the receipt's own bytes. ADR-039 makes the activation receipt
/// write-once per release id, so the second install must leave its
/// `activationSequence` and `installedAt` exactly as the first activation
/// recorded them and must not burn a sequence off the counter.
#[test]
fn hig_release_script_reactivates_an_installed_release_without_touching_its_receipt() {
    let harness = ReleaseGuardHarness::new("hig-release-reinstall-idempotent");
    let release_id = release_id_from_package(&harness.package_dir);
    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("reinstall-{target}"));
        let bin_dir = harness.fixture.root.join(format!("reinstall-bin-{target}"));
        let first = harness.install(target, &install_root, &bin_dir);
        assert!(
            first.status.success(),
            "{target}: first install failed: {}",
            String::from_utf8_lossy(&first.stderr)
        );
        let releases = install_root.join("releases");
        let receipt_path = releases.join(format!("{release_id}.receipt.json"));
        let receipt_before = fs::read(&receipt_path).unwrap();
        let counter_path = releases.join(".activation-sequence");
        let counter_before = fs::read(&counter_path).unwrap();

        let again = harness.install(target, &install_root, &bin_dir);
        assert!(
            again.status.success(),
            "{target}: re-activating an installed release was refused: {}",
            String::from_utf8_lossy(&again.stderr)
        );
        assert_eq!(
            fs::read(&receipt_path).unwrap(),
            receipt_before,
            "{target}: the second activation rewrote the write-once receipt"
        );
        assert_eq!(
            fs::read(&counter_path).unwrap(),
            counter_before,
            "{target}: the second activation issued a new sequence"
        );
        assert_release_view(&install_root, &bin_dir, &releases.join(&release_id));
        let reported: Value = serde_json::from_slice(&again.stdout).unwrap();
        assert_eq!(
            PathBuf::from(reported["receipt"].as_str().unwrap()),
            receipt_path,
            "{target}: the re-install reported a different receipt path"
        );
    }
}

/// `releaseId` is `sourceCommit-manifestSha256`, not the commit: two builds of
/// one commit that did not produce the same bytes are two releases. Keyed on
/// the commit alone the second install would find the first release directory
/// already there, skip staging entirely, and activate the first build's bytes
/// while reporting the second build's id -- the exact shape of a rollback
/// that lands on something nobody built.
#[test]
fn hig_release_script_installs_two_distinct_builds_of_one_commit_as_two_releases() {
    let harness = ReleaseGuardHarness::new("hig-release-same-commit");
    let commit = "0123456789abcdef0123456789abcdef0000ab01";
    let first = harness.fixture.root.join("build-a");
    clone_release_package(&harness.package_dir, &first, commit);
    let second = harness.fixture.root.join("build-b");
    clone_release_package(&harness.package_dir, &second, commit);

    // The second build ships a different `kanban` reporting a different
    // version and a different embedded bundle: one commit, two builds, which
    // is what the manifest hash is there to tell apart. It is still the
    // platform a release targets - a rebuild that is not is refused, and that
    // is a different case - so the reporting payload sits under the header
    // the installer reads.
    let rebuilt = second.join("kanban");
    let rebuilt_bundle = "bb".repeat(32);
    write_release_platform_image(
        &rebuilt,
        format!("#!/bin/sh\nset -eu\nprintf 'kanban 0.3.0-rebuild\\nbundle {rebuilt_bundle}\\n'\n")
            .as_bytes(),
    );
    let entry = json!({
        "name": "kanban",
        "sha256": file_sha256(&rebuilt),
        "bytes": fs::metadata(&rebuilt).unwrap().len(),
        "version": "kanban 0.3.0-rebuild",
    });
    for artifact in [
        second.join("manifest.json"),
        second.with_extension("receipt.json"),
    ] {
        let mut document: Value = serde_json::from_slice(&fs::read(&artifact).unwrap()).unwrap();
        assert_eq!(
            document["files"][0]["name"],
            json!("kanban"),
            "the release set no longer starts with kanban, so this rewrite patches the wrong entry"
        );
        document["files"][0] = entry.clone();
        // The rebuild carries its own operator UI, so the artefacts name it:
        // a package whose binary and manifest disagree about the bundle is a
        // different case, and the installer refuses it (SPA-03).
        document["bundleSha256"] = json!(rebuilt_bundle);
        fs::write(&artifact, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
    }
    let receipt_path = second.with_extension("receipt.json");
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    receipt["manifestSha256"] = json!(file_sha256(&second.join("manifest.json")));
    fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();

    let ids = [
        release_id_from_package(&first),
        release_id_from_package(&second),
    ];
    assert_ne!(
        ids[0], ids[1],
        "two builds of one commit must derive two release ids"
    );
    for id in &ids {
        assert!(id.starts_with(commit), "release id {id} lost its commit");
    }

    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("same-commit-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("same-commit-bin-{target}"));
        let mut release_dirs = Vec::new();
        for (index, package) in [&first, &second].into_iter().enumerate() {
            let hax_install_root = if target == "hig" {
                install_matching_hax_package(
                    &harness.hax_context(),
                    package,
                    commit,
                    &format!("same-commit-{target}-{index}"),
                )
            } else {
                harness.hax_install_root.clone()
            };
            let installed =
                harness.install_from(target, package, &hax_install_root, &install_root, &bin_dir);
            assert!(
                installed.status.success(),
                "{target}: build {index} of one commit failed to install: {}\nstderr: {}",
                String::from_utf8_lossy(&installed.stdout),
                String::from_utf8_lossy(&installed.stderr)
            );
            let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
            release_dirs.push(PathBuf::from(installed["releaseDir"].as_str().unwrap()));
        }

        // Two releases, each stored under the id its own manifest hashes to.
        assert_ne!(release_dirs[0], release_dirs[1], "{target}: builds aliased");
        for (release_dir, id) in release_dirs.iter().zip(&ids) {
            assert!(
                release_dir.is_dir(),
                "{target}: release {id} is not a directory"
            );
            assert_eq!(
                release_dir.file_name().unwrap().to_str().unwrap(),
                id.as_str()
            );
            assert_eq!(
                format!(
                    "{commit}-{}",
                    file_sha256(&release_dir.join("manifest.json"))
                ),
                *id,
                "{target}: release {id} does not hash to the name it is stored under"
            );
        }
        assert_eq!(
            fs::read(release_dirs[0].join("kanban")).unwrap(),
            fs::read(first.join("kanban")).unwrap(),
            "{target}: the first build's binary was replaced by the second's"
        );
        assert_eq!(
            fs::read(release_dirs[1].join("kanban")).unwrap(),
            fs::read(second.join("kanban")).unwrap(),
            "{target}: the second build activated the first build's binary"
        );

        // Only the newest is current, and each activation is its own event.
        assert_release_view(&install_root, &bin_dir, &release_dirs[1]);
        let sequences: Vec<u64> = ids
            .iter()
            .map(|id| {
                let receipt: Value = serde_json::from_slice(
                    &fs::read(
                        install_root
                            .join("releases")
                            .join(format!("{id}.receipt.json")),
                    )
                    .unwrap(),
                )
                .unwrap();
                receipt["activationSequence"].as_u64().unwrap()
            })
            .collect();
        assert_eq!(
            sequences,
            vec![1, 2],
            "{target}: the two activations did not get their own ordered sequence"
        );
    }
}

/// A release directory is born from `mktemp -d`, which makes 0700, and `mv`
/// carries that mode onto the release. Historically, while `kanban-serve` ran
/// as the `kanban` user, a 0700 release caused 203/EXEC on 2026-09-07. Today
/// only root reads the store (a-e5391903), but new release directories remain
/// 0755 so a future service-traversed store needs no installer change. This
/// test asserts the activated release's mode and its managed links.
#[test]
fn hig_release_script_installs_a_release_directory_another_identity_can_traverse() {
    let harness = ReleaseGuardHarness::new("hig-release-traversable");
    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("traversable-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("traversable-bin-{target}"));
        let installed = harness.install(target, &install_root, &bin_dir);
        assert!(
            installed.status.success(),
            "{target}: install failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );

        // The directory the activated view resolves to, not the one the
        // installer reported: what the service execs is `current/<binary>`.
        let release_dir = fs::read_link(install_root.join("current")).unwrap();
        assert_release_view(&install_root, &bin_dir, &release_dir);

        let dir_mode = fs::metadata(&release_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            dir_mode,
            0o755,
            "{target}: release directory {} installed 0{dir_mode:o}, not 0755",
            release_dir.display()
        );
        // Restated as the service identity experiences it: a non-owner must be
        // able to traverse into the release at all.
        assert_eq!(
            dir_mode & 0o005,
            0o005,
            "{target}: release directory {} cannot be traversed without owner rights (0{dir_mode:o})",
            release_dir.display()
        );

        for name in declared_bin_names() {
            let binary = release_dir.join(&name);
            let mode = fs::metadata(&binary).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o755,
                "{target}: installed {name} is 0{mode:o}, not 0755"
            );
            assert_ne!(
                mode & 0o001,
                0,
                "{target}: installed {name} is not executable without owner rights (0{mode:o})"
            );
        }
    }
}

/// `files[].name` is joined onto the package directory and then EXECUTED:
/// `file_version` runs `"$dir/$name" version` to prove the binary reports what
/// the manifest claims. A name that escapes the package directory therefore
/// runs whatever sits at the escaped path -- and `${name##*/}` is what
/// `release_binary_known` sees, so `../kanban` reads as the known release
/// binary `kanban`. Pinning the name list to the release set exactly is the
/// only thing between a hand-edited manifest and arbitrary execution.
#[test]
fn hig_release_script_refuses_a_manifest_file_name_that_escapes_the_package_directory() {
    let harness = ReleaseGuardHarness::new("hig-release-manifest-traversal");
    let escape_root = harness.fixture.root.join("escape");
    fs::create_dir_all(&escape_root).unwrap();
    let executed = escape_root.join("EXECUTED");
    let planted = escape_root.join("kanban");
    write_executable(
        &planted,
        &format!(
            "#!/bin/sh\n: > {}\nprintf 'kanban 0.0.0-planted\\n'\n",
            executed.display()
        ),
    );
    let planted_entry = |name: &str| {
        json!({
            "name": name,
            "sha256": file_sha256(&planted),
            "bytes": fs::metadata(&planted).unwrap().len(),
            "version": "kanban 0.0.0-planted",
        })
    };
    let commit = |tag: u32| format!("0123456789abcdef0123456789abcdef{tag:08x}");

    // Relative escape, absolute path, and a subdirectory: all three would be
    // read and run out of the package directory's parent or off the root.
    let mut packages = Vec::new();
    for (index, name) in [
        "../kanban".to_string(),
        planted.to_str().unwrap().to_string(),
        "sub/kanban".to_string(),
    ]
    .into_iter()
    .enumerate()
    {
        let package = escape_root.join(format!("package-{index}"));
        clone_release_package(&harness.package_dir, &package, &commit(0xb0 + index as u32));
        let manifest_path = package.join("manifest.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["files"][0] = planted_entry(&name);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let receipt_path = package.with_extension("receipt.json");
        let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        receipt["manifestSha256"] = json!(file_sha256(&manifest_path));
        fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        packages.push((name, package));
    }

    let before = snapshot_tree(&escape_root);
    for target in ["hax", "hig"] {
        for (index, (name, package)) in packages.iter().enumerate() {
            let install_root = harness
                .fixture
                .root
                .join(format!("traversal-{target}-{index}"));
            let bin_dir = harness
                .fixture
                .root
                .join(format!("traversal-bin-{target}-{index}"));
            harness.assert_package_refused_without_mutation(
                target,
                package,
                &harness.hax_install_root,
                &install_root,
                &bin_dir,
                "package manifest is incomplete or mismatched",
                &[&install_root, &bin_dir],
            );
            assert!(
                !executed.exists(),
                "{target}: manifest name {name} was executed outside the package"
            );
            assert!(
                fs::symlink_metadata(&install_root).is_err(),
                "{target}: manifest name {name} reached staging"
            );
        }
    }
    // Nothing was written outside a release directory -- there is no release
    // directory, and the tree the escape aimed at is byte-identical.
    assert_eq!(
        snapshot_tree(&escape_root),
        before,
        "a refused traversal wrote outside the install root"
    );
}

/// A hard link is indistinguishable from the file it shares an inode with:
/// nothing to `readlink`, no `-L`, no target text to inspect. What refuses it
/// is the managed-symlink rule -- a destination this installer replaces must
/// be a symlink this installer wrote -- so an operator file that is also
/// linked from outside the tree is neither followed nor unlinked. Without
/// that rule the installer would `os.replace` over the name and the outside
/// data would be silently detached from the estate that referenced it.
#[test]
fn hig_release_script_refuses_a_hard_link_planted_at_a_managed_destination() {
    let harness = ReleaseGuardHarness::new("hig-release-hard-link");
    let outside = harness.fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let secret = outside.join("operator-secret");
    fs::write(&secret, b"operator secret\n").unwrap();
    let secret_inode = fs::symlink_metadata(&secret).unwrap().ino();

    for target in ["hax", "hig"] {
        // At a public binary destination.
        let install_root = harness.fixture.root.join(format!("hard-link-bin-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("hard-link-bin-dir-{target}"));
        fs::create_dir_all(&bin_dir).unwrap();
        fs::hard_link(&secret, bin_dir.join("kanban")).unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/kanban: it is not a symlink into {}/current managed by this installer",
                bin_dir.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        let planted = bin_dir.join("kanban");
        assert_eq!(
            fs::symlink_metadata(&planted).unwrap().ino(),
            secret_inode,
            "{target}: the planted hard link was replaced at the bin destination"
        );
        assert!(
            !fs::symlink_metadata(&planted).unwrap().is_symlink(),
            "{target}: the hard link was converted into a managed symlink"
        );

        // At `current`.
        let install_root = harness
            .fixture
            .root
            .join(format!("hard-link-current-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("hard-link-current-bin-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        fs::hard_link(&secret, install_root.join("current")).unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/current: it is not a symlink into {}/releases managed by this installer",
                install_root.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        let planted = install_root.join("current");
        assert_eq!(
            fs::symlink_metadata(&planted).unwrap().ino(),
            secret_inode,
            "{target}: the planted hard link at current was replaced"
        );
        assert_eq!(
            fs::read(&secret).unwrap(),
            b"operator secret\n",
            "{target}: the outside file behind the hard link was written"
        );
    }
}

/// A device node needs privileges this test does not have, so the stand-in is
/// a FIFO -- the sharper probe anyway: `open()` on a FIFO blocks until the
/// other end appears, so an installer that opened a destination before
/// classifying it would hang forever rather than fail, with no output and no
/// timeout. Every install here is therefore run against a deadline, and the
/// deadline is the assertion.
#[test]
fn hig_release_script_refuses_a_fifo_at_a_managed_destination_without_opening_it() {
    let harness = ReleaseGuardHarness::new("hig-release-fifo");
    let refused_before_deadline = |target: &str, install_root: &Path, bin_dir: &Path| -> Output {
        let mut child = harness
            .install_command(
                target,
                &harness.package_dir,
                &harness.hax_install_root,
                install_root,
                bin_dir,
            )
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match child.try_wait().unwrap() {
                Some(_) => break,
                None if Instant::now() >= deadline => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("{target}: the installer blocked on a FIFO instead of refusing it");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !output.status.success(),
            "{target}: a FIFO was installed through\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        output
    };

    for target in ["hax", "hig"] {
        // Every destination an activation writes through, one at a time.
        for spot in ["bin-link", "current", "releases", "bin-dir"] {
            let install_root = harness
                .fixture
                .root
                .join(format!("fifo-{spot}-{target}-root"));
            let bin_dir = harness
                .fixture
                .root
                .join(format!("fifo-{spot}-{target}-bin"));
            let (fifo, refusal) = match spot {
                "bin-link" => {
                    fs::create_dir_all(&bin_dir).unwrap();
                    (
                        bin_dir.join("kanban"),
                        format!(
                            "refusing to replace {}/kanban: it is not a symlink into {}/current managed by this installer",
                            bin_dir.display(),
                            install_root.display()
                        ),
                    )
                }
                "current" => {
                    fs::create_dir_all(&install_root).unwrap();
                    (
                        install_root.join("current"),
                        format!(
                            "refusing to replace {}/current: it is not a symlink into {}/releases managed by this installer",
                            install_root.display(),
                            install_root.display()
                        ),
                    )
                }
                "releases" => {
                    fs::create_dir_all(&install_root).unwrap();
                    (
                        install_root.join("releases"),
                        format!(
                            "refusing to install: {}/releases is not a directory",
                            install_root.display()
                        ),
                    )
                }
                _ => (
                    bin_dir.clone(),
                    format!("bin dir is not a directory: {}", bin_dir.display()),
                ),
            };
            assert!(
                Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .expect("mkfifo must be available to exercise the blocking-open case")
                    .success(),
                "mkfifo failed"
            );

            let output = refused_before_deadline(target, &install_root, &bin_dir);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&refusal),
                "{target}/{spot}: expected {refusal:?} in:\n{stderr}"
            );
            // The FIFO is still a FIFO: it was classified, never opened and
            // never replaced.
            assert!(
                fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo(),
                "{target}/{spot}: the FIFO was replaced"
            );
            assert!(
                !install_root.join("releases").is_dir(),
                "{target}/{spot}: a refused install created releases/"
            );
        }
    }
}

/// Two ways an install cannot finish, and neither may leave the estate half
/// moved: a package binary the version probe cannot execute, and an install
/// root that cannot be written at the moment of the cutover. The second is
/// the interesting one -- the release directory is already staged and moved
/// into place by then, so only the ERR-trap rollback keeps the previous view
/// intact and the retention count honest.
#[test]
fn hig_release_script_fails_closed_on_an_unexecutable_binary_and_an_unwritable_install_root() {
    let harness = ReleaseGuardHarness::new("hig-release-fails-closed");
    let commit = |tag: u32| format!("0123456789abcdef0123456789abcdef{tag:08x}");
    let receipt_count = |install_root: &Path| {
        fs::read_dir(install_root.join("releases"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".receipt.json"))
            })
            .count()
    };
    // Root ignores the mode bits, so the unwritable-root scenario is
    // unreachable when this runs privileged. Both branches then assert a true
    // thing rather than skipping.
    let privileged = {
        let probe = harness.fixture.root.join("write-probe");
        fs::create_dir_all(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o555)).unwrap();
        let writable = fs::write(probe.join("probe"), b"probe").is_ok();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)).unwrap();
        writable
    };

    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("fails-closed-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("fails-closed-bin-{target}"));
        let installed = harness.install(target, &install_root, &bin_dir);
        assert!(
            installed.status.success(),
            "{target}: the release to protect failed to install: {}",
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
        let live_release_dir = PathBuf::from(installed["releaseDir"].as_str().unwrap());
        let live_links = capture_release_links(&install_root, &bin_dir);
        let live_release = snapshot_tree(&live_release_dir);

        // A package binary that is not executable.
        let unexecutable = harness.fixture.root.join(format!("unexecutable-{target}"));
        clone_release_package(&harness.package_dir, &unexecutable, &commit(0xc1));
        fs::set_permissions(
            unexecutable.join("kanban"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let refused = harness.install_from(
            target,
            &unexecutable,
            &harness.hax_install_root,
            &install_root,
            &bin_dir,
        );
        assert!(
            !refused.status.success(),
            "{target}: a package binary the installer cannot run was accepted\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        // The script refuses on the mode bit itself, in every loop that
        // measures a release binary, so this holds whoever runs the binary
        // afterwards: it is not the version probe discovering it, and on a
        // host that probes through HIG_RELEASE_TARGET_RUNNER it is not the
        // runner's opinion either. The exact wording stays unpinned; what
        // must hold is that the refusal names the binary and nothing moved.
        assert!(
            stderr.contains("package binary kanban"),
            "{target}: expected the refusal to name the binary:\n{stderr}"
        );
        assert!(
            !stderr.contains("version mismatch"),
            "{target}: the mode bit was left for the version probe to find:\n{stderr}"
        );
        assert_eq!(
            capture_release_links(&install_root, &bin_dir),
            live_links,
            "{target}: an unexecutable package moved the public view"
        );
        assert_eq!(
            receipt_count(&install_root),
            1,
            "{target}: an unexecutable package counted toward retention"
        );
        assert_eq!(
            snapshot_tree(&live_release_dir),
            live_release,
            "{target}: an unexecutable package changed the live release"
        );

        // An install root that cannot be written when the cutover happens.
        let second = harness.fixture.root.join(format!("second-build-{target}"));
        clone_release_package(&harness.package_dir, &second, &commit(0xc2));
        let hax_install_root = if target == "hig" {
            install_matching_hax_package(
                &harness.hax_context(),
                &second,
                &commit(0xc2),
                &format!("fails-closed-{target}"),
            )
        } else {
            harness.hax_install_root.clone()
        };
        fs::set_permissions(&install_root, fs::Permissions::from_mode(0o555)).unwrap();
        let outcome =
            harness.install_from(target, &second, &hax_install_root, &install_root, &bin_dir);
        fs::set_permissions(&install_root, fs::Permissions::from_mode(0o755)).unwrap();
        if privileged {
            assert!(
                outcome.status.success(),
                "{target}: running privileged, so the root was writable and the install should have finished: {}",
                String::from_utf8_lossy(&outcome.stderr)
            );
            continue;
        }
        assert!(
            !outcome.status.success(),
            "{target}: an unwritable install root reported success\nstdout: {}",
            String::from_utf8_lossy(&outcome.stdout)
        );
        let second_id = release_id_from_package(&second);
        assert_eq!(
            capture_release_links(&install_root, &bin_dir),
            live_links,
            "{target}: a failed cutover repointed the public view"
        );
        assert!(
            !install_root.join("releases").join(&second_id).exists(),
            "{target}: the half-installed release directory was left behind"
        );
        assert!(
            !install_root
                .join("releases")
                .join(format!("{second_id}.receipt.json"))
                .exists(),
            "{target}: the half-installed release receipt was left behind"
        );
        assert_eq!(
            receipt_count(&install_root),
            1,
            "{target}: a failed cutover counted toward retention"
        );
        assert_eq!(
            snapshot_tree(&live_release_dir),
            live_release,
            "{target}: a failed cutover changed the live release"
        );
    }
}

/// Two installs of different releases, raced into one install root. The
/// activation sequence is the release store's clock: retention keeps the
/// newest ten by it and rollback walks it, so two activations that agree on a
/// number are two releases nobody can order. `next_activation_sequence` takes
/// an exclusive `flock` for exactly this. `current` is sampled throughout,
/// because the other way to lose is to publish a staging directory: the
/// release is assembled in `releases/.<id>.XXXXXX` and only then renamed, so
/// `current` must never name one.
#[test]
fn hig_release_script_serializes_racing_activations_on_one_install_root() {
    let harness = ReleaseGuardHarness::new("hig-release-race");
    let commit = |tag: u32| format!("0123456789abcdef0123456789abcdef{tag:08x}");
    // The local install path: `next_activation_sequence` is the same
    // flock-guarded counter in the embedded remote script, but only here can
    // two installers be started against one root with nothing between them.
    let packages: Vec<PathBuf> = (0..2)
        .map(|index| {
            let package = harness.fixture.root.join(format!("race-build-{index}"));
            clone_release_package(&harness.package_dir, &package, &commit(0xd1 + index));
            package
        })
        .collect();
    let ids: Vec<String> = packages
        .iter()
        .map(|package| release_id_from_package(package))
        .collect();
    assert_ne!(ids[0], ids[1], "the racers must be two distinct releases");
    let install_root = harness.fixture.root.join("race-install");
    let bin_dir = harness.fixture.root.join("race-bin");
    let release_dirs: Vec<PathBuf> = ids
        .iter()
        .map(|id| install_root.join("releases").join(id))
        .collect();

    let mut children: Vec<Child> = packages
        .iter()
        .map(|package| {
            harness
                .install_command(
                    "hax",
                    package,
                    &harness.hax_install_root,
                    &install_root,
                    &bin_dir,
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    let current = install_root.join("current");
    let mut samples: Vec<PathBuf> = Vec::new();
    let mut settled = vec![false; children.len()];
    let deadline = Instant::now() + Duration::from_secs(180);
    while !settled.iter().all(|done| *done) {
        for (index, child) in children.iter_mut().enumerate() {
            if !settled[index] {
                settled[index] = child.try_wait().unwrap().is_some();
            }
        }
        if let Ok(target) = fs::read_link(&current)
            && samples.last() != Some(&target)
        {
            samples.push(target);
        }
        assert!(
            Instant::now() < deadline,
            "the racing installs never finished"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    for (index, child) in children.into_iter().enumerate() {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "racer {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Both releases are there, and each activation claimed its own number.
    let mut sequences: Vec<u64> = ids
        .iter()
        .map(|id| {
            let receipt: Value = serde_json::from_slice(
                &fs::read(
                    install_root
                        .join("releases")
                        .join(format!("{id}.receipt.json")),
                )
                .unwrap(),
            )
            .unwrap();
            receipt["activationSequence"].as_u64().unwrap()
        })
        .collect();
    for release_dir in &release_dirs {
        assert!(
            release_dir.is_dir(),
            "racing installs lost {}",
            release_dir.display()
        );
    }
    sequences.sort_unstable();
    assert_eq!(
        sequences,
        vec![1, 2],
        "racing activations did not take distinct, monotonic sequence numbers"
    );
    assert_eq!(
        fs::read_to_string(install_root.join("releases/.activation-sequence"))
            .unwrap()
            .trim(),
        "2",
        "the activation counter does not match the activations it handed out"
    );

    // `current` only ever named a finished release, never a staging directory.
    assert!(
        !samples.is_empty(),
        "current was never observed during the race"
    );
    for sample in &samples {
        let name = sample.file_name().unwrap().to_str().unwrap().to_string();
        assert!(
            !name.starts_with('.'),
            "current named the staging directory {name}"
        );
        assert!(
            release_dirs.contains(sample),
            "current named {}, which is neither racer's release",
            sample.display()
        );
    }
    let settled_current = fs::read_link(&current).unwrap();
    assert!(
        release_dirs.contains(&settled_current),
        "the race settled on {}",
        settled_current.display()
    );
    assert_release_view(&install_root, &bin_dir, &settled_current);
}
