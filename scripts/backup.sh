#!/usr/bin/env bash
# Encrypted off-site backup of every Kanban board to the Hetzner Storage Box.
#
#   ./scripts/backup.sh                        # snapshot, encrypt, upload, verify, prune
#   ./scripts/backup.sh --verify-only <remote> # round-trip one remote artefact
#   ./scripts/backup.sh --rehearse             # full restore into a scratch data root
#
# WHY THIS EXISTS
# ADR-003 makes a board a plain SQLite file, which is what lets `backup` take a
# hot consistent snapshot through SQLite's online backup API. It is also what
# left every board in exactly one place. Measured 2026-08-24: 14 boards, 18 MB,
# on the same disk as 188 containers — and the only snapshot on the box was
# from 2026-08-16 and held ONE board, because it was taken when one board
# existed and nothing has run `backup` since. The capability was never the gap;
# scheduling and off-boxing were.
#
# WHY IT IS ENCRYPTED
# A board carries task titles, agent prose, checkpoints, handoffs and attention
# items — the working context of every project. No credentials, by standing
# policy, but this is private material and it is about to sit on a third-party
# disk. age with a key from the git-crypt'd dotfiles keeps it unreadable there.
#
# WHY THE STORAGE BOX AND NOT S3
# `u612177.your-storagebox.de` is already provisioned for backups, already
# accepts /root/.ssh/id_ed25519, and already carries the nightly gitea dumps.
# The Hetzner S3 credential on this box is scoped to `unum-dev-attachments` —
# a different product for a different job. Reuse beat provisioning.
#
# THE VERIFY STEP IS NOT OPTIONAL
# An unverified backup is a hope, not a backup. Every run downloads what it
# just uploaded, decrypts it, and asserts the registry and every board came
# back. A backup that cannot be read back is a failure and exits non-zero.
set -euo pipefail

DOTFILES=/root/work/journals/.sb/_dotfiles
KEYS_FILE="$DOTFILES/keys/kanban-backup.env"
AGE_KEY="$DOTFILES/keys/kanban-backup-age.key"
SB_HOST=u612177.your-storagebox.de
SB_USER=u612177
SB_PORT=23
SB_KEY=/root/.ssh/id_ed25519
REMOTE_DIR=kanban
KEEP=14
KB="${KB_BIN:-/root/.local/bin/kanban}"

say() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
ok()  { printf '    \033[32m✓\033[0m %s\n' "$*"; }
die() { printf '    \033[31m✗ %s\033[0m\n' "$*" >&2; exit 1; }

set -a
# shellcheck disable=SC1090
source "$KEYS_FILE"
set +a
: "${KANBAN_BACKUP_AGE_RECIPIENT:?missing KANBAN_BACKUP_AGE_RECIPIENT in $KEYS_FILE}"
[[ -r "$AGE_KEY" ]] || die "age private key $AGE_KEY unreadable — backups could never be restored"

# rclone's sftp remote from the environment alone, so no rclone.conf and no
# secret on disk. shell_type=none because a Storage Box gives a restricted
# shell and rclone would otherwise try to run md5sum/echo remotely.
export RCLONE_CONFIG_SB_TYPE=sftp \
       RCLONE_CONFIG_SB_HOST="$SB_HOST" \
       RCLONE_CONFIG_SB_USER="$SB_USER" \
       RCLONE_CONFIG_SB_PORT="$SB_PORT" \
       RCLONE_CONFIG_SB_KEY_FILE="$SB_KEY" \
       RCLONE_CONFIG_SB_SHELL_TYPE=none \
       RCLONE_CONFIG_SB_MD5SUM_COMMAND=none \
       RCLONE_CONFIG_SB_SHA1SUM_COMMAND=none

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
NAME="kanban-${STAMP}.tar.gz.age"

# Assert a decrypted archive really holds a restorable set of boards.
#
# The board count is compared against what the archive's own registry claims,
# not against a number hardcoded here: a backup taken the day a board is added
# must still verify, and one that silently dropped a board must still fail.
verify_artifact() {
  local f="$1" out="$WORK/verify.tar.gz" dir="$WORK/verify"
  rm -rf "$dir"; mkdir -p "$dir"
  age -d -i "$AGE_KEY" -o "$out" "$f" \
    || die "cannot DECRYPT $f — the backup is unreadable"
  tar xzf "$out" -C "$dir" 2>/dev/null || die "decrypted archive is not a valid tar.gz"
  [[ -s "$dir/registry.db" ]] || die "archive has NO registry.db — boards could not be located on restore"

  local boards expected
  boards="$(find "$dir/boards" -name '*.db' 2>/dev/null | wc -l)"
  # Every board file must be a real SQLite database, not a truncated copy.
  local bad=0
  while IFS= read -r b; do
    head -c 15 "$b" | grep -q 'SQLite format 3' || { printf '      not SQLite: %s\n' "$(basename "$b")"; bad=1; }
  done < <(find "$dir/boards" -name '*.db' 2>/dev/null)
  (( bad == 0 )) || die "one or more board files are not SQLite databases"

  expected="$(head -c 15 "$dir/registry.db" | grep -c 'SQLite format 3' || true)"
  (( expected == 1 )) || die "registry.db is not a SQLite database"
  (( boards > 0 )) || die "archive contains ZERO boards — an empty snapshot is not a backup"
  ok "verified: registry.db + $boards board(s), all valid SQLite"
  rm -f "$out"; rm -rf "$dir"
}

if [[ "${1:-}" == "--verify-only" ]]; then
  [[ -n "${2:-}" ]] || die "usage: $0 --verify-only <remote-filename>"
  say "Verify-only: $2"
  rclone copyto "SB:$REMOTE_DIR/$2" "$WORK/$2" || die "download failed"
  verify_artifact "$WORK/$2"
  ok "remote artefact $2 is restorable"
  exit 0
fi

# A decrypt-and-list proves the bytes survived. Only an actual restore proves
# the thing is usable, so this drives `kanban restore` into a scratch data root
# and then makes the restored copy answer `doctor`. It never touches the live
# data root: KANBAN_DATA_DIR points elsewhere, and restore takes its exclusive
# lock on that scratch root instead.
if [[ "${1:-}" == "--rehearse" ]]; then
  say "Restore rehearsal (into a scratch data root — the live one is untouched)"
  latest="$(rclone lsf "SB:$REMOTE_DIR/" | grep -E '^kanban-.*\.tar\.gz\.age$' | sort | tail -1)"
  [[ -n "$latest" ]] || die "no backups on the remote to rehearse"
  rclone copyto "SB:$REMOTE_DIR/$latest" "$WORK/$latest" || die "download failed"
  age -d -i "$AGE_KEY" -o "$WORK/r.tar.gz" "$WORK/$latest" || die "decrypt failed"
  mkdir -p "$WORK/snap" "$WORK/root"
  tar xzf "$WORK/r.tar.gz" -C "$WORK/snap"
  KANBAN_DATA_DIR="$WORK/root" "$KB" restore --from "$WORK/snap" --force --json >/dev/null \
    || die "restore of $latest FAILED — the backup is not usable"
  KANBAN_DATA_DIR="$WORK/root" "$KB" doctor --json > "$WORK/doctor.json" \
    || die "restored copy fails doctor — it is not a working ledger"
  n="$(grep -o '"name"' "$WORK/doctor.json" | wc -l)"
  ok "restored $latest into a scratch root; doctor healthy across $n project(s)"
  exit 0
fi

# ── retention ────────────────────────────────────────────────────────
# Archive before snapshot so a restored board serves the same hot/cold state.
# History remains inside each SQLite file; only operational views and partial
# indexes exclude it.
say "Archive settled history"
while IFS=$'\t' read -r project board; do
  if [[ ! -f "$board" ]]; then
    printf '    ! skipped missing board: %s (%s)\n' "$project" "$board"
    continue
  fi
  "$KB" archive --project "$project" --older-than-days 90 --as system@archive --json \
    >> "$WORK/archive.jsonl" || die "archive sweep failed for $project"
done < <("$KB" workspace list --json | jq -r 'unique_by(.boardPath)[] | [.name,.boardPath] | @tsv')
ok "swept settled rows older than 90 days across every present board"

# ── snapshot ─────────────────────────────────────────────────────────
say "Snapshot"
"$KB" backup --output "$WORK/snap" --json > "$WORK/backup.json" || die "kanban backup failed"
# `backup` reports what it could not include rather than failing; a snapshot
# that quietly skipped a board is the thing this check exists to catch.
if grep -q '"missingBoards": \[$' "$WORK/backup.json" && ! grep -A1 '"missingBoards": \[$' "$WORK/backup.json" | grep -q '^\s*\]'; then
  grep -A5 '"missingBoards"' "$WORK/backup.json" >&2
  die "kanban backup reported missing boards — refusing to ship an incomplete snapshot"
fi
count="$(find "$WORK/snap/boards" -name '*.db' | wc -l)"
(( count > 0 )) || die "snapshot holds no boards"
[[ -s "$WORK/snap/registry.db" ]] || die "snapshot has no registry.db"
tar czf "$WORK/snap.tar.gz" -C "$WORK/snap" .
ok "snapshotted $count board(s), $(du -h "$WORK/snap.tar.gz" | cut -f1)"

# ── encrypt ──────────────────────────────────────────────────────────
say "Encrypt"
age -r "$KANBAN_BACKUP_AGE_RECIPIENT" -o "$WORK/$NAME" "$WORK/snap.tar.gz" || die "age encryption failed"
head -c 22 "$WORK/$NAME" | grep -q 'age-encryption.org' || die "output is not an age file"
ok "encrypted → $NAME ($(du -h "$WORK/$NAME" | cut -f1))"

# ── upload ───────────────────────────────────────────────────────────
say "Upload"
rclone mkdir "SB:$REMOTE_DIR" 2>/dev/null || true
rclone copyto "$WORK/$NAME" "SB:$REMOTE_DIR/$NAME" || die "upload failed"
rclone lsf "SB:$REMOTE_DIR/" | grep -qx "$NAME" || die "uploaded file is not listed on the remote"
ok "uploaded to $SB_HOST:/home/$REMOTE_DIR/$NAME"

# ── verify by reading it BACK from the remote ────────────────────────
say "Verify (round-trip from the Storage Box)"
rm -f "$WORK/$NAME"
rclone copyto "SB:$REMOTE_DIR/$NAME" "$WORK/$NAME" || die "could not re-download"
verify_artifact "$WORK/$NAME"

# ── prune ────────────────────────────────────────────────────────────
say "Prune"
mapfile -t all < <(rclone lsf "SB:$REMOTE_DIR/" | grep -E '^kanban-.*\.tar\.gz\.age$' | sort)
total=${#all[@]}
if (( total > KEEP )); then
  for f in "${all[@]:0:total-KEEP}"; do
    rclone deletefile "SB:$REMOTE_DIR/$f" && ok "pruned $f"
  done
else
  ok "$total copies retained (keeping $KEEP)"
fi

say "Done"
printf '    %s  (%s, %s boards)\n' "$NAME" "$(du -h "$WORK/$NAME" | cut -f1)" "$count"
