//! Turn the committed `web/dist` into an embedded asset table, and stamp the
//! bundle's fingerprint into the binary.
//!
//! This script does not build the bundle. It cannot: the release is compiled
//! in a pinned Rust container with no Node in it (ADR-044, ADR-048 §1), so
//! the cargo build has to be offline and toolchain-free. The bundle is built
//! on a developer machine by `web/build.mjs`, committed under `web/dist`, and
//! proved reproducible by `web/check-reproducible.sh` on the gate host.
//!
//! What this script does is three things a hand-written `include_bytes!`
//! could not:
//!
//! 1. It reads the file names, which carry a content hash nobody can type
//!    ahead of time, and writes the `include_bytes!` for each of them.
//! 2. It CHECKS that hash against the file's own bytes, so a `web/dist` that
//!    was edited by hand — the one way a committed artefact can lie — fails
//!    `cargo build` instead of being served.
//! 3. It computes the bundle fingerprint the release receipt records and the
//!    binary prints (`kanban --version`, second line): the sha256 of the
//!    canonical `"<sha256>  <name>"` listing, sorted by name. One number for
//!    the whole bundle, derived from the bytes and from nothing else.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let dist = manifest_dir.join("web/dist");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=web/dist");

    let mut assets = read_assets(&dist);
    assets.sort_by(|left, right| left.name.cmp(&right.name));
    assert!(
        !assets.is_empty(),
        "web/dist is empty: build the bundle with `cd web && bun run build` and commit it"
    );

    let script = exactly_one(&assets, "js");
    let stylesheet = exactly_one(&assets, "css");
    let fingerprint = bundle_sha256(&assets);

    let mut generated = String::with_capacity(2048);
    generated.push_str(
        "/// One embedded file of the bundle, with the `Content-Type` its extension earns.\n\
         pub(crate) struct Asset {\n\
         pub(crate) name: &'static str,\n\
         pub(crate) content_type: &'static str,\n\
         pub(crate) bytes: &'static [u8],\n\
         }\n\n\
         /// Every file of the bundle, sorted by name.\n\
         pub(crate) const ASSETS: &[Asset] = &[\n",
    );
    for asset in &assets {
        writeln!(
            generated,
            "    Asset {{ name: {name:?}, content_type: {content_type:?}, bytes: include_bytes!({path:?}) }},",
            name = asset.name,
            content_type = asset.content_type,
            path = asset.path.to_str().expect("web/dist path is UTF-8"),
        )
        .expect("writing to a String cannot fail");
    }
    generated.push_str("];\n\n");
    writeln!(
        generated,
        "/// The bundle's script, as the shell links it.\n\
         pub(crate) const SCRIPT: &str = {script:?};\n\n\
         /// The bundle's stylesheet, as the shell links it.\n\
         pub(crate) const STYLESHEET: &str = {stylesheet:?};\n\n\
         /// The fingerprint of the whole bundle: sha256 of the canonical\n\
         /// `\"<sha256>  <name>\"` listing, sorted by name.\n\
         pub(crate) const SHA256: &str = {fingerprint:?};"
    )
    .expect("writing to a String cannot fail");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"))
        .join("bundle_assets.rs");
    std::fs::write(&out, generated).unwrap_or_else(|error| {
        panic!(
            "write the generated asset table to {}: {error}",
            out.display()
        )
    });
    // Available to the crate as `env!("KANBAN_BUNDLE_SHA256")` as well as
    // through the generated module, because the version banner is built in
    // `rust/lib.rs` and a second path to one number is cheaper than a
    // dependency between two modules for it.
    println!("cargo:rustc-env=KANBAN_BUNDLE_SHA256={fingerprint}");
}

struct BundleAsset {
    name: String,
    content_type: &'static str,
    path: PathBuf,
    sha256: String,
}

fn read_assets(dist: &Path) -> Vec<BundleAsset> {
    let entries = std::fs::read_dir(dist).unwrap_or_else(|error| {
        panic!(
            "read the committed bundle at {}: {error}: build it with `cd web && bun run build`",
            dist.display()
        )
    });
    let mut assets = Vec::new();
    for entry in entries {
        let entry = entry.expect("read one web/dist entry");
        let path = entry.path();
        if !entry.file_type().expect("stat a web/dist entry").is_file() {
            panic!("{} is not a regular file", path.display());
        }
        let name = entry
            .file_name()
            .into_string()
            .unwrap_or_else(|name| panic!("web/dist carries a non-UTF-8 name: {name:?}"));
        let bytes =
            std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        let sha256 = hex(&Sha256::digest(&bytes));
        // The name says what the bytes hash to; if it does not, the
        // committed artefact and its name disagree and one of them is a lie.
        let (stem, extension) = name
            .rsplit_once('.')
            .unwrap_or_else(|| panic!("{name} has no extension"));
        let declared = stem
            .rsplit_once('.')
            .unwrap_or_else(|| panic!("{name} carries no content hash"))
            .1;
        assert!(
            sha256.starts_with(declared) && !declared.is_empty(),
            "{name} claims content hash {declared}, but its bytes hash to {sha256}"
        );
        assets.push(BundleAsset {
            content_type: content_type(extension, &name),
            name,
            path,
            sha256,
        });
    }
    assets
}

/// The `Content-Type` an extension earns. Unknown extensions are refused
/// rather than served as bytes of unknown meaning: a new asset kind is a
/// decision, not a default.
fn content_type(extension: &str, name: &str) -> &'static str {
    match extension {
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "map" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "woff2" => "font/woff2",
        other => panic!(
            "{name}: nothing decides the Content-Type of a .{other}; add it to build.rs's \
             content_type"
        ),
    }
}

fn exactly_one(assets: &[BundleAsset], extension: &str) -> String {
    let suffix = format!(".{extension}");
    let mut matching = assets
        .iter()
        .filter(|asset| asset.name.ends_with(&suffix))
        .map(|asset| asset.name.clone());
    let first = matching.next().unwrap_or_else(|| {
        panic!("the bundle has no {suffix} file; rebuild it with `cd web && bun run build`")
    });
    assert!(
        matching.next().is_none(),
        "the bundle has more than one {suffix} file, so the shell cannot say which to link"
    );
    first
}

/// One number for the whole bundle: sha256 over `"<sha256>  <name>\n"` per
/// file, sorted by name. Stable under a rebuild that changes nothing, and
/// different the moment any byte of any file is.
fn bundle_sha256(assets: &[BundleAsset]) -> String {
    let mut listing = String::new();
    for asset in assets {
        writeln!(listing, "{}  {}", asset.sha256, asset.name)
            .expect("writing to a String cannot fail");
    }
    hex(&Sha256::digest(listing.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
    }
    out
}
