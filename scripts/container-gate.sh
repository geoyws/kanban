#!/usr/bin/env bash
# Run the release gate, or one test in a loop, inside Linux Docker on this
# machine, holding a host-wide gate slot for as long as the container runs.
#
#   scripts/container-gate.sh [--name NAME] WORKTREE
#   scripts/container-gate.sh [--name NAME] --loop TARGET TEST --iterations N WORKTREE
#
# WORKTREE is a clean checkout of the candidate. It is mounted read-only, so
# the run measures exactly the committed candidate and nothing can change it
# mid-run; a dirty tree is refused for the same reason.
#
# Default mode runs `scripts/release-gate.sh`, all of it. `--loop` runs one
# integration test N times, isolated and single-threaded, and reports the
# pass/fail count: it measures a flaky test, it is never gate evidence.
#
# Output goes to stdout and to $KANBAN_GATE_STATE/NAME.log. The exit status is
# the gate's (or, in loop mode, non-zero when any iteration failed).
#
# Why these container settings (each was measured, 2026-09-23/24, t-a3928b36):
#   - uid:gid of the caller, with a passwd entry baked into the image: the
#     authz suite resolves the caller through `id -un`, and the bind-mounted
#     cargo dirs stay owned by the host user.
#   - --security-opt seccomp=unconfined: Chromium's user-namespace sandbox
#     answers "No usable sandbox" under Docker's default profile. The browser
#     still runs WITH its sandbox, because tests disable it only for uid 0.
#   - --tmpfs .../web/node_modules with exec: the worktree is read-only and
#     `bun install` must write somewhere executable; Docker tmpfs is noexec by
#     default.
#   - --init: reaps the orphans the process-boundary tests leave behind.
#
# Host-wide limit: the run is wrapped in medic's `gate-slot` (medic ADR-004),
# so at most N heavy gates run on this host at once and a start beyond that
# waits for a slot. The container runs in the FOREGROUND inside the slot: a
# detached `docker run -d` would return at once and free the slot while the
# gate was still running. Without medic installed the run is unlimited and
# says so.
#
# Environment:
#   KANBAN_GATE_STATE  cache and log directory (default ~/.cache/kanban-gate)
#   KANBAN_GATE_IMAGE  image tag (default kanban-gate:1.95-chrome-u<uid>);
#                      built from scripts/container-gate.Dockerfile if absent
set -Eeuo pipefail

die() {
    printf 'container-gate: %s\n' "$*" >&2
    exit 64
}

name=""
loop_target=""
loop_test=""
iterations=""
worktree=""
while (($#)); do
    case "$1" in
        --name)
            name="${2:?--name requires a value}"
            shift 2
            ;;
        --loop)
            loop_target="${2:?--loop requires TARGET TEST}"
            loop_test="${3:?--loop requires TARGET TEST}"
            shift 3
            ;;
        --iterations)
            iterations="${2:?--iterations requires a count}"
            shift 2
            ;;
        -h | --help)
            sed -n '2,7p' "$0" >&2
            exit 64
            ;;
        -*)
            die "unknown flag $1"
            ;;
        *)
            [[ -z "$worktree" ]] || die "one WORKTREE only, got a second: $1"
            worktree="$1"
            shift
            ;;
    esac
done

[[ -n "$worktree" ]] || die "WORKTREE is required"
[[ -d "$worktree" ]] || die "not a directory: $worktree"
worktree="$(cd "$worktree" && pwd -P)"
if [[ -n "$loop_target" ]]; then
    [[ "$iterations" =~ ^[1-9][0-9]*$ ]] || die "--loop needs --iterations N (N >= 1)"
elif [[ -n "$iterations" ]]; then
    die "--iterations applies only to --loop"
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
top="$(git -C "$worktree" rev-parse --show-toplevel)"
[[ "$top" == "$worktree" ]] || die "WORKTREE must be a checkout root, not $worktree"
[[ -f "$worktree/scripts/release-gate.sh" ]] || die "no scripts/release-gate.sh in $worktree"
[[ -f "$worktree/skills/kb/SKILL.md" ]] ||
    die "skills/kb is not initialized in $worktree (git submodule update --init skills/kb)"
dirty="$(git -C "$worktree" status --porcelain)"
[[ -z "$dirty" ]] || die "WORKTREE is dirty; commit the candidate first:
$dirty"
candidate="$(git -C "$worktree" rev-parse HEAD)"

uid="$(id -u)"
gid="$(id -g)"
state="${KANBAN_GATE_STATE:-$HOME/.cache/kanban-gate}"
image="${KANBAN_GATE_IMAGE:-kanban-gate:1.95-chrome-u$uid}"
if [[ -z "$name" ]]; then
    mode=gate
    [[ -z "$loop_target" ]] || mode=loop
    name="kanban-${mode}-$(basename "$worktree")-${candidate:0:7}"
fi
[[ "$name" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || die "--name must be a container-safe name: $name"

mkdir -p "$state/cargo" "$state/target-$name" "$worktree/web/node_modules"
log="$state/$name.log"

if ! docker image inspect "$image" >/dev/null 2>&1; then
    printf 'container-gate: building %s\n' "$image" >&2
    docker build --build-arg "GATE_UID=$uid" --build-arg "GATE_GID=$gid" \
        -t "$image" -f "$here/container-gate.Dockerfile" "$here" >&2
fi
image_id="$(docker image inspect --format '{{.Id}}' "$image")"

# What runs inside, as the host uid. Arguments arrive positionally so nothing
# in them is re-parsed by a shell.
# shellcheck disable=SC2016 # expands inside the container, never here
inner='
set -Eeuo pipefail
uid="$1" gid="$2" loop_target="$3" loop_test="$4" iterations="$5"
as_gate() {
    setpriv --reuid "$uid" --regid "$gid" --clear-groups env \
        HOME=/tmp/gate-home PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin \
        CARGO_HOME=/gate-cargo CARGO_TARGET_DIR=/gate-target \
        RUSTUP_HOME=/usr/local/rustup KANBAN_CHROME=/usr/bin/chromium "$@"
}
mkdir -p /tmp/gate-home
chown "$uid:$gid" /tmp/gate-home
(cd /work/web && as_gate bun install)
# `bun install` runs first with the container log pipe as its stdout; if
# anything under it leaves O_NONBLOCK set, every later burst
# write to that pipe can fail with EAGAIN (see scripts/release-gate.sh).
# Heal the flag here so the loop iterations and their verdict echoes below
# never inherit a poisoned stream. Gate mode heals again itself: release-gate
# clears the flag at startup and gives each step a private pipe through `cat`.
# Double quotes (never single quotes or $ sigils): this whole block is a
# single-quoted string on the host passed to the container through
# `bash -c "$inner"`, so a $ here would be eaten by the container shell
# before perl ever saw it. The straight-line form has no variables at all.
if command -v perl >/dev/null 2>&1; then
    perl -MFcntl=F_GETFL,F_SETFL,O_NONBLOCK -e "fcntl(STDOUT, F_SETFL, fcntl(STDOUT, F_GETFL, 0) & ~O_NONBLOCK); fcntl(STDERR, F_SETFL, fcntl(STDERR, F_GETFL, 0) & ~O_NONBLOCK)" || true
fi
cd /work
if [[ -z "$loop_target" ]]; then
    as_gate bash scripts/release-gate.sh
    exit
fi
as_gate cargo test --locked --test "$loop_target" --no-run
# `--exact` with a name that matches nothing runs zero tests and exits 0, so a
# typo would count as a clean loop. Refuse up front, then count an iteration
# as passed only when exactly this one test ran and passed.
listed="$(as_gate cargo test --locked --test "$loop_target" -- --list --exact "$loop_test" 2>/dev/null |
    grep -c ": test$" || true)"
if [[ "$listed" != 1 ]]; then
    echo "container-gate: $loop_target has no test named exactly $loop_test" >&2
    exit 64
fi
pass=0 fail=0
for i in $(seq 1 "$iterations"); do
    if as_gate cargo test --locked --test "$loop_target" -- --exact "$loop_test" \
        --test-threads=1 >/tmp/iteration.log 2>&1 &&
        grep -q "test result: ok. 1 passed; 0 failed" /tmp/iteration.log; then
        pass=$((pass + 1)); echo "iteration $i PASS"
    else
        fail=$((fail + 1)); echo "iteration $i FAIL"
        grep -m1 -A2 "panicked at" /tmp/iteration.log || tail -5 /tmp/iteration.log
    fi
done
echo "container-gate: loop $loop_target::$loop_test pass=$pass fail=$fail n=$iterations"
((fail == 0))
'

run_container=(
    docker run --rm --name "$name"
    --init
    --security-opt seccomp=unconfined
    --mount "type=bind,src=$worktree,dst=/work,readonly"
    --mount "type=bind,src=$state/target-$name,dst=/gate-target"
    --mount "type=bind,src=$state/cargo,dst=/gate-cargo"
    --tmpfs "/work/web/node_modules:uid=$uid,gid=$gid,mode=0755,exec,size=2g"
    "$image"
    bash -c "$inner" inner "$uid" "$gid" "$loop_target" "$loop_test" "${iterations:-0}"
)

gate_slot="$(realpath ~/.agents/skills 2>/dev/null)/../../medic/skills/gate-slot/bin/gate-slot"
if [[ -x "$gate_slot" ]]; then
    run=("$gate_slot" run --name "$name" -- "${run_container[@]}")
else
    printf 'container-gate: warning: gate-slot not found (medic absent); running unlimited\n' >&2
    run=("${run_container[@]}")
fi

{
    printf 'container-gate: %s\n' "$name"
    printf 'container-gate: candidate %s\n' "$candidate"
    printf 'container-gate: worktree %s\n' "$worktree"
    printf 'container-gate: image %s %s\n' "$image" "$image_id"
    printf 'container-gate: started %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} | tee "$log"

set +e
"${run[@]}" 2>&1 | tee -a "$log"
status=${PIPESTATUS[0]}
set -e
printf 'container-gate: exit %s\n' "$status" | tee -a "$log"
exit "$status"
