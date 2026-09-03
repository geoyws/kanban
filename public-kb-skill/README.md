# kb public package

This directory stages the public wrapper surface for a board-aware `kb`
installation.

## What is here

- `SKILL.md` documents the public skill surface.
- `scripts/kb-board` routes commands by board and preserves argv literally.
- `scripts/kb-host` routes registry commands by board home host and preserves
  argv literally.
- `scripts/denylist-check`, `scripts/leak-gate`, `scripts/commit-gate`, and
  `.githooks/pre-commit` enforce the publication hygiene gate.
- `scripts/install-hooks` and `scripts/check-hooks` manage the versioned hook
  path.
- `tests/kb-wrapper-tests.sh` exercises the wrapper and gate behavior.

## Routing model

The installed package expects either `KB_HOSTS_TABLE` or an adjacent
`hosts.tsv`. The table is consumer-owned and must define, in order:

1. board identifier
2. board home host
3. SSH target
4. expected remote hostname
5. absolute KB executable path

The parser rejects missing, duplicate, unknown, or malformed board mappings.
The KB executable path must be absolute. `kb-board` injects the project
selector for board-owned commands; `kb-host` preserves registry commands
without a board selector. Remote execution uses the board home host identity,
the SSH target, and the remote hostname check is fail-closed.

Example row copied into a TSV table:

```tsv
board_identifier	board_home_host	ssh_target	expected_remote_hostname	<absolute_kb_binary_path>
```

Example `kb-board` body-file invocation copied from the package docs:

```bash
scripts/kb-board board_identifier t new "Title" --body-file /tmp/plan.md --json
```

## Hygiene model

The leak gate requires an explicit denylist file path and `gitleaks`. The
commit gate requires `KB_DENYLIST_FILE` and delegates to the leak gate.
