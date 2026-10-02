#!/usr/bin/env bash
# Test for scripts/container-gate.sh's host-wide limiter lookup.
#
#   ./scripts/container-gate.test.sh
#
# Runs no container: `docker` is a stub on PATH that records its argv, and the
# limiter is a stub gate-slot that records its argv and runs the command after
# `--`. Proves: a valid GATE_SLOT wraps the container run as
# `run --name NAME -- docker run ...`; a GATE_SLOT that is not an executable
# file is refused before docker is ever called; with GATE_SLOT unset and no
# medic checkout, the run proceeds unlimited with the existing warning; and a
# parent slot (MEDIC_GATE_HELD) with no limiter found is run inside, not
# reported as unlimited.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
WORK="$(cd "$WORK" && pwd -P)"

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

# Stubs. Each call appends one record: its argv joined by spaces, then a
# record separator line, so a multi-line inner script stays one record.
mkdir -p "$WORK/bin"
cat >"$WORK/bin/docker" <<'EOF'
#!/usr/bin/env bash
{ printf '%s ' "$@"; printf '\n--end--\n'; } >>"$STUB_LOG/docker.log"
case "$1 ${2:-}" in
  "image inspect")
    [[ "$3" != --format ]] || printf 'sha256:stub-image\n'
    exit 0
    ;;
  "run "*)
    printf 'stub container ran\n'
    exit 0
    ;;
esac
exit 0
EOF
cat >"$WORK/bin/gate-slot" <<'EOF'
#!/usr/bin/env bash
{ printf '%s ' "$@"; printf '\n--end--\n'; } >>"$STUB_LOG/gate-slot.log"
while (($#)) && [[ "$1" != -- ]]; do shift; done
shift
exec "$@"
EOF
chmod +x "$WORK/bin/docker" "$WORK/bin/gate-slot"
printf '#!/bin/sh\nexit 0\n' >"$WORK/not-executable"

# A clean candidate checkout: the script refuses anything else.
tree="$WORK/tree"
mkdir -p "$tree/scripts" "$tree/skills/kb"
printf '#!/usr/bin/env bash\n' >"$tree/scripts/release-gate.sh"
printf 'fixture\n' >"$tree/skills/kb/SKILL.md"
git -C "$tree" init -q
git -C "$tree" add -A
git -C "$tree" -c user.name=fixture -c user.email=fixture@example.invalid \
  commit -q -m fixture

# HOME with ~/.agents/skills as a real directory, not a link into a dotfiles
# checkout: the default lookup finds no medic there.
home="$WORK/home"
mkdir -p "$home/.agents/skills"

# run CASE [VAR=VALUE...]: one container-gate.sh run in a scrubbed environment;
# stdout, stderr and exit status land in $WORK/CASE.{out,err,code}.
run_case() {
  local case="$1"; shift
  mkdir -p "$WORK/$case"
  set +e
  env -i PATH="$WORK/bin:/usr/local/bin:/usr/bin:/bin" HOME="$home" \
    STUB_LOG="$WORK/$case" KANBAN_GATE_STATE="$WORK/$case/state" \
    KANBAN_GATE_IMAGE=kanban-gate:stub "$@" \
    bash scripts/container-gate.sh --name "gate-$case" "$tree" \
    >"$WORK/$case.out" 2>"$WORK/$case.err"
  echo "$?" >"$WORK/$case.code"
  set -e
}

# GATE_SLOT valid: the container run is wrapped by the limiter.
run_case valid GATE_SLOT="$WORK/bin/gate-slot"
assert_eq "valid: exits with the container's status" "0" "$(cat "$WORK/valid.code")"
assert_eq "valid: gate-slot called once" "1" \
  "$(grep -c '^--end--$' "$WORK/valid/gate-slot.log" 2>/dev/null)"
assert "valid: gate-slot argv is run --name NAME -- docker run" \
  grep -q '^run --name gate-valid -- docker run --rm --name gate-valid --init ' \
  "$WORK/valid/gate-slot.log"
assert "valid: docker run reached through the limiter" \
  grep -q '^run --rm --name gate-valid --init ' "$WORK/valid/docker.log"
assert "valid: container output reached stdout" grep -qx 'stub container ran' "$WORK/valid.out"
assert "valid: no unlimited warning" bash -c "! grep -q 'running unlimited' '$WORK/valid.err'"

# GATE_SLOT set to a missing path, a non-executable file, a directory, or
# empty: refused, and docker is never called.
for bad in missing noexec dir empty; do
  case "$bad" in
    missing) value="$WORK/no/such/gate-slot" ;;
    noexec) value="$WORK/not-executable" ;;
    dir) value="$WORK/bin" ;;
    empty) value="" ;;
  esac
  run_case "bad-$bad" GATE_SLOT="$value"
  assert_eq "bad $bad: refused with exit 64" "64" "$(cat "$WORK/bad-$bad.code")"
  assert "bad $bad: says why" grep -qF \
    "container-gate: GATE_SLOT is set but is not an executable file: '$value'; refusing to run unlimited" \
    "$WORK/bad-$bad.err"
  assert "bad $bad: docker never called" test ! -e "$WORK/bad-$bad/docker.log"
  assert "bad $bad: gate-slot never called" test ! -e "$WORK/bad-$bad/gate-slot.log"
done

# GATE_SLOT unset, no medic: the existing warning, then an unlimited run.
run_case unset
assert_eq "unset: exits with the container's status" "0" "$(cat "$WORK/unset.code")"
assert "unset: existing warning unchanged" grep -qx \
  'container-gate: warning: gate-slot not found (medic absent); running unlimited' \
  "$WORK/unset.err"
assert "unset: docker run called directly" \
  grep -q '^run --rm --name gate-unset --init ' "$WORK/unset/docker.log"
assert "unset: gate-slot never called" test ! -e "$WORK/unset/gate-slot.log"

# GATE_SLOT unset, no medic, but a parent holds a slot: run inside it.
run_case held MEDIC_GATE_HELD='/gates|0|1'
assert_eq "held: exits with the container's status" "0" "$(cat "$WORK/held.code")"
assert "held: names the parent slot" grep -qx \
  'container-gate: gate-slot not found; running inside the parent slot held by MEDIC_GATE_HELD=/gates|0|1' \
  "$WORK/held.err"
assert "held: not reported as unlimited" bash -c "! grep -q 'running unlimited' '$WORK/held.err'"
assert "held: docker run called directly" \
  grep -q '^run --rm --name gate-held --init ' "$WORK/held/docker.log"

# GATE_SLOT valid with a parent slot: still wrapped; gate-slot verifies it.
run_case held-valid GATE_SLOT="$WORK/bin/gate-slot" MEDIC_GATE_HELD='/gates|0|1'
assert "held+valid: still wrapped by the limiter" \
  grep -q '^run --name gate-held-valid -- docker run ' "$WORK/held-valid/gate-slot.log"

printf 'container-gate.test: %d passed, %d failed\n' "$pass" "$fail" >&2
((fail == 0))
