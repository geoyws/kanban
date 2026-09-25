#!/usr/bin/env bash
# The repo's gate: the one command that must be green before a release is
# packaged. There is no second list to keep in step with this one -- what the
# documentation calls the gate is this file, and
# `docs/testing/compiled-rust-e2e-matrix.md` names it rather than restating
# its steps.
#
# The order is cheapest-failure-first, so a typo in the web tree is not paid
# for with a forty-minute browser suite:
#
#   1. `web/gate.sh` -- the bundle's own three steps (`tsc --noEmit`,
#      `biome check`, `web/check-reproducible.sh`). The Rust half below
#      embeds the committed `web/dist` bytes, so proving those bytes first
#      is the cheapest way to fail (ADR-048).
#   2. `cargo fmt --all -- --check` -- no compilation at all.
#   3. `cargo clippy --locked --all-targets -- -D warnings` -- one build of
#      everything, including the test targets, with warnings fatal. A
#      compile error in any integration target surfaces here rather than
#      forty minutes into the browser suite.
#   4. `cargo test --locked --lib` -- the in-process logic and the drift
#      guards, plus the two fixed-descriptor remap tests, which are
#      `#[ignore]`d and must be run serially and in isolation (they rebind
#      fixed file descriptors, so a parallel test in the same process can
#      lose its own).
#   5. `bash skills/kb/tests/kb-wrapper-tests.sh` -- the pinned public
#      package's own wrapper tests, run out of the `skills/kb` submodule
#      (ADR-036). The submodule is initialized path-specifically, never
#      recursively, because that is the only initialization AGENTS.md
#      admits and `rust/lib.rs` `include_str!`s `skills/kb/SKILL.md`.
#   6. Every integration target, one at a time, each with
#      `-- --test-threads=1`, ending with `e2e`.
#
# Why the integration targets are serialized rather than run as one
# `cargo test --all-targets`: `tests/e2e.rs` drives a REAL Chrome against a
# real compiled `kanban serve`. Two of those at once contend for the same
# browser cache, the same ephemeral ports and the whole machine's CPU, and
# what that produces is a flaky failure that reads like a product bug. So
# the browser suite never runs concurrently with another cargo command:
# one target at a time, single-threaded inside the target, `e2e` last.
# Serialized is the answer to "too slow", and so is "make it faster".
# Sampling is not: there is no `--only`, no `--skip`, no quick mode, and
# nothing here reads an environment variable that turns a step off.
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

# A child that marks our stdout/stderr non-blocking poisons every later
# writer to the same pipe: O_NONBLOCK lives on the open file description, so
# it survives the child that set it, and the next burst write to a momentarily
# full pipe fails with EAGAIN instead of blocking. Inside the Linux container
# our stdout is the one docker log pipe shared with `bun install` and every
# web tool that runs before cargo, and libtest's burst print of a failing
# test then dies as `error: io error when listing tests: Os { code: 11,
# kind: WouldBlock }` with no verdict line (gate-acc14-0d2f527, step 18).
# Clear the flag up front. Perl is Debian-essential and ships on macOS too;
# a missing perl skips the heal rather than failing the gate.
if command -v perl >/dev/null 2>&1; then
    perl -MFcntl=F_GETFL,F_SETFL,O_NONBLOCK -e '
for my $fh (*STDOUT, *STDERR) {
    if (defined(my $flags = fcntl($fh, F_GETFL, 0))) {
        fcntl($fh, F_SETFL, $flags & ~O_NONBLOCK);
    }
}' || true
fi

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
    # Each step writes through `cat`, so the step's stdout/stderr is a private
    # pipe of this step: a child that flips O_NONBLOCK can at worst poison
    # this step's pipe, never the shared log stream the later steps write to.
    # The step's own status still rules the gate (`pipefail` is set above, so
    # a failing step fails the pipeline; `cat` only forwards bytes).
    if ! "$@" 2>&1 | cat; then
        printf '\nrelease-gate: FAILED at step %d: %s\n' "$step" "$label" >&2
        printf 'release-gate: command was: %s\n' "$*" >&2
        exit 1
    fi
}

run 'web bundle gate' ./web/gate.sh

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

# Cheapest target first, `e2e` last: it is the long one and the only one
# that drives a browser.
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
    authz_bypass_matrix_e2e
    e2e
)
for target in "${integration_targets[@]}"; do
    run "cargo test --test $target (serial)" \
        cargo test --locked --test "$target" -- --test-threads=1
done

printf '\nrelease-gate: green (%d steps)\n' "$step"
