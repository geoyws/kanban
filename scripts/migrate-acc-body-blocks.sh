#!/usr/bin/env bash
# One-shot migration: legacy `ACC:` body blocks become native check fields.
#
#   ./scripts/migrate-acc-body-blocks.sh --db BOARD.db --about SUBJECT [--limit N] [ID...]
#
# ACC-16 (`docs/specs/acc.md`) advances the board schema at the ADR-045 version
# boundary and converts an open row whose body starts with a valid legacy `ACC:`
# block into the five native check inputs once per board. This script is that
# conversion. It runs on hax after release, once per board file; the board file
# is found through `kanban workspace list --json` (`[].boardPath`).
#
# WHY THIS EXISTS
# Before the native check, the clerk synthesis lived in prose: a block at the
# head of the row body carrying the question, lettered choices, an `answer:`
# line and an `explain:` line. No script ever parsed it — the digest showed
# HEAD: and the clerk read it by eye — so there is no exact grammar to match,
# only that prose contract. The readers are cut over to the native field and
# the body-block path is retired, which would strand those rows as drafting
# debt. This script parses the block leniently and writes it through the
# compiled binary's own check flags, so the Store's validation (bounds, the
# `?` shape, the three `about` shapes, the diagnosis markers, raiser-only
# authoring) is the authority, not a second parser kept in step by hand.
#
# LEGACY GRAMMAR (lenient; anything else is hand migration, never a guess)
# The body must START with the block — its first line reads `ACC:` followed by
# the question, or a bare `ACC:` with the question on the next non-blank line.
# Then, in order, blank lines allowed between parts:
#   1. two through four choice lines:  `a) Label` (one letter or digit; the
#      separator may be `)`, `.` or `:`; a multi-character key starts no legacy
#      block and is hand migration, which keeps `answer:` and `explain:` lines
#      from ever parsing as choices)
# The block ends at the `explain:` line. Everything after it is the remainder
# and is written back byte-for-byte. Legacy blocks never carried `about:`
# (the subject is new with the native field), so the operator supplies one
# `--about` for the run and it is stamped on every migrated row and printed in
# every receipt. Rows that genuinely share a subject migrate together; a row
# whose subject differs wants its own run (pass its ID explicitly) or hand
# migration.
#
# BEHAVIOUR
# - Only open rows. Resolved rows are immutable history; an explicitly passed
#   non-open ID is skipped, never rewritten.
# - A row already carrying a native check is skipped: the field wins and the
#   body is left alone, which is what makes a re-run safe.
# - A row whose body does not start with `ACC:` is byte-for-byte unchanged.
# - A row whose block does not parse, whose remainder would empty the required
#   body, or whose update the binary refuses (bounds, markers, raiser rule) is
#   reported as needing hand migration and left untouched — the binary validates
#   before it writes, so a refusal changes nothing.
# - The block is STRIPPED from the body on migration. The retired reader would
#   otherwise duplicate the quiz under the digest's HEAD: and leak the stored
#   answer into clerk transcripts.
# - The update runs as the row's raiser (`--as <raisedBy`>), because only the
#   raiser may author a check — the operator has no exception (ACC-05).
#
# RECEIPTS (stdout, one per row) and SUMMARY (stderr):
#   migrated <id> choices=<n> about=<subject>
#   skip <id> <reason>
#   needs-hand-migration <id> <reason>
# Exit 0 when every row migrated or skipped cleanly; exit 1 when any row needs
# hand migration or a binary call failed; exit 2 on usage errors.
set -Eeuo pipefail
KB="${KB_BIN:-kanban}"

DB=""
ABOUT=""
LIMIT=1000
IDS=()

usage() {
  cat <<'EOF'
usage: migrate-acc-body-blocks.sh --db BOARD.db --about SUBJECT [--limit N] [ID...]

  --db PATH       board file to migrate (from `workspace list --json` boardPath)
  --about SUBJECT native check subject stamped on every migrated row:
                  a path with `/` or an extension, @@host/@_tier, UPPER_SNAKE or --flag
  --limit N       open rows to consider when no IDs are given (default 1000)
  ID...           restrict the run to these attention IDs
EOF
}

while (($# > 0)); do
  case "$1" in
    --db) DB="${2:-}"; shift 2 ;;
    --about) ABOUT="${2:-}"; shift 2 ;;
    --limit) LIMIT="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    --) shift; while (($# > 0)); do IDS+=("$1"); shift; done ;;
    -*) printf 'migrate-acc-body-blocks: unknown flag %s\n' "$1" >&2; usage >&2; exit 2 ;;
    *) IDS+=("$1"); shift ;;
  esac
done

[[ -n "$DB" ]] || { printf 'migrate-acc-body-blocks: --db is required\n' >&2; usage >&2; exit 2; }
[[ -n "$ABOUT" ]] || { printf 'migrate-acc-body-blocks: --about is required\n' >&2; usage >&2; exit 2; }
[[ "$LIMIT" =~ ^[0-9]+$ && "$LIMIT" -ge 1 ]] || { printf 'migrate-acc-body-blocks: --limit must be a positive integer\n' >&2; exit 2; }
command -v "$KB" >/dev/null 2>&1 || { printf 'migrate-acc-body-blocks: binary %s not found (set KB_BIN)\n' "$KB" >&2; exit 2; }
command -v jq >/dev/null 2>&1 || { printf 'migrate-acc-body-blocks: jq is required\n' >&2; exit 2; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

trim() {
  local s="$1"
  s="${s#"${s%%[![:space:]]*}"}"
  s="${s%"${s##*[![:space:]]}"}"
  printf '%s' "$s"
}

is_blank() { [[ "$1" =~ ^[[:space:]]*$ ]]; }

# parse_acc_block <body-file> <out-dir>
# exit 0 with question.txt/choices.txt/answer.txt/explain.txt/remainder.txt,
# 1 when the body starts with no ACC: block, 2 with parse-error.txt otherwise.
# Regexes live in variables: an unquoted `(` or `)` in `[[ =~ ]]` is shell
# syntax first and an ERE group second, so inline patterns misparse.
RE_ACC='^ACC:(.*)$'
RE_CHOICE='^([A-Za-z0-9])[).:][[:space:]]+(.+)$'
RE_ANSWER='^[Aa]nswer:[[:space:]]*([^[:space:]]+)[[:space:]]*$'
RE_EXPLAIN='^[Ee]xplain:[[:space:]]+(.+)$'

parse_acc_block() {
  local body="$1" out="$2"
  local -a lines
  mapfile -t lines < "$body"
  ((${#lines[@]} > 0)) || return 1
  [[ "${lines[0]}" =~ $RE_ACC ]] || return 1
  local question
  question="$(trim "${BASH_REMATCH[1]}")"
  local i=1
  if [[ -z "$question" ]]; then
    while ((i < ${#lines[@]})) && is_blank "${lines[i]}"; do ((i++)); done
    if ((i < ${#lines[@]})) \
      && [[ ! "${lines[i]}" =~ $RE_CHOICE ]] \
      && [[ ! "${lines[i]}" =~ $RE_ANSWER ]] \
      && [[ ! "${lines[i]}" =~ $RE_EXPLAIN ]]; then
      question="$(trim "${lines[i]}")"
      ((i++))
    else
      printf 'no question after the leading ACC: line' >"$out/parse-error.txt"
      return 2
    fi
  fi
  [[ -n "$question" ]] || { printf 'empty check question' >"$out/parse-error.txt"; return 2; }
  local -a keys=() labels=()
  while ((i < ${#lines[@]})); do
    is_blank "${lines[i]}" && { ((i++)); continue; }
    if [[ "${lines[i]}" =~ $RE_CHOICE ]]; then
      local label
      label="$(trim "${BASH_REMATCH[2]}")"
      [[ -n "$label" ]] || break
      keys+=("${BASH_REMATCH[1]}")
      labels+=("$label")
      ((i++))
    else
      break
    fi
  done
  if ((${#keys[@]} < 2)); then
    printf 'fewer than two choice lines after the question' >"$out/parse-error.txt"
    return 2
  fi
  if ((${#keys[@]} > 4)); then
    printf 'more than four choice lines; the native check carries 2-4' >"$out/parse-error.txt"
    return 2
  fi
  while ((i < ${#lines[@]})) && is_blank "${lines[i]}"; do ((i++)); done
  if ((i >= ${#lines[@]})) || [[ ! "${lines[i]}" =~ $RE_ANSWER ]]; then
    printf 'no answer: line naming a declared choice after the choices' >"$out/parse-error.txt"
    return 2
  fi
  local answer="${BASH_REMATCH[1]}"
  ((i++))
  local declared=0
  local key
  for key in "${keys[@]}"; do [[ "$key" == "$answer" ]] && declared=1; done
  if ((declared == 0)); then
    printf 'answer: %s names no declared choice' "$answer" >"$out/parse-error.txt"
    return 2
  fi
  while ((i < ${#lines[@]})) && is_blank "${lines[i]}"; do ((i++)); done
  if ((i >= ${#lines[@]})) || [[ ! "${lines[i]}" =~ $RE_EXPLAIN ]]; then
    printf 'no explain: line after the answer' >"$out/parse-error.txt"
    return 2
  fi
  local explain
  explain="$(trim "${BASH_REMATCH[1]}")"
  ((i++))
  [[ -n "$explain" ]] || { printf 'empty explanation' >"$out/parse-error.txt"; return 2; }
  local remainder_blank=1
  local j
  for ((j = i; j < ${#lines[@]}; j++)); do
    is_blank "${lines[j]}" || remainder_blank=0
  done
  if ((remainder_blank == 1)); then
    printf 'the body holds only the ACC: block; stripping it would empty the required body' >"$out/parse-error.txt"
    return 2
  fi
  printf '%s' "$question" >"$out/question.txt"
  printf '%s' "$answer" >"$out/answer.txt"
  printf '%s' "$explain" >"$out/explain.txt"
  : >"$out/choices.txt"
  for ((j = 0; j < ${#keys[@]}; j++)); do
    printf '%s=%s\n' "${keys[j]}" "${labels[j]}" >>"$out/choices.txt"
  done
  : >"$out/remainder.txt"
  local first=1
  for ((j = i; j < ${#lines[@]}; j++)); do
    if ((first == 1)); then first=0; else printf '\n' >>"$out/remainder.txt"; fi
    printf '%s' "${lines[j]}" >>"$out/remainder.txt"
  done
  return 0
}

if ((${#IDS[@]} == 0)); then
  mapfile -t IDS < <("$KB" --db "$DB" attention list --status open --limit "$LIMIT" --json | jq -r '.[].id')
  if ((${#IDS[@]} >= LIMIT)); then
    printf 'migrate-acc-body-blocks: warning: %s open rows hit --limit %s; the board may hold more — rerun with a larger --limit or explicit IDs\n' "${#IDS[@]}" "$LIMIT" >&2
  fi
fi

migrated=0
skipped=0
hand=0

if ((${#IDS[@]} == 0)); then
  printf 'migrate-acc-body-blocks: 0 migrated, 0 skipped, 0 need hand migration (board %s)\n' "$DB" >&2
  exit 0
fi

for id in "${IDS[@]}"; do
  row="$WORK/row.json"
  if ! "$KB" --db "$DB" attention show "$id" --json >"$row" 2>"$WORK/show.err"; then
    printf 'needs-hand-migration %s show failed: %s\n' "$id" "$(head -n 1 "$WORK/show.err")"
    hand=$((hand + 1))
    continue
  fi
  status="$(jq -r '.status' "$row")"
  if [[ "$status" != "open" ]]; then
    printf 'skip %s status is %s, only open rows migrate\n' "$id" "$status"
    skipped=$((skipped + 1))
    continue
  fi
  if [[ "$(jq -r 'has("check") and .check != null' "$row")" == "true" ]]; then
    printf 'skip %s already carries a native check\n' "$id"
    skipped=$((skipped + 1))
    continue
  fi
  raised_by="$(jq -r '.raisedBy' "$row")"
  jq -r '.body' "$row" >"$WORK/body.txt"
  out="$WORK/parse"

  rm -rf "$out"; mkdir -p "$out"
  code=0
  parse_acc_block "$WORK/body.txt" "$out" || code=$?
  if ((code == 1)); then
    printf 'skip %s body does not start with an ACC: block\n' "$id"
    skipped=$((skipped + 1))
    continue
  fi
  if ((code != 0)); then
    printf 'needs-hand-migration %s %s\n' "$id" "$(cat "$out/parse-error.txt")"
    hand=$((hand + 1))
    continue
  fi
  update_args=(attention update "$id" --as "$raised_by"
    --check "$(cat "$out/question.txt")"
    --check-answer "$(cat "$out/answer.txt")"
    --check-explain "$(cat "$out/explain.txt")"
    --check-about "$ABOUT"
    --body-file "$out/remainder.txt")
  while IFS= read -r choice; do
    update_args+=(--check-choice "$choice")
  done <"$out/choices.txt"
  if "$KB" --db "$DB" "${update_args[@]}" --json >"$WORK/update.json" 2>"$WORK/update.err"; then
    printf 'migrated %s choices=%s about=%s\n' "$id" "$(wc -l <"$out/choices.txt" | tr -d ' ')" "$ABOUT"
    migrated=$((migrated + 1))
  else
    printf 'needs-hand-migration %s update refused: %s\n' "$id" "$(head -n 1 "$WORK/update.err")"
    hand=$((hand + 1))
  fi
done

printf 'migrate-acc-body-blocks: %d migrated, %d skipped, %d need hand migration (board %s)\n' \
  "$migrated" "$skipped" "$hand" "$DB" >&2

((hand == 0))
