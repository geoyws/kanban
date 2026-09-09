#!/usr/bin/env bash

set -Eeuo pipefail

# Every executable the crate declares, in Cargo.toml order. This is the one
# literal list in the release path: every other site that needs the set
# (package contents, manifest and receipt name checks, the version probe, the
# bin-link loops, and the embedded remote installer) derives it from here.
BINARIES=(
  kanban
  kb
  kanban-dispatcher
  kanban-codex-queue-adapter
  kanban-codex-app-server-adapter
  kanban-claude-print-adapter
  kanban-opencode-adapter
  kanban-kimi-acp-adapter
  kanban-cursor-worker-adapter
  kanban-zcode-notify-adapter
)
MAX_RELEASES=10
HOSTNAME_BIN="${HOSTNAME_BIN:-/bin/hostname}"
BIN_DIR_DEFAULT="${BIN_DIR_DEFAULT:-${HOME:-/root}/.local/bin}"
TEMP_PATHS=()

die() {
  printf 'hig-release: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local status=$?
  set +u
  for path in "${TEMP_PATHS[@]}"; do
    [[ -n "$path" && -e "$path" ]] || continue
    rm -rf -- "$path"
  done
  exit "$status"
}

trap cleanup EXIT INT TERM HUP

track_temp() {
  TEMP_PATHS+=("$1")
}

usage() {
  cat >&2 <<'EOF'
usage:
  hig-release.sh package hax [--output DIR]
  hig-release.sh install <hax|hig> --package DIR --install-root DIR [--hax-install-root DIR]
  hig-release.sh rollback <hax|hig> --install-root DIR [--steps N]
EOF
  exit 64
}

repo_root() {
  git rev-parse --show-toplevel
}

host_short() {
  "$HOSTNAME_BIN" -s 2>/dev/null || "$HOSTNAME_BIN"
}

require_host() {
  local expected="$1"
  local actual
  actual="$(host_short)"
  [[ "$actual" == "$expected" ]] || die "target $expected requires host $expected, but this shell is $actual"
}

sha256_of() {
  sha256sum "$1" | awk '{print $1}'
}

# The release set as a JSON array, so the manifest and receipt name checks
# compare against BINARIES rather than a second literal list that can drift.
binaries_json() {
  printf '%s\n' "${BINARIES[@]}" | jq -R -s -c 'split("\n") | map(select(length > 0))'
}

# Membership in the release set, derived from BINARIES so a name the package
# does not ship can never be probed as though it belonged to the release.
release_binary_known() {
  local candidate="$1"
  local binary
  for binary in "${BINARIES[@]}"; do
    [[ "$binary" != "$candidate" ]] || return 0
  done
  return 1
}

file_version() {
  local binary="$1"
  local name="${binary##*/}"
  release_binary_known "$name" || die "unknown release binary $name"
  # `kanban` and `kb` are the operator CLI, whose version is a subcommand; the
  # dispatcher and every adapter answer the `--version` flag instead.
  case "$name" in
    kanban | kb)
      "$binary" version
      ;;
    *)
      "$binary" --version
      ;;
  esac | tr -d '\r' | sed 's/[[:space:]]*$//'
}

ensure_regular_dir() {
  local path="$1"
  [[ -d "$path" ]] || die "directory does not exist: $path"
  [[ ! -L "$path" ]] || die "path must not be a symlink: $path"
}

ensure_fresh_output() {
  local path="$1"
  [[ ! -e "$path" ]] || die "output already exists: $path"
  local parent
  parent="$(dirname "$path")"
  [[ -d "$parent" ]] || die "output parent does not exist: $parent"
  [[ ! -L "$parent" ]] || die "output parent must not be a symlink: $parent"
}

manifest_path() {
  printf '%s/manifest.json' "$1"
}

receipt_path() {
  printf '%s.receipt.json' "$1"
}

activation_sequence_path() {
  printf '%s/releases/.activation-sequence' "$1"
}

exact_package_entries() {
  printf '%s\n' manifest.json "${BINARIES[@]}" | sort
}

package_manifest_sha256() {
  sha256_of "$(manifest_path "$1")"
}

package_targets_json() {
  local target="$1"
  case "$target" in
    hax)
      printf '["hax","hig"]'
      ;;
    hig)
      die "package target must be hax"
      ;;
    *)
      die "target must be hax or hig, got $target"
      ;;
  esac
}

# The release store's identity is derived, never carried: `releaseId` is
# "$sourceCommit-$manifestSha256", and every activation field is written by the
# installer when it activates. A package artifact that carries either is
# refused here, before anything is written: otherwise the id a hand-edited
# manifest or receipt names and the id it actually installs as disagree
# silently, because the activation receipt is the package receipt plus the
# installer's own fields and the installer's copy wins the merge.
#
# Embedded verbatim in the remote install script (see install_remote).
reject_carried_release_identity() {
  local file="$1"
  local expected_release_id="${2:-}"
  jq -e --arg expected "$expected_release_id" '
    (["activationSequence", "installedAt", "releaseDir", "currentLink", "binDir", "installerHost", "target"]
      + (if $expected == "" then ["releaseId"] else [] end)) as $forbidden
    | . as $doc
    | (($forbidden | map(select(. as $key | $doc | has($key))) | length) == 0)
      and (($doc | .releaseId // $expected) == $expected)
  ' "$file" >/dev/null ||
    die "refusing $file: a release artifact must not carry the release identity the installer derives"
}

validate_release_files() {
  local dir="$1"
  local target="$2"
  local manifest
  manifest="$(manifest_path "$dir")"
  [[ -f "$manifest" ]] || die "package has no manifest.json"
  ensure_regular_dir "$dir"

  local expected actual
  expected="$(exact_package_entries)"
  actual="$(find "$dir" -mindepth 1 -maxdepth 1 -print | sed 's|.*/||' | sort)"
  [[ "$actual" == "$expected" ]] || {
    printf 'hig-release: unexpected package contents in %s\n' "$dir" >&2
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$actual" >&2
    exit 1
  }

  jq -e --arg target "$target" --argjson expected_files "$(binaries_json)" '
    (.formatVersion == 1) and
    (.targets | type == "array") and
    (.targets | length == 2) and
    (.targets | index("hax") != null) and
    (.targets | index("hig") != null) and
    (.targets | index($target) != null) and
    (.sourceTreeClean == true) and
    ((.sourceCommit | type) == "string") and
    ((.sourceCommit | length) == 40) and
    ((.files | length) == ($expected_files | length)) and
    ([.files[].name] == $expected_files)
  ' "$manifest" >/dev/null || die "package manifest is incomplete or mismatched"
  reject_carried_release_identity "$manifest"

  while IFS=$'\t' read -r name sha256 size version; do
    local path="$dir/$name"
    [[ -f "$path" ]] || die "package is missing binary $name"
    [[ ! -L "$path" ]] || die "package binary must not be a symlink: $name"
    [[ "$(wc -c <"$path" | tr -d '[:space:]')" == "$size" ]] || {
      die "package binary $name has the wrong size"
    }
    [[ "$(sha256_of "$path")" == "$sha256" ]] || {
      die "package binary $name hash mismatch"
    }
    [[ "$(file_version "$path")" == "$version" ]] || {
      die "package binary $name version mismatch"
    }
  done < <(jq -r '.files[] | [.name, .sha256, (.bytes | tostring), .version] | @tsv' "$manifest")
}

validate_package() {
  local package_dir="$1"
  local target="$2"
  validate_release_files "$package_dir" "$target"
}

validate_receipt() {
  local receipt="$1"
  local target="$2"
  local package_dir="$3"
  local manifest_sha
  local manifest_commit
  manifest_sha="$(package_manifest_sha256 "$package_dir")"
  manifest_commit="$(jq -r '.sourceCommit' "$(manifest_path "$package_dir")")"
  [[ -f "$receipt" ]] || die "package receipt is missing: $receipt"
  ensure_regular_dir "$(dirname "$receipt")"
  jq -e --arg target "$target" --arg manifest_sha "$manifest_sha" --argjson expected_files "$(binaries_json)" '
    (.formatVersion == 1) and
    (.host == "hax") and
    (.targets | type == "array") and
    (.targets | length == 2) and
    (.targets | index("hax") != null) and
    (.targets | index("hig") != null) and
    (.targets | index($target) != null) and
    (.sourceTreeClean == true) and
    ((.sourceCommit | type) == "string") and
    ((.sourceCommit | length) == 40) and
    (.manifestSha256 == $manifest_sha) and
    ((.files | length) == ($expected_files | length)) and
    ([.files[].name] == $expected_files)
  ' "$receipt" >/dev/null || die "package receipt is incomplete or mismatched"
  [[ "$(jq -r '.sourceCommit' "$receipt")" == "$manifest_commit" ]] || {
    die "package receipt source commit mismatch"
  }
  reject_carried_release_identity "$receipt" "${manifest_commit}-${manifest_sha}"
}

next_activation_sequence() {
  local install_root="$1"
  local sequence_path
  sequence_path="$(activation_sequence_path "$install_root")"
  mkdir -p "$(dirname "$sequence_path")"
  python3 - "$sequence_path" <<'PY'
import fcntl
import os
import sys

path = sys.argv[1]
with open(path, "a+", encoding="utf-8") as handle:
    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
    handle.seek(0)
    current = handle.read().strip()
    next_value = int(current) + 1 if current else 1
    handle.seek(0)
    handle.truncate()
    handle.write(f"{next_value}\n")
    handle.flush()
    os.fsync(handle.fileno())
    print(next_value)
PY
}

validate_hax_activation_receipt() {
  local hax_install_root="$1"
  local target="$2"
  local package_dir="$3"
  local manifest
  local manifest_sha
  local manifest_commit
  local release_id
  local receipt
  local release_dir
  local current_path
  manifest="$package_dir/manifest.json"
  manifest_sha="$(sha256sum "$manifest" | awk '{print $1}')"
  manifest_commit="$(jq -r '.sourceCommit' "$manifest")"
  release_id="${manifest_commit}-${manifest_sha}"
  receipt="$(release_receipt "$hax_install_root" "$release_id")"
  release_dir="$(release_dir "$hax_install_root" "$release_id")"
  current_path="$(current_link "$hax_install_root")"
  ensure_regular_dir "$hax_install_root"
  [[ -f "$receipt" ]] || die "hax activation receipt is missing: $receipt"
  [[ ! -L "$receipt" ]] || die "hax activation receipt must not be a symlink: $receipt"
  [[ -d "$release_dir" ]] || die "hax release directory is missing: $release_dir"
  [[ ! -L "$release_dir" ]] || die "hax release directory must not be a symlink: $release_dir"
  jq -e --arg target "$target" --arg manifest_sha "$manifest_sha" --arg manifest_commit "$manifest_commit" --arg release_id "$release_id" --arg release_dir "$release_dir" --arg current_path "$current_path" '
    (.formatVersion == 1) and
    (.host == "hax") and
    (.target == "hax") and
    (.installerHost == "hax") and
    (.targets | type == "array") and
    (.targets | length == 2) and
    (.targets | index("hax") != null) and
    (.targets | index("hig") != null) and
    (.targets | index($target) != null) and
    (.sourceTreeClean == true) and
    (.sourceCommit == $manifest_commit) and
    (.manifestSha256 == $manifest_sha) and
    (.releaseId == $release_id) and
    (.releaseDir == $release_dir) and
    (.currentLink == $current_path) and
    ((.releaseDir | type) == "string") and
    ((.currentLink | type) == "string") and
    ((.binDir | type) == "string") and
    ((.installedAt | type) == "number") and
    ((.installedAt > 0) == true)
  ' "$receipt" >/dev/null || die "hax activation receipt is incomplete or mismatched"
  [[ "$(readlink "$current_path")" == "$release_dir" ]] || die "hax current pointer does not match the installed release"
  [[ -f "$release_dir/manifest.json" ]] || die "hax release manifest is missing: $release_dir/manifest.json"
  while IFS=$'\t' read -r name sha256 size version; do
    local path="$release_dir/$name"
    [[ -f "$path" ]] || die "hax release directory is missing binary $name"
    [[ "$(wc -c <"$path" | tr -d '[:space:]')" == "$size" ]] || die "hax release binary $name has the wrong size"
    [[ "$(sha256sum "$path" | awk '{print $1}')" == "$sha256" ]] || die "hax release binary $name hash mismatch"
    [[ "$(file_version "$path")" == "$version" ]] || die "hax release binary $name version mismatch"
  done < <(jq -r '.files[] | [.name, .sha256, (.bytes | tostring), .version] | @tsv' "$package_dir/manifest.json")
}

release_id_from_receipt() {
  local receipt="$1"
  jq -r '.sourceCommit + "-" + .manifestSha256' "$receipt"
}

current_link() {
  printf '%s/current' "$1"
}

release_dir() {
  local root="$1"
  local id="$2"
  printf '%s/releases/%s' "$root" "$id"
}

release_receipt() {
  local root="$1"
  local id="$2"
  printf '%s/releases/%s.receipt.json' "$root" "$id"
}

bin_link_path() {
  printf '%s/%s' "$1" "$2"
}

# The five functions below are embedded verbatim in the remote install script
# (see install_remote); hig_release_script_local_and_remote_install_guards_are_identical
# in tests/e2e.rs fails when the two copies drift.
physical_dir() {
  (cd -P -- "$1" 2>/dev/null && pwd -P)
}

# A managed symlink is one this installer wrote: its target sits directly in
# "$install_root/$expected_parent" (current -> releases/<id>, bin/<name> ->
# current/<name>). Anything else belongs to the operator and is never replaced.
managed_symlink() {
  local link="$1"
  local install_root="$2"
  local expected_parent="$3"
  [[ -L "$link" ]] || return 1
  local target parent root_physical parent_physical
  target="$(readlink "$link")"
  [[ "$target" == /* ]] || target="$(dirname "$link")/$target"
  parent="$(dirname "$target")"
  [[ "${parent##*/}" == "$expected_parent" ]] || return 1
  root_physical="$(physical_dir "$install_root")" || return 1
  parent_physical="$(physical_dir "$(dirname "$parent")")" || return 1
  [[ "$parent_physical" == "$root_physical" ]]
}

# `releases/<id>.receipt.json` is as much a managed destination as `current`
# and the bin links: the installer writes it only when nothing is there, so a
# receipt already at that path is adopted whole -- reported back as this
# activation's receipt, and read by release_entries as the ordering key that
# decides retention and rollback. A planted one therefore pins itself newest
# and evicts a real release without ever having been installed. A receipt
# this installer wrote names the release being installed in all three
# identity fields and carries a sequence this install root actually issued,
# so re-activating an installed release still passes; anything else is
# refused before a byte is written.
ensure_managed_activation_receipt() {
  local receipt="$1"
  local install_root="$2"
  local release_id="$3"
  [[ -e "$receipt" || -L "$receipt" ]] || return 0
  [[ ! -L "$receipt" ]] ||
    die "refusing to install: $receipt is a symlink, not an activation receipt this installer wrote; move it aside before installing"
  [[ -f "$receipt" ]] ||
    die "refusing to install: $receipt is not a regular file, so it is not an activation receipt this installer wrote; move it aside before installing"
  jq -e 'type == "object"' "$receipt" >/dev/null 2>&1 ||
    die "refusing to install: $receipt does not parse as a JSON object; move it aside before installing"
  jq -e --arg release_id "$release_id" --arg source_commit "${release_id%%-*}" --arg manifest_sha "${release_id##*-}" '
    (.releaseId == $release_id) and
    (.sourceCommit == $source_commit) and
    (.manifestSha256 == $manifest_sha)
  ' "$receipt" >/dev/null ||
    die "refusing to install: $receipt does not name the release being installed ($release_id); move it aside before installing"
  local counter=0 sequence sequence_path
  sequence="$(jq -c '.activationSequence' "$receipt")"
  sequence_path="$install_root/releases/.activation-sequence"
  if [[ -f "$sequence_path" && ! -L "$sequence_path" ]]; then
    counter="$(tr -d '[:space:]' <"$sequence_path")"
    [[ "$counter" =~ ^[0-9]+$ ]] || counter=0
  fi
  jq -e --argjson counter "$counter" '
    ((.activationSequence | type) == "number") and
    ((.activationSequence | floor) == .activationSequence) and
    (.activationSequence >= 1) and
    (.activationSequence <= $counter)
  ' "$receipt" >/dev/null ||
    die "refusing to install: $receipt carries activationSequence $sequence, which is not an integer this install root has issued (its counter stands at $counter); move it aside before installing"
}

# Refuses, before anything is written, every path an activation writes through
# unless it has the shape this installer creates: releases/ and releases/<id>
# real directories, current and bin/<name> absent or managed symlinks, the bin
# dir a real directory. Called from a clean state so a refusal mutates nothing.
ensure_safe_release_view() {
  local install_root="$1"
  local bin_dir="$2"
  local release_id="$3"
  shift 3
  local releases="$install_root/releases"
  local current="$install_root/current"
  local release_path="$releases/$release_id"
  local release_meta="$releases/$release_id.receipt.json"
  [[ ! -L "$install_root" ]] || die "install root must not be a symlink: $install_root"
  [[ ! -e "$install_root" || -d "$install_root" ]] || die "install root is not a directory: $install_root"
  if [[ -e "$releases" || -L "$releases" ]]; then
    [[ ! -L "$releases" ]] || die "refusing to install through a symlink at $releases; remove it so releases/ is a real directory inside $install_root"
    [[ -d "$releases" ]] || die "refusing to install: $releases is not a directory; move it aside so the installer can create releases/"
  fi
  if [[ -e "$release_path" || -L "$release_path" ]]; then
    [[ ! -L "$release_path" ]] || die "refusing to activate a symlink at $release_path; remove it so the release directory is a real directory inside $releases"
    [[ -d "$release_path" ]] || die "refusing to activate: $release_path is not a directory; move it aside so the installer can create the release directory"
  fi
  ensure_managed_activation_receipt "$release_meta" "$install_root" "$release_id"
  if [[ -e "$current" || -L "$current" ]]; then
    managed_symlink "$current" "$install_root" releases ||
      die "refusing to replace $current: it is not a symlink into $releases managed by this installer; move it aside before installing"
  fi
  if [[ -e "$bin_dir" || -L "$bin_dir" ]]; then
    [[ ! -L "$bin_dir" ]] || die "bin dir must not be a symlink: $bin_dir; pass --bin-dir with the real directory"
    [[ -d "$bin_dir" ]] || die "bin dir is not a directory: $bin_dir; pass --bin-dir with a directory"
  fi
  local binary link
  for binary in "$@"; do
    link="$bin_dir/$binary"
    if [[ -e "$link" || -L "$link" ]]; then
      managed_symlink "$link" "$install_root" current ||
        die "refusing to replace $link: it is not a symlink into $current managed by this installer; move it aside before installing"
    fi
  done
}

atomic_symlink() {
  local target="$1"
  local link_path="$2"
  local link_parent
  link_parent="$(dirname "$link_path")"
  if [[ -e "$link_path" || -L "$link_path" ]] && [[ ! -L "$link_path" ]]; then
    printf 'hig-release: refusing to replace %s: it is not a symlink; move it aside before installing\n' "$link_path" >&2
    return 1
  fi
  if [[ -L "$link_parent" ]]; then
    printf 'hig-release: refusing to write through a symlink at %s; replace it with a real directory\n' "$link_parent" >&2
    return 1
  fi
  mkdir -p "$link_parent"
  local staging
  staging="$(mktemp -d "$link_parent/.${link_path##*/}.XXXXXX")"
  ln -s "$target" "$staging/link"
  python3 -c 'import os, sys; os.replace(sys.argv[1], sys.argv[2])' "$staging/link" "$link_path"
  rmdir "$staging"
}

ensure_public_binary_links() {
  local install_root="$1"
  local bin_dir="$2"
  local current_path
  current_path="$(current_link "$install_root")"
  mkdir -p "$bin_dir"
  for binary in "${BINARIES[@]}"; do
    local link_path target
    link_path="$(bin_link_path "$bin_dir" "$binary")"
    target="$current_path/$binary"
    if [[ ! -L "$link_path" || "$(readlink "$link_path")" != "$target" ]]; then
      atomic_symlink "$target" "$link_path"
    fi
  done
}

rollback_activation_view() {
  local status=$?
  local install_root="$1"
  local release_path="$2"
  local release_meta="$3"
  local previous_current="$4"
  local current_switched="$5"
  local release_created="$6"
  local bin_dir="$7"

  trap - ERR
  set +e
  (( status != 0 )) || status=1

  local current_path
  current_path="$(current_link "$install_root")"

  if [[ "$current_switched" == 1 ]]; then
    if [[ -n "$previous_current" ]]; then
      atomic_symlink "$previous_current" "$current_path"
    else
      rm -f -- "$current_path"
    fi
  fi

  if [[ -z "$previous_current" ]]; then
    local binary
    for binary in "${BINARIES[@]}"; do
      rm -f -- "$(bin_link_path "$bin_dir" "$binary")"
    done
  fi

  # Restoring the links restores the PATHS. If this activation had already
  # restarted the unit, the PROCESS is still the candidate's, so it has to
  # follow the links back before the candidate's executable is deleted from
  # under it - and a recovery that cannot be proved keeps the candidate on
  # disk and says so next to the failure that started this.
  # `current` may legitimately hold a relative target - ensure_safe_release_view
  # accepts one, resolving it against the link's own directory - so the value
  # is restored verbatim and normalised against the install root before it is
  # handed to a proof that has to resolve it as a path.
  local previous_release="$previous_current"
  [[ -z "$previous_release" || "$previous_release" == /* ]] || previous_release="$install_root/$previous_release"
  local recovered=1
  if [[ -n "${SERVE_RESTARTED_UNIT:-}" ]]; then
    serve_restore_previous "$SERVE_RESTARTED_UNIT" "$previous_release" || recovered=0
  fi

  if (( recovered )); then
    if [[ "$release_created" == 1 ]]; then
      rm -rf -- "$release_path"
      rm -f -- "$release_meta"
    fi
  else
    printf 'hig-release: the activation failed AND the recovery failed: %s is not proved to be serving, and %s was kept so the host can be recovered from it\n' "${SERVE_RESTARTED_UNIT:-kanban-serve}" "$release_path" >&2
  fi

  exit "$status"
}

maybe_fail_after_current() {
  if [[ "${HIG_RELEASE_FAIL_AFTER_CURRENT:-}" == "1" ]]; then
    printf 'hig-release: injected failure after current activation\n' >&2
    return 1
  fi
  return 0
}

# An install is not green until the process answering the unit's own listener
# is the release it just activated. On 2026-09-09 an install on hax swapped
# `current` and the bin links without restarting kanban-serve: the previous
# exe kept serving, the new release's board schema migrated the first board it
# was asked for, and every route answered 500 for two minutes until the unit
# was restarted by hand. So activation restarts the unit and then MEASURES the
# result - the unit is active with a MainPID, that pid's executable IS the
# `kanban` binary retained in the release just installed, the listener its own
# ExecStart names answers 200, and the pid and exe still agree after it did -
# and carries the measurement into the install receipt through
# SERVE_RESTART_JSON. A failed measurement returns non-zero naming what was
# measured, which the caller's ERR trap turns into a rollback that restores
# the previous view AND puts the previous release back in service. A host with
# no kanban-serve unit (a build box, the e2e harness) is not a failure - it
# says so once and records why. A manager that cannot be asked IS a failure:
# a skip is a claim about the unit, and an unreachable manager supports none.
# The identical-guards test in tests/e2e.rs fails when the two copies drift.
serve_restart_and_prove() {
  local release_path="$1"
  local unit=kanban-serve
  local skipped=""
  if ! command -v systemctl >/dev/null 2>&1; then
    skipped="systemctl is not on PATH"
  else
    serve_unit_disposition "$unit" || return 1
    case "$SERVE_UNIT_VERDICT" in
      restart)
        ;;
      absent)
        skipped="the $unit unit is not present on this host"
        ;;
      *)
        skipped="the $unit unit is present but neither enabled nor active on this host ($SERVE_UNIT_DETAIL)"
        ;;
    esac
  fi
  if [[ -n "$skipped" ]]; then
    printf 'hig-release: serve restart skipped: %s\n' "$skipped" >&2
    SERVE_RESTART_JSON="$(jq -n -S --arg skipped "$skipped" '{skipped:$skipped}')"
    return 0
  fi
  serve_unit_listener "$unit" || return 1
  local wait_seconds
  wait_seconds="$(serve_deadline_seconds)" || return 1
  # The HTTP half of the proof needs curl, and a curl that is not there exits
  # 127 into a swallowed status: the proof would read as a timeout, the
  # activation would roll back, and the recovery would fail the same way. So
  # the dependency is checked while the candidate is still un-restarted.
  command -v curl >/dev/null 2>&1 || {
    printf 'hig-release: cannot prove the release is serving: curl is not on PATH and the HTTP proof needs it; refusing before %s is restarted\n' "$unit" >&2
    return 1
  }
  # Set BEFORE the restart: a restart that fails may still have stopped the
  # process that was serving, so the rollback owes the host a recovery either
  # way. systemd writes job progress to stdout, which here is the install
  # receipt's channel, so the unit's output goes to stderr with the diagnostics.
  SERVE_RESTARTED_UNIT="$unit"
  systemctl restart "$unit" >&2 || {
    printf 'hig-release: serve restart failed: systemctl restart %s exited non-zero\n' "$unit" >&2
    return 1
  }
  serve_prove_release "$unit" "$release_path" "$SERVE_LISTENER_KIND" "$SERVE_LISTENER_VALUE" "$wait_seconds" "the release just installed" || return 1
  SERVE_RESTART_JSON="$(jq -n -S --argjson main_pid "$SERVE_PROVED_PID" --arg exe "$SERVE_PROVED_EXE" --arg exe_source "$SERVE_PROVED_SOURCE" --argjson listener "$(serve_listener_json)" --argjson http "$SERVE_PROVED_HTTP" '{restarted:true, mainPid:$main_pid, exe:$exe, exeSource:$exe_source, listener:$listener, http:$http}')"
}

# Called from the rollback when the activation that failed had already
# restarted the unit onto the candidate. The links are back on the previous
# release by then, so the process follows them: restart, and prove the
# PREVIOUS release is the one serving, before the candidate's executable is
# removed. A first install has no previous release to serve, so the unit is
# STOPPED instead - deleting the executable of a unit left running is how an
# install leaves a restart loop behind on a host it just failed to change.
serve_restore_previous() {
  local unit="$1"
  local previous_release="$2"
  if [[ -z "$previous_release" ]]; then
    systemctl stop "$unit" >&2 || {
      printf 'hig-release: recovery failed: systemctl stop %s exited non-zero; it may still be running the candidate executable\n' "$unit" >&2
      return 1
    }
    return 0
  fi
  systemctl restart "$unit" >&2 || {
    printf 'hig-release: recovery failed: systemctl restart %s exited non-zero with the previous release restored\n' "$unit" >&2
    return 1
  }
  serve_unit_listener "$unit" || {
    printf 'hig-release: recovery failed: the %s listener could not be read\n' "$unit" >&2
    return 1
  }
  local wait_seconds
  wait_seconds="$(serve_deadline_seconds)" || return 1
  serve_prove_release "$unit" "$previous_release" "$SERVE_LISTENER_KIND" "$SERVE_LISTENER_VALUE" "$wait_seconds" "the previous release" || {
    printf 'hig-release: recovery failed: %s is not serving the previous release %s\n' "$unit" "$previous_release" >&2
    return 1
  }
}

# The unit's disposition, from ONE manager query. `is-enabled` and `is-active`
# cannot answer this between them: a manager that cannot be reached fails both
# exactly as a unit that does not exist does, and reading that pair as "no unit
# on this host" turns the only proof an install has into a skip notice. But
# `systemctl show` exits 0 whenever the MANAGER answers, unit or no unit, so a
# non-zero exit or an answer carrying no LoadState IS the manager failing, and
# LoadState=not-found IS the unit being absent. A unit that is loaded but
# neither enabled nor active is idle: it is stopped on purpose, and an
# installer that started it would be overruling whoever stopped it.
serve_unit_disposition() {
  local unit="$1"
  SERVE_UNIT_VERDICT=""
  SERVE_UNIT_DETAIL=""
  local properties
  properties="$(systemctl show -p LoadState -p UnitFileState -p ActiveState "$unit" 2>&1)" || {
    printf 'hig-release: the manager could not be asked about %s: systemctl show exited non-zero: %s\n' "$unit" "${properties//$'\n'/ }" >&2
    return 1
  }
  local load_state="" unit_file_state="" active_state="" line
  while IFS= read -r line; do
    case "$line" in
      LoadState=*)
        load_state="${line#LoadState=}"
        ;;
      UnitFileState=*)
        unit_file_state="${line#UnitFileState=}"
        ;;
      ActiveState=*)
        active_state="${line#ActiveState=}"
        ;;
    esac
  done <<<"$properties"
  [[ -n "$load_state" ]] || {
    printf 'hig-release: the manager could not be asked about %s: systemctl show answered no LoadState: %s\n' "$unit" "${properties//$'\n'/ }" >&2
    return 1
  }
  SERVE_UNIT_DETAIL="LoadState=$load_state UnitFileState=${unit_file_state:-<none>} ActiveState=${active_state:-<none>}"
  if [[ "$load_state" == not-found ]]; then
    SERVE_UNIT_VERDICT=absent
    return 0
  fi
  # The unit-file states systemd's own `is-enabled` exits 0 for, so this
  # restarts exactly the units the pair of probes it replaces restarted.
  case "$active_state:$unit_file_state" in
    active:* | *:enabled | *:enabled-runtime | *:alias | *:static | *:indirect | *:generated | *:transient)
      SERVE_UNIT_VERDICT=restart
      ;;
    *)
      SERVE_UNIT_VERDICT=idle
      ;;
  esac
}

# The listener the unit itself names. `kanban serve` takes exactly one -
# `--port N` or `--socket PATH`, never both and never neither (README, "The
# web view") - so a unit naming none or two is refused rather than probed at
# some remembered default: a 200 from a port this release does not listen on
# is a stranger's answer. systemd renders ExecStart as a property blob whose
# argv[] is space-separated with any word containing a space quoted, so it is
# tokenized rather than split, and never eval'd.
serve_unit_listener() {
  local unit="$1"
  SERVE_LISTENER_KIND=""
  SERVE_LISTENER_VALUE=""
  local exec_start
  exec_start="$(systemctl show -p ExecStart --value "$unit" 2>&1)" || {
    printf 'hig-release: cannot read the listener: systemctl show -p ExecStart %s exited non-zero: %s\n' "$unit" "${exec_start//$'\n'/ }" >&2
    return 1
  }
  [[ "$exec_start" == *'argv[]='* ]] || {
    printf 'hig-release: cannot read the listener: the %s ExecStart names no command line: %s\n' "$unit" "${exec_start:-<empty>}" >&2
    return 1
  }
  local argv="${exec_start#*'argv[]='}"
  argv="${argv%%' ; '*}"
  serve_argv_tokens "$argv" || {
    printf 'hig-release: cannot read the listener: the %s ExecStart argv does not tokenize - unbalanced quote or trailing escape: %s\n' "$unit" "$argv" >&2
    return 1
  }
  local index kind value token
  for (( index = 0; index < ${#SERVE_ARGV_TOKENS[@]}; index += 1 )); do
    token="${SERVE_ARGV_TOKENS[index]}"
    case "$token" in
      --port | --socket)
        kind="${token#--}"
        value="${SERVE_ARGV_TOKENS[index + 1]:-}"
        ;;
      --port=* | --socket=*)
        kind="${token%%=*}"
        kind="${kind#--}"
        value="${token#*=}"
        ;;
      *)
        continue
        ;;
    esac
    [[ -z "$SERVE_LISTENER_KIND" ]] || {
      printf 'hig-release: cannot read the listener: the %s ExecStart names more than one listener: %s\n' "$unit" "$argv" >&2
      return 1
    }
    SERVE_LISTENER_KIND="$kind"
    SERVE_LISTENER_VALUE="$value"
  done
  case "$SERVE_LISTENER_KIND" in
    port)
      [[ "$SERVE_LISTENER_VALUE" =~ ^[1-9][0-9]*$ ]] && (( SERVE_LISTENER_VALUE <= 65535 )) || {
        printf 'hig-release: cannot read the listener: the %s ExecStart names --port %s, which is not a port\n' "$unit" "${SERVE_LISTENER_VALUE:-<nothing>}" >&2
        return 1
      }
      ;;
    socket)
      [[ "$SERVE_LISTENER_VALUE" == /* ]] || {
        printf 'hig-release: cannot read the listener: the %s ExecStart names --socket %s, which is not an absolute path\n' "$unit" "${SERVE_LISTENER_VALUE:-<nothing>}" >&2
        return 1
      }
      ;;
    *)
      printf 'hig-release: cannot read the listener: the %s ExecStart names no listener, and kanban serve has no default one: %s\n' "$unit" "$argv" >&2
      return 1
      ;;
  esac
}

# systemd's argv[] rendering, back into arguments: space-separated words, a
# word containing a space rendered in double quotes, backslash escapes inside
# it. The words land in SERVE_ARGV_TOKENS rather than on stdout so a path
# holding anything at all survives, and so a rendering this does NOT
# understand - an unbalanced quote, a trailing escape - is a non-zero return
# the caller refuses on, rather than a guess. No `eval` anywhere near an
# operator's unit file.
serve_argv_tokens() {
  local argv="$1"
  SERVE_ARGV_TOKENS=()
  local token="" quoted=0 escaped=0 index char
  for (( index = 0; index < ${#argv}; index += 1 )); do
    char="${argv:index:1}"
    if (( escaped )); then
      token+="$char"
      escaped=0
      continue
    fi
    case "$char" in
      '\')
        escaped=1
        ;;
      '"')
        quoted=$(( 1 - quoted ))
        ;;
      ' ')
        if (( quoted )); then
          token+=' '
        else
          [[ -z "$token" ]] || SERVE_ARGV_TOKENS+=("$token")
          token=""
        fi
        ;;
      *)
        token+="$char"
        ;;
    esac
  done
  (( quoted == 0 && escaped == 0 )) || return 1
  [[ -z "$token" ]] || SERVE_ARGV_TOKENS+=("$token")
}

serve_listener_json() {
  case "$SERVE_LISTENER_KIND" in
    socket)
      jq -n -S --arg socket "$SERVE_LISTENER_VALUE" '{socket:$socket}'
      ;;
    *)
      jq -n -S --argjson port "$SERVE_LISTENER_VALUE" '{port:$port}'
      ;;
  esac
}

# The deadline the proof is allowed, overridable so a test that measures the
# timeout itself does not have to wait out the real one. An override that is
# not a positive whole number of seconds is refused rather than quietly
# replaced by the default: a bound nobody set is not a bound anybody chose.
serve_deadline_seconds() {
  local seconds="${HIG_RELEASE_SERVE_DEADLINE_SECONDS:-15}"
  [[ "$seconds" =~ ^[1-9][0-9]*$ ]] || {
    printf 'hig-release: HIG_RELEASE_SERVE_DEADLINE_SECONDS must be a positive whole number of seconds, got %s\n' "$seconds" >&2
    return 1
  }
  printf '%s\n' "$seconds"
}

# /proc/<pid>/exe is the only witness that survives a symlink swap, and it is
# what the hosts this installs to have. HIG_RELEASE_EXE_OF_PID is the seam the
# test suite reads it through on a host without /proc; a proof taken through
# the seam records `override:` as its source, so a receipt can never present a
# fixture's answer as the kernel's.
serve_exe_of_pid() {
  local pid="$1"
  if [[ -n "${HIG_RELEASE_EXE_OF_PID:-}" ]]; then
    "$HIG_RELEASE_EXE_OF_PID" "$pid" 2>/dev/null || true
  else
    readlink "/proc/$pid/exe" 2>/dev/null || true
  fi
}

serve_exe_source() {
  local pid="$1"
  if [[ -n "${HIG_RELEASE_EXE_OF_PID:-}" ]]; then
    printf 'override:%s\n' "$HIG_RELEASE_EXE_OF_PID"
  else
    printf '/proc/%s/exe\n' "$pid"
  fi
}

# The one executable a green install accepts: the `kanban` binary retained
# INSIDE the release just activated. A directory-prefix test is not that test -
# it passes for `kb`, for `manifest.json`, and for `<path> (deleted)`, the
# string the kernel hands back once the binary a pid is running has been
# unlinked, which is exactly what a rolled-back release leaves a service
# holding. So the exe must resolve to <release>/kanban, and that path must
# still be a regular executable file.
serve_exe_is_release_binary() {
  local exe="$1"
  local release_physical="$2"
  [[ -n "$exe" && -n "$release_physical" ]] || return 1
  [[ "$exe" != *' (deleted)' ]] || return 1
  local exe_dir
  exe_dir="$(physical_dir "$(dirname "$exe")" || true)"
  [[ -n "$exe_dir" ]] || return 1
  [[ "$exe_dir/${exe##*/}" == "$release_physical/kanban" ]] || return 1
  [[ -f "$release_physical/kanban" && -x "$release_physical/kanban" ]]
}

# Every request is bounded by what is LEFT of the deadline, connect and
# transfer both: curl with no bound waits on a stalled socket for as long as
# the peer holds it, and a deadline the loop only consults BETWEEN requests is
# never reached.
serve_http_status() {
  local kind="$1"
  local value="$2"
  local budget="$3"
  case "$kind" in
    socket)
      curl -sS -o /dev/null -w '%{http_code}' --connect-timeout "$budget" --max-time "$budget" --unix-socket "$value" 'http://localhost/' 2>/dev/null || true
      ;;
    *)
      curl -sS -o /dev/null -w '%{http_code}' --connect-timeout "$budget" --max-time "$budget" "http://127.0.0.1:$value/" 2>/dev/null || true
      ;;
  esac
}

serve_prove_release() {
  local unit="$1"
  local release_path="$2"
  local kind="$3"
  local value="$4"
  local wait_seconds="$5"
  local subject="$6"
  SERVE_PROVED_PID=0
  SERVE_PROVED_EXE=""
  SERVE_PROVED_SOURCE=""
  SERVE_PROVED_HTTP=0
  local release_physical
  release_physical="$(physical_dir "$release_path" || true)"
  # Readiness and the exe are ONE poll, because a unit that reports active
  # with a MainPID is not yet running its own ExecStart: in that window
  # /proc/<MainPID>/exe resolves to systemd's pre-exec helper,
  # /usr/lib/systemd/systemd-executor. Reading the exe once, straight after an
  # active-poll, refused a CORRECT install of 8332e03 on hax at 03:10 MYT on
  # 2026-09-09 and rolled it back. So an exe that is empty, unreadable or not
  # the release binary is `not yet`, never a verdict - the loop sleeps and
  # looks again, and only the deadline is fatal, naming the last state, pid
  # and exe it saw.
  local deadline=$(( SECONDS + wait_seconds ))
  local state="" main_pid=0 exe=""
  while :; do
    state="$(systemctl show -p ActiveState --value "$unit" 2>/dev/null || true)"
    main_pid="$(systemctl show -p MainPID --value "$unit" 2>/dev/null || true)"
    [[ "$main_pid" =~ ^[1-9][0-9]*$ ]] || main_pid=0
    exe=""
    if [[ "$state" == active && "$main_pid" != 0 ]]; then
      exe="$(serve_exe_of_pid "$main_pid")"
      ! serve_exe_is_release_binary "$exe" "$release_physical" || break
    fi
    if (( SECONDS >= deadline )); then
      printf 'hig-release: %s is not running %s within %ss: ActiveState=%s MainPID=%s exe=%s release=%s\n' "$unit" "$subject" "$wait_seconds" "${state:-unknown}" "$main_pid" "${exe:-<unreadable>}" "${release_physical:-$release_path}" >&2
      return 1
    fi
    sleep 0.5
  done
  local http="" remaining=0
  while :; do
    remaining=$(( deadline - SECONDS ))
    if (( remaining <= 0 )); then
      printf 'hig-release: %s did not answer 200 for %s within %ss: MainPID=%s exe=%s listener=%s status=%s\n' "$unit" "$subject" "$wait_seconds" "$main_pid" "$exe" "$kind $value" "${http:-<none>}" >&2
      return 1
    fi
    http="$(serve_http_status "$kind" "$value" "$remaining")"
    [[ "$http" != 200 ]] || break
    sleep 0.5
  done
  # A 200 proves something answered the listener, not that the process proved
  # above answered it: a candidate that crashes after its exe was read leaves
  # the listener to whatever systemd starts next, and an unrelated process
  # holding the same port or socket answers just as convincingly. So the pid
  # and its exe are read AGAIN, and a green install is one where the three
  # measurements still agree.
  local final_state final_pid final_exe
  final_state="$(systemctl show -p ActiveState --value "$unit" 2>/dev/null || true)"
  final_pid="$(systemctl show -p MainPID --value "$unit" 2>/dev/null || true)"
  [[ "$final_pid" =~ ^[1-9][0-9]*$ ]] || final_pid=0
  final_exe=""
  (( final_pid == 0 )) || final_exe="$(serve_exe_of_pid "$final_pid")"
  if [[ "$final_state" != active ]] || (( final_pid != main_pid )) || [[ "$final_exe" != "$exe" ]] || ! serve_exe_is_release_binary "$final_exe" "$release_physical"; then
    printf 'hig-release: %s answered 200 but the process serving it is not the one proved: ActiveState=%s MainPID=%s (proved %s) exe=%s (proved %s)\n' "$unit" "${final_state:-unknown}" "$final_pid" "$main_pid" "${final_exe:-<unreadable>}" "$exe" >&2
    return 1
  fi
  SERVE_PROVED_PID="$main_pid"
  SERVE_PROVED_EXE="$exe"
  SERVE_PROVED_SOURCE="$(serve_exe_source "$main_pid")"
  SERVE_PROVED_HTTP="$http"
}

release_entries() {
  local root="$1"
  local files=()
  while IFS= read -r -d '' file; do
    files+=("$file")
  done < <(find "$root/releases" -mindepth 1 -maxdepth 1 -type f -name '*.receipt.json' -print0 2>/dev/null)
  if ((${#files[@]} == 0)); then
    return 0
  fi
  jq -r '. as $meta | [($meta.activationSequence // 0), ($meta.installedAt // 0), $meta.releaseId] | @tsv' "${files[@]}" |
    sort -t $'\t' -k1,1nr -k2,2nr |
    awk -F '\t' '{print $3}'
}

prune_releases() {
  local root="$1"
  local keep="${2:-$MAX_RELEASES}"
  local releases=()
  while IFS= read -r release; do
    [[ -n "$release" ]] || continue
    releases+=("$release")
  done < <(release_entries "$root")
  if (( ${#releases[@]} <= keep )); then
    return 0
  fi
  local index=0 failed=0
  for release in "${releases[@]}"; do
    (( index += 1 ))
    if (( index > keep )); then
      # Callers handle failure explicitly, disabling errexit in this function.
      # Keep the receipt indexing any directory we could not remove, and do
      # not let a later successful removal erase an earlier failure.
      if rm -rf -- "$root/releases/$release"; then
        rm -f -- "$root/releases/$release.receipt.json" || failed=1
      else
        failed=1
      fi
    fi
  done
  return "$failed"
}

validate_target() {
  case "$1" in
    hax|hig) ;;
    *) die "target must be hax or hig, got $1" ;;
  esac
}

manifest_path() {
  printf '%s/manifest.json' "$1"
}

package_validate() {
  local package_dir="$1"
  local target="$2"
  local manifest
  manifest="$(manifest_path "$package_dir")"
  [[ -f "$manifest" ]] || die "package has no manifest.json"

  jq -e --arg target "$target" --argjson expected_files "$(binaries_json)" '
    (.formatVersion == 1) and
    (.targets | type == "array") and
    (.targets | length == 2) and
    (.targets | index("hax") != null) and
    (.targets | index("hig") != null) and
    (.targets | index($target) != null) and
    (.sourceTreeClean == true) and
    ((.sourceCommit | type) == "string") and
    ((.sourceCommit | length) == 40) and
    ((.files | length) == ($expected_files | length)) and
    ([.files[].name] == $expected_files)
  ' "$manifest" >/dev/null || die "package manifest is incomplete or mismatched"
  reject_carried_release_identity "$manifest"

  while IFS=$'\t' read -r name sha256 size version; do
    local path="$package_dir/$name"
    [[ -f "$path" ]] || die "package is missing binary $name"
    [[ "$(wc -c <"$path" | tr -d '[:space:]')" == "$size" ]] || {
      die "package binary $name has the wrong size"
    }
    [[ "$(sha256_of "$path")" == "$sha256" ]] || {
      die "package binary $name hash mismatch"
    }
    [[ "$(file_version "$path")" == "$version" ]] || {
      die "package binary $name version mismatch"
    }
  done < <(jq -r '.files[] | [.name, .sha256, (.bytes | tostring), .version] | @tsv' "$manifest")
}

write_manifest() {
  local output="$1"
  local target="$2"
  local commit="$3"
  local files="$4"
  local targets
  targets="$(package_targets_json "$target")"
  jq -n -S \
    --argjson targets "$targets" \
    --arg source_commit "$commit" \
    --argjson files "$files" \
    '{
      formatVersion: 1,
      targets: $targets,
      sourceCommit: $source_commit,
      sourceTreeClean: true,
      files: $files
    }' > "$output/manifest.json"
}

package_create() {
  local target="$1"
  [[ "$target" == hax ]] || die "package target must be hax"
  shift
  local output=""
  while (($#)); do
    case "$1" in
      --output)
        output="${2:?--output requires a directory}"
        shift 2
        ;;
      --output=*)
        output="${1#*=}"
        shift
        ;;
      *)
        die "unknown package flag $1"
      ;;
    esac
  done

  require_host hax
  if [[ -z "$output" ]]; then
    output="$(mktemp -d "${TMPDIR:-/tmp}/kanban-release-${target}.XXXXXX")"
  else
    ensure_fresh_output "$output"
  fi

  local root
  root="$(repo_root)"
  [[ -f "$root/skills/kb/SKILL.md" ]] ||
    die "skills/kb is not initialized; run: git submodule update --init skills/kb"
  local dirty_before
  dirty_before="$(git -C "$root" status --porcelain=v1 --untracked-files=all)"
  [[ -z "$dirty_before" ]] || die "worktree must be clean before release packaging"

  local build_root
  build_root="$(mktemp -d "${TMPDIR:-/tmp}/kanban-release-build-${target}.XXXXXX")"
  track_temp "$build_root"
  (
    cd "$root"
    CARGO_TARGET_DIR="$build_root/target" cargo build --release --locked --bins
  )
  local commit
  commit="$(git -C "$root" rev-parse HEAD)"
  local files='[]'
  mkdir -p "$output"
  for binary in "${BINARIES[@]}"; do
    local source="$build_root/target/release/$binary"
    [[ -x "$source" ]] || die "release build did not produce $binary"
    install -m 0755 "$source" "$output/$binary"
    local bytes sha256 version
    bytes="$(wc -c <"$output/$binary" | tr -d '[:space:]')"
    sha256="$(sha256_of "$output/$binary")"
    version="$(file_version "$output/$binary")"
    files="$(jq -n \
      --argjson files "$files" \
      --arg name "$binary" \
      --arg sha256 "$sha256" \
      --arg version "$version" \
      --argjson bytes "$bytes" \
      '$files + [{name:$name, sha256:$sha256, bytes:$bytes, version:$version}]')"
  done

  write_manifest "$output" "$target" "$commit" "$files"
  local dirty_after
  dirty_after="$(git -C "$root" status --porcelain=v1 --untracked-files=all)"
  [[ "$dirty_after" == "$dirty_before" ]] || die "release packaging changed the worktree"
  package_validate "$output" "$target"
  local manifest_sha receipt
  manifest_sha="$(package_manifest_sha256 "$output")"
  receipt="$(receipt_path "$output")"
  jq -n -S \
    --arg host "hax" \
    --arg manifest_sha "$manifest_sha" \
    --arg source_commit "$commit" \
    --argjson files "$files" \
    --argjson targets "$(package_targets_json "$target")" \
    '{
      formatVersion: 1,
      host: $host,
      targets: $targets,
      manifestSha256: $manifest_sha,
      sourceCommit: $source_commit,
      sourceTreeClean: true,
      files: $files
    }' > "$receipt"
  jq -n -S \
    --arg packageDir "$output" \
    --arg manifest "$(manifest_path "$output")" \
    --arg receipt "$receipt" \
    --arg manifestSha256 "$manifest_sha" \
    --argjson targets "$(package_targets_json "$target")" \
    '{packageDir:$packageDir, manifest:$manifest, receipt:$receipt, manifestSha256:$manifestSha256, targets:$targets}'
}

install_release_tree() {
  local target="$1"
  local package_dir="$2"
  local receipt="$3"
  local install_root="$4"
  local bin_dir="$5"

  validate_package "$package_dir" "$target"
  validate_receipt "$receipt" "$target" "$package_dir"

  local release_id release_path release_meta staging receipt_json installed_at activation_sequence
  local previous_current="" current_switched=0 release_created=0
  release_id="$(release_id_from_receipt "$receipt")"
  ensure_safe_release_view "$install_root" "$bin_dir" "$release_id" "${BINARIES[@]}"
  mkdir -p "$install_root/releases"

  release_path="$(release_dir "$install_root" "$release_id")"
  release_meta="$(release_receipt "$install_root" "$release_id")"
  if [[ -L "$(current_link "$install_root")" ]]; then
    previous_current="$(readlink "$(current_link "$install_root")")"
  fi

  if [[ ! -d "$release_path" ]]; then
    staging="$(mktemp -d "$install_root/releases/.${release_id}.XXXXXX")"
    # mktemp makes 0700; the release dir it becomes must be traversable by the
    # service identity, which is no longer the installing root (hax, 2026-09-06).
    chmod 0755 "$staging"
    track_temp "$staging"
    for binary in "${BINARIES[@]}"; do
      install -m 0755 "$package_dir/$binary" "$staging/$binary"
    done
    cp "$package_dir/manifest.json" "$staging/manifest.json"
    validate_release_files "$staging" "$target"
    mv "$staging" "$release_path"
    release_created=1
  else
    validate_release_files "$release_path" "$target"
  fi

  trap 'rollback_activation_view "$install_root" "$release_path" "$release_meta" "$previous_current" "$current_switched" "$release_created" "$bin_dir"' ERR
  ensure_public_binary_links "$install_root" "$bin_dir"

  atomic_symlink "$release_path" "$(current_link "$install_root")"
  current_switched=1
  set +e
  maybe_fail_after_current
  local activation_status=$?
  set -e
  if (( activation_status != 0 )); then
    if [[ -n "$previous_current" ]]; then
      atomic_symlink "$previous_current" "$(current_link "$install_root")"
    else
      rm -f -- "$(current_link "$install_root")"
    fi
    if [[ "$release_created" == 1 ]]; then
      rm -rf -- "$release_path"
      rm -f -- "$release_meta"
    fi
    if [[ -z "$previous_current" ]]; then
      local binary
      for binary in "${BINARIES[@]}"; do
        rm -f -- "$(bin_link_path "$bin_dir" "$binary")"
      done
    fi
    trap - ERR
    return 1
  fi

  serve_restart_and_prove "$release_path"
  # Every check that can still refuse this activation runs BEFORE the receipt
  # is written; the receipt is the commit.
  validate_release_files "$release_path" "$target"

  if [[ ! -f "$release_meta" ]]; then
    receipt_json="$(jq -c '.' "$receipt")"
    activation_sequence="$(next_activation_sequence "$install_root")"
    installed_at="$(( $(date +%s) * 1000 ))"
    jq -n -S \
      --argjson receipt "$receipt_json" \
      --argjson activation_sequence "$activation_sequence" \
      --argjson installed_at "$installed_at" \
      --arg target "$target" \
      --arg release_id "$release_id" \
      --arg release_dir "$release_path" \
      --arg current_link "$(current_link "$install_root")" \
      --arg bin_dir "$bin_dir" \
      --arg installer "$(host_short)" \
      '$receipt + {
        activationSequence: $activation_sequence,
        installedAt: $installed_at,
        target: $target,
        releaseId: $release_id,
        releaseDir: $release_dir,
        currentLink: $current_link,
        binDir: $bin_dir,
        installerHost: $installer
      }' > "${release_meta}.tmp"
    mv -f "${release_meta}.tmp" "$release_meta"
  fi

  # Committed: the release is proved to be serving and its receipt is on disk,
  # so nothing after this line may roll it back. Retention still runs, and a
  # retention failure is reported as what it is rather than becoming a reason
  # to delete the release that is answering the port.
  trap - ERR
  local housekeeping=0
  prune_releases "$install_root" "$MAX_RELEASES" || housekeeping=1

  jq -n -S \
    --arg installRoot "$install_root" \
    --arg releaseDir "$release_path" \
    --arg current "$(current_link "$install_root")" \
    --arg receipt "$release_meta" \
    --arg binDir "$bin_dir" \
    --arg target "$target" \
    --argjson serve "$SERVE_RESTART_JSON" \
    '{installRoot:$installRoot, releaseDir:$releaseDir, current:$current, receipt:$receipt, binDir:$binDir, target:$target, serve:$serve}'
  if (( housekeeping )); then
    printf 'hig-release: %s is installed, proved to be serving and recorded; pruning older releases under %s failed and needs a look by hand - do NOT roll this activation back on account of it\n' "$release_path" "$install_root" >&2
    return 1
  fi
}

install_local() {
  local target="$1"
  shift
  local package_dir=""
  local install_root=""
  local bin_dir="$BIN_DIR_DEFAULT"
  while (($#)); do
    case "$1" in
      --package)
        package_dir="${2:?--package requires a directory}"
        shift 2
        ;;
      --package=*)
        package_dir="${1#*=}"
        shift
        ;;
      --install-root)
        install_root="${2:?--install-root requires a directory}"
        shift 2
        ;;
      --install-root=*)
        install_root="${1#*=}"
        shift
        ;;
      --bin-dir)
        bin_dir="${2:?--bin-dir requires a directory}"
        shift 2
        ;;
      --bin-dir=*)
        bin_dir="${1#*=}"
        shift
        ;;
      *)
        die "unknown install flag $1"
        ;;
    esac
  done

  [[ -n "$package_dir" ]] || die "--package is required"
  [[ -n "$install_root" ]] || die "--install-root is required"
  [[ -d "$package_dir" ]] || die "package directory does not exist: $package_dir"
  local receipt
  receipt="$(receipt_path "$package_dir")"
  [[ -f "$receipt" ]] || die "package receipt is required: $receipt"
  install_release_tree "$target" "$package_dir" "$receipt" "$install_root" "$bin_dir"
}

install_remote() {
  local target="$1"
  shift
  local package_dir=""
  local install_root=""
  local bin_dir="$BIN_DIR_DEFAULT"
  local hax_install_root=""
  while (($#)); do
    case "$1" in
      --package)
        package_dir="${2:?--package requires a directory}"
        shift 2
        ;;
      --package=*)
        package_dir="${1#*=}"
        shift
        ;;
      --install-root)
        install_root="${2:?--install-root requires a directory}"
        shift 2
        ;;
      --install-root=*)
        install_root="${1#*=}"
        shift
        ;;
      --bin-dir)
        bin_dir="${2:?--bin-dir requires a directory}"
        shift 2
        ;;
      --bin-dir=*)
        bin_dir="${1#*=}"
        shift
        ;;
      --hax-install-root)
        hax_install_root="${2:?--hax-install-root requires a directory}"
        shift 2
        ;;
      --hax-install-root=*)
        hax_install_root="${1#*=}"
        shift
        ;;
      *)
        die "unknown install flag $1"
        ;;
    esac
  done

  [[ -n "$package_dir" ]] || die "--package is required"
  [[ -n "$install_root" ]] || die "--install-root is required"
  [[ -d "$package_dir" ]] || die "package directory does not exist: $package_dir"
  local receipt
  receipt="$(receipt_path "$package_dir")"
  [[ -f "$receipt" ]] || die "package receipt is required: $receipt"
  package_validate "$package_dir" "$target"
  validate_receipt "$receipt" "$target" "$package_dir"
  [[ -n "$hax_install_root" ]] || die "--hax-install-root is required for hig installs"
  [[ -d "$hax_install_root" ]] || die "hax install root does not exist: $hax_install_root"
  validate_hax_activation_receipt "$hax_install_root" "$target" "$package_dir"
  local release_id release_dir release_meta
  release_id="$(release_id_from_receipt "$receipt")"
  release_dir="$(release_dir "$install_root" "$release_id")"
  release_meta="$(release_receipt "$install_root" "$release_id")"

  local remote_stage
  remote_stage="$(ssh "$target" 'mktemp -d "${TMPDIR:-/tmp}/kanban-release-install-remote.XXXXXX"')"
  tar -C "$package_dir" -cf - . | ssh "$target" "mkdir -p '$remote_stage/package' && tar -C '$remote_stage/package' -xf -"
  ssh "$target" "cat > '$remote_stage/package.receipt.json'" < "$receipt"
  local remote_serve remote_status=0 remote_output
  remote_output="$(mktemp "${TMPDIR:-/tmp}/kanban-serve-proof.XXXXXX")"
  track_temp "$remote_output"
  # Keep the quoted heredoc outside command substitution for Bash 3.2.
  if HOSTNAME_BIN="$HOSTNAME_BIN" ssh "$target" bash -s -- "$remote_stage" "$remote_stage/package" "$remote_stage/package.receipt.json" "$install_root" "$target" "$bin_dir" "$MAX_RELEASES" "${BINARIES[@]}" > "$remote_output" <<'REMOTE'
set -Eeuo pipefail

stage_root="$1"
package_dir="$2"
receipt="$3"
install_root="$4"
target="$5"
bin_dir="$6"
keep="$7"
shift 7
# The release set arrives as the caller's BINARIES instead of a literal list
# embedded here: a second copy of the set is exactly what drifted before, and
# a remote installer that links fewer binaries than the package carries would
# leave a validated release only partly reachable on the host.
BINARIES=("$@")

die() {
  printf 'hig-release: %s\n' "$*" >&2
  exit 1
}

(( ${#BINARIES[@]} > 0 )) || die "remote install received no release binary names"

# Membership in the release set, derived from BINARIES so a name the package
# does not ship can never be probed as though it belonged to the release.
release_binary_known() {
  local candidate="$1"
  local binary
  for binary in "${BINARIES[@]}"; do
    [[ "$binary" != "$candidate" ]] || return 0
  done
  return 1
}

file_version() {
  local binary="$1"
  local name="${binary##*/}"
  release_binary_known "$name" || die "unknown release binary $name"
  # `kanban` and `kb` are the operator CLI, whose version is a subcommand; the
  # dispatcher and every adapter answer the `--version` flag instead.
  case "$name" in
    kanban | kb)
      "$binary" version
      ;;
    *)
      "$binary" --version
      ;;
  esac | tr -d '\r' | sed 's/[[:space:]]*$//'
}

# The release set as a JSON array, so the receipt name check compares against
# BINARIES rather than a second literal list that can drift.
binaries_json() {
  printf '%s\n' "${BINARIES[@]}" | jq -R -s -c 'split("\n") | map(select(length > 0))'
}

# Verbatim copy of the local guard; see the comment above the local
# reject_carried_release_identity for why a carried identity is refused.
reject_carried_release_identity() {
  local file="$1"
  local expected_release_id="${2:-}"
  jq -e --arg expected "$expected_release_id" '
    (["activationSequence", "installedAt", "releaseDir", "currentLink", "binDir", "installerHost", "target"]
      + (if $expected == "" then ["releaseId"] else [] end)) as $forbidden
    | . as $doc
    | (($forbidden | map(select(. as $key | $doc | has($key))) | length) == 0)
      and (($doc | .releaseId // $expected) == $expected)
  ' "$file" >/dev/null ||
    die "refusing $file: a release artifact must not carry the release identity the installer derives"
}

cleanup_remote() {
  rm -rf -- "$stage_root"
}

trap cleanup_remote EXIT

# Verbatim copies of the local guard functions; see the comment above the
# local physical_dir for the drift test that pins them.
physical_dir() {
  (cd -P -- "$1" 2>/dev/null && pwd -P)
}

# A managed symlink is one this installer wrote: its target sits directly in
# "$install_root/$expected_parent" (current -> releases/<id>, bin/<name> ->
# current/<name>). Anything else belongs to the operator and is never replaced.
managed_symlink() {
  local link="$1"
  local install_root="$2"
  local expected_parent="$3"
  [[ -L "$link" ]] || return 1
  local target parent root_physical parent_physical
  target="$(readlink "$link")"
  [[ "$target" == /* ]] || target="$(dirname "$link")/$target"
  parent="$(dirname "$target")"
  [[ "${parent##*/}" == "$expected_parent" ]] || return 1
  root_physical="$(physical_dir "$install_root")" || return 1
  parent_physical="$(physical_dir "$(dirname "$parent")")" || return 1
  [[ "$parent_physical" == "$root_physical" ]]
}

# Verbatim copy of the local guard; see the comment above the local
# ensure_managed_activation_receipt for why a receipt already at the release
# receipt path is refused unless this installer could have written it.
ensure_managed_activation_receipt() {
  local receipt="$1"
  local install_root="$2"
  local release_id="$3"
  [[ -e "$receipt" || -L "$receipt" ]] || return 0
  [[ ! -L "$receipt" ]] ||
    die "refusing to install: $receipt is a symlink, not an activation receipt this installer wrote; move it aside before installing"
  [[ -f "$receipt" ]] ||
    die "refusing to install: $receipt is not a regular file, so it is not an activation receipt this installer wrote; move it aside before installing"
  jq -e 'type == "object"' "$receipt" >/dev/null 2>&1 ||
    die "refusing to install: $receipt does not parse as a JSON object; move it aside before installing"
  jq -e --arg release_id "$release_id" --arg source_commit "${release_id%%-*}" --arg manifest_sha "${release_id##*-}" '
    (.releaseId == $release_id) and
    (.sourceCommit == $source_commit) and
    (.manifestSha256 == $manifest_sha)
  ' "$receipt" >/dev/null ||
    die "refusing to install: $receipt does not name the release being installed ($release_id); move it aside before installing"
  local counter=0 sequence sequence_path
  sequence="$(jq -c '.activationSequence' "$receipt")"
  sequence_path="$install_root/releases/.activation-sequence"
  if [[ -f "$sequence_path" && ! -L "$sequence_path" ]]; then
    counter="$(tr -d '[:space:]' <"$sequence_path")"
    [[ "$counter" =~ ^[0-9]+$ ]] || counter=0
  fi
  jq -e --argjson counter "$counter" '
    ((.activationSequence | type) == "number") and
    ((.activationSequence | floor) == .activationSequence) and
    (.activationSequence >= 1) and
    (.activationSequence <= $counter)
  ' "$receipt" >/dev/null ||
    die "refusing to install: $receipt carries activationSequence $sequence, which is not an integer this install root has issued (its counter stands at $counter); move it aside before installing"
}

# Refuses, before anything is written, every path an activation writes through
# unless it has the shape this installer creates: releases/ and releases/<id>
# real directories, current and bin/<name> absent or managed symlinks, the bin
# dir a real directory. Called from a clean state so a refusal mutates nothing.
ensure_safe_release_view() {
  local install_root="$1"
  local bin_dir="$2"
  local release_id="$3"
  shift 3
  local releases="$install_root/releases"
  local current="$install_root/current"
  local release_path="$releases/$release_id"
  local release_meta="$releases/$release_id.receipt.json"
  [[ ! -L "$install_root" ]] || die "install root must not be a symlink: $install_root"
  [[ ! -e "$install_root" || -d "$install_root" ]] || die "install root is not a directory: $install_root"
  if [[ -e "$releases" || -L "$releases" ]]; then
    [[ ! -L "$releases" ]] || die "refusing to install through a symlink at $releases; remove it so releases/ is a real directory inside $install_root"
    [[ -d "$releases" ]] || die "refusing to install: $releases is not a directory; move it aside so the installer can create releases/"
  fi
  if [[ -e "$release_path" || -L "$release_path" ]]; then
    [[ ! -L "$release_path" ]] || die "refusing to activate a symlink at $release_path; remove it so the release directory is a real directory inside $releases"
    [[ -d "$release_path" ]] || die "refusing to activate: $release_path is not a directory; move it aside so the installer can create the release directory"
  fi
  ensure_managed_activation_receipt "$release_meta" "$install_root" "$release_id"
  if [[ -e "$current" || -L "$current" ]]; then
    managed_symlink "$current" "$install_root" releases ||
      die "refusing to replace $current: it is not a symlink into $releases managed by this installer; move it aside before installing"
  fi
  if [[ -e "$bin_dir" || -L "$bin_dir" ]]; then
    [[ ! -L "$bin_dir" ]] || die "bin dir must not be a symlink: $bin_dir; pass --bin-dir with the real directory"
    [[ -d "$bin_dir" ]] || die "bin dir is not a directory: $bin_dir; pass --bin-dir with a directory"
  fi
  local binary link
  for binary in "$@"; do
    link="$bin_dir/$binary"
    if [[ -e "$link" || -L "$link" ]]; then
      managed_symlink "$link" "$install_root" current ||
        die "refusing to replace $link: it is not a symlink into $current managed by this installer; move it aside before installing"
    fi
  done
}

atomic_symlink() {
  local target="$1"
  local link_path="$2"
  local link_parent
  link_parent="$(dirname "$link_path")"
  if [[ -e "$link_path" || -L "$link_path" ]] && [[ ! -L "$link_path" ]]; then
    printf 'hig-release: refusing to replace %s: it is not a symlink; move it aside before installing\n' "$link_path" >&2
    return 1
  fi
  if [[ -L "$link_parent" ]]; then
    printf 'hig-release: refusing to write through a symlink at %s; replace it with a real directory\n' "$link_parent" >&2
    return 1
  fi
  mkdir -p "$link_parent"
  local staging
  staging="$(mktemp -d "$link_parent/.${link_path##*/}.XXXXXX")"
  ln -s "$target" "$staging/link"
  python3 -c 'import os, sys; os.replace(sys.argv[1], sys.argv[2])' "$staging/link" "$link_path"
  rmdir "$staging"
}

ensure_public_binary_links() {
  local install_root="$1"
  local bin_dir="$2"
  local current_path
  current_path="$install_root/current"
  mkdir -p "$bin_dir"
  for binary in "${BINARIES[@]}"; do
    local link_path target
    link_path="$bin_dir/$binary"
    target="$current_path/$binary"
    if [[ ! -L "$link_path" || "$(readlink "$link_path")" != "$target" ]]; then
      atomic_symlink "$target" "$link_path"
    fi
  done
}

rollback_activation_view() {
  local status=$?
  local install_root="$1"
  local release_path="$2"
  local release_meta="$3"
  local previous_current="$4"
  local current_switched="$5"
  local release_created="$6"
  local bin_dir="$7"

  trap - ERR
  set +e
  (( status != 0 )) || status=1

  local current_path="$install_root/current"
  if [[ "$current_switched" == 1 ]]; then
    if [[ -n "$previous_current" ]]; then
      atomic_symlink "$previous_current" "$current_path"
    else
      rm -f -- "$current_path"
    fi
  fi
  if [[ -z "$previous_current" ]]; then
    local binary
    for binary in "${BINARIES[@]}"; do
      rm -f -- "$bin_dir/$binary"
    done
  fi

  # Restoring the links restores the PATHS. If this activation had already
  # restarted the unit, the PROCESS is still the candidate's, so it has to
  # follow the links back before the candidate's executable is deleted from
  # under it - and a recovery that cannot be proved keeps the candidate on
  # disk and says so next to the failure that started this.
  # `current` may legitimately hold a relative target - ensure_safe_release_view
  # accepts one, resolving it against the link's own directory - so the value
  # is restored verbatim and normalised against the install root before it is
  # handed to a proof that has to resolve it as a path.
  local previous_release="$previous_current"
  [[ -z "$previous_release" || "$previous_release" == /* ]] || previous_release="$install_root/$previous_release"
  local recovered=1
  if [[ -n "${SERVE_RESTARTED_UNIT:-}" ]]; then
    serve_restore_previous "$SERVE_RESTARTED_UNIT" "$previous_release" || recovered=0
  fi

  if (( recovered )); then
    if [[ "$release_created" == 1 ]]; then
      rm -rf -- "$release_path"
      rm -f -- "$release_meta"
    fi
  else
    printf 'hig-release: the activation failed AND the recovery failed: %s is not proved to be serving, and %s was kept so the host can be recovered from it\n' "${SERVE_RESTARTED_UNIT:-kanban-serve}" "$release_path" >&2
  fi

  exit "$status"
}

maybe_fail_after_current() {
  if [[ "${HIG_RELEASE_FAIL_AFTER_CURRENT:-}" == "1" ]]; then
    printf 'hig-release: injected failure after current activation\n' >&2
    return 1
  fi
  return 0
}

# Verbatim copies of the local guards; see the comment above the local
# serve_restart_and_prove for the incidents they measure against. The
# identical-guards test in tests/e2e.rs fails when either copy drifts.
serve_restart_and_prove() {
  local release_path="$1"
  local unit=kanban-serve
  local skipped=""
  if ! command -v systemctl >/dev/null 2>&1; then
    skipped="systemctl is not on PATH"
  else
    serve_unit_disposition "$unit" || return 1
    case "$SERVE_UNIT_VERDICT" in
      restart)
        ;;
      absent)
        skipped="the $unit unit is not present on this host"
        ;;
      *)
        skipped="the $unit unit is present but neither enabled nor active on this host ($SERVE_UNIT_DETAIL)"
        ;;
    esac
  fi
  if [[ -n "$skipped" ]]; then
    printf 'hig-release: serve restart skipped: %s\n' "$skipped" >&2
    SERVE_RESTART_JSON="$(jq -n -S --arg skipped "$skipped" '{skipped:$skipped}')"
    return 0
  fi
  serve_unit_listener "$unit" || return 1
  local wait_seconds
  wait_seconds="$(serve_deadline_seconds)" || return 1
  # The HTTP half of the proof needs curl, and a curl that is not there exits
  # 127 into a swallowed status: the proof would read as a timeout, the
  # activation would roll back, and the recovery would fail the same way. So
  # the dependency is checked while the candidate is still un-restarted.
  command -v curl >/dev/null 2>&1 || {
    printf 'hig-release: cannot prove the release is serving: curl is not on PATH and the HTTP proof needs it; refusing before %s is restarted\n' "$unit" >&2
    return 1
  }
  # Set BEFORE the restart: a restart that fails may still have stopped the
  # process that was serving, so the rollback owes the host a recovery either
  # way. systemd writes job progress to stdout, which here is the install
  # receipt's channel, so the unit's output goes to stderr with the diagnostics.
  SERVE_RESTARTED_UNIT="$unit"
  systemctl restart "$unit" >&2 || {
    printf 'hig-release: serve restart failed: systemctl restart %s exited non-zero\n' "$unit" >&2
    return 1
  }
  serve_prove_release "$unit" "$release_path" "$SERVE_LISTENER_KIND" "$SERVE_LISTENER_VALUE" "$wait_seconds" "the release just installed" || return 1
  SERVE_RESTART_JSON="$(jq -n -S --argjson main_pid "$SERVE_PROVED_PID" --arg exe "$SERVE_PROVED_EXE" --arg exe_source "$SERVE_PROVED_SOURCE" --argjson listener "$(serve_listener_json)" --argjson http "$SERVE_PROVED_HTTP" '{restarted:true, mainPid:$main_pid, exe:$exe, exeSource:$exe_source, listener:$listener, http:$http}')"
}

serve_restore_previous() {
  local unit="$1"
  local previous_release="$2"
  if [[ -z "$previous_release" ]]; then
    systemctl stop "$unit" >&2 || {
      printf 'hig-release: recovery failed: systemctl stop %s exited non-zero; it may still be running the candidate executable\n' "$unit" >&2
      return 1
    }
    return 0
  fi
  systemctl restart "$unit" >&2 || {
    printf 'hig-release: recovery failed: systemctl restart %s exited non-zero with the previous release restored\n' "$unit" >&2
    return 1
  }
  serve_unit_listener "$unit" || {
    printf 'hig-release: recovery failed: the %s listener could not be read\n' "$unit" >&2
    return 1
  }
  local wait_seconds
  wait_seconds="$(serve_deadline_seconds)" || return 1
  serve_prove_release "$unit" "$previous_release" "$SERVE_LISTENER_KIND" "$SERVE_LISTENER_VALUE" "$wait_seconds" "the previous release" || {
    printf 'hig-release: recovery failed: %s is not serving the previous release %s\n' "$unit" "$previous_release" >&2
    return 1
  }
}

serve_unit_disposition() {
  local unit="$1"
  SERVE_UNIT_VERDICT=""
  SERVE_UNIT_DETAIL=""
  local properties
  properties="$(systemctl show -p LoadState -p UnitFileState -p ActiveState "$unit" 2>&1)" || {
    printf 'hig-release: the manager could not be asked about %s: systemctl show exited non-zero: %s\n' "$unit" "${properties//$'\n'/ }" >&2
    return 1
  }
  local load_state="" unit_file_state="" active_state="" line
  while IFS= read -r line; do
    case "$line" in
      LoadState=*)
        load_state="${line#LoadState=}"
        ;;
      UnitFileState=*)
        unit_file_state="${line#UnitFileState=}"
        ;;
      ActiveState=*)
        active_state="${line#ActiveState=}"
        ;;
    esac
  done <<<"$properties"
  [[ -n "$load_state" ]] || {
    printf 'hig-release: the manager could not be asked about %s: systemctl show answered no LoadState: %s\n' "$unit" "${properties//$'\n'/ }" >&2
    return 1
  }
  SERVE_UNIT_DETAIL="LoadState=$load_state UnitFileState=${unit_file_state:-<none>} ActiveState=${active_state:-<none>}"
  if [[ "$load_state" == not-found ]]; then
    SERVE_UNIT_VERDICT=absent
    return 0
  fi
  # The unit-file states systemd's own `is-enabled` exits 0 for, so this
  # restarts exactly the units the pair of probes it replaces restarted.
  case "$active_state:$unit_file_state" in
    active:* | *:enabled | *:enabled-runtime | *:alias | *:static | *:indirect | *:generated | *:transient)
      SERVE_UNIT_VERDICT=restart
      ;;
    *)
      SERVE_UNIT_VERDICT=idle
      ;;
  esac
}

serve_unit_listener() {
  local unit="$1"
  SERVE_LISTENER_KIND=""
  SERVE_LISTENER_VALUE=""
  local exec_start
  exec_start="$(systemctl show -p ExecStart --value "$unit" 2>&1)" || {
    printf 'hig-release: cannot read the listener: systemctl show -p ExecStart %s exited non-zero: %s\n' "$unit" "${exec_start//$'\n'/ }" >&2
    return 1
  }
  [[ "$exec_start" == *'argv[]='* ]] || {
    printf 'hig-release: cannot read the listener: the %s ExecStart names no command line: %s\n' "$unit" "${exec_start:-<empty>}" >&2
    return 1
  }
  local argv="${exec_start#*'argv[]='}"
  argv="${argv%%' ; '*}"
  serve_argv_tokens "$argv" || {
    printf 'hig-release: cannot read the listener: the %s ExecStart argv does not tokenize - unbalanced quote or trailing escape: %s\n' "$unit" "$argv" >&2
    return 1
  }
  local index kind value token
  for (( index = 0; index < ${#SERVE_ARGV_TOKENS[@]}; index += 1 )); do
    token="${SERVE_ARGV_TOKENS[index]}"
    case "$token" in
      --port | --socket)
        kind="${token#--}"
        value="${SERVE_ARGV_TOKENS[index + 1]:-}"
        ;;
      --port=* | --socket=*)
        kind="${token%%=*}"
        kind="${kind#--}"
        value="${token#*=}"
        ;;
      *)
        continue
        ;;
    esac
    [[ -z "$SERVE_LISTENER_KIND" ]] || {
      printf 'hig-release: cannot read the listener: the %s ExecStart names more than one listener: %s\n' "$unit" "$argv" >&2
      return 1
    }
    SERVE_LISTENER_KIND="$kind"
    SERVE_LISTENER_VALUE="$value"
  done
  case "$SERVE_LISTENER_KIND" in
    port)
      [[ "$SERVE_LISTENER_VALUE" =~ ^[1-9][0-9]*$ ]] && (( SERVE_LISTENER_VALUE <= 65535 )) || {
        printf 'hig-release: cannot read the listener: the %s ExecStart names --port %s, which is not a port\n' "$unit" "${SERVE_LISTENER_VALUE:-<nothing>}" >&2
        return 1
      }
      ;;
    socket)
      [[ "$SERVE_LISTENER_VALUE" == /* ]] || {
        printf 'hig-release: cannot read the listener: the %s ExecStart names --socket %s, which is not an absolute path\n' "$unit" "${SERVE_LISTENER_VALUE:-<nothing>}" >&2
        return 1
      }
      ;;
    *)
      printf 'hig-release: cannot read the listener: the %s ExecStart names no listener, and kanban serve has no default one: %s\n' "$unit" "$argv" >&2
      return 1
      ;;
  esac
}

serve_argv_tokens() {
  local argv="$1"
  SERVE_ARGV_TOKENS=()
  local token="" quoted=0 escaped=0 index char
  for (( index = 0; index < ${#argv}; index += 1 )); do
    char="${argv:index:1}"
    if (( escaped )); then
      token+="$char"
      escaped=0
      continue
    fi
    case "$char" in
      '\')
        escaped=1
        ;;
      '"')
        quoted=$(( 1 - quoted ))
        ;;
      ' ')
        if (( quoted )); then
          token+=' '
        else
          [[ -z "$token" ]] || SERVE_ARGV_TOKENS+=("$token")
          token=""
        fi
        ;;
      *)
        token+="$char"
        ;;
    esac
  done
  (( quoted == 0 && escaped == 0 )) || return 1
  [[ -z "$token" ]] || SERVE_ARGV_TOKENS+=("$token")
}

serve_listener_json() {
  case "$SERVE_LISTENER_KIND" in
    socket)
      jq -n -S --arg socket "$SERVE_LISTENER_VALUE" '{socket:$socket}'
      ;;
    *)
      jq -n -S --argjson port "$SERVE_LISTENER_VALUE" '{port:$port}'
      ;;
  esac
}

serve_deadline_seconds() {
  local seconds="${HIG_RELEASE_SERVE_DEADLINE_SECONDS:-15}"
  [[ "$seconds" =~ ^[1-9][0-9]*$ ]] || {
    printf 'hig-release: HIG_RELEASE_SERVE_DEADLINE_SECONDS must be a positive whole number of seconds, got %s\n' "$seconds" >&2
    return 1
  }
  printf '%s\n' "$seconds"
}

serve_exe_of_pid() {
  local pid="$1"
  if [[ -n "${HIG_RELEASE_EXE_OF_PID:-}" ]]; then
    "$HIG_RELEASE_EXE_OF_PID" "$pid" 2>/dev/null || true
  else
    readlink "/proc/$pid/exe" 2>/dev/null || true
  fi
}

serve_exe_source() {
  local pid="$1"
  if [[ -n "${HIG_RELEASE_EXE_OF_PID:-}" ]]; then
    printf 'override:%s\n' "$HIG_RELEASE_EXE_OF_PID"
  else
    printf '/proc/%s/exe\n' "$pid"
  fi
}

serve_exe_is_release_binary() {
  local exe="$1"
  local release_physical="$2"
  [[ -n "$exe" && -n "$release_physical" ]] || return 1
  [[ "$exe" != *' (deleted)' ]] || return 1
  local exe_dir
  exe_dir="$(physical_dir "$(dirname "$exe")" || true)"
  [[ -n "$exe_dir" ]] || return 1
  [[ "$exe_dir/${exe##*/}" == "$release_physical/kanban" ]] || return 1
  [[ -f "$release_physical/kanban" && -x "$release_physical/kanban" ]]
}

serve_http_status() {
  local kind="$1"
  local value="$2"
  local budget="$3"
  case "$kind" in
    socket)
      curl -sS -o /dev/null -w '%{http_code}' --connect-timeout "$budget" --max-time "$budget" --unix-socket "$value" 'http://localhost/' 2>/dev/null || true
      ;;
    *)
      curl -sS -o /dev/null -w '%{http_code}' --connect-timeout "$budget" --max-time "$budget" "http://127.0.0.1:$value/" 2>/dev/null || true
      ;;
  esac
}

serve_prove_release() {
  local unit="$1"
  local release_path="$2"
  local kind="$3"
  local value="$4"
  local wait_seconds="$5"
  local subject="$6"
  SERVE_PROVED_PID=0
  SERVE_PROVED_EXE=""
  SERVE_PROVED_SOURCE=""
  SERVE_PROVED_HTTP=0
  local release_physical
  release_physical="$(physical_dir "$release_path" || true)"
  # Readiness and the exe are ONE poll, because a unit that reports active
  # with a MainPID is not yet running its own ExecStart: in that window
  # /proc/<MainPID>/exe resolves to systemd's pre-exec helper,
  # /usr/lib/systemd/systemd-executor. Reading the exe once, straight after an
  # active-poll, refused a CORRECT install of 8332e03 on hax at 03:10 MYT on
  # 2026-09-09 and rolled it back. So an exe that is empty, unreadable or not
  # the release binary is `not yet`, never a verdict - the loop sleeps and
  # looks again, and only the deadline is fatal, naming the last state, pid
  # and exe it saw.
  local deadline=$(( SECONDS + wait_seconds ))
  local state="" main_pid=0 exe=""
  while :; do
    state="$(systemctl show -p ActiveState --value "$unit" 2>/dev/null || true)"
    main_pid="$(systemctl show -p MainPID --value "$unit" 2>/dev/null || true)"
    [[ "$main_pid" =~ ^[1-9][0-9]*$ ]] || main_pid=0
    exe=""
    if [[ "$state" == active && "$main_pid" != 0 ]]; then
      exe="$(serve_exe_of_pid "$main_pid")"
      ! serve_exe_is_release_binary "$exe" "$release_physical" || break
    fi
    if (( SECONDS >= deadline )); then
      printf 'hig-release: %s is not running %s within %ss: ActiveState=%s MainPID=%s exe=%s release=%s\n' "$unit" "$subject" "$wait_seconds" "${state:-unknown}" "$main_pid" "${exe:-<unreadable>}" "${release_physical:-$release_path}" >&2
      return 1
    fi
    sleep 0.5
  done
  local http="" remaining=0
  while :; do
    remaining=$(( deadline - SECONDS ))
    if (( remaining <= 0 )); then
      printf 'hig-release: %s did not answer 200 for %s within %ss: MainPID=%s exe=%s listener=%s status=%s\n' "$unit" "$subject" "$wait_seconds" "$main_pid" "$exe" "$kind $value" "${http:-<none>}" >&2
      return 1
    fi
    http="$(serve_http_status "$kind" "$value" "$remaining")"
    [[ "$http" != 200 ]] || break
    sleep 0.5
  done
  # A 200 proves something answered the listener, not that the process proved
  # above answered it: a candidate that crashes after its exe was read leaves
  # the listener to whatever systemd starts next, and an unrelated process
  # holding the same port or socket answers just as convincingly. So the pid
  # and its exe are read AGAIN, and a green install is one where the three
  # measurements still agree.
  local final_state final_pid final_exe
  final_state="$(systemctl show -p ActiveState --value "$unit" 2>/dev/null || true)"
  final_pid="$(systemctl show -p MainPID --value "$unit" 2>/dev/null || true)"
  [[ "$final_pid" =~ ^[1-9][0-9]*$ ]] || final_pid=0
  final_exe=""
  (( final_pid == 0 )) || final_exe="$(serve_exe_of_pid "$final_pid")"
  if [[ "$final_state" != active ]] || (( final_pid != main_pid )) || [[ "$final_exe" != "$exe" ]] || ! serve_exe_is_release_binary "$final_exe" "$release_physical"; then
    printf 'hig-release: %s answered 200 but the process serving it is not the one proved: ActiveState=%s MainPID=%s (proved %s) exe=%s (proved %s)\n' "$unit" "${final_state:-unknown}" "$final_pid" "$main_pid" "${final_exe:-<unreadable>}" "$exe" >&2
    return 1
  fi
  SERVE_PROVED_PID="$main_pid"
  SERVE_PROVED_EXE="$exe"
  SERVE_PROVED_SOURCE="$(serve_exe_source "$main_pid")"
  SERVE_PROVED_HTTP="$http"
}

next_activation_sequence() {
  local install_root="$1"
  local sequence_path
  sequence_path="$install_root/releases/.activation-sequence"
  mkdir -p "$(dirname "$sequence_path")"
  python3 - "$sequence_path" <<'PY'
import fcntl
import os
import sys

path = sys.argv[1]
with open(path, "a+", encoding="utf-8") as handle:
    fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
    handle.seek(0)
    current = handle.read().strip()
    next_value = int(current) + 1 if current else 1
    handle.seek(0)
    handle.truncate()
    handle.write(f"{next_value}\n")
    handle.flush()
    os.fsync(handle.fileno())
    print(next_value)
PY
}

release_entries() {
  local root="$1"
  local files=()
  while IFS= read -r -d '' file; do
    files+=("$file")
  done < <(find "$root/releases" -mindepth 1 -maxdepth 1 -type f -name '*.receipt.json' -print0 2>/dev/null)
  if ((${#files[@]} == 0)); then
    return 0
  fi
  jq -r '. as $meta | [($meta.activationSequence // 0), ($meta.installedAt // 0), $meta.releaseId] | @tsv' "${files[@]}" |
    sort -t $'\t' -k1,1nr -k2,2nr |
    awk -F '\t' '{print $3}'
}

prune_releases() {
  local root="$1"
  local keep="${2:-$MAX_RELEASES}"
  local releases=()
  while IFS= read -r release; do
    [[ -n "$release" ]] || continue
    releases+=("$release")
  done < <(release_entries "$root")
  if (( ${#releases[@]} <= keep )); then
    return 0
  fi
  local index=0 failed=0
  for release in "${releases[@]}"; do
    (( index += 1 ))
    if (( index > keep )); then
      # Callers handle failure explicitly, disabling errexit in this function.
      # Keep the receipt indexing any directory we could not remove, and do
      # not let a later successful removal erase an earlier failure.
      if rm -rf -- "$root/releases/$release"; then
        rm -f -- "$root/releases/$release.receipt.json" || failed=1
      else
        failed=1
      fi
    fi
  done
  return "$failed"
}

hostname_bin="${HOSTNAME_BIN:-/bin/hostname}"
host="$("$hostname_bin" -s 2>/dev/null || "$hostname_bin")"
[[ "$host" == "$target" ]] || die "remote host $target did not identify itself as $target"
manifest="$package_dir/manifest.json"
[[ -f "$manifest" ]] || die "remote package has no manifest.json"
reject_carried_release_identity "$manifest"

  jq -e --arg target "$target" --argjson expected_files "$(binaries_json)" '
    (.formatVersion == 1) and
    (.targets | type == "array") and
    (.targets | length == 2) and
    (.targets | index("hax") != null) and
    (.targets | index("hig") != null) and
    (.targets | index($target) != null) and
    (.sourceTreeClean == true) and
    (.host == "hax") and
  ((.sourceCommit | type) == "string") and
  ((.sourceCommit | length) == 40) and
  ((.manifestSha256 | type) == "string") and
  ((.manifestSha256 | length) == 64) and
  ((.files | length) == ($expected_files | length)) and
  ([.files[].name] == $expected_files)
' "$receipt" >/dev/null || die "remote package receipt is incomplete or mismatched"
reject_carried_release_identity "$receipt" "$(jq -r '.sourceCommit + "-" + .manifestSha256' "$receipt")"

while IFS=$'\t' read -r name sha256 size version; do
  path="$package_dir/$name"
  [[ -f "$path" ]] || die "remote package is missing binary $name"
  [[ "$(wc -c <"$path" | tr -d '[:space:]')" == "$size" ]] || die "remote binary $name has the wrong size"
  [[ "$(sha256sum "$path" | awk '{print $1}')" == "$sha256" ]] || die "remote binary $name hash mismatch"
  [[ "$(file_version "$path")" == "$version" ]] || die "remote binary $name version mismatch"
done < <(jq -r '.files[] | [.name, .sha256, (.bytes | tostring), .version] | @tsv' "$receipt")

release_id="$(jq -r '.sourceCommit + "-" + .manifestSha256' "$receipt")"
ensure_safe_release_view "$install_root" "$bin_dir" "$release_id" "${BINARIES[@]}"
mkdir -p "$install_root/releases"
release_path="$install_root/releases/$release_id"
release_receipt="$install_root/releases/$release_id.receipt.json"
previous_current=""
current_switched=0
release_created=0
if [[ -L "$install_root/current" ]]; then
  previous_current="$(readlink "$install_root/current")"
fi
if [[ ! -d "$release_path" ]]; then
  staging="$(mktemp -d "$install_root/releases/.${release_id}.XXXXXX")"
  # mktemp makes 0700; the release dir it becomes must be traversable by the
  # service identity, which is no longer the installing root (hax, 2026-09-06).
  chmod 0755 "$staging"
  for binary in "${BINARIES[@]}"; do
    install -m 0755 "$package_dir/$binary" "$staging/$binary"
  done
  cp "$package_dir/manifest.json" "$staging/manifest.json"
  [[ "$(sha256sum "$staging/manifest.json" | awk '{print $1}')" == "$(jq -r '.manifestSha256' "$receipt")" ]] || {
    die "remote manifest hash mismatch"
  }
  mv "$staging" "$release_path"
  release_created=1
fi

trap 'rollback_activation_view "$install_root" "$release_path" "$release_receipt" "$previous_current" "$current_switched" "$release_created" "$bin_dir"' ERR

ensure_public_binary_links "$install_root" "$bin_dir"
atomic_symlink "$release_path" "$install_root/current"
current_switched=1
set +e
maybe_fail_after_current
activation_status=$?
set -e
if (( activation_status != 0 )); then
  if [[ -n "$previous_current" ]]; then
    atomic_symlink "$previous_current" "$install_root/current"
  else
    rm -f -- "$install_root/current"
  fi
  if [[ "$release_created" == 1 ]]; then
    rm -rf -- "$release_path"
    rm -f -- "$release_receipt"
  fi
  if [[ -z "$previous_current" ]]; then
    for binary in "${BINARIES[@]}"; do
      rm -f -- "$bin_dir/$binary"
    done
  fi
  trap - ERR
  exit 1
fi

serve_restart_and_prove "$release_path"

installed_at="$(( $(date +%s) * 1000 ))"
if [[ ! -f "$release_receipt" ]]; then
  activation_sequence="$(next_activation_sequence "$install_root")"
  jq -n -S \
    --argjson receipt "$(jq -c '.' "$receipt")" \
    --argjson activation_sequence "$activation_sequence" \
    --argjson installed_at "$installed_at" \
    --arg target "$target" \
    --arg release_id "$release_id" \
    --arg release_dir "$release_path" \
    --arg current_link "$install_root/current" \
    --arg bin_dir "$bin_dir" \
    --arg installer "$host" \
    '$receipt + {
      activationSequence: $activation_sequence,
      installedAt: $installed_at,
      target: $target,
      releaseId: $release_id,
      releaseDir: $release_dir,
      currentLink: $current_link,
      binDir: $bin_dir,
      installerHost: $installer
  }' > "$release_receipt"
fi

jq -e --arg target "$target" '
  (.targets | type == "array") and
  (.targets | length == 2) and
  (.targets | index("hax") != null) and
  (.targets | index("hig") != null) and
  (.targets | index($target) != null)
' "$release_receipt" >/dev/null || die "remote receipt target mismatch"
# Committed: the release is proved to be serving and its receipt is on disk, so
# nothing after this line may roll it back. Retention still runs, and a
# retention failure is reported as what it is rather than becoming a reason to
# delete the release that is answering the listener.
trap - ERR
housekeeping=0
prune_releases "$install_root" "$keep" || housekeeping=1
printf '%s\n' "$SERVE_RESTART_JSON"
if (( housekeeping )); then
  printf 'hig-release: %s is installed, proved to be serving and recorded; pruning older releases under %s failed and needs a look by hand - do NOT roll this activation back on account of it\n' "$release_path" "$install_root" >&2
  exit 1
fi
REMOTE
  then
    remote_status=0
  else
    remote_status=$?
  fi
  remote_serve="$(cat "$remote_output")"
  if [[ -z "$remote_serve" ]]; then
    # Pre-commit failures have no summary: propagate the failure, not a
    # fabricated receipt. A committed cleanup failure does carry its proof.
    (( remote_status == 0 )) || return "$remote_status"
    die "remote install did not report the kanban-serve restart proof"
  fi

  jq -n -S \
    --arg installRoot "$install_root" \
    --arg releaseId "$release_id" \
    --arg releaseDir "$release_dir" \
    --arg current "$(current_link "$install_root")" \
    --arg receipt "$release_meta" \
    --arg packageDir "$package_dir" \
    --arg target "$target" \
    --arg binDir "$bin_dir" \
    --argjson serve "$remote_serve" \
    '{installRoot:$installRoot, releaseId:$releaseId, releaseDir:$releaseDir, current:$current, receipt:$receipt, packageDir:$packageDir, target:$target, binDir:$binDir, serve:$serve}'
  return "$remote_status"
}

rollback_release() {
  local target="$1"
  shift
  local install_root=""
  local bin_dir="$BIN_DIR_DEFAULT"
  local steps=1
  while (($#)); do
    case "$1" in
      --install-root)
        install_root="${2:?--install-root requires a directory}"
        shift 2
        ;;
      --install-root=*)
        install_root="${1#*=}"
        shift
        ;;
      --bin-dir)
        bin_dir="${2:?--bin-dir requires a directory}"
        shift 2
        ;;
      --bin-dir=*)
        bin_dir="${1#*=}"
        shift
        ;;
      --steps)
        steps="${2:?--steps requires a number}"
        shift 2
        ;;
      --steps=*)
        steps="${1#*=}"
        shift
        ;;
      *)
        die "unknown rollback flag $1"
        ;;
    esac
  done

  [[ -n "$install_root" ]] || die "--install-root is required"
  [[ -d "$install_root" ]] || die "install root does not exist: $install_root"
  [[ ! -L "$install_root" ]] || die "install root must not be a symlink: $install_root"
  [[ "$steps" =~ ^[0-9]+$ ]] || die "--steps must be a non-negative integer"

  releases=()
  while IFS= read -r release; do
    [[ -n "$release" ]] || continue
    releases+=("$release")
  done < <(release_entries "$install_root")
  (( ${#releases[@]} > steps )) || die "no retained release available for rollback"
  local release_id release_path release_receipt
  local previous_current="" current_switched=0 release_created=0
  release_id="${releases[$steps]}"
  release_path="$(release_dir "$install_root" "$release_id")"
  release_receipt="$(release_receipt "$install_root" "$release_id")"
  if [[ -L "$(current_link "$install_root")" ]]; then
    previous_current="$(readlink "$(current_link "$install_root")")"
  fi
  [[ -f "$release_receipt" ]] || die "release metadata missing for $release_id"
  ensure_safe_release_view "$install_root" "$bin_dir" "$release_id" "${BINARIES[@]}"
  validate_release_files "$release_path" "$target"
  trap 'rollback_activation_view "$install_root" "$release_path" "$release_receipt" "$previous_current" "$current_switched" "$release_created" "$bin_dir"' ERR
  ensure_public_binary_links "$install_root" "$bin_dir"
  atomic_symlink "$release_path" "$(current_link "$install_root")"
  current_switched=1
  set +e
  maybe_fail_after_current
  local activation_status=$?
  set -e
  if (( activation_status != 0 )); then
    if [[ -n "$previous_current" ]]; then
      atomic_symlink "$previous_current" "$(current_link "$install_root")"
    else
      rm -f -- "$(current_link "$install_root")"
    fi
    if [[ -z "$previous_current" ]]; then
      local binary
      for binary in "${BINARIES[@]}"; do
        rm -f -- "$(bin_link_path "$bin_dir" "$binary")"
      done
    fi
    trap - ERR
    return 1
  fi

  # An operator rollback moves the same links an install moves, so it owes the
  # same proof: the release it just pointed `current` at has to be the one
  # serving, or the rollback is a claim about symlinks. A failed proof takes
  # the same ERR trap, which restores the release that WAS current and puts it
  # back in service.
  serve_restart_and_prove "$release_path"

  trap - ERR
  local housekeeping=0
  prune_releases "$install_root" "$MAX_RELEASES" || housekeeping=1
  jq -n -S \
    --arg installRoot "$install_root" \
    --arg releaseId "$release_id" \
    --arg releaseDir "$release_path" \
    --arg current "$(current_link "$install_root")" \
    --arg binDir "$bin_dir" \
    --arg target "$target" \
    --argjson serve "$SERVE_RESTART_JSON" \
    '{installRoot:$installRoot, releaseId:$releaseId, releaseDir:$releaseDir, current:$current, binDir:$binDir, target:$target, serve:$serve}'
  if (( housekeeping )); then
    printf 'hig-release: %s is rolled back to and proved to be serving; pruning older releases under %s failed and needs a look by hand - do NOT undo this rollback on account of it\n' "$release_path" "$install_root" >&2
    return 1
  fi
}

install_package() {
  local target="$1"
  shift
  require_host hax
  case "$target" in
    hax)
      install_local "$target" "$@"
      ;;
    hig)
      install_remote "$target" "$@"
      ;;
  esac
}

main() {
  [[ $# -ge 1 ]] || usage
  local command="$1"
  shift
  [[ $# -ge 1 ]] || usage
  local target="$1"
  shift
  validate_target "$target"

  case "$command" in
    package)
      package_create "$target" "$@"
      ;;
    install)
      install_package "$target" "$@"
      ;;
    rollback)
      rollback_release "$target" "$@"
      ;;
    *)
      usage
      ;;
  esac
}

main "$@"
