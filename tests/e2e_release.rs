//! Compiled-process E2E for `scripts/hig-release.sh`: packaging, platform and
//! toolchain probes, install refusals, prune and rollback, and the managed
//! destination guards.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

/// The release set `scripts/hig-release.sh` enumerates: its single `BINARIES`
/// array, which every other site in the script derives from.
fn release_script_binaries() -> Vec<String> {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh"))
            .unwrap();
    const HEADER: &str = "\nBINARIES=(\n";
    assert_eq!(
        script.matches(HEADER).count(),
        1,
        "scripts/hig-release.sh must hold exactly one literal BINARIES array; \
         a second one is a second source of truth that can drift"
    );
    let start = script.find(HEADER).unwrap() + HEADER.len();
    let end = script[start..]
        .find("\n)\n")
        .expect("BINARIES array unterminated")
        + start;
    script[start..end]
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

/// The diagnostic an install writes: the bundle the installed executable was
/// proved to be carrying (SPA-03). Everything else on stderr is still
/// unexpected output from a successful install.
fn stderr_beyond_the_install_notices(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .filter(|line| !line.starts_with("hig-release: the installed kanban carries bundle "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// `MH_MAGIC_64` as it sits on disk here. The realistic wrong answer: it is
/// what `cargo build --release` produces on this Mac.
fn mach_o_header() -> [u8; 20] {
    let mut header = [0_u8; 20];
    header[..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    header
}

/// `FAT_MAGIC` as it sits on disk: a universal binary, which is what a Mac
/// build that was asked for two architectures produces.
fn universal_mach_o_header() -> [u8; 20] {
    let mut header = [0_u8; 20];
    header[..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
    header
}

/// A build output for a case whose binaries must not be the release platform:
/// the crafted header, then enough payload that the file is a whole image
/// rather than a truncated one.
fn write_release_image(path: &Path, header: &[u8]) {
    let mut image = header.to_vec();
    image.extend_from_slice(b"payload that is not an executable\n");
    fs::write(path, &image).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn hig_release_script_requires_the_initialized_kb_skill_submodule() {
    let fixture = Fixture::new("hig-release-kb-submodule");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_binary_dir = Path::new(env!("CARGO_BIN_EXE_kanban")).parent().unwrap();
    let kb_skill = fake_repo_root.join("skills/kb/SKILL.md");
    fs::remove_file(&kb_skill).unwrap();

    let run_package = || {
        Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_RELEASE_BINARY_DIR", release_binary_dir)
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .env(
                "KANBAN_RELEASE_CONTAINER_RUNTIME",
                release_container_runtime(&hostname_bin),
            )
            .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
            .arg(&script)
            .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
            .output()
            .unwrap()
    };

    let refused = run_package();
    assert!(
        !refused.status.success(),
        "package unexpectedly succeeded without skills/kb"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("skills/kb is not initialized"), "{stderr}");
    assert!(
        stderr.contains("git submodule update --init skills/kb"),
        "{stderr}"
    );
    assert!(!output_dir.exists(), "refused package left output behind");

    fs::write(&kb_skill, "# initialized kb skill\n").unwrap();
    let packaged = run_package();
    assert!(
        packaged.status.success(),
        "initialized package failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
}

/// The whole release path, end to end, for every executable the crate
/// declares: packaged bytes come from the Cargo binary targets, HAX activates
/// them, HIG installs them through the embedded remote script with the HAX
/// install root hidden, and a package missing one binary is refused before
/// anything is activated. The name stays count-free on purpose: an assertion
/// that says "six" while the package ships ten is a lie a reader would trust.
#[test]
fn hig_release_script_installs_every_declared_binary_without_remote_hax_access_and_refuses_partial_activation()
 {
    let fixture = Fixture::new("hig-release");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let broken_install_root = fixture.root.join("broken-install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_binaries = [
        ("kanban", Path::new(env!("CARGO_BIN_EXE_kanban"))),
        ("kb", Path::new(env!("CARGO_BIN_EXE_kb"))),
        (
            "kanban-dispatcher",
            Path::new(env!("CARGO_BIN_EXE_kanban-dispatcher")),
        ),
        (
            "kanban-codex-queue-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-codex-queue-adapter")),
        ),
        (
            "kanban-codex-app-server-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-codex-app-server-adapter")),
        ),
        (
            "kanban-claude-print-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-claude-print-adapter")),
        ),
        (
            "kanban-opencode-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-opencode-adapter")),
        ),
        (
            "kanban-kimi-acp-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-kimi-acp-adapter")),
        ),
        (
            "kanban-cursor-worker-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-cursor-worker-adapter")),
        ),
        (
            "kanban-zcode-notify-adapter",
            Path::new(env!("CARGO_BIN_EXE_kanban-zcode-notify-adapter")),
        ),
    ];
    // These are the compiled Cargo targets, so they pin the manifest's own
    // order too: a `[[bin]]` block added without touching this list fails
    // here rather than shipping a package the crate does not match.
    assert_eq!(
        release_binaries
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        declared_bin_names(),
        "the release binaries under test are not the executables Cargo.toml declares"
    );
    let release_binary_dir = release_binaries[0].1.parent().unwrap();

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_RELEASE_BINARY_DIR", release_binary_dir)
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "package failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let manifest_path = output_dir.join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["formatVersion"], 1);
    assert_eq!(manifest["targets"], json!(["hax", "hig"]));
    assert_eq!(
        manifest["sourceCommit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(manifest["sourceTreeClean"], true);
    assert_eq!(
        manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        declared_bin_names()
    );
    let receipt_path = output_dir.with_extension("receipt.json");
    let receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    assert_eq!(receipt["host"], "hax");
    assert_eq!(receipt["targets"], json!(["hax", "hig"]));
    assert_eq!(
        receipt["manifestSha256"],
        json!(file_sha256(&manifest_path))
    );
    assert_eq!(
        receipt["sourceCommit"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    for (name, source) in &release_binaries {
        let packaged_binary = output_dir.join(name);
        assert!(packaged_binary.is_file(), "missing package binary {name}");
        // The fake build stamps the linux x86-64 header a real release build
        // would carry onto each Cargo binary, so what the package must hold is
        // that header followed by THIS target's bytes: a stub standing in for
        // all ten still fails here.
        let mut expected = linux_x86_64_elf_header().to_vec();
        expected.extend_from_slice(&fs::read(source).unwrap());
        assert_eq!(
            fs::read(&packaged_binary).unwrap(),
            expected,
            "package binary {name} did not come from its Cargo binary target"
        );
    }

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "HAX install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&hax_installed.stdout),
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let hax_installed_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();
    let hax_release_dir = PathBuf::from(hax_installed_json["releaseDir"].as_str().unwrap());
    assert!(hax_release_dir.is_dir(), "HAX release dir missing");
    assert!(
        hax_release_dir.join("manifest.json").is_file(),
        "HAX release manifest missing"
    );
    let hax_release_receipt = PathBuf::from(hax_installed_json["receipt"].as_str().unwrap());
    let hax_release_receipt_json: Value =
        serde_json::from_slice(&fs::read(&hax_release_receipt).unwrap()).unwrap();
    assert_eq!(
        hax_release_receipt_json["releaseDir"],
        json!(hax_release_dir.to_str().unwrap())
    );
    assert_eq!(hax_release_receipt_json["target"], "hax");
    assert_eq!(hax_release_receipt_json["targets"], json!(["hax", "hig"]));
    assert_eq!(
        hax_release_receipt_json["manifestSha256"],
        json!(file_sha256(&manifest_path))
    );
    let hax_release_receipt_bytes = fs::read(&hax_release_receipt).unwrap();
    let hax_installed_again = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed_again.status.success(),
        "HAX reinstall failed: {}\nstderr: {}",
        String::from_utf8_lossy(&hax_installed_again.stdout),
        String::from_utf8_lossy(&hax_installed_again.stderr)
    );
    assert_eq!(
        fs::read(&hax_release_receipt).unwrap(),
        hax_release_receipt_bytes,
        "HAX receipt bytes changed on reactivation"
    );
    assert_release_view(&hax_install_root, &hax_bin_dir, &hax_release_dir);
    for name in declared_bin_names()
        .into_iter()
        .chain(["manifest.json".to_string()])
    {
        assert!(
            hax_release_dir.join(&name).exists(),
            "missing HAX installed file {name}"
        );
    }

    let installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_SSH_HIDE_PATH", &hax_install_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "HIG install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        hax_install_root.is_dir(),
        "fake SSH did not restore the hidden HAX install root"
    );
    assert!(
        stderr_beyond_the_install_notices(&installed.stderr).is_empty(),
        "HIG install wrote unexpected stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    let release_dir = PathBuf::from(installed_json["releaseDir"].as_str().unwrap());
    assert!(release_dir.is_dir(), "release dir missing");
    assert!(
        release_dir.join("manifest.json").is_file(),
        "release manifest missing"
    );
    let release_receipt = PathBuf::from(installed_json["receipt"].as_str().unwrap());
    let release_receipt_json: Value =
        serde_json::from_slice(&fs::read(&release_receipt).unwrap()).unwrap();
    assert_eq!(
        release_receipt_json["releaseDir"],
        json!(release_dir.to_str().unwrap())
    );
    assert_eq!(release_receipt_json["target"], "hig");
    assert_eq!(
        release_receipt_json["manifestSha256"],
        hax_release_receipt_json["manifestSha256"]
    );
    for field in [
        "formatVersion",
        "host",
        "targets",
        "manifestSha256",
        "sourceCommit",
        "sourceTreeClean",
        "files",
    ] {
        assert_eq!(
            release_receipt_json[field], hax_release_receipt_json[field],
            "HIG release receipt changed canonical field {field}"
        );
    }
    assert_release_view(&install_root, &bin_dir, &release_dir);
    for name in declared_bin_names()
        .into_iter()
        .chain(["manifest.json".to_string()])
    {
        assert!(
            release_dir.join(&name).exists(),
            "missing installed file {name}"
        );
        assert_eq!(
            fs::read(release_dir.join(&name)).unwrap(),
            fs::read(hax_release_dir.join(&name)).unwrap(),
            "HIG installed bytes differ from the HAX release for {name}"
        );
    }

    let installed_again = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed_again.status.success(),
        "idempotent install failed: {}\nstderr: {}",
        String::from_utf8_lossy(&installed_again.stdout),
        String::from_utf8_lossy(&installed_again.stderr)
    );

    let partial_package = fixture.root.join("partial-package");
    fs::create_dir_all(&partial_package).unwrap();
    for entry in fs::read_dir(&output_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("kb") {
            continue;
        }
        fs::copy(&path, partial_package.join(path.file_name().unwrap())).unwrap();
    }
    fs::copy(
        &receipt_path,
        partial_package.with_extension("receipt.json"),
    )
    .unwrap();
    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            partial_package.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            broken_install_root.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "partial package activated successfully"
    );
    assert!(
        !broken_install_root.exists(),
        "partial package left an activated install root behind"
    );
}

/// The release path must enumerate exactly the executables the crate
/// declares. `cargo build --release --locked --bins` builds every `[[bin]]`
/// target, so a binary the script leaves out of `BINARIES` is built on every
/// release and then reaches no host at all - which is how the package spent
/// four adapters behind the crate. Both sides are derived here: the expected
/// set from `Cargo.toml`, the actual set from the script, so the next added
/// `[[bin]]` fails this test by name instead of silently missing the package.
#[test]
fn hig_release_script_enumerates_exactly_the_executables_the_crate_declares() {
    let declared = declared_bin_names();
    let enumerated = release_script_binaries();
    // Proves the manifest parse describes real build output rather than text:
    // these are the compiled Cargo targets this test binary was built beside.
    let built = Path::new(env!("CARGO_BIN_EXE_kanban")).parent().unwrap();
    for name in &declared {
        assert!(
            built.join(name).is_file(),
            "Cargo.toml declares [[bin]] {name}, but cargo built no such executable in {}",
            built.display()
        );
    }

    let missing: Vec<&str> = declared
        .iter()
        .filter(|name| !enumerated.contains(name))
        .map(String::as_str)
        .collect();
    assert!(
        missing.is_empty(),
        "scripts/hig-release.sh does not enumerate declared executables {missing:?}; \
         every release builds them and no release ships them"
    );
    let unknown: Vec<&str> = enumerated
        .iter()
        .filter(|name| !declared.contains(name))
        .map(String::as_str)
        .collect();
    assert!(
        unknown.is_empty(),
        "scripts/hig-release.sh enumerates {unknown:?}, which Cargo.toml declares no [[bin]] for; \
         packaging would fail on a binary the release build never produces"
    );
    assert_eq!(
        enumerated, declared,
        "scripts/hig-release.sh BINARIES is not in Cargo.toml [[bin]] order, \
         so the manifest file order stops matching the crate"
    );
}

#[test]
fn hig_release_script_keeps_the_previous_view_when_reactivation_fails_after_current() {
    let fixture = Fixture::new("hig-release-reactivation-failure");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );

    let installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(
        stderr_beyond_the_install_notices(&installed.stderr).is_empty(),
        "HIG install wrote unexpected stderr: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
    let release_dir = PathBuf::from(installed_json["releaseDir"].as_str().unwrap());
    let release_receipt = PathBuf::from(installed_json["receipt"].as_str().unwrap());
    let release_receipt_bytes = fs::read(&release_receipt).unwrap();
    let stable_links = capture_release_links(&install_root, &bin_dir);

    let failed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "reactivation failure unexpectedly succeeded"
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "reactivation failure changed the public release view"
    );
    assert!(
        release_dir.is_dir(),
        "reactivation failure removed the release tree"
    );
    assert_eq!(
        fs::read(&release_receipt).unwrap(),
        release_receipt_bytes,
        "reactivation failure rewrote the release receipt"
    );
    let receipt_count = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        receipt_count, 1,
        "reactivation failure left receipt residue"
    );
}

#[test]
fn hig_release_script_usage_refuses_missing_and_unknown_commands_with_exit_64() {
    let fixture = Fixture::new("hig-release-usage");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");

    for args in [Vec::<&str>::new(), vec!["bogus", "hax"]] {
        let output = Command::new("bash")
            .current_dir(&fixture.main)
            .arg(&script)
            .args(args.iter().copied())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(64),
            "unexpected exit code for {:?}",
            args
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("usage:"), "{stderr}");
        assert!(
            stderr.contains("hig-release.sh package hax [--output DIR]"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("package <hax|hig>"),
            "stale usage text leaked into stderr: {stderr}"
        );
    }
}

#[test]
fn hig_release_script_rejects_package_target_hig() {
    let fixture = Fixture::new("hig-release-package-hig");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args(["package", "hig", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !packaged.status.success(),
        "package hig unexpectedly succeeded"
    );
    assert_eq!(packaged.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&packaged.stderr).contains("package target must be hax"),
        "stderr: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );
}

/// A packaging run whose fake build can be pointed at any image, so a case
/// can make `cargo build --release` produce something other than the platform
/// a Kanban release targets and watch the script refuse it.
struct ReleasePackagingCase {
    fixture: Fixture,
    script: PathBuf,
    path: String,
    hostname_bin: PathBuf,
    fake_repo_root: PathBuf,
    remote_root: PathBuf,
}

impl ReleasePackagingCase {
    fn new(label: &str) -> Self {
        let fixture = Fixture::new(label);
        let fake_repo_root = fixture.root.join("fake-repo");
        fs::create_dir_all(&fake_repo_root).unwrap();
        let remote_root = fixture.root.join("remote-root");
        fs::create_dir_all(&remote_root).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
        let stubs = write_release_tool_stubs(
            &fixture,
            &fake_repo_root,
            "0123456789abcdef0123456789abcdef01234567",
            env!("CARGO_BIN_EXE_kanban"),
            "hax",
        );
        let hostname_bin = stubs.join("hostname");
        let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
        Self {
            fixture,
            script,
            path,
            hostname_bin,
            fake_repo_root,
            remote_root,
        }
    }

    fn package(&self, output: &Path) -> Command {
        let mut command = Command::new("bash");
        command
            .current_dir(&self.fixture.main)
            .env("PATH", &self.path)
            .env("HOSTNAME_BIN", &self.hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&self.hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &self.fake_repo_root)
            .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &self.remote_root)
            .env(
                "KANBAN_RELEASE_CONTAINER_RUNTIME",
                release_container_runtime(&self.hostname_bin),
            )
            .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
            .arg(&self.script)
            .args(["package", "hax", "--output", output.to_str().unwrap()]);
        command
    }
}

/// The sentence every platform refusal states the requirement with.
const RELEASE_PLATFORM: &str = "a Kanban release targets linux x86-64 (ELF 64-bit, little-endian)";

/// A refused packaging run records no release: no manifest, no receipt.
fn assert_no_release_recorded(output: &Path) {
    assert!(
        !output.join("manifest.json").exists(),
        "a refused packaging run wrote {}/manifest.json",
        output.display()
    );
    assert!(
        !output.with_extension("receipt.json").exists(),
        "a refused packaging run wrote {}",
        output.with_extension("receipt.json").display()
    );
}

/// A release is linux x86-64 whatever the host that built it runs, and the
/// build at scripts/hig-release.sh takes no `--target`: on a Mac it produces
/// Mach-O, and on any other cross-built host something else again. So the
/// packaging path reads the built image's own header and refuses it unless it
/// is a 64-bit little-endian x86-64 program - an object file, a core dump and
/// an ET_NONE image all carry an x86-64 ELF header and none of them is one -
/// naming the file and what it actually is. Each row here proves the refusal
/// happens on the FIRST binary of the release set, before a byte of it is
/// copied into the package, hashed, or named in a manifest.
#[test]
fn hig_release_script_refuses_to_package_a_binary_that_is_not_linux_x86_64() {
    let case = ReleasePackagingCase::new("hig-release-platform-gate");
    for (label, header, actually) in [
        // What `cargo build --release` produces on this host today.
        ("mach-o", mach_o_header(), "Mach-O (magic 0xcffaedfe)"),
        (
            "universal",
            universal_mach_o_header(),
            "a universal (fat) Mach-O (magic 0xcafebabe)",
        ),
        (
            "elf32",
            elf_header(1, 1, 0x03, 3),
            "ELF class 1, data 1, type 0x0003, machine 0x0003",
        ),
        (
            "aarch64",
            elf_header(2, 1, 0xb7, 3),
            "ELF class 2, data 1, type 0x0003, machine 0x00b7",
        ),
        // Right class, right machine, wrong byte order.
        (
            "big-endian",
            elf_header(2, 2, 0x3e, 3),
            "ELF class 2, data 2, type 0x0003, machine 0x003e",
        ),
        // Right class, right data, right machine, and still not a program:
        // ET_REL is what `cargo build` leaves in target/release/*.o, ET_CORE
        // is a crash dump, ET_NONE is neither.
        (
            "relocatable-object",
            elf_header(2, 1, 0x3e, 1),
            "ELF class 2, data 1, type 0x0001, machine 0x003e",
        ),
        (
            "core-dump",
            elf_header(2, 1, 0x3e, 4),
            "ELF class 2, data 1, type 0x0004, machine 0x003e",
        ),
        (
            "type-none",
            elf_header(2, 1, 0x3e, 0),
            "ELF class 2, data 1, type 0x0000, machine 0x003e",
        ),
    ] {
        let image = case.fixture.root.join(format!("image-{label}"));
        write_release_image(&image, &header);
        let output = case.fixture.root.join(format!("package-{label}"));
        let packaged = case
            .package(&output)
            .env("FAKE_RELEASE_IMAGE", &image)
            .output()
            .unwrap();
        assert!(
            !packaged.status.success(),
            "{label}: packaging a {actually} succeeded\nstdout: {}",
            String::from_utf8_lossy(&packaged.stdout)
        );
        let stderr = String::from_utf8_lossy(&packaged.stderr);
        // The gate runs on the build output, so the file it names is the one
        // the build produced - the first binary in the release set.
        let refusal = format!("/kanban: {RELEASE_PLATFORM}, but this file is {actually}");
        assert!(
            stderr.contains("hig-release: refusing ") && stderr.contains(&refusal),
            "{label}: expected a refusal ending {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&output);
        // The gate judged the BUILD OUTPUT, not the packaged copy: had it run
        // after `install -m 0755`, the binary would be sitting here. An
        // unreadable output directory is a failure, never a pass.
        let left_behind: Vec<_> = match fs::read_dir(&output) {
            Ok(entries) => entries
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            Err(error) if error.kind() == ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("{label}: reading {}: {error}", output.display()),
        };
        assert!(
            left_behind.is_empty(),
            "{label}: a refused packaging run copied {left_behind:?} into the output"
        );
    }
}

/// ADR-044 §4 asks for an EXECUTABLE object type, and `e_type` is the half of
/// the header no machine check can see: `cargo build --release` leaves
/// relocatable objects in target/release beside the binaries, and one of them
/// carries the right ELF class, the right byte order and the right machine
/// while being nothing a host can run. The refusal names the type it
/// observed, and the boundary is drawn where the linker draws it - both
/// executable types package, so the gate refuses an object rather than
/// refusing whatever is not exactly what today's profile happens to link.
#[test]
fn hig_release_script_refuses_a_relocatable_object_carrying_the_right_machine() {
    let case = ReleasePackagingCase::new("hig-release-elf-object-type");
    let object = case.fixture.root.join("kanban.o");
    write_release_image(&object, &elf_header(2, 1, 0x3e, 1));
    let output = case.fixture.root.join("package-et-rel");
    let refused = case
        .package(&output)
        .env("FAKE_RELEASE_IMAGE", &object)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "an ET_REL object was packaged as a release\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal = format!(
        "/kanban: {RELEASE_PLATFORM}, but this file is ELF class 2, data 1, type 0x0001, \
         machine 0x003e"
    );
    assert!(
        stderr.contains("hig-release: refusing ") && stderr.contains(&refusal),
        "expected a refusal ending {refusal:?} in:\n{stderr}"
    );
    assert_no_release_recorded(&output);
    let left_behind: Vec<_> = fs::read_dir(&output)
        .unwrap_or_else(|error| panic!("reading {}: {error}", output.display()))
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        left_behind.is_empty(),
        "a refused object type left {left_behind:?} in the package directory"
    );

    // The same boundary from the other side, so the refusal above is the
    // e_type check and not an accident of this fixture: ET_DYN is what the
    // linux release profile links, ET_EXEC is what a non-PIE build of the
    // same source is, and each one packages and answers for its own version.
    for (label, etype) in [("et-exec", 2_u16), ("et-dyn", 3_u16)] {
        let image = case.fixture.root.join(format!("image-{label}"));
        let mut bytes = elf_header(2, 1, 0x3e, etype).to_vec();
        bytes
            .extend_from_slice(format!("#!/bin/sh\nprintf 'kanban 0.0.0-{label}\\n'\n").as_bytes());
        fs::write(&image, &bytes).unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o755)).unwrap();
        let output = case.fixture.root.join(format!("package-{label}"));
        let packaged = case
            .package(&output)
            .env("FAKE_RELEASE_IMAGE", &image)
            .output()
            .unwrap();
        assert!(
            packaged.status.success(),
            "{label}: an executable object type was refused\nstderr: {}",
            String::from_utf8_lossy(&packaged.stderr)
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
        let versions: Vec<String> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["version"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            versions,
            vec![format!("kanban 0.0.0-{label}"); declared_bin_names().len()],
            "{label}: the packaged binaries did not answer for themselves"
        );
    }
}

/// A release binary is the file itself, and a verdict on it must terminate.
/// So the gate judges the path's TYPE before it opens anything: a FIFO would
/// hold the header read open forever - the release script has no timeout to
/// save it - and a symlink is an answer about some other file. Both are
/// refused, and the FIFO row is bounded by a deadline, because a case that
/// waits forever for a refusal reports as a hang and not as a failure.
#[test]
fn hig_release_script_refuses_a_build_output_that_is_not_a_regular_file() {
    let case = ReleasePackagingCase::new("hig-release-platform-irregular");
    // A real linux x86-64 image for the symlink to point at, so what is
    // refused is the symlink and not the thing at the end of it.
    let real = case.fixture.root.join("real-kanban");
    write_release_platform_image(&real, b"#!/bin/sh\nprintf 'kanban 0.0.0-linked\\n'\n");

    let output = case.fixture.root.join("package-fifo");
    let mut child = case
        .package(&output)
        .env("FAKE_RELEASE_FIFO", "1")
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
                panic!("packaging blocked on a FIFO build output instead of refusing it");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let refused = child.wait_with_output().unwrap();
    assert!(
        !refused.status.success(),
        "a FIFO was packaged\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal = format!("/kanban: {RELEASE_PLATFORM}, but this path is not a regular file");
    assert!(
        stderr.contains(&refusal),
        "expected a refusal ending {refusal:?} in:\n{stderr}"
    );
    assert_no_release_recorded(&output);

    let output = case.fixture.root.join("package-symlink");
    let refused = case
        .package(&output)
        .env("FAKE_RELEASE_SYMLINK", &real)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "a symlinked build output was packaged\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal = format!(
        "/kanban: {RELEASE_PLATFORM}, but this path is a symlink, not the release binary itself"
    );
    assert!(
        stderr.contains(&refusal),
        "expected a refusal ending {refusal:?} in:\n{stderr}"
    );
    assert_no_release_recorded(&output);
}

/// A file with nothing to read is refused, never waved through: an empty or
/// truncated build output has no header to judge, and the refusal says how
/// many bytes a verdict needs and how many the file gave.
#[test]
fn hig_release_script_refuses_a_payload_with_no_readable_executable_header() {
    let case = ReleasePackagingCase::new("hig-release-platform-short");
    for (label, bytes, produced) in [("truncated", &b"\x7fEL"[..], 3), ("empty", &b""[..], 0)] {
        let image = case.fixture.root.join(format!("image-{label}"));
        fs::write(&image, bytes).unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o755)).unwrap();
        let output = case.fixture.root.join(format!("package-{label}"));
        let packaged = case
            .package(&output)
            .env("FAKE_RELEASE_IMAGE", &image)
            .output()
            .unwrap();
        assert!(
            !packaged.status.success(),
            "{label}: packaging a {produced}-byte image succeeded\nstdout: {}",
            String::from_utf8_lossy(&packaged.stdout)
        );
        let stderr = String::from_utf8_lossy(&packaged.stderr);
        let refusal = format!(
            "/kanban: {RELEASE_PLATFORM}, but no executable header could be read from it: \
             20 bytes are needed and it produced {produced}"
        );
        assert!(
            stderr.contains(&refusal),
            "{label}: expected a refusal ending {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&output);
    }
}

/// The happy path on a host that cannot execute the platform it packages for.
/// Every binary the fake build produces really is a linux x86-64 image, so
/// the gate passes it; the version each one reports comes back through
/// HIG_RELEASE_TARGET_RUNNER, which is how this host runs a target-platform
/// binary at all, and the recorded probe is `version` for the two operator
/// CLIs and `--version` for the dispatcher and every adapter.
///
/// What this case does NOT prove, because the script does not check it: that
/// a runner told the truth. This fixture's runner runs the payload inside the
/// image, so the recorded version is the packaged bytes' own answer here -
/// but a set runner is trusted, and one that printed anything at all would be
/// recorded and re-validated against itself. The provenance wave's receipt
/// field naming the probe is what will make that checkable.
#[test]
fn hig_release_script_runs_target_binaries_through_the_configured_target_runner() {
    let case = ReleasePackagingCase::new("hig-release-target-runner");
    let runner_log = case.fixture.root.join("target-runner.log");
    let output = case.fixture.root.join("package");
    let packaged = case
        .package(&output)
        .env("FAKE_TARGET_RUNNER_LOG", &runner_log)
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "packaging linux x86-64 binaries failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let probe_of = |name: &str| match name {
        "kanban" | "kb" => "version",
        _ => "--version",
    };
    let reported = |probe: &str| {
        let answered = Command::new(env!("CARGO_BIN_EXE_kanban"))
            .arg(probe)
            .output()
            .unwrap();
        assert!(answered.status.success(), "the real binary refused {probe}");
        // The FIRST line: `kanban version` answers with the version and then
        // `bundle <sha256>`, and files[].version is the version (ADR-048).
        String::from_utf8_lossy(&answered.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    let mut expected_probes = Vec::new();
    for name in declared_bin_names() {
        let packaged_binary = output.join(&name);
        let bytes = fs::read(&packaged_binary).unwrap();
        assert_eq!(
            &bytes[..20],
            &linux_x86_64_elf_header(),
            "{name} was packaged without a linux x86-64 header"
        );
        let recorded = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} is missing from the manifest"));
        assert_eq!(
            recorded["version"],
            json!(reported(probe_of(&name))),
            "{name}: the manifest recorded a version the binary never reported"
        );
        expected_probes.push(format!("{name} {}", probe_of(&name)));
    }
    let mut probes: Vec<String> = fs::read_to_string(&runner_log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    probes.sort();
    probes.dedup();
    expected_probes.sort();
    assert_eq!(
        probes, expected_probes,
        "the versions in the manifest did not all come through the target runner"
    );
}

/// The runner is how a version is obtained, never a second answer to what the
/// version is: a runner that cannot run the image, or that runs it and says
/// nothing, is a refusal naming the binary. An empty version string is never
/// recorded in a manifest.
#[test]
fn hig_release_script_refuses_a_target_runner_that_reports_no_version() {
    let case = ReleasePackagingCase::new("hig-release-target-runner-mute");
    for (label, switch, refusal) in [
        (
            "silent",
            "FAKE_TARGET_RUNNER_SILENT",
            "the version probe reported nothing",
        ),
        (
            "failing",
            "FAKE_TARGET_RUNNER_FAILS",
            "could not report a version for it",
        ),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let packaged = case.package(&output).env(switch, "1").output().unwrap();
        assert!(
            !packaged.status.success(),
            "{label}: packaging succeeded with no version to record\nstdout: {}",
            String::from_utf8_lossy(&packaged.stdout)
        );
        let stderr = String::from_utf8_lossy(&packaged.stderr);
        let named = format!("refusing {}: ", output.join("kanban").display());
        assert!(
            stderr.contains(&named) && stderr.contains(refusal),
            "{label}: expected {named:?} and {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&output);
    }
}

/// The branch hax and hig actually take: no runner, the release binary asked
/// for its own version by being run. Every other case in this suite lands on
/// the runner branch, so without this one the production branch has no
/// coverage at all.
///
/// It is now reachable only on a machine that IS the release platform: the
/// capability gate hands a machine that is not one a runner into the image it
/// built in, so "no runner configured" and "the artifact is executed here"
/// stopped being the same statement. The uname stub is what declares this run
/// a linux x86-64 machine, and the container runtime is configured throughout
/// and must go untouched - a native build that quietly used a container would
/// be a different build from the one the receipt would name.
///
/// On this host that branch cannot succeed and must not pretend to: bash
/// special-cases the ELF magic in check_binary_file, so a linux x86-64 image
/// is refused by exec here rather than run as a script, and the packaging run
/// refuses with the binary named. That refusal is the assertion. The empty
/// string is a second row because an operator who exports the variable with
/// no value must land on the same branch as one who never set it - not on a
/// runner called "".
#[test]
fn hig_release_script_probes_the_version_by_running_the_binary_when_no_runner_is_configured() {
    let case = ReleasePackagingCase::new("hig-release-native-probe");
    for (label, runner) in [("unset", None), ("empty", Some(""))] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let container_log = case.fixture.root.join(format!("container-{label}.log"));
        let mut command = case.package(&output);
        command
            .env("FAKE_UNAME_S", "Linux")
            .env("FAKE_UNAME_M", "x86_64")
            .env("FAKE_CONTAINER_LOG", &container_log);
        match runner {
            None => command.env_remove("HIG_RELEASE_TARGET_RUNNER"),
            Some(value) => command.env("HIG_RELEASE_TARGET_RUNNER", value),
        };
        let packaged = command.output().unwrap();
        assert!(
            !packaged.status.success(),
            "{label}: this host cannot run a linux x86-64 image, so packaging must not report success\nstdout: {}",
            String::from_utf8_lossy(&packaged.stdout)
        );
        let stderr = String::from_utf8_lossy(&packaged.stderr);
        // The native branch's own refusal: no runner is named in it, which is
        // what tells the two branches apart.
        let refusal = format!(
            "refusing {}: it could not report a version",
            output.join("kanban").display()
        );
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the native probe's refusal {refusal:?} in:\n{stderr}"
        );
        assert!(
            !stderr.contains("could not report a version for it"),
            "{label}: the run went through a target runner instead of executing the binary:\n{stderr}"
        );
        assert!(
            !container_log.exists(),
            "{label}: a native build reached for the container runtime: {}",
            fs::read_to_string(&container_log).unwrap_or_default()
        );
        assert_no_release_recorded(&output);
    }
}

/// The runner is ONE executable program that takes no arguments of its own.
/// An operator who writes an invocation into it - `docker run --rm image`, a
/// path with flags after it, a file that is merely readable - has configured
/// something this script cannot call, and is told so by name instead of
/// leaving a bare exec failure to be read as the binary's fault.
#[test]
fn hig_release_script_refuses_a_target_runner_that_is_not_one_executable_program() {
    let case = ReleasePackagingCase::new("hig-release-runner-contract");
    let readable = case.fixture.root.join("not-executable-runner");
    fs::write(&readable, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
    let with_arguments = format!(
        "{} --rm",
        release_target_runner(&case.hostname_bin).display()
    );
    for (label, runner) in [
        ("not-executable", readable.display().to_string()),
        ("carries-arguments", with_arguments),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let packaged = case
            .package(&output)
            .env("HIG_RELEASE_TARGET_RUNNER", &runner)
            .output()
            .unwrap();
        assert!(
            !packaged.status.success(),
            "{label}: packaging accepted a runner it cannot call\nstdout: {}",
            String::from_utf8_lossy(&packaged.stdout)
        );
        let stderr = String::from_utf8_lossy(&packaged.stderr);
        let refusal = format!(
            "HIG_RELEASE_TARGET_RUNNER must name one executable program, and {runner} is not one"
        );
        assert!(
            stderr.contains(&refusal),
            "{label}: expected a refusal ending {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&output);
    }
}

/// A PATH that offers the release stubs, the shell, and the handful of
/// programs the capability gate itself runs - and nothing else this machine
/// happens to have installed. A case that must prove no container runtime is
/// reachable cannot inherit the one this workstation really has, and that
/// includes the PATH `Command::new("bash")` resolves the shell through, so
/// each program is located on the real PATH once and symlinked into a
/// directory of its own. The list is the gate's own dependencies: `uname`
/// through `tr`, the deadline's `mktemp` and `sleep`, `sed` for the answer it
/// reads back, and `rm` for the temporary files the exit trap clears.
fn path_with_stubs_and_bash_only(fixture: &Fixture, stubs: &Path, extra: Option<&Path>) -> String {
    let tools = fixture.root.join("gate-tools");
    fs::create_dir_all(&tools).unwrap();
    for tool in ["bash", "tr", "sed", "mktemp", "sleep", "rm", "cat"] {
        let link = tools.join(tool);
        if link.exists() {
            continue;
        }
        let located = Command::new("bash")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .unwrap();
        assert!(
            located.status.success(),
            "this machine has no {tool} on PATH"
        );
        let resolved = String::from_utf8_lossy(&located.stdout).trim().to_string();
        std::os::unix::fs::symlink(&resolved, &link).unwrap();
    }
    let mut entries: Vec<String> = Vec::new();
    if let Some(extra) = extra {
        entries.push(extra.display().to_string());
    }
    entries.push(stubs.display().to_string());
    entries.push(tools.display().to_string());
    entries.join(":")
}

/// The prefix every capability-gate refusal carries on the machine this suite
/// declares itself to be: it names the platform that cannot produce a release
/// before it names what was missing.
const CANNOT_PACKAGE_HERE: &str =
    "hig-release: cannot package a linux x86-64 release on darwin-arm64: ";

/// Provenance is a MEASUREMENT. Packaging on a machine that is not the
/// release platform records where it was built, what the binaries are, how
/// they were built, which image built them and which toolchain that image
/// carries - and the two are different facts throughout: `buildPlatform` is
/// darwin-arm64 here and `artifactPlatform` is linux-x86_64.
///
/// The host name is in the receipt and authorizes nothing, which is the whole
/// difference from v1: this run calls itself geoywsMBP, not hax, and packages
/// anyway. And the toolchain is asked in the environment that COMPILED - this
/// host's own rustc and cargo answer differently and neither string appears
/// anywhere in the receipt.
#[test]
fn hig_release_script_package_records_the_real_build_host_platform_and_toolchain() {
    let case = ReleasePackagingCase::new("hig-release-provenance");
    let output = case.fixture.root.join("package");
    let container_log = case.fixture.root.join("container.log");
    let packaged = case
        .package(&output)
        .env("FAKE_HOST", "geoywsMBP")
        .env("FAKE_CONTAINER_LOG", &container_log)
        .env("FAKE_CONTAINER_RUSTC", "rustc 1.90.0 (pinned image)")
        .env("FAKE_CONTAINER_CARGO", "cargo 1.90.0 (pinned image)")
        .env(
            "FAKE_RUSTC_VERSION",
            "rustc 9.9.9 (this host, which compiled nothing)",
        )
        .env(
            "FAKE_CARGO_VERSION",
            "cargo 9.9.9 (this host, which compiled nothing)",
        )
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "packaging on a machine that is not the release platform failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let receipt_bytes = fs::read(output.with_extension("receipt.json")).unwrap();
    let receipt: Value = serde_json::from_slice(&receipt_bytes).unwrap();
    assert_eq!(receipt["formatVersion"], json!(2));
    assert_eq!(
        receipt["host"],
        json!("geoywsMBP"),
        "the receipt did not record the machine that ran it"
    );
    assert_eq!(receipt["buildPlatform"], json!("darwin-arm64"));
    assert_eq!(receipt["artifactPlatform"], json!("linux-x86_64"));
    assert_eq!(receipt["buildKind"], json!("container"));
    assert_eq!(receipt["builderImage"], json!(RELEASE_BUILDER_IMAGE));
    assert_eq!(
        receipt["toolchain"],
        json!({"rustc": "rustc 1.90.0 (pinned image)", "cargo": "cargo 1.90.0 (pinned image)"}),
        "the toolchain recorded is not the one the build environment reported"
    );
    assert_eq!(receipt["versionProbe"], json!("runner"));
    assert!(
        !String::from_utf8_lossy(&receipt_bytes).contains("9.9.9"),
        "the receipt recorded this host's toolchain instead of the one that compiled the binaries"
    );

    // The build itself, as the runtime saw it: one cargo build, in the pinned
    // image, asked for linux/amd64, with the worktree read-only and the target
    // directory outside it.
    let log = fs::read_to_string(&container_log).unwrap();
    let build: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("cargo build --release --locked --bins"))
        .collect();
    assert_eq!(
        build.len(),
        1,
        "expected exactly one container build in:\n{log}"
    );
    let build = build[0];
    assert!(
        build.contains("--platform linux/amd64"),
        "the container build did not ask for linux/amd64: {build}"
    );
    assert!(
        build.contains(RELEASE_BUILDER_IMAGE),
        "the container build did not run in the pinned image: {build}"
    );
    let repo = case.fake_repo_root.display().to_string();
    assert!(
        build.contains(&format!("--volume {repo}:{repo}:ro")),
        "the container build did not mount the worktree read-only: {build}"
    );
    let target_dir = build
        .split_whitespace()
        .find_map(|token| token.strip_prefix("CARGO_TARGET_DIR="))
        .unwrap_or_else(|| panic!("the container build set no CARGO_TARGET_DIR: {build}"));
    assert!(
        !target_dir.starts_with(&repo),
        "the container build wrote its target directory into the worktree: {target_dir}"
    );
    assert!(
        log.lines().any(|line| line.ends_with("uname -m")),
        "the gate never measured what the pinned image runs:\n{log}"
    );

    // A machine that cannot say what it is called does not get to write a
    // receipt that says nothing. Before this refusal existed, a misdirected
    // HOSTNAME_BIN left a bash error on stderr, wrote "host": "" and exited
    // 0 with a success summary - a package only hax would ever reject, and
    // only after it had been carried there.
    for (label, reported, said) in [
        ("silent", "", "nothing"),
        ("spaced", "two words", "two words"),
    ] {
        let refused_output = case.fixture.root.join(format!("package-{label}-host"));
        let refused_log = case
            .fixture
            .root
            .join(format!("container-{label}-host.log"));
        let refused = case
            .package(&refused_output)
            .env("FAKE_HOST", reported)
            .env("FAKE_CONTAINER_LOG", &refused_log)
            .output()
            .unwrap();
        assert!(
            !refused.status.success(),
            "{label}: a release was packaged by a machine that could not name itself\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let refusal = format!(
            "hig-release: refusing to package: {} did not report a usable short hostname (it said \
             {said}), and a release receipt records the machine that built it",
            case.hostname_bin.display()
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&refused_output);
        assert!(
            !fs::read_to_string(&refused_log)
                .unwrap_or_default()
                .contains("cargo build"),
            "{label}: the refusal came after the build instead of before it"
        );
    }
}

/// The image is named by digest on the receipt because the digest is what
/// names the bytes that built the release - and because it is also what the
/// version probe goes back into. With no runner of the operator's own, the
/// script installs one that dispatches into the SAME pinned image against the
/// package directory, so every files[].version is the packaged bytes' own
/// answer obtained the only way this machine can obtain it.
#[test]
fn hig_release_script_package_records_the_pinned_builder_image_digest_on_the_container_path() {
    let case = ReleasePackagingCase::new("hig-release-image-digest");
    let output = case.fixture.root.join("package");
    let container_log = case.fixture.root.join("container.log");
    let runner_log = case.fixture.root.join("operator-runner.log");
    let packaged = case
        .package(&output)
        .env_remove("HIG_RELEASE_TARGET_RUNNER")
        .env("FAKE_CONTAINER_LOG", &container_log)
        .env("FAKE_TARGET_RUNNER_LOG", &runner_log)
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "packaging through the pinned image failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let receipt: Value =
        serde_json::from_slice(&fs::read(output.with_extension("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["buildKind"], json!("container"));
    assert_eq!(receipt["builderImage"], json!(RELEASE_BUILDER_IMAGE));
    assert_eq!(receipt["versionProbe"], json!("runner"));
    assert!(
        !runner_log.exists(),
        "a runner the operator never configured answered the version probe"
    );
    let log = fs::read_to_string(&container_log).unwrap();
    let manifest: Value =
        serde_json::from_slice(&fs::read(output.join("manifest.json")).unwrap()).unwrap();
    // What the packaged bytes answer when they are run: the fixture's build
    // stamps the release header onto this very binary, so the version the
    // image reports back has to be this binary's own.
    let reported = |probe: &str| {
        let answered = Command::new(env!("CARGO_BIN_EXE_kanban"))
            .arg(probe)
            .output()
            .unwrap();
        assert!(answered.status.success(), "the real binary refused {probe}");
        // The FIRST line: `kanban version` answers with the version and then
        // `bundle <sha256>`, and files[].version is the version (ADR-048).
        String::from_utf8_lossy(&answered.stdout)
            .lines()
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    for name in declared_bin_names() {
        let probe = if name == "kanban" || name == "kb" {
            "version"
        } else {
            "--version"
        };
        let dispatch = format!(
            "{RELEASE_BUILDER_IMAGE} {} {probe}",
            output.join(&name).display()
        );
        assert!(
            log.lines().any(|line| line.ends_with(&dispatch)),
            "{name} was never probed inside the pinned image:\n{log}"
        );
        let recorded = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|file| file["name"] == json!(name))
            .unwrap_or_else(|| panic!("{name} is missing from the manifest"));
        assert_eq!(
            recorded["version"],
            json!(reported(probe)),
            "{name}: the manifest recorded a version the image never reported"
        );
    }
}

/// A machine that already IS the release platform builds as it always has:
/// the gate picks `native`, the container runtime it was handed is never
/// called even though one is configured and an image is named, and the
/// receipt says so - `builderImage` is JSON null, which says "no image was
/// used" rather than "unknown", and the key is present so a reader never has
/// to tell an absent field from an unknown value.
///
/// `versionProbe` is still `runner` here, because this host cannot execute a
/// linux x86-64 image and the fixture hands the script a runner that can.
/// That is the pairing the schema allows and the receipt states: a native
/// build whose version came through a named runner.
#[test]
fn hig_release_script_package_builds_natively_when_the_machine_is_already_the_release_platform() {
    let case = ReleasePackagingCase::new("hig-release-native-gate");
    let output = case.fixture.root.join("package");
    let container_log = case.fixture.root.join("container.log");
    let packaged = case
        .package(&output)
        .env("FAKE_UNAME_S", "Linux")
        .env("FAKE_UNAME_M", "x86_64")
        .env("FAKE_CONTAINER_LOG", &container_log)
        .env("FAKE_RUSTC_VERSION", "rustc 1.90.0 (this host)")
        .env("FAKE_CARGO_VERSION", "cargo 1.90.0 (this host)")
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "a native release build failed: {}\nstderr: {}",
        String::from_utf8_lossy(&packaged.stdout),
        String::from_utf8_lossy(&packaged.stderr)
    );
    let receipt: Value =
        serde_json::from_slice(&fs::read(output.with_extension("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["formatVersion"], json!(2));
    assert_eq!(receipt["buildKind"], json!("native"));
    assert_eq!(receipt["buildPlatform"], json!("linux-x86_64"));
    assert_eq!(receipt["artifactPlatform"], json!("linux-x86_64"));
    assert!(
        receipt.as_object().unwrap().contains_key("builderImage"),
        "a native receipt dropped the builderImage key instead of writing null"
    );
    assert_eq!(receipt["builderImage"], json!(null));
    assert_eq!(
        receipt["toolchain"],
        json!({"rustc": "rustc 1.90.0 (this host)", "cargo": "cargo 1.90.0 (this host)"})
    );
    assert_eq!(receipt["versionProbe"], json!("runner"));
    assert!(
        !container_log.exists(),
        "a native build ran through a container runtime: {}",
        fs::read_to_string(&container_log).unwrap_or_default()
    );
}

/// A machine that cannot produce a linux x86-64 artifact is told so in one
/// sentence naming the platform it is and the one thing that was missing -
/// before anything is built, written or even measured about the repository.
#[test]
fn hig_release_script_package_refuses_a_machine_that_cannot_produce_a_linux_x86_64_artifact_naming_what_was_missing()
 {
    let case = ReleasePackagingCase::new("hig-release-capability-gate");
    let stubs = case.hostname_bin.parent().unwrap().to_path_buf();
    let unusable = case.fixture.root.join("not-a-container-runtime");
    fs::write(&unusable, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&unusable, fs::Permissions::from_mode(0o644)).unwrap();
    for (label, refusal) in [
        (
            "no-runtime",
            format!(
                "{CANNOT_PACKAGE_HERE}no container runtime found; none of docker, podman, nerdctl \
                 is on PATH and KANBAN_RELEASE_CONTAINER_RUNTIME is unset, so there is no linux \
                 x86-64 build environment on this machine"
            ),
        ),
        (
            "unusable-runtime",
            format!(
                "{CANNOT_PACKAGE_HERE}no container runtime found; \
                 KANBAN_RELEASE_CONTAINER_RUNTIME names {}, which is not an executable program, \
                 so there is no linux x86-64 build environment on this machine",
                unusable.display()
            ),
        ),
        (
            "no-image",
            format!(
                "{CANNOT_PACKAGE_HERE}no builder image was named; pass --builder-image \
                 <ref>@sha256:<64 hex> or set KANBAN_RELEASE_BUILDER_IMAGE"
            ),
        ),
        (
            "foreign-image",
            format!(
                "{CANNOT_PACKAGE_HERE}builder image {RELEASE_BUILDER_IMAGE} does not run linux \
                 x86-64; uname -m inside it reported aarch64"
            ),
        ),
        (
            // A runtime that answers nothing, ever: there is no deadline
            // anywhere else in packaging, so without one here the release
            // path hangs and says nothing at all.
            "wedged-runtime",
            format!(
                "{CANNOT_PACKAGE_HERE}builder image {RELEASE_BUILDER_IMAGE} did not answer \
                 uname -m through {} within 1s, so nothing here shows it runs linux x86-64; \
                 raise KANBAN_RELEASE_CONTAINER_PROBE_SECONDS if this machine still has to \
                 pull it",
                release_container_runtime(&case.hostname_bin).display()
            ),
        ),
        (
            // The runtime's own complaint, which is the only thing that says
            // whether the daemon is down or the image cannot be pulled.
            "failing-runtime",
            format!(
                "{CANNOT_PACKAGE_HERE}{} could not run builder image {RELEASE_BUILDER_IMAGE}: \
                 cannot connect to the container daemon: is it running?",
                release_container_runtime(&case.hostname_bin).display()
            ),
        ),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let container_log = case.fixture.root.join(format!("container-{label}.log"));
        let mut command = case.package(&output);
        command.env("FAKE_CONTAINER_LOG", &container_log);
        match label {
            // A PATH with no runtime on it at all: the gate refuses before
            // anything else on that PATH would have been needed.
            "no-runtime" => command
                .env(
                    "PATH",
                    path_with_stubs_and_bash_only(&case.fixture, &stubs, None),
                )
                .env_remove("KANBAN_RELEASE_CONTAINER_RUNTIME"),
            "unusable-runtime" => command.env("KANBAN_RELEASE_CONTAINER_RUNTIME", &unusable),
            "no-image" => command.env_remove("KANBAN_RELEASE_BUILDER_IMAGE"),
            "foreign-image" => command.env("FAKE_CONTAINER_UNAME", "aarch64"),
            "wedged-runtime" => command
                .env("FAKE_CONTAINER_HANGS", "1")
                .env("KANBAN_RELEASE_CONTAINER_PROBE_SECONDS", "1"),
            "failing-runtime" => command.env("FAKE_CONTAINER_FAILS", "1"),
            other => unreachable!("unhandled capability row {other}"),
        };
        // Spawned against a deadline rather than waited on: a gate that
        // failed to bound the wedged runtime would hang this case, and a
        // hang reports as neither a pass nor a failure.
        let started = Instant::now();
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = started + Duration::from_secs(60);
        loop {
            match child.try_wait().unwrap() {
                Some(_) => break,
                None if Instant::now() >= deadline => {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("{label}: packaging never came back instead of refusing");
                }
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let elapsed = started.elapsed();
        let refused = child.wait_with_output().unwrap();
        if label == "wedged-runtime" {
            // The deadline the gate was given is 1s; the wall time is the
            // measurement that says it was honoured rather than merely
            // survived by this case's own timeout.
            assert!(
                elapsed < Duration::from_secs(30),
                "the 1s probe deadline took {elapsed:?} to refuse a runtime that never answers"
            );
        }
        assert!(
            !refused.status.success(),
            "{label}: a machine that cannot build a release packaged one\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert!(
            !output.exists(),
            "{label}: a refused gate created {}",
            output.display()
        );
        assert_no_release_recorded(&output);
        if !matches!(
            label,
            "foreign-image" | "wedged-runtime" | "failing-runtime"
        ) {
            assert!(
                !container_log.exists(),
                "{label}: the gate called a runtime it had already refused"
            );
        }
    }
}

/// `build_platform` is the FIRST measurement packaging makes, and the only
/// thing it can measure is what this machine answers: a `uname` that says
/// nothing leaves `buildPlatform` with nothing to record, so the run ends
/// there rather than writing a platform nobody measured or comparing an empty
/// string against the artifact's own. The last row is the one the
/// `|| kernel=""` fallbacks exist for - a `uname` that FAILS rather than
/// answering emptily, which under `set -e` would otherwise end the run with
/// no sentence at all.
///
/// Driven through the seam `build_platform` itself names: `uname` resolves
/// through PATH exactly as `rustc` and `cargo` do, so a stub ahead of the
/// suite's own on PATH is what a machine that cannot answer looks like.
#[test]
fn hig_release_script_package_refuses_a_machine_that_reports_no_platform() {
    let case = ReleasePackagingCase::new("hig-release-build-platform");
    let mute = case.fixture.root.join("mute-uname");
    fs::create_dir_all(&mute).unwrap();
    write_executable(
        &mute.join("uname"),
        // FAKE_UNAME_MUTE names the flag this machine answers nothing for -
        // `s`, `m` or `both` - and FAKE_UNAME_MUTE_FAILS makes that silence
        // an outright failure instead of an empty answer.
        r#"#!/bin/sh
set -eu
flag="${1:-}"
case "$flag" in
  -s)
    answer="${FAKE_UNAME_S:-Darwin}"
    ;;
  -m)
    answer="${FAKE_UNAME_M:-arm64}"
    ;;
  *)
    printf 'unexpected uname %s\n' "$*" >&2
    exit 1
    ;;
esac
case "${FAKE_UNAME_MUTE:?}:$flag" in
  both:* | s:-s | m:-m)
    answer=""
    ;;
esac
[ -z "$answer" ] || {
  printf '%s\n' "$answer"
  exit 0
}
[ -z "${FAKE_UNAME_MUTE_FAILS:-}" ] || {
  printf 'uname: cannot determine %s\n' "$flag" >&2
  exit 1
}
exit 0
"#,
    );
    let path = format!("{}:{}", mute.display(), case.path);
    for (label, muted, fails, said) in [
        (
            "no-kernel",
            "s",
            false,
            "uname -s said nothing and uname -m said arm64",
        ),
        (
            "no-machine",
            "m",
            false,
            "uname -s said darwin and uname -m said nothing",
        ),
        (
            "uname-fails",
            "both",
            true,
            "uname -s said nothing and uname -m said nothing",
        ),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let container_log = case.fixture.root.join(format!("container-{label}.log"));
        let mut command = case.package(&output);
        command
            .env("PATH", &path)
            .env("FAKE_UNAME_MUTE", muted)
            .env("FAKE_CONTAINER_LOG", &container_log);
        if fails {
            command.env("FAKE_UNAME_MUTE_FAILS", "1");
        }
        let refused = command.output().unwrap();
        assert!(
            !refused.status.success(),
            "{label}: a machine that reported no platform packaged a release\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert_eq!(refused.status.code(), Some(1), "{label}");
        let refusal = format!("hig-release: this machine did not report a platform: {said}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert!(
            !output.exists(),
            "{label}: an unmeasured platform created {}",
            output.display()
        );
        assert_no_release_recorded(&output);
        // Nothing was built, because the platform is measured before the gate
        // has an opinion about a runtime, an image or a toolchain.
        assert!(
            !container_log.exists(),
            "{label}: the run reached a container runtime: {}",
            fs::read_to_string(&container_log).unwrap_or_default()
        );
    }
}

/// The capability probe is not the only container call a release makes
/// before it can still be refused for free: the toolchain the receipt
/// records is asked for inside the same image, and a runtime that wedges
/// there would hang packaging just as completely. It is bounded by the same
/// deadline and refuses with the same kind of sentence - naming the image,
/// the runtime, the seconds and the variable that raises them.
#[test]
fn hig_release_script_package_refuses_a_container_toolchain_probe_that_never_answers() {
    let case = ReleasePackagingCase::new("hig-release-toolchain-deadline");
    let output = case.fixture.root.join("package");
    let started = Instant::now();
    let mut child = case
        .package(&output)
        .env("FAKE_CONTAINER_HANGS_TOOLCHAIN", "1")
        // Only the TOOLCHAIN probe gets the 1s ceiling: the capability and
        // version probes share the container knob and must keep its 120s
        // default, because on a loaded machine those probes still have to
        // succeed - shrinking their budget was the flake this case used to
        // carry (wrong refusal, right knob, 2026-09-11).
        .env("KANBAN_RELEASE_TOOLCHAIN_PROBE_SECONDS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = started + Duration::from_secs(60);
    loop {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("a wedged toolchain probe hung packaging instead of refusing it");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let elapsed = started.elapsed();
    let refused = child.wait_with_output().unwrap();
    assert!(
        !refused.status.success(),
        "a toolchain that never answered was recorded anyway\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal = format!(
        "hig-release: refusing to record this release's toolchain: the builder image \
         {RELEASE_BUILDER_IMAGE} did not answer rustc --version through {} within 1s; raise \
         KANBAN_RELEASE_TOOLCHAIN_PROBE_SECONDS if this machine needs longer",
        release_container_runtime(&case.hostname_bin).display()
    );
    assert!(
        stderr.contains(&refusal),
        "expected the refusal {refusal:?} in:\n{stderr}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "the 1s toolchain deadline took {elapsed:?} to refuse"
    );
    assert_no_release_recorded(&output);
}

/// An unmeasured toolchain is not a toolchain field, so a builder image that
/// runs but cannot say what compiled the binaries ends the release. The
/// refusal carries the image's OWN complaint, because only it knows whether
/// the toolchain is absent or broken, and falls back to the exit status when
/// it says nothing at all - and the tool named is whichever one failed, so
/// `cargo` is measured as well as `rustc`.
///
/// Where this refusal sits is the point of the last assertion: the toolchain
/// is asked AFTER the container build, so a completed build is thrown away
/// rather than recorded beside a toolchain nobody measured. Nothing is
/// written either way - no package directory, no manifest, no receipt.
#[test]
fn hig_release_script_package_refuses_a_toolchain_the_builder_image_cannot_report() {
    let case = ReleasePackagingCase::new("hig-release-toolchain-mute");
    let toolchain = "hig-release: refusing to record this release's toolchain:";
    for (label, mute, fails, complaint, refusal) in [
        (
            "rustc-complains",
            None,
            Some("rustc"),
            Some("exec: rustc: not found"),
            format!(
                "{toolchain} the builder image {RELEASE_BUILDER_IMAGE} could not report a rustc \
                 version: exec: rustc: not found"
            ),
        ),
        (
            // A tool that fails without a word: the status is the only thing
            // left to say, and saying it beats an empty clause.
            "rustc-fails-mutely",
            None,
            Some("rustc"),
            None,
            format!(
                "{toolchain} the builder image {RELEASE_BUILDER_IMAGE} could not report a rustc \
                 version: it exited 3 without saying why"
            ),
        ),
        (
            // The second half of the recorded toolchain, asked after rustc
            // answered: the refusal names cargo, not the tool that worked.
            "cargo-complains",
            None,
            Some("cargo"),
            Some("error: no override and no default toolchain set"),
            format!(
                "{toolchain} the builder image {RELEASE_BUILDER_IMAGE} could not report a cargo \
                 version: error: no override and no default toolchain set"
            ),
        ),
        (
            // Exit 0 and an empty answer, which is the failure mode a status
            // check alone would wave through into the receipt.
            "rustc-answers-nothing",
            Some("rustc"),
            None,
            None,
            format!("{toolchain} the rustc version probe reported nothing"),
        ),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let container_log = case.fixture.root.join(format!("container-{label}.log"));
        let mut command = case.package(&output);
        command.env("FAKE_CONTAINER_LOG", &container_log);
        if let Some(tool) = mute {
            command.env("FAKE_CONTAINER_TOOLCHAIN_MUTE", tool);
        }
        if let Some(tool) = fails {
            command.env("FAKE_CONTAINER_TOOLCHAIN_FAILS", tool);
        }
        if let Some(text) = complaint {
            command.env("FAKE_CONTAINER_TOOLCHAIN_COMPLAINT", text);
        }
        let refused = command.output().unwrap();
        assert!(
            !refused.status.success(),
            "{label}: an unmeasured toolchain was recorded\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert_eq!(refused.status.code(), Some(1), "{label}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert!(
            !output.exists(),
            "{label}: a refused toolchain measurement created {}",
            output.display()
        );
        assert_no_release_recorded(&output);
        let logged = fs::read_to_string(&container_log)
            .unwrap_or_else(|error| panic!("{label}: no runtime was called at all: {error}"));
        assert!(
            logged.contains("cargo build --release --locked --bins"),
            "{label}: the toolchain was refused before the build it describes:\n{logged}"
        );
    }
}

/// The probe deadline is a bound an operator CHOSE, so a value that is not
/// one is refused rather than quietly replaced by the default: a ceiling of
/// `0`, of `1.5`, or of `60s` would otherwise become 120 seconds nobody
/// asked for, or - worse for `0` - a probe with no time to answer at all.
/// The refusal quotes the value back, because an operator who exported it in
/// another shell cannot see it from here.
///
/// Every row refuses before the runtime is called even once, which is what
/// makes a mistyped deadline free: the container log does not exist, so
/// nothing was probed, pulled or built.
#[test]
fn hig_release_script_package_refuses_a_probe_deadline_that_is_not_a_positive_whole_number() {
    let case = ReleasePackagingCase::new("hig-release-probe-deadline");
    for (label, seconds) in [
        ("zero", "0"),
        ("negative", "-30"),
        ("fractional", "1.5"),
        ("with-units", "60s"),
        ("words", "two minutes"),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let container_log = case.fixture.root.join(format!("container-{label}.log"));
        let refused = case
            .package(&output)
            .env("KANBAN_RELEASE_CONTAINER_PROBE_SECONDS", seconds)
            .env("FAKE_CONTAINER_LOG", &container_log)
            .output()
            .unwrap();
        assert!(
            !refused.status.success(),
            "{label}: a probe deadline of {seconds:?} was accepted\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        assert_eq!(refused.status.code(), Some(1), "{label}");
        let refusal = format!(
            "hig-release: KANBAN_RELEASE_CONTAINER_PROBE_SECONDS must be a whole number of \
             seconds greater than zero, and it is {seconds}"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert!(
            !output.exists(),
            "{label}: a refused deadline created {}",
            output.display()
        );
        assert_no_release_recorded(&output);
        assert!(
            !container_log.exists(),
            "{label}: a runtime was called under a deadline the script had refused: {}",
            fs::read_to_string(&container_log).unwrap_or_default()
        );
    }
}

/// A tag can move, so a tag cannot name the bytes that built a release. The
/// gate takes a digest-pinned reference and nothing that merely resembles
/// one, and the flag beats the environment so an operator who pins on the
/// command line is not silently overridden by a stale export.
#[test]
fn hig_release_script_package_refuses_a_builder_image_that_is_not_digest_pinned() {
    let case = ReleasePackagingCase::new("hig-release-image-pinning");
    for (label, image, on_the_flag) in [
        ("tag", "rust:1.90-bookworm".to_string(), false),
        ("bare-name", "rust".to_string(), false),
        (
            "short-digest",
            format!("rust@sha256:{}", "a".repeat(63)),
            false,
        ),
        (
            "upper-case-digest",
            format!("rust@sha256:{}", "A".repeat(64)),
            false,
        ),
        (
            "other-algorithm",
            format!("rust@sha512:{}", "a".repeat(64)),
            false,
        ),
        // The flag is what the operator typed; a pinned export must not save
        // an unpinned flag.
        ("tag-on-the-flag", "rust:1.90-bookworm".to_string(), true),
    ] {
        let output = case.fixture.root.join(format!("package-{label}"));
        let mut command = case.package(&output);
        if on_the_flag {
            command.args(["--builder-image", &image]);
        } else {
            command.env("KANBAN_RELEASE_BUILDER_IMAGE", &image);
        }
        let refused = command.output().unwrap();
        assert!(
            !refused.status.success(),
            "{label}: an unpinned builder image was accepted\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let refusal = format!(
            "{CANNOT_PACKAGE_HERE}builder image {image} is not digest-pinned, and a tag can \
             move, so its digest would not name the bytes that built this release"
        );
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(
            stderr.contains(&refusal),
            "{label}: expected the refusal {refusal:?} in:\n{stderr}"
        );
        assert_no_release_recorded(&output);
    }
}

/// Which runtime drives the image is a capability, not a brand: the gate
/// takes the first of docker, podman and nerdctl that is on PATH, in that
/// order, and KANBAN_RELEASE_CONTAINER_RUNTIME replaces the search entirely.
/// Every row here refuses at the image measurement, because the question is
/// only which program was asked - and a refusal answers it without building
/// anything.
#[test]
fn hig_release_script_package_discovers_a_container_runtime_by_name_in_order() {
    let case = ReleasePackagingCase::new("hig-release-runtime-discovery");
    let stubs = case.hostname_bin.parent().unwrap().to_path_buf();
    let runtime = release_container_runtime(&case.hostname_bin);
    for (label, offered, overridden, expected) in [
        (
            "all-three",
            &["docker", "podman", "nerdctl"][..],
            None,
            "docker",
        ),
        ("no-docker", &["podman", "nerdctl"][..], None, "podman"),
        ("nerdctl-only", &["nerdctl"][..], None, "nerdctl"),
        (
            "operator-override",
            &["docker", "podman", "nerdctl"][..],
            Some("nerdctl"),
            "nerdctl",
        ),
    ] {
        let offered_dir = case.fixture.root.join(format!("runtimes-{label}"));
        fs::create_dir_all(&offered_dir).unwrap();
        for name in offered {
            fs::copy(&runtime, offered_dir.join(name)).unwrap();
        }
        let log = case.fixture.root.join(format!("discovery-{label}.log"));
        let output = case.fixture.root.join(format!("package-{label}"));
        let mut command = case.package(&output);
        command
            .env(
                "PATH",
                path_with_stubs_and_bash_only(&case.fixture, &stubs, Some(&offered_dir)),
            )
            .env("FAKE_CONTAINER_LOG", &log)
            .env("FAKE_CONTAINER_UNAME", "aarch64");
        match overridden {
            None => command.env_remove("KANBAN_RELEASE_CONTAINER_RUNTIME"),
            Some(name) => command.env("KANBAN_RELEASE_CONTAINER_RUNTIME", offered_dir.join(name)),
        };
        let refused = command.output().unwrap();
        assert!(
            !refused.status.success(),
            "{label}: an image reporting aarch64 was accepted\nstdout: {}",
            String::from_utf8_lossy(&refused.stdout)
        );
        let logged = fs::read_to_string(&log)
            .unwrap_or_else(|error| panic!("{label}: no runtime was called at all: {error}"));
        let called = logged
            .lines()
            .next()
            .unwrap_or_else(|| panic!("{label}: the runtime log is empty"))
            .split_whitespace()
            .next()
            .unwrap();
        assert_eq!(called, expected, "{label}: the wrong runtime was chosen");
    }
}

/// `versionProbe` names the branch that actually answered, and it is written
/// from that same branch rather than from a flag or from `buildKind`.
///
/// What this suite cannot show is a receipt reading `native`: that requires a
/// machine that both IS the release platform and can execute the artifact,
/// and no Mac can execute a linux x86-64 image at all - the native row below
/// therefore ends in the refusal that branch produces here, with no receipt
/// written. A receipt that CLAIMS `native` on a build platform that is not
/// the artifact's own is refused by every validator, which is asserted in
/// hig_release_script_install_refuses_a_receipt_whose_provenance_could_not_have_been_produced.
#[test]
fn hig_release_script_package_records_which_branch_measured_the_version() {
    let case = ReleasePackagingCase::new("hig-release-version-probe");

    let output = case.fixture.root.join("package-operator-runner");
    let runner_log = case.fixture.root.join("operator-runner.log");
    let container_log = case.fixture.root.join("container.log");
    let packaged = case
        .package(&output)
        .env("FAKE_TARGET_RUNNER_LOG", &runner_log)
        .env("FAKE_CONTAINER_LOG", &container_log)
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "packaging through the operator's runner failed: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );
    let receipt: Value =
        serde_json::from_slice(&fs::read(output.with_extension("receipt.json")).unwrap()).unwrap();
    assert_eq!(receipt["versionProbe"], json!("runner"));
    let probed = fs::read_to_string(&runner_log).unwrap();
    for name in declared_bin_names() {
        assert!(
            probed
                .lines()
                .any(|line| line.starts_with(&format!("{name} "))),
            "{name} did not go through the operator's runner, which the receipt says answered:\n{probed}"
        );
    }
    let containerised = fs::read_to_string(&container_log).unwrap();
    assert!(
        !containerised
            .lines()
            .any(|line| line.contains(&output.join("kanban").display().to_string())),
        "the operator's runner was configured and the image answered anyway:\n{containerised}"
    );

    // The native branch, on a machine that is the release platform and has no
    // runner: it executes the artifact, which this host cannot do, so it
    // refuses instead of recording anything.
    let native = case.fixture.root.join("package-native-probe");
    let refused = case
        .package(&native)
        .env("FAKE_UNAME_S", "Linux")
        .env("FAKE_UNAME_M", "x86_64")
        .env_remove("HIG_RELEASE_TARGET_RUNNER")
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "the native probe reported a version on a host that cannot run the artifact"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains(&format!(
            "refusing {}: it could not report a version",
            native.join("kanban").display()
        )),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_no_release_recorded(&native);
}

/// The six provenance fields went into the receipt and NOT into the manifest,
/// which keeps its formatVersion 1 and gains nothing about the BUILD. That is
/// what leaves every `releaseId` on both hosts where it is: the identity is
/// derived from the manifest bytes, and the manifest bytes move only when
/// what is IN the package moves. `bundleSha256` is in the manifest for
/// exactly that reason — it names an artefact the package carries, like
/// `files[]`, not a fact about the machine that built it (ADR-048).
#[test]
fn hig_release_script_provenance_fields_leave_the_manifest_bytes_and_release_id_unchanged() {
    let case = ReleasePackagingCase::new("hig-release-manifest-untouched");
    let output = case.fixture.root.join("package");
    let packaged = case.package(&output).output().unwrap();
    assert!(
        packaged.status.success(),
        "packaging failed: {}",
        String::from_utf8_lossy(&packaged.stderr)
    );
    let manifest_path = output.join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["formatVersion"], json!(1));
    assert_eq!(
        manifest.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec![
            "files",
            "formatVersion",
            "sourceCommit",
            "sourceTreeClean",
            "targets"
        ],
        "the manifest gained or lost a field"
    );
    for field in [
        "host",
        "buildPlatform",
        "artifactPlatform",
        "buildKind",
        "builderImage",
        "toolchain",
        "versionProbe",
    ] {
        assert!(
            manifest.get(field).is_none(),
            "the manifest carries the provenance field {field}, which would rename every release"
        );
        assert!(
            manifest["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|file| file.get(field).is_none()),
            "a manifest file row carries the provenance field {field}"
        );
    }
    assert_eq!(
        release_id_from_package(&output),
        format!(
            "0123456789abcdef0123456789abcdef01234567-{}",
            file_sha256(&manifest_path)
        ),
        "the release identity is no longer the source commit and the manifest's own hash"
    );
}

/// A hig install is only allowed because the same release is already serving
/// on hax, which the installer verifies by measuring the release directory
/// hax activated. That loop measures binaries, so it reads their platform
/// too: a hax release directory holding something that is not linux x86-64
/// is not a release to certify hig against, and it is refused before the
/// package is shipped anywhere.
#[test]
fn hig_release_script_refuses_a_hig_install_when_the_activated_hax_release_is_not_linux_x86_64() {
    let harness = ReleaseGuardHarness::new("hig-release-hax-release-platform");
    let release_id = release_id_from_package(&harness.package_dir);
    let activated = harness
        .hax_install_root
        .join("releases")
        .join(&release_id)
        .join("kanban");
    assert!(
        activated.is_file(),
        "the hax release to corrupt is missing: {}",
        activated.display()
    );
    write_release_image(&activated, &elf_header(2, 1, 0xb7, 3));
    let install_root = harness.fixture.root.join("hax-release-platform-install");
    let bin_dir = harness.fixture.root.join("hax-release-platform-bin");
    let refused = harness.install_from(
        "hig",
        &harness.package_dir,
        &harness.hax_install_root,
        &install_root,
        &bin_dir,
    );
    assert!(
        !refused.status.success(),
        "hig was certified against a hax release that is not the release platform\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal = format!(
        "refusing {}: {RELEASE_PLATFORM}, but this file is ELF class 2, data 1, type 0x0003, machine 0x00b7",
        activated.display()
    );
    assert!(
        stderr.contains(&refusal),
        "expected the activated hax binary to be named in a refusal ending {refusal:?}:\n{stderr}"
    );
    assert!(
        !install_root.exists(),
        "a refused hig install created {}",
        install_root.display()
    );
}

/// The live store sits under a root-only ancestor: `/root` is 0700 on hig and
/// `/root/.local` is 0750 on hax, and George kept it there (a-e5391903,
/// 2026-09-30) because only root reads it. An installer that demanded
/// other-traverse on every ancestor refused the 102799b release on hax before
/// writing anything (2026-10-01), so both targets must install and activate
/// under an ancestor that grants other nothing.
#[test]
fn hig_release_script_installs_under_a_root_only_ancestor() {
    let harness = ReleaseGuardHarness::new("hig-release-root-only-ancestor");
    let private = harness.fixture.root.join("root-only-ancestor");
    fs::create_dir_all(&private).unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
    // Production's TMPDIR does not contain the store, so neither may this one.
    let tmp = harness.fixture.root.join("root-only-tmp");
    fs::create_dir_all(&tmp).unwrap();
    let mut outcomes = Vec::new();
    for target in ["hax", "hig"] {
        let install_root = private.join(format!("store-{target}"));
        let bin_dir = harness.fixture.root.join(format!("root-only-bin-{target}"));
        let installed = harness
            .install_command(
                target,
                &harness.package_dir,
                &harness.hax_install_root,
                &install_root,
                &bin_dir,
            )
            .env("TMPDIR", &tmp)
            .output()
            .unwrap();
        let current = fs::read_link(install_root.join("current")).ok();
        outcomes.push((target, installed, install_root, bin_dir, current));
    }
    fs::set_permissions(&private, fs::Permissions::from_mode(0o755)).unwrap();
    for (target, installed, install_root, bin_dir, current) in outcomes {
        assert!(
            installed.status.success(),
            "{target}: install under a 0700 ancestor failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let release_dir = current.unwrap_or_else(|| panic!("{target}: no current link"));
        assert_release_view(&install_root, &bin_dir, &release_dir);
    }
}

/// Packaging is not the only way a binary reaches a release store: a package
/// directory can be handed to `install` by anyone. So the installer reads the
/// header of every binary it is about to activate, and it reads it before the
/// size and the hash, because a foreign image whose manifest agrees with it
/// perfectly is still not a release. Both legs refuse, and neither touches
/// the install root on the way out.
#[test]
fn hig_release_script_refuses_to_install_a_package_binary_that_is_not_linux_x86_64() {
    let harness = ReleaseGuardHarness::new("hig-release-install-platform");
    let forged = harness.fixture.root.join("forged");
    clone_release_package(
        &harness.package_dir,
        &forged,
        "0123456789abcdef0123456789abcdef0000fa01",
    );
    let foreign = forged.join("kanban");
    write_release_image(&foreign, &elf_header(2, 1, 0xb7, 3));
    // Everything else about the package stays true - the manifest and the
    // receipt record the foreign image's own size and hash - so the platform
    // gate is the only thing left that can refuse it.
    let bytes = fs::metadata(&foreign).unwrap().len();
    let sha256 = file_sha256(&foreign);
    let manifest_path = forged.join("manifest.json");
    let receipt_path = forged.with_extension("receipt.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let mut receipt: Value = serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    for document in [&mut manifest, &mut receipt] {
        for file in document["files"].as_array_mut().unwrap() {
            if file["name"] == json!("kanban") {
                file["sha256"] = json!(sha256);
                file["bytes"] = json!(bytes);
            }
        }
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    receipt["manifestSha256"] = json!(file_sha256(&manifest_path));
    fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();

    let refusal = format!(
        "refusing {}: {RELEASE_PLATFORM}, but this file is ELF class 2, data 1, type 0x0003, machine 0x00b7",
        foreign.display()
    );
    for target in ["hax", "hig"] {
        let install_root = harness
            .fixture
            .root
            .join(format!("install-platform-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("install-platform-bin-{target}"));
        harness.assert_package_refused_without_mutation(
            target,
            &forged,
            &harness.hax_install_root,
            &install_root,
            &bin_dir,
            &refusal,
            &[&install_root, &bin_dir, &harness.hax_install_root],
        );
    }
}

/// hax validates a package before it ships it, so the only way a foreign
/// binary reaches hig's release store is by arriving different from what was
/// validated: a transfer that garbled it, or a directory staged by hand. The
/// remote leg therefore judges the bytes it actually holds - the staged copy
/// is replaced after the transfer and before the remote script runs - and
/// refuses without creating a release store at all.
#[test]
fn hig_release_script_remote_install_refuses_a_staged_binary_that_is_not_linux_x86_64() {
    let harness = ReleaseGuardHarness::new("hig-release-remote-platform");
    let foreign = harness.fixture.root.join("foreign-kanban");
    write_release_image(&foreign, &mach_o_header());
    let install_root = harness.fixture.root.join("remote-platform-install");
    let bin_dir = harness.fixture.root.join("remote-platform-bin");
    let refused = harness
        .install_command(
            "hig",
            &harness.package_dir,
            &harness.hax_install_root,
            &install_root,
            &bin_dir,
        )
        .env("FAKE_SSH_SWAP_STAGED", &foreign)
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "the remote leg activated a staged binary that is not the release platform\nstdout: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    let refusal =
        format!("/package/kanban: {RELEASE_PLATFORM}, but this file is Mach-O (magic 0xcffaedfe)");
    assert!(
        stderr.contains(&refusal),
        "expected the staged copy to be named in a refusal ending {refusal:?}:\n{stderr}"
    );
    assert!(
        !install_root.exists(),
        "a refused remote install created {}",
        install_root.display()
    );
    assert!(
        !bin_dir.exists(),
        "a refused remote install created {}",
        bin_dir.display()
    );
}

#[test]
fn hig_release_script_rejects_hig_install_without_hax_install_root() {
    let fixture = Fixture::new("hig-release-hig-before-hax");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "HIG install without hax install root succeeded"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("--hax-install-root is required for hig installs"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn hig_release_script_rejects_the_build_provenance_receipt_for_hig_install() {
    let fixture = Fixture::new("hig-release-build-receipt");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let build_receipt = output_dir.with_extension("receipt.json");
    let fake_hax_install_root = fixture.root.join("fake-hax-install");
    let fake_release_dir = fake_hax_install_root
        .join("releases")
        .join(release_id_from_package(&output_dir));
    fs::create_dir_all(&fake_release_dir).unwrap();
    for entry in fs::read_dir(&output_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        fs::copy(&path, fake_release_dir.join(path.file_name().unwrap())).unwrap();
    }
    fs::copy(
        &build_receipt,
        fake_release_dir.with_extension("receipt.json"),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&fake_release_dir, fake_hax_install_root.join("current")).unwrap();
    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            fake_hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "build provenance receipt unexpectedly authorized HIG install"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("hax activation receipt is incomplete or mismatched")
            || String::from_utf8_lossy(&refused.stderr)
                .contains("hax activation receipt is missing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn hig_release_script_rejects_a_mismatched_hax_install_receipt_before_ssh() {
    let fixture = Fixture::new("hig-release-hax-receipt-mismatch");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef01234567",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let ssh_invocation_log = fixture.root.join("unexpected-ssh-invocation.log");

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();
    let hax_receipt_path = PathBuf::from(hax_install_json["receipt"].as_str().unwrap());
    let mut forged: Value = serde_json::from_slice(&fs::read(&hax_receipt_path).unwrap()).unwrap();
    forged["releaseId"] = json!("mismatched-release-id");
    fs::write(
        &hax_receipt_path,
        serde_json::to_vec_pretty(&forged).unwrap(),
    )
    .unwrap();

    let refused = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", "0123456789abcdef0123456789abcdef01234567")
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_SSH_INVOCATION_LOG", &ssh_invocation_log)
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            output_dir.to_str().unwrap(),
            "--hax-install-root",
            hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !refused.status.success(),
        "mismatched hax receipt unexpectedly authorized HIG install"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("hax activation receipt is incomplete or mismatched"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        !ssh_invocation_log.exists(),
        "HIG staging began before local HAX activation validation"
    );
}

#[test]
fn hig_release_script_prunes_to_ten_and_rolls_back_to_the_previous_release() {
    let fixture = Fixture::new("hig-release-rollback");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef00000000",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let release_second = "1700000000";
    let commit = |index: usize| format!("0123456789abcdef0123456789abcdef{:08x}", index);
    let hax_ctx = HaxInstallContext {
        fixture: &fixture,
        script: &script,
        path: &path,
        hostname_bin: &hostname_bin,
        fake_repo_root: &fake_repo_root,
        remote_root: &remote_root,
    };

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let mut release_dirs = Vec::new();
    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let _hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();

    for index in 0..5 {
        let package_dir = if index == 0 {
            output_dir.clone()
        } else {
            let cloned = fixture.root.join(format!("package-{index:02}"));
            clone_release_package(&output_dir, &cloned, &commit(index));
            cloned
        };
        let hax_install_root = if index == 0 {
            hax_install_root.clone()
        } else {
            install_matching_hax_package(
                &hax_ctx,
                &package_dir,
                &commit(index),
                &format!("rollback-hax-{index:02}"),
            )
        };
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .env("FAKE_RELEASE_DATE_SECONDS", release_second)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                package_dir.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let failed_package = fixture.root.join("package-failed");
    clone_release_package(&output_dir, &failed_package, &commit(99));
    let failed_hax_install_root = install_matching_hax_package(
        &hax_ctx,
        &failed_package,
        &commit(99),
        "rollback-hax-failed",
    );
    let stable_links = capture_release_links(&install_root, &bin_dir);
    let failed_release_dir = install_root.join(format!(
        "releases/{}",
        release_id_from_package(&failed_package)
    ));
    let failed_install = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(99))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "install",
            "hig",
            "--package",
            failed_package.to_str().unwrap(),
            "--hax-install-root",
            failed_hax_install_root.to_str().unwrap(),
            "--install-root",
            install_root.to_str().unwrap(),
            "--bin-dir",
            bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !failed_install.status.success(),
        "failed activation unexpectedly succeeded"
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "failed activation changed the public release view"
    );
    assert!(
        !failed_release_dir.exists(),
        "failed release directory was retained"
    );
    assert!(
        !failed_release_dir.with_extension("receipt.json").exists(),
        "failed release receipt was retained"
    );
    let release_receipts_after_failure = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        release_receipts_after_failure, 5,
        "failed release counted toward retention"
    );

    for index in 5..11 {
        let cloned = fixture.root.join(format!("package-{index:02}"));
        clone_release_package(&output_dir, &cloned, &commit(index));
        let hax_install_root = install_matching_hax_package(
            &hax_ctx,
            &cloned,
            &commit(index),
            &format!("rollback-hax-{index:02}"),
        );
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .env("FAKE_RELEASE_DATE_SECONDS", release_second)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                cloned.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let release_receipts = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(
        release_receipts, 10,
        "retention did not stop at ten releases"
    );

    let rollback = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(10))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("FAKE_RELEASE_DATE_SECONDS", release_second)
        .arg(&script)
        .args([
            "rollback",
            "hig",
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
        rollback.status.success(),
        "rollback failed: {}\nstderr: {}",
        String::from_utf8_lossy(&rollback.stdout),
        String::from_utf8_lossy(&rollback.stderr)
    );
    let rollback_json: Value = serde_json::from_slice(&rollback.stdout).unwrap();
    assert_eq!(
        PathBuf::from(rollback_json["releaseDir"].as_str().unwrap()),
        release_dirs[9]
    );
    let current_link = install_root.join("current");
    assert_eq!(fs::read_link(&current_link).unwrap(), release_dirs[9]);
    assert_release_view(&install_root, &bin_dir, &release_dirs[9]);
}

#[test]
fn hig_release_script_restores_the_previous_view_when_rollback_fails_mid_cutover() {
    let fixture = Fixture::new("hig-release-rollback-fail");
    let fake_repo_root = fixture.root.join("fake-repo");
    fs::create_dir_all(&fake_repo_root).unwrap();
    let remote_root = fixture.root.join("remote-root");
    fs::create_dir_all(&remote_root).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh");
    let stubs = write_release_tool_stubs(
        &fixture,
        &fake_repo_root,
        "0123456789abcdef0123456789abcdef11111111",
        env!("CARGO_BIN_EXE_kanban"),
        "hax",
    );
    let hostname_bin = stubs.join("hostname");
    let output_dir = fixture.root.join("package");
    let hax_install_root = fixture.root.join("install-hax");
    let hax_bin_dir = fixture.root.join("bin-hax");
    let install_root = fixture.root.join("install");
    let bin_dir = fixture.root.join("bin");
    let path = format!("{}:{}", stubs.display(), env::var("PATH").unwrap());
    let commit = |index: usize| format!("0123456789abcdef0123456789abcdef{:08x}", index);
    let hax_ctx = HaxInstallContext {
        fixture: &fixture,
        script: &script,
        path: &path,
        hostname_bin: &hostname_bin,
        fake_repo_root: &fake_repo_root,
        remote_root: &remote_root,
    };

    let packaged = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env(
            "KANBAN_RELEASE_CONTAINER_RUNTIME",
            release_container_runtime(&hostname_bin),
        )
        .env("KANBAN_RELEASE_BUILDER_IMAGE", RELEASE_BUILDER_IMAGE)
        .arg(&script)
        .args(["package", "hax", "--output", output_dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let mut release_dirs = Vec::new();
    let hax_installed = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(0))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .arg(&script)
        .args([
            "install",
            "hax",
            "--package",
            output_dir.to_str().unwrap(),
            "--install-root",
            hax_install_root.to_str().unwrap(),
            "--bin-dir",
            hax_bin_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        hax_installed.status.success(),
        "{}",
        String::from_utf8_lossy(&hax_installed.stderr)
    );
    let _hax_install_json: Value = serde_json::from_slice(&hax_installed.stdout).unwrap();

    for index in 1..3 {
        let package_dir = if index == 0 {
            output_dir.clone()
        } else {
            let cloned = fixture.root.join(format!("rollback-package-{index:02}"));
            clone_release_package(&output_dir, &cloned, &commit(index));
            cloned
        };
        let hax_install_root = if index == 0 {
            hax_install_root.clone()
        } else {
            install_matching_hax_package(
                &hax_ctx,
                &package_dir,
                &commit(index),
                &format!("rollback-fail-hax-{index:02}"),
            )
        };
        let installed = Command::new("bash")
            .current_dir(&fixture.main)
            .env("PATH", &path)
            .env("HOSTNAME_BIN", &hostname_bin)
            .env(
                "HIG_RELEASE_TARGET_RUNNER",
                release_target_runner(&hostname_bin),
            )
            .env("FAKE_HOST", "hax")
            .env("FAKE_REPO_ROOT", &fake_repo_root)
            .env("FAKE_GIT_HEAD", commit(index))
            .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
            .env("FAKE_REMOTE_ROOT", &remote_root)
            .arg(&script)
            .args([
                "install",
                "hig",
                "--package",
                package_dir.to_str().unwrap(),
                "--hax-install-root",
                hax_install_root.to_str().unwrap(),
                "--install-root",
                install_root.to_str().unwrap(),
                "--bin-dir",
                bin_dir.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            installed.status.success(),
            "install {index} failed: {}\nstderr: {}",
            String::from_utf8_lossy(&installed.stdout),
            String::from_utf8_lossy(&installed.stderr)
        );
        let installed_json: Value = serde_json::from_slice(&installed.stdout).unwrap();
        release_dirs.push(PathBuf::from(
            installed_json["releaseDir"].as_str().unwrap(),
        ));
    }

    let stable_links = capture_release_links(&install_root, &bin_dir);
    let failed_rollback = Command::new("bash")
        .current_dir(&fixture.main)
        .env("PATH", &path)
        .env("HOSTNAME_BIN", &hostname_bin)
        .env(
            "HIG_RELEASE_TARGET_RUNNER",
            release_target_runner(&hostname_bin),
        )
        .env("FAKE_HOST", "hax")
        .env("FAKE_REPO_ROOT", &fake_repo_root)
        .env("FAKE_GIT_HEAD", commit(1))
        .env("FAKE_RELEASE_BINARY", env!("CARGO_BIN_EXE_kanban"))
        .env("FAKE_REMOTE_ROOT", &remote_root)
        .env("HIG_RELEASE_FAIL_AFTER_CURRENT", "1")
        .arg(&script)
        .args([
            "rollback",
            "hig",
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
        !failed_rollback.status.success(),
        "injected rollback failure unexpectedly succeeded\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed_rollback.stdout),
        String::from_utf8_lossy(&failed_rollback.stderr)
    );
    assert_eq!(
        capture_release_links(&install_root, &bin_dir),
        stable_links,
        "rollback failure changed the public release view"
    );
    assert_eq!(
        fs::read_link(install_root.join("current")).unwrap(),
        release_dirs[1],
        "rollback failure changed current\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&failed_rollback.stdout),
        String::from_utf8_lossy(&failed_rollback.stderr)
    );
    let release_receipts = fs::read_dir(install_root.join("releases"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".receipt.json"))
        })
        .count();
    assert_eq!(release_receipts, 2, "rollback failure altered retention");
}

#[test]
fn hig_release_script_refuses_a_planted_releases_symlink_before_writing_outside_the_tree() {
    let harness = ReleaseGuardHarness::new("hig-release-releases-symlink");
    for target in ["hax", "hig"] {
        let install_root = harness.fixture.root.join(format!("planted-{target}"));
        let bin_dir = harness.fixture.root.join(format!("planted-bin-{target}"));
        let outside = harness.fixture.root.join(format!("outside-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("operator.txt"), b"do not touch\n").unwrap();
        symlink(&outside, install_root.join("releases")).unwrap();

        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to install through a symlink at {}/releases; remove it so releases/ is a real directory inside {}",
                install_root.display(),
                install_root.display()
            ),
            &[&outside, &install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(outside.join("operator.txt")).unwrap(),
            b"do not touch\n",
            "{target}: the directory behind the planted symlink was written"
        );
        assert!(
            fs::symlink_metadata(&bin_dir).is_err(),
            "{target}: refused install created the bin dir"
        );
    }
}

#[test]
fn hig_release_script_refuses_to_replace_operator_files_and_foreign_links_at_current_and_bin_destinations()
 {
    let harness = ReleaseGuardHarness::new("hig-release-operator-files");
    let outside = harness.fixture.root.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("kanban"), b"#!/bin/sh\necho operator kanban\n").unwrap();

    for target in ["hax", "hig"] {
        // A regular file where the managed `current` symlink belongs.
        let install_root = harness.fixture.root.join(format!("current-file-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("current-file-bin-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        fs::write(install_root.join("current"), b"operator notes\n").unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/current: it is not a symlink into {}/releases managed by this installer; move it aside before installing",
                install_root.display(),
                install_root.display()
            ),
            &[&install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(install_root.join("current")).unwrap(),
            b"operator notes\n",
            "{target}: the operator's current file was clobbered"
        );
        assert!(
            fs::symlink_metadata(install_root.join("releases")).is_err(),
            "{target}: refused install created releases/"
        );

        // A symlink at `current` that the installer did not write.
        let install_root = harness
            .fixture
            .root
            .join(format!("current-foreign-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("current-foreign-bin-{target}"));
        fs::create_dir_all(&install_root).unwrap();
        symlink(&outside, install_root.join("current")).unwrap();
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
        assert_eq!(
            fs::read_link(install_root.join("current")).unwrap(),
            outside,
            "{target}: the operator's current link was repointed"
        );

        // A regular file at a public binary destination.
        let install_root = harness.fixture.root.join(format!("bin-file-{target}"));
        let bin_dir = harness.fixture.root.join(format!("bin-file-bin-{target}"));
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("kb"), b"#!/bin/sh\necho operator kb\n").unwrap();
        harness.assert_refused_without_mutation(
            target,
            &install_root,
            &bin_dir,
            &format!(
                "refusing to replace {}/kb: it is not a symlink into {}/current managed by this installer; move it aside before installing",
                bin_dir.display(),
                install_root.display()
            ),
            &[&install_root, &bin_dir],
        );
        assert_eq!(
            fs::read(bin_dir.join("kb")).unwrap(),
            b"#!/bin/sh\necho operator kb\n",
            "{target}: the operator's kb file was clobbered"
        );
        assert!(
            fs::symlink_metadata(&install_root).is_err(),
            "{target}: refused install created the install root"
        );

        // A symlink at a public binary destination that points somewhere else.
        let install_root = harness.fixture.root.join(format!("bin-foreign-{target}"));
        let bin_dir = harness
            .fixture
            .root
            .join(format!("bin-foreign-bin-{target}"));
        fs::create_dir_all(&bin_dir).unwrap();
        symlink(outside.join("kanban"), bin_dir.join("kanban")).unwrap();
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
        assert_eq!(
            fs::read_link(bin_dir.join("kanban")).unwrap(),
            outside.join("kanban"),
            "{target}: the operator's kanban link was repointed"
        );
    }
}

#[test]
fn hig_release_script_local_and_remote_install_guards_are_identical() {
    let script =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hig-release.sh"))
            .unwrap();
    let remote_start = script.find("<<'REMOTE'\n").unwrap();
    let remote_end = script[remote_start..].find("\nREMOTE\n").unwrap() + remote_start;
    for name in [
        "physical_dir",
        "ensure_managed_activation_receipt",
        "ensure_safe_release_view",
        "reject_carried_release_identity",
        "require_release_platform",
        // The version probe and the release-set membership check it calls:
        // both legs record files[].version, so a remote copy that drifted -
        // or that quietly lost the runner branch or the empty-version
        // refusal - would install versions the package never claimed.
        "file_version",
        "release_binary_known",
        // The probe both of those read, and the bundle fingerprint the
        // INSTALLED executable is asked for: a remote copy that drifted here
        // would prove a different thing about the release than the local leg
        // proved about the package (SPA-03).
        "file_probe",
        "file_bundle_sha256",
        "prove_installed_bundle",
        // The branch the version probe took, which the package receipt
        // records as versionProbe: a remote copy reading a different variable
        // would take a different branch from the one the receipt names.
        "version_probe_kind",
        // The provenance a formatVersion 2 receipt must carry, as one jq
        // definition: the local receipt check, the hax activation check and
        // the remote installer all prepend it, so a remote copy that dropped
        // a cross-field invariant would install a receipt the local leg
        // refuses.
        "receipt_provenance_defs",
    ] {
        let header = format!("\n{name}() {{\n");
        let definitions: Vec<(usize, &str)> = script
            .match_indices(&header)
            .map(|(at, _)| {
                let start = at + 1;
                let end = script[start..].find("\n}\n").unwrap() + start + 3;
                (start, &script[start..end])
            })
            .collect();
        assert_eq!(
            definitions.len(),
            2,
            "{name} must be defined exactly twice: locally and inside the embedded remote script"
        );
        assert!(
            definitions[0].0 < remote_start
                && (remote_start..remote_end).contains(&definitions[1].0),
            "{name}: expected one local definition and one inside the REMOTE heredoc"
        );
        assert_eq!(
            definitions[0].1, definitions[1].1,
            "{name} drifted between the local and embedded remote install paths"
        );
    }
}

fn elapsed_from_file_marker(marker: &Path, end: SystemTime) -> Result<Duration, String> {
    let start = fs::metadata(marker)
        .and_then(|metadata| metadata.modified())
        .map_err(|error| format!("cannot read {} timestamp: {error}", marker.display()))?;
    end.duration_since(start).map_err(|error| {
        format!(
            "clock moved backwards between {} and process exit: {error}",
            marker.display()
        )
    })
}

fn spawn_bounded_stalled_accept_worker(
    listener: std::net::TcpListener,
    start_rx: mpsc::Receiver<()>,
    ready_tx: mpsc::SyncSender<()>,
    stop_rx: mpsc::Receiver<()>,
) -> std::thread::JoinHandle<Option<Instant>> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        ready_tx.send(()).unwrap();
        if start_rx.recv().is_err() {
            return None;
        }
        let mut connections = Vec::new();
        let mut reached = None;
        loop {
            loop {
                match listener.accept() {
                    Ok((connection, _)) => {
                        reached.get_or_insert_with(Instant::now);
                        connections.push(connection);
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(error) => panic!("stalled listener accept failed: {error}"),
                }
            }
            match stop_rx.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    // A real connection can become ready between the regular
                    // drain and observing stop. Drain once more before exit;
                    // unlike a synthetic wake-up, every socket here was
                    // queued by the installer while the listener was alive.
                    loop {
                        match listener.accept() {
                            Ok((connection, _)) => {
                                reached.get_or_insert_with(Instant::now);
                                connections.push(connection);
                            }
                            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                            Err(error) => panic!("stalled listener final accept failed: {error}"),
                        }
                    }
                    return reached;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    })
}

#[test]
fn stalled_accept_worker_keeps_a_real_connection_queued_before_stop() {
    let fixture = Fixture::new("stalled-accept-late-observer");
    let marker = fixture.root.join("curl-started");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let queued_probe = listener.try_clone().unwrap();
    let address = listener.local_addr().unwrap();
    let (start_tx, start_rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let (stop_tx, stop_rx) = mpsc::channel();
    let worker = spawn_bounded_stalled_accept_worker(listener, start_rx, ready_tx, stop_rx);
    ready_rx.recv().unwrap();
    fs::write(&marker, []).unwrap();
    let connection = std::net::TcpStream::connect(address).unwrap();
    // Client connect completion is not proof that the listener is ready to accept.
    let mut queued_events = libc::pollfd {
        fd: queued_probe.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let poll_result = unsafe { libc::poll(&mut queued_events, 1, 2_000) };
    assert!(
        poll_result >= 0,
        "listener readiness poll failed: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        poll_result, 1,
        "no connection queued on the listener within two seconds"
    );
    assert_ne!(
        queued_events.revents & libc::POLLIN,
        0,
        "listener woke without a queued connection: {:#x}",
        queued_events.revents
    );
    let simulated_child_exit = SystemTime::now();
    // Model observation delayed until after the real probe was queued and the
    // child exited. Connection proof survives pending stop; elapsed time comes
    // from curl's pre-launch marker rather than the late userspace dequeue.
    let _ = stop_tx.send(());
    start_tx.send(()).unwrap();
    worker
        .join()
        .unwrap()
        .expect("queued real connection was discarded when stop was pending");
    let measured = elapsed_from_file_marker(&marker, simulated_child_exit).unwrap();
    assert!(
        measured <= Duration::from_secs(1),
        "marker-to-exit measurement included delayed observation: {measured:?}"
    );
    drop(connection);
}
