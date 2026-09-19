#!/usr/bin/env bash
# Prove that the COMMITTED bundle under `web/dist` is the one this source
# tree builds.
#
# `web/dist` is committed (see `web/README.md`): the release path is
# cargo-only and offline, so the bytes the Rust build embeds arrive in the
# repository rather than out of a bundler nobody runs on the serving hosts.
# A committed artefact is only as trustworthy as the check that it is still
# the artefact its source produces, and that check is this script: install
# the pinned toolchain from the lockfile, rebuild into a temporary
# directory, and `cmp` every file both ways. A rebuild that differs by one
# byte fails here, on the gate host, rather than being noticed never.
set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
committed="$here/dist"
rebuilt="$(mktemp -d "${TMPDIR:-/tmp}/kanban-web-dist.XXXXXX")"
trap 'rm -rf "$rebuilt"' EXIT

[[ -d "$committed" ]] || {
  printf 'web/check-reproducible.sh: %s does not exist; run `bun run build`\n' "$committed" >&2
  exit 1
}

cd "$here"
bun install --frozen-lockfile >/dev/null
KANBAN_WEB_DIST="$rebuilt" bun run build.mjs >/dev/null

# The file SET first: a rebuild that dropped an asset, or a committed tree
# carrying one the build no longer produces, is a mismatch even when every
# shared file compares equal.
committed_names="$(cd "$committed" && ls -A | sort)"
rebuilt_names="$(cd "$rebuilt" && ls -A | sort)"
if [[ "$committed_names" != "$rebuilt_names" ]]; then
  printf 'web/check-reproducible.sh: the rebuilt bundle is a different set of files\n' >&2
  diff <(printf '%s\n' "$committed_names") <(printf '%s\n' "$rebuilt_names") >&2 || true
  exit 1
fi

status=0
while IFS= read -r name; do
  [[ -n "$name" ]] || continue
  if cmp -s "$committed/$name" "$rebuilt/$name"; then
    printf 'web/dist/%s identical\n' "$name"
  else
    printf 'web/check-reproducible.sh: web/dist/%s differs from the rebuild\n' "$name" >&2
    status=1
  fi
done <<< "$committed_names"

if (( status != 0 )); then
  printf 'web/check-reproducible.sh: the committed bundle is not what this tree builds; run `bun run build` and commit the result\n' >&2
  exit 1
fi
printf 'web/check-reproducible.sh: the committed bundle is byte-for-byte reproducible\n'
