#!/usr/bin/env bash
# The repo's gate: the one command that must be green before a release is
# packaged. There is no second list to keep in step with this one -- what the
# documentation calls the gate is this file, and
# `docs/testing/compiled-rust-e2e-matrix.md` names it rather than restating
# its steps.
#
# The order is cheapest-failure-first:
#
#   1. `cargo fmt --all -- --check` -- no compilation at all.
#   2. `cargo clippy --locked --all-targets -- -D warnings` -- one build of
#      everything, including the test targets, with warnings fatal. A
#      compile error in any integration target surfaces here rather than
#      forty minutes into the browser suite.
#   3. `cargo test --locked --lib` -- the in-process logic and the drift
#      guards, plus the two fixed-descriptor remap tests, which are
#      `#[ignore]`d and must be run serially and in isolation (they rebind
#      fixed file descriptors, so a parallel test in the same process can
#      lose its own).
#   4. `bash skills/kb/tests/kb-wrapper-tests.sh` -- the pinned public
#      package's own wrapper tests, run out of the `skills/kb` submodule
#      (ADR-036). The submodule is initialized path-specifically, never
#      recursively, because that is the only initialization AGENTS.md
#      admits and `rust/lib.rs` `include_str!`s `skills/kb/SKILL.md`.
#   5. Every integration target outside `e2e_areas`, one at a time, each
#      with `-- --test-threads=1`.
#   6. One build of the `e2e_areas` targets, then all of them side by side,
#      each still `-- --test-threads=1`, as the last step.
#
# Before step 1 the gate refuses to start if `tests/*.rs` and
# `integration_targets` disagree: Cargo builds every file in `tests/` as a
# target, so a file missing from the list is compiled by clippy but never
# run, and every green gate silently omits it (kb kanban t-4fd18062: it
# happened to `done_gate_e2e` and `secret_guard_e2e`).
#
# Why the integration targets are not run as one `cargo test --all-targets`:
# that runs each target's cases on every core at once, and the cases here
# spawn processes, hold locks and time out against wall clocks. Inside a
# target everything is single-threaded. Across targets, only the
# `e2e_*` areas run concurrently, and only with each other: they were cut
# from one serial `e2e` target (t-2aeec40c) that took 49-56 minutes on the
# test host and pushed the gate past its 60-minute container-to-verdict
# budget, every case in them owns its fixture under a temp root named by its
# own pid, and none drives a browser.
# Each area's output goes to its own file and is printed whole once all
# have exited. The answer to "too slow" is "make it faster" or split it this
# way. Sampling is not: there is no `--only`, no `--skip`, no quick mode,
# and nothing here reads an environment variable that turns a step off.
#
# Environment:
#   KANBAN_CHROME  optional absolute path to the Chrome/Chromium executable
#                  the browser tests should drive. It is the FIRST entry in
#                  the discovery order (`KANBAN_CHROME`, then platform/PATH
#                  system candidates, then the newest Playwright Chromium
#                  under `ms-playwright`), so it is how a host whose system
#                  Chrome is broken still runs the real-browser evidence.
#                  Exported into every step; a value naming a file that is
#                  not executable is refused below rather than silently
#                  falling through to a different browser.
#   CARGO, BUN     honoured through `PATH` as usual; nothing is vendored.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$here"

# A value that names nothing executable is an operator mistake, and the
# discovery order would quietly resolve to some other browser -- which is a
# green run measuring a browser nobody chose.
if [[ -n "${KANBAN_CHROME:-}" && ! ( -f "${KANBAN_CHROME}" && -x "${KANBAN_CHROME}" ) ]]; then
    printf 'release-gate: KANBAN_CHROME is set but not executable: %s\n' \
        "$KANBAN_CHROME" >&2
    exit 1
fi
export KANBAN_CHROME="${KANBAN_CHROME:-}"
[[ -n "$KANBAN_CHROME" ]] || unset KANBAN_CHROME

step=0
run() {
    local label="$1"
    shift
    step=$((step + 1))
    printf '\nrelease-gate: [%d] %s\n' "$step" "$label"
    if ! "$@"; then
        printf '\nrelease-gate: FAILED at step %d: %s\n' "$step" "$label" >&2
        printf 'release-gate: command was: %s\n' "$*" >&2
        exit 1
    fi
}

# Cheapest target first. Every integration target Cargo discovers — each
# `tests/NAME.rs` and each `tests/NAME/main.rs` — is listed in one of these
# two arrays and nothing else is; the check below holds them in
# step with `tests/`. `integration_targets` run one at a time; `e2e_areas`
# run last, side by side (see the header).
integration_targets=(
    claude_print_adapter_e2e
    codex_queue_adapter_e2e
    access_refusals_e2e
    opencode_adapter_e2e
    kimi_acp_adapter_e2e
    cursor_worker_adapter_e2e
    zcode_notify_adapter_e2e
    dispatcher_e2e
    codex_app_server_adapter_e2e
    secret_guard_e2e
    done_gate_e2e
    authz_bypass_matrix_e2e
    plugin_e2e
    cross_board_e2e
    identity_e2e
    worker_identity_e2e
    linked_e2e
    linked_evidence_e2e
)
e2e_areas=(
    e2e_core
    e2e_restore_watch
    e2e_mcp_batch_attention
    e2e_limits
    e2e_tags_workspace_rules
    e2e_release
    e2e_release_receipts
    e2e_lifecycle
)

listed=" ${integration_targets[*]} ${e2e_areas[*]} "
target_drift=0
for file in tests/*.rs tests/*/main.rs; do
    # An unmatched pattern stays literal; only real files are targets.
    [[ -f "$file" ]] || continue
    if [[ "$file" == */main.rs ]]; then
        name="$(basename "$(dirname "$file")")"
    else
        name="$(basename "$file" .rs)"
    fi
    if [[ "$listed" != *" $name "* ]]; then
        printf 'release-gate: %s is in neither integration_targets nor e2e_areas, so the gate would never run it\n' \
            "$file" >&2
        target_drift=1
    fi
done
for target in "${integration_targets[@]}" "${e2e_areas[@]}"; do
    if [[ ! -f "tests/$target.rs" && ! -f "tests/$target/main.rs" ]]; then
        printf 'release-gate: integration target %s has no tests/%s.rs or tests/%s/main.rs\n' \
            "$target" "$target" "$target" >&2
        target_drift=1
    fi
done
((target_drift == 0)) || exit 1

run 'cargo fmt' cargo fmt --all -- --check

run 'cargo clippy' \
    cargo clippy --locked --all-targets -- -D warnings

run 'cargo test --lib' cargo test --locked --lib

# The two remap tests are ignored by default and are evidence only when they
# run alone; see `docs/testing/compiled-rust-e2e-matrix.md`.
run 'cargo test --lib (ignored fd-remap, serial)' \
    cargo test --locked --lib workspace_adopt_fd_remap_handles_ \
    -- --ignored --test-threads=1

if [[ ! -f skills/kb/SKILL.md ]]; then
    run 'init skills/kb submodule' \
        git submodule update --init skills/kb
fi
run 'kb skill wrapper tests' bash skills/kb/tests/kb-wrapper-tests.sh
run 'migrate ACC body blocks' bash scripts/migrate-acc-body-blocks.test.sh
run 'container gate limiter lookup' bash scripts/container-gate.test.sh

# The serial integration targets, in the order listed above.
for target in "${integration_targets[@]}"; do
    run "cargo test --test $target (serial)" \
        cargo test --locked --test "$target" -- --test-threads=1
done

# One build first, so the concurrent runs below only execute.
area_flags=()
for target in "${e2e_areas[@]}"; do
    area_flags+=(--test "$target")
done
run 'cargo test --no-run (e2e areas)' cargo test --locked --no-run "${area_flags[@]}"

step=$((step + 1))
printf '\nrelease-gate: [%d] cargo test, %d e2e areas side by side (each serial)\n' \
    "$step" "${#e2e_areas[@]}"
area_logs="$(mktemp -d)"
area_pids=()
for target in "${e2e_areas[@]}"; do
    cargo test --locked --test "$target" -- --test-threads=1 \
        >"$area_logs/$target.log" 2>&1 &
    area_pids+=("$!")
done
area_failed=()
for index in "${!e2e_areas[@]}"; do
    wait "${area_pids[$index]}" || area_failed+=("${e2e_areas[$index]}")
done
for target in "${e2e_areas[@]}"; do
    printf '\nrelease-gate: --- cargo test --test %s (serial) ---\n' "$target"
    cat "$area_logs/$target.log"
done
rm -rf "$area_logs"
if ((${#area_failed[@]})); then
    printf '\nrelease-gate: FAILED at step %d: e2e areas %s\n' "$step" "${area_failed[*]}" >&2
    exit 1
fi

printf '\nrelease-gate: green (%d steps)\n' "$step"
