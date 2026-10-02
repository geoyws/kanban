#!/usr/bin/env bash
# Test for the remote install targets of scripts/hig-release.sh (hig, hal).
#
#   ./scripts/hig-release.test.sh
#
# No cargo build and no host is touched: fake `ssh`, `tar` and `hostname` sit
# first on PATH, every install root and bin dir lives under one temp dir, and
# the "remote" leg runs locally under the host name ssh was asked for. The
# packages are hand-made release packages - ten linux x86-64 ELF headers over
# a one-line payload - and a fake target runner answers each binary's version
# probe from its payload, so the version follows the bytes.
#
# For each remote target it proves: install refuses off hax and opens no ssh;
# install copies the package over that target's ssh name and writes a receipt
# whose canonical fields equal hax's, with bytes identical to the hax release;
# a package that differs from the hax canonical release, a garbled transfer,
# and an installed release dir that differs from the hax receipt all fail;
# rollback flips current back; a foreign bin symlink (hal's kb -> kb-remote.sh)
# is refused without --replace-wrapper, replaced with it, restored when the
# activation fails, and a regular file is never replaced.
#
# HIG_RELEASE_SCRIPT runs the cases against another copy of the script, and
# HIG_RELEASE_TEST_BASELINE=1 runs only the hig cases that predate hal, with
# two-target packages - which is how this file proves the hig path unchanged
# against the script as it was before hal.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${HIG_RELEASE_SCRIPT:-$here/scripts/hig-release.sh}"
BASELINE="${HIG_RELEASE_TEST_BASELINE:-0}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STUBS="$WORK/stubs"
mkdir -p "$STUBS" "$WORK/home" "$WORK/remote-tmp"

pass=0
fail=0
assert() {
  local label="$1"; shift
  if "$@" >/dev/null 2>&1; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    printf 'FAIL: %s\n' "$label" >&2
  fi
}
assert_eq() {
  local label="$1" want="$2" got="$3"
  if [[ "$want" == "$got" ]]; then
    pass=$((pass + 1))
  else
    fail=$((fail + 1))
    printf 'FAIL: %s\n  want: %s\n  got:  %s\n' "$label" "$want" "$got" >&2
  fi
}

# The release set, read from the script under test so a package here always
# carries exactly what that script expects.
BINARIES=()
while IFS= read -r name; do
  BINARIES+=("$name")
done < <(awk '/^BINARIES=\(/{on=1; next} on && /^\)/{exit} on {gsub(/[[:space:]]/, ""); if (length) print}' "$SCRIPT")
(( ${#BINARIES[@]} > 0 )) || { printf 'no BINARIES in %s\n' "$SCRIPT" >&2; exit 1; }

cat >"$STUBS/hostname" <<'EOF'
#!/bin/sh
printf '%s\n' "${FAKE_HOST:?}"
EOF
# The "remote" host is whoever ssh was asked for. Every call is logged so a
# case can prove which ssh name a target used and that a refusal opened none.
# FAKE_SSH_SWAP_STAGED replaces one staged binary after the transfer, which is
# a transfer that garbled it.
cat >"$STUBS/ssh" <<'EOF'
#!/bin/sh
set -eu
host="$1"
shift
printf '%s %s\n' "$host" "$1" >> "${FAKE_SSH_LOG:?}"
if [ "${1:-}" = bash ]; then
  shift 3
  if [ -n "${FAKE_SSH_SWAP_STAGED:-}" ]; then
    cp "$FAKE_SSH_SWAP_STAGED" "$2/kanban"
  fi
  FAKE_HOST="$host" TMPDIR="${FAKE_REMOTE_TMP:?}" exec bash -s -- "$@"
fi
FAKE_HOST="$host" TMPDIR="${FAKE_REMOTE_TMP:?}" exec bash -c "$*"
EOF
cat >"$STUBS/tar" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "${FAKE_TAR_LOG:?}"
exec /usr/bin/tar "$@"
EOF
# How this host "runs" a linux x86-64 release binary: the first line of the
# payload behind the 20-byte header is its version.
cat >"$STUBS/runner" <<'EOF'
#!/bin/sh
[ -x "$1" ] || exit 126
tail -c +21 "$1" | head -n 1
EOF
chmod 0755 "$STUBS"/*

export PATH="$STUBS:$PATH"
export HOME="$WORK/home"
export HOSTNAME_BIN="$STUBS/hostname"
export HIG_RELEASE_TARGET_RUNNER="$STUBS/runner"
export FAKE_SSH_LOG="$WORK/ssh.log"
export FAKE_TAR_LOG="$WORK/tar.log"
export FAKE_REMOTE_TMP="$WORK/remote-tmp"
: >"$FAKE_SSH_LOG"
: >"$FAKE_TAR_LOG"

elf_binary() { # path, payload
  printf '\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\003\000\076\000' >"$1"
  printf '%s\n' "$2" >>"$1"
  chmod 0755 "$1"
}

# make_package DIR COMMIT TARGETS_JSON: a package dir plus DIR.receipt.json.
make_package() {
  local dir="$1" commit="$2" targets="$3" files='[]' name
  mkdir -p "$dir"
  for name in "${BINARIES[@]}"; do
    elf_binary "$dir/$name" "$name ${commit:0:7}"
    files="$(jq -c --arg name "$name" \
      --arg sha "$(sha256sum "$dir/$name" | awk '{print $1}')" \
      --argjson bytes "$(wc -c <"$dir/$name" | tr -d '[:space:]')" \
      --arg version "$name ${commit:0:7}" \
      '. + [{name:$name, sha256:$sha, bytes:$bytes, version:$version}]' <<<"$files")"
  done
  jq -n -S --argjson targets "$targets" --arg commit "$commit" --argjson files "$files" \
    '{formatVersion:1, targets:$targets, sourceCommit:$commit, sourceTreeClean:true, files:$files}' >"$dir/manifest.json"
  jq -n -S --argjson targets "$targets" --arg commit "$commit" --argjson files "$files" \
    --arg manifest_sha "$(sha256sum "$dir/manifest.json" | awk '{print $1}')" \
    '{formatVersion:2, host:"hax", buildPlatform:"linux-x86_64", artifactPlatform:"linux-x86_64",
      buildKind:"native", builderImage:null, toolchain:{rustc:"rustc 1.90.0", cargo:"cargo 1.90.0"},
      versionProbe:"runner", targets:$targets, manifestSha256:$manifest_sha, sourceCommit:$commit,
      sourceTreeClean:true, files:$files}' >"$dir.receipt.json"
}

release_id_of() { # package dir
  printf '%s-%s' "$(jq -r .sourceCommit "$1/manifest.json")" "$(sha256sum "$1/manifest.json" | awk '{print $1}')"
}

# run HOST OUT_PREFIX ARGS...: the script as HOST; exit status into $status.
run() {
  local host="$1" out="$2"
  shift 2
  set +e
  FAKE_HOST="$host" bash "$SCRIPT" "$@" >"$out.out" 2>"$out.err"
  status=$?
  set -e
}

HAX_ROOT="$WORK/hax/root"
HAX_BIN="$WORK/hax/bin"
install_on_hax() { # package
  run hax "$WORK/hax-install" install hax --package "$1" --install-root "$HAX_ROOT" --bin-dir "$HAX_BIN"
  if (( status != 0 )); then
    cat "$WORK/hax-install.err" >&2
    printf 'hax install of %s failed\n' "$1" >&2
    exit 1
  fi
}

remote_install() { # target out root bin package extra...
  local target="$1" out="$2" root="$3" bin="$4" package="$5"
  shift 5
  run hax "$out" install "$target" --package "$package" --install-root "$root" --bin-dir "$bin" \
    --hax-install-root "$HAX_ROOT" "$@"
}

if [[ "$BASELINE" == 1 ]]; then
  ALL_TARGETS='["hax","hig"]'
else
  ALL_TARGETS='["hax","hig","hal"]'
fi
COMMIT_A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
COMMIT_B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
COMMIT_C=cccccccccccccccccccccccccccccccccccccccc
PKG_A="$WORK/pkg/a"
PKG_B="$WORK/pkg/b"
make_package "$PKG_A" "$COMMIT_A" "$ALL_TARGETS"
make_package "$PKG_B" "$COMMIT_B" "$ALL_TARGETS"
ID_A="$(release_id_of "$PKG_A")"
ID_B="$(release_id_of "$PKG_B")"

# Cases every remote target has had since hig (and the baseline run proves
# against the script as it was before hal).
common_cases() {
  local t="$1" pkg_a="$2" pkg_b="$3" id_a="$4" id_b="$5" tag="$6"
  local base="$WORK/$t-$tag"
  local root="$base/root" bin="$base/bin"
  mkdir -p "$base"
  # hax activates a release before any remote target receives it.
  install_on_hax "$pkg_a"

  # Off hax: refused before anything is copied, and no ssh is opened.
  : >"$FAKE_SSH_LOG"
  run "$t" "$base/offhax" install "$t" --package "$pkg_a" --install-root "$root" \
    --bin-dir "$bin" --hax-install-root "$HAX_ROOT"
  assert_eq "$t/$tag: install off hax fails" 1 "$status"
  assert "$t/$tag: off-hax refusal names hax" grep -q "target hax requires host hax, but this shell is $t" "$base/offhax.err"
  assert_eq "$t/$tag: off-hax install opened no ssh" "" "$(cat "$FAKE_SSH_LOG")"
  assert "$t/$tag: off-hax install wrote nothing" test ! -e "$root"

  # Install: the package goes over the target's own ssh name, tar over ssh.
  : >"$FAKE_SSH_LOG"
  : >"$FAKE_TAR_LOG"
  remote_install "$t" "$base/install" "$root" "$bin" "$pkg_a"
  assert_eq "$t/$tag: install exits 0" 0 "$status"
  [[ "$status" == 0 ]] || cat "$base/install.err" >&2
  assert_eq "$t/$tag: every ssh call went to $t" "$t" "$(awk '{print $1}' "$FAKE_SSH_LOG" | sort -u)"
  assert "$t/$tag: the package crossed as a tar stream" grep -q -- "-cf - ." "$FAKE_TAR_LOG"
  assert_eq "$t/$tag: output names the target" "$t" "$(jq -r .target "$base/install.out")"
  local receipt="$root/releases/$id_a.receipt.json"
  local hax_receipt="$HAX_ROOT/releases/$id_a.receipt.json"
  assert "$t/$tag: remote receipt written" test -f "$receipt"
  local field
  for field in formatVersion host targets manifestSha256 sourceCommit sourceTreeClean files; do
    assert_eq "$t/$tag: receipt $field equals hax's" "$(jq -cS ".$field" "$hax_receipt")" "$(jq -cS ".$field" "$receipt")"
  done
  assert_eq "$t/$tag: receipt target" "$t" "$(jq -r .target "$receipt")"
  assert_eq "$t/$tag: receipt installerHost" "$t" "$(jq -r .installerHost "$receipt")"
  assert_eq "$t/$tag: current points at the release" "$root/releases/$id_a" "$(readlink "$root/current")"
  local name
  for name in "${BINARIES[@]}" manifest.json; do
    assert "$t/$tag: $name byte-identical to hax" cmp -s "$HAX_ROOT/releases/$id_a/$name" "$root/releases/$id_a/$name"
  done
  assert_eq "$t/$tag: kb link points into current" "$root/current/kb" "$(readlink "$bin/kb")"

  # Re-install of the same release is idempotent.
  remote_install "$t" "$base/again" "$root" "$bin" "$pkg_a"
  assert_eq "$t/$tag: re-install exits 0" 0 "$status"

  # A package that is not what hax activated is refused before any copy.
  local bad="$base/bad-pkg"
  cp -R "$pkg_a" "$bad"
  cp "$pkg_a.receipt.json" "$bad.receipt.json"
  cp "$HAX_ROOT/releases/$id_a/kb" "$base/kb.saved"
  chmod u+w "$HAX_ROOT/releases/$id_a/kb"
  printf 'x' >>"$HAX_ROOT/releases/$id_a/kb"
  : >"$FAKE_SSH_LOG"
  remote_install "$t" "$base/hax-mismatch" "$base/root-mismatch" "$base/bin-mismatch" "$bad"
  assert_eq "$t/$tag: hax canonical mismatch fails" 1 "$status"
  assert "$t/$tag: hax canonical mismatch named" grep -q "hax release binary kb has the wrong size" "$base/hax-mismatch.err"
  assert_eq "$t/$tag: hax canonical mismatch opened no ssh" "" "$(cat "$FAKE_SSH_LOG")"
  cp "$base/kb.saved" "$HAX_ROOT/releases/$id_a/kb"

  # A transfer that garbled a binary is refused by the remote leg, which
  # leaves no activation behind.
  elf_binary "$base/garbled" "garbled"
  FAKE_SSH_SWAP_STAGED="$base/garbled" remote_install "$t" "$base/garbled-install" "$base/root-garbled" "$base/bin-garbled" "$pkg_a"
  assert_eq "$t/$tag: garbled transfer fails" 1 "$status"
  assert "$t/$tag: garbled transfer named" grep -q "remote binary kanban" "$base/garbled-install.err"
  assert "$t/$tag: garbled transfer activated nothing" test ! -e "$base/root-garbled/current"

  # Rollback, run on the target itself exactly like hig's.
  install_on_hax "$pkg_b"
  remote_install "$t" "$base/install-b" "$root" "$bin" "$pkg_b"
  assert_eq "$t/$tag: second release installs" 0 "$status"
  assert_eq "$t/$tag: current is the second release" "$root/releases/$id_b" "$(readlink "$root/current")"
  run "$t" "$base/rollback" rollback "$t" --install-root "$root" --bin-dir "$bin"
  assert_eq "$t/$tag: rollback exits 0" 0 "$status"
  [[ "$status" == 0 ]] || cat "$base/rollback.err" >&2
  assert_eq "$t/$tag: rollback restores the first release" "$root/releases/$id_a" "$(readlink "$root/current")"
  assert_eq "$t/$tag: rollback output names the release" "$id_a" "$(jq -r .releaseId "$base/rollback.out")"

  install_on_hax "$pkg_a"
  # A foreign bin symlink is the operator's and is refused, untouched.
  local wroot="$base/wrapper-root" wbin="$base/wrapper-bin"
  mkdir -p "$wbin"
  ln -s /root/work/journals/.sb/_dotfiles/bin/kb-remote.sh "$wbin/kb"
  remote_install "$t" "$base/wrapper-refused" "$wroot" "$wbin" "$pkg_a"
  assert_eq "$t/$tag: foreign kb symlink refused" 1 "$status"
  assert "$t/$tag: refusal names the kb link" grep -q "refusing to replace $wbin/kb: it is not a symlink into $wroot/current managed by this installer" "$base/wrapper-refused.err"
  assert_eq "$t/$tag: refused kb link untouched" /root/work/journals/.sb/_dotfiles/bin/kb-remote.sh "$(readlink "$wbin/kb")"
  assert "$t/$tag: refusal activated nothing" test ! -e "$wroot/current"
}

# Cases that arrived with hal and apply to every remote target alike.
new_cases() {
  local t="$1"
  local base="$WORK/$t-new"
  local root="$base/root" bin="$base/bin"
  mkdir -p "$base"

  install_on_hax "$PKG_A"
  # Byte identity is measured after activation and printed.
  remote_install "$t" "$base/install" "$root" "$bin" "$PKG_A"
  assert_eq "$t: install exits 0" 0 "$status"
  assert_eq "$t: byteIdentity verified" true "$(jq -r .byteIdentity.verified "$base/install.out")"
  assert_eq "$t: byteIdentity is against the hax receipt" "$HAX_ROOT/releases/$ID_A.receipt.json" "$(jq -r .byteIdentity.haxReceipt "$base/install.out")"
  assert_eq "$t: byteIdentity covers every binary and the manifest" "$(( ${#BINARIES[@]} + 1 ))" "$(jq '[.byteIdentity.files[] | select(.identical)] | length' "$base/install.out")"
  assert_eq "$t: byteIdentity kanban sha is hax's" "$(jq -r '.files[] | select(.name == "kanban") | .sha256' "$HAX_ROOT/releases/$ID_A.receipt.json")" \
    "$(jq -r '.byteIdentity.files[] | select(.name == "kanban") | .sha256' "$base/install.out")"

  # An installed release dir that no longer matches hax: a re-install does
  # not re-copy it, so only the post-activation measurement can catch it.
  chmod u+w "$root/releases/$ID_A/kanban-dispatcher"
  printf 'x' >>"$root/releases/$ID_A/kanban-dispatcher"
  remote_install "$t" "$base/tampered" "$root" "$bin" "$PKG_A"
  assert_eq "$t: byte-identity mismatch fails" 1 "$status"
  assert_eq "$t: mismatch reported unverified" false "$(jq -r .byteIdentity.verified "$base/tampered.out")"
  assert_eq "$t: mismatch names the file" kanban-dispatcher "$(jq -r '[.byteIdentity.files[] | select(.identical | not) | .name] | join(",")' "$base/tampered.out")"
  assert "$t: mismatch explained on stderr" grep -q "NOT byte-identical to the hax canonical receipt (kanban-dispatcher)" "$base/tampered.err"

  # --replace-wrapper replaces a foreign SYMLINK and only a symlink.
  local wrapper=/root/work/journals/.sb/_dotfiles/bin/kb-remote.sh
  local wroot="$base/wrapper-root" wbin="$base/wrapper-bin"
  mkdir -p "$wbin"
  ln -s "$wrapper" "$wbin/kb"
  HIG_RELEASE_FAIL_AFTER_CURRENT=1 remote_install "$t" "$base/wrapper-fail" "$wroot" "$wbin" "$PKG_A" --replace-wrapper
  assert_eq "$t: failed activation with --replace-wrapper fails" 1 "$status"
  assert_eq "$t: failed activation restores the wrapper" "$wrapper" "$(readlink "$wbin/kb")"
  assert "$t: failed activation leaves no current" test ! -e "$wroot/current"
  remote_install "$t" "$base/wrapper-replaced" "$wroot" "$wbin" "$PKG_A" --replace-wrapper
  assert_eq "$t: --replace-wrapper install exits 0" 0 "$status"
  [[ "$status" == 0 ]] || cat "$base/wrapper-replaced.err" >&2
  assert_eq "$t: kb now points into current" "$wroot/current/kb" "$(readlink "$wbin/kb")"
  assert "$t: the replacement is announced" grep -q -- "--replace-wrapper: replacing operator symlink $wbin/kb -> $wrapper" "$base/wrapper-replaced.err"
  assert_eq "$t: byteIdentity verified after cutover" true "$(jq -r .byteIdentity.verified "$base/wrapper-replaced.out")"

  local froot="$base/file-root" fbin="$base/file-bin"
  mkdir -p "$fbin"
  printf '#!/bin/sh\n' >"$fbin/kb"
  remote_install "$t" "$base/file-refused" "$froot" "$fbin" "$PKG_A" --replace-wrapper
  assert_eq "$t: a regular kb file is never replaced" 1 "$status"
  assert "$t: regular kb file untouched" test ! -L "$fbin/kb"
}

if [[ "$BASELINE" == 1 ]]; then
  common_cases hig "$PKG_A" "$PKG_B" "$ID_A" "$ID_B" baseline
else
  for target in hig hal; do
    common_cases "$target" "$PKG_A" "$PKG_B" "$ID_A" "$ID_B" current
    new_cases "$target"
  done

  # A two-target package - every release packaged before hal - still installs
  # on hig and is refused on hal, whose name it does not carry.
  PKG_LEGACY="$WORK/pkg/legacy"
  make_package "$PKG_LEGACY" "$COMMIT_C" '["hax","hig"]'
  install_on_hax "$PKG_LEGACY"
  common_cases hig "$PKG_LEGACY" "$PKG_B" "$(release_id_of "$PKG_LEGACY")" "$ID_B" legacy
  : >"$FAKE_SSH_LOG"
  remote_install hal "$WORK/legacy-hal" "$WORK/legacy-hal-root" "$WORK/legacy-hal-bin" "$PKG_LEGACY"
  assert_eq "hal: a package without hal in targets fails" 1 "$status"
  assert "hal: the refusal is the manifest check" grep -q "package manifest is incomplete or mismatched" "$WORK/legacy-hal.err"
  assert_eq "hal: the refusal opened no ssh" "" "$(cat "$FAKE_SSH_LOG")"

  # Packaging stays hax-only: neither remote target is a package target.
  for target in hig hal; do
    run hax "$WORK/package-$target" package "$target"
    assert_eq "package $target refused" 1 "$status"
    assert "package $target refusal" grep -q "package target must be hax" "$WORK/package-$target.err"
  done
fi

printf 'hig-release.test: %d passed, %d failed\n' "$pass" "$fail" >&2
((fail == 0))
