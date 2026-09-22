#!/usr/bin/env bash
# Test for scripts/migrate-acc-body-blocks.sh (ACC-16).
#
#   ./scripts/migrate-acc-body-blocks.test.sh
#
# Builds the binary once, raises a fixture board through the CLI carrying one
# of each row shape, runs the migration twice, and proves: a valid legacy
# block becomes the native field with the block stripped and the remainder
# byte-for-byte; a row with no block, an already-native row and a resolved row
# are untouched; invalid prose and binary-refused blocks are reported for hand
# migration with the row unchanged; the second run migrates nothing.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

cargo build --locked --quiet

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
export KANBAN_DATA_DIR="$WORK/data"

pass=0
fail=0
export KB_BIN="$here/target/debug/kanban"
KB="$KB_BIN"
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

"$KB" init --name MIG --json >"$WORK/init.json"
BOARD="$(jq -r '.boardPath' "$WORK/init.json")"

raise() { # key, extra... — body comes from $WORK/body-<key>.txt; prints the row id
  local key="$1"; shift
  "$KB" --db "$BOARD" attention raise "$(cat "$WORK/body-$key.txt")" --as tester "$@" --json \
    | jq -r '.id'
}

cat >"$WORK/body-a.txt" <<'EOF'
ACC: Where is the check answer recorded?
a) In the Store transaction
b) In the browser bundle
answer: a
explain: The Store writes answered, correct and answeredAt in one transaction.
This row decides the rollout order.
EOF
cat >"$WORK/body-b.txt" <<'EOF'
Ship the pin on Friday after the review lands.
EOF
cat >"$WORK/body-c.txt" <<'EOF'
ACC: stale prose that must stay exactly as written.
EOF
cat >"$WORK/body-d.txt" <<'EOF'
ACC: A block with no choices at all
answer: a
explain: Nothing declares a, so this block cannot become a check.
Trailing prose stays put.
EOF
cat >"$WORK/body-e.txt" <<'EOF'
ACC: Where the answer lands
a) Store
b) Browser
answer: a
explain: The Store records the triple in one write inside the component store.
Trailing prose stays put.
EOF
cat >"$WORK/body-f.txt" <<'EOF'
A verdict-first body with no legacy block; it resolves normally.
EOF

A="$(raise a)"
B="$(raise b)"
C="$(raise c \
  --check "Which layer owns the invariant?" \
  --check-choice "store=The Store enforces it" \
  --check-choice "client=The client enforces it" \
  --check-answer store \
  --check-explain "The Store is the trusted layer for this invariant." \
  --check-about "rust/store.rs")"
D="$(raise d)"
E="$(raise e)"
F="$(raise f)"
"$KB" --db "$BOARD" attention resolve "$F" --as tester --choice approve \
  --note "resolved for the migration fixture" --json >"$WORK/resolve-f.json"

for id in "$A" "$B" "$C" "$D" "$E" "$F"; do
  "$KB" --db "$BOARD" attention show "$id" --json | jq -r '.body' >"$WORK/before-$id.txt"
done

set +e
run_out="$WORK/run1.txt"
bash scripts/migrate-acc-body-blocks.sh --db "$BOARD" --about "rust/store.rs" "$F" >"$run_out" 2>"$WORK/run1.err"
code_explicit=$?
bash scripts/migrate-acc-body-blocks.sh --db "$BOARD" --about "rust/store.rs" >"$WORK/run-all.txt" 2>"$WORK/run-all.err"
code_all=$?
set -e

# Explicit IDs: resolved row skipped, nothing migrated.
assert_eq "explicit run exits 0 (skip only)" "0" "$code_explicit"
assert "explicit run skips resolved row" grep -q "skip $F status is resolved" "$run_out"

# Full run: D and E need hand migration, so exit 1.
assert_eq "full run exits 1 (two rows need hand work)" "1" "$code_all"
assert "receipt migrates A" grep -q "^migrated $A choices=2 about=rust/store.rs$" "$WORK/run-all.txt"
assert "receipt skips B (no block)" grep -q "^skip $B body does not start with an ACC: block$" "$WORK/run-all.txt"
assert "receipt skips C (already native)" grep -q "^skip $C already carries a native check$" "$WORK/run-all.txt"
assert "receipt hands D (invalid prose)" grep -q "^needs-hand-migration $D fewer than two choice lines" "$WORK/run-all.txt"
assert "receipt hands E (binary refusal)" grep -q "^needs-hand-migration $E update refused:" "$WORK/run-all.txt"

show() { "$KB" --db "$BOARD" attention show "$1" --json; }

# Row A: native field set, block stripped, remainder byte-for-byte.
assert_eq "A question" "Where is the check answer recorded?" "$(show "$A" | jq -r '.check.question')"
assert_eq "A about" "rust/store.rs" "$(show "$A" | jq -r '.check.about')"
assert_eq "A choices" '["a","b"]' "$(show "$A" | jq -c '[.check.choices[].key]')"
assert_eq "A choice label" "In the Store transaction" "$(show "$A" | jq -r '.check.choices[0].label')"
assert_eq "A answer withheld pre-answer" "false" "$(show "$A" | jq -r '.check | has("answer")')"
assert_eq "A explanation withheld pre-answer" "false" "$(show "$A" | jq -r '.check | has("explanation")')"
assert_eq "A body is the remainder" "This row decides the rollout order." "$(show "$A" | jq -r '.body')"

# Rows B..F: bodies byte-identical, and only A and C carry a check.
for id in "$B" "$C" "$D" "$E" "$F"; do
  show "$id" | jq -r '.body' >"$WORK/after-$id.txt"
  assert "$id body unchanged" cmp -s "$WORK/before-$id.txt" "$WORK/after-$id.txt"
done
assert_eq "B has no check" "false" "$(show "$B" | jq -r 'has("check") and .check != null')"
assert_eq "D has no check" "false" "$(show "$D" | jq -r 'has("check") and .check != null')"
assert_eq "E has no check" "false" "$(show "$E" | jq -r 'has("check") and .check != null')"
assert_eq "C keeps its authored question" "Which layer owns the invariant?" "$(show "$C" | jq -r '.check.question')"

# Idempotency: a second full run migrates nothing.
set +e
bash scripts/migrate-acc-body-blocks.sh --db "$BOARD" --about "rust/store.rs" >"$WORK/run2.txt" 2>"$WORK/run2.err"
code2=$?
set -e
assert_eq "second run still exits 1 (d/e still need hands)" "1" "$code2"
assert "second run skips A as already native" grep -q "^skip $A already carries a native check$" "$WORK/run2.txt"
assert "second run migrates nothing" bash -c "! grep -q '^migrated' '$WORK/run2.txt'"
show "$A" | jq -r '.body' >"$WORK/after2-a.txt"
assert "A body stable across re-run" cmp -s "$WORK/after2-a.txt" <(printf 'This row decides the rollout order.\n')

printf 'migrate-acc-body-blocks.test: %d passed, %d failed\n' "$pass" "$fail" >&2
((fail == 0))
