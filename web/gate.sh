#!/usr/bin/env bash
# The bundle's gate: everything about `web/` that must be true before the
# Rust gate is worth running.
#
# Three steps and no more, in the order that fails cheapest first:
#
#   1. `tsc --noEmit` -- the type check IS the first half of the lint. The
#      application is TypeScript in `strict` mode with
#      `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`, so the
#      class of bug an ESLint type-aware ruleset is usually installed for is
#      already a compile error here.
#   2. `biome check` -- the second half: one pinned binary (no plugin tree,
#      no `eslint` + `@typescript-eslint` + parser + config packages), lint
#      and format in one pass, and fast enough that nobody is tempted to
#      skip it. Recorded in `web/README.md` as the deliberate choice.
#   3. `web/check-reproducible.sh` -- the committed `web/dist` is what this
#      source builds.
#
# The Rust half (`cargo fmt`, `cargo clippy`, `cargo test`) is NOT run from
# here: `scripts/release-gate.sh` is the repo's serialized gate command, it
# runs this script as its first and cheapest step, and this script stays
# runnable on its own as the web tree's own gate.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

printf 'web/gate.sh: typecheck\n'
bun run --silent typecheck

printf 'web/gate.sh: lint\n'
bun run --silent lint

printf 'web/gate.sh: reproducible bundle\n'
./check-reproducible.sh

printf 'web/gate.sh: green\n'
