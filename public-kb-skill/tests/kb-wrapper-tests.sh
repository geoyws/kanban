#!/bin/bash
set -euo pipefail

script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
package_dir=$(CDPATH= cd -- "$script_dir/.." && pwd)

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/public-kb-skill-tests.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

assert_contains() {
  local haystack=$1
  local needle=$2
  local label=$3
  case "$haystack" in
    *"$needle"*) ;;
    *) fail "$label: expected output to contain '$needle'; got: $haystack" ;;
  esac
}

assert_log_clean() {
  local file=$1
  local label=$2
  if [[ -e "$file" && -s "$file" ]]; then
    fail "$label: expected no stub call, but $file was populated"
  fi
}

assert_argv_file() {
  local file=$1
  shift
  local -a expected=("$@")
  local -a actual=()

  while IFS= read -r -d '' item; do
    actual+=("$item")
  done <"$file"

  if [[ "${#actual[@]}" -ne "${#expected[@]}" ]]; then
    fail "$file: expected ${#expected[@]} argv items, got ${#actual[@]}"
  fi

  local i
  for i in "${!expected[@]}"; do
    if [[ "${actual[$i]}" != "${expected[$i]}" ]]; then
      fail "$file argv[$i] mismatch: expected '${expected[$i]}', got '${actual[$i]}'"
    fi
  done
}

assert_argv_prefix() {
  local file=$1
  shift
  local -a expected=("$@")
  local -a actual=()

  while IFS= read -r -d '' item; do
    actual+=("$item")
  done <"$file"

  if [[ "${#actual[@]}" -lt "${#expected[@]}" ]]; then
    fail "$file: expected at least ${#expected[@]} argv items, got ${#actual[@]}"
  fi

  local i
  for i in "${!expected[@]}"; do
    if [[ "${actual[$i]}" != "${expected[$i]}" ]]; then
      fail "$file argv[$i] mismatch: expected '${expected[$i]}', got '${actual[$i]}'"
    fi
  done
}

contains_item() {
  local needle=$1
  shift
  local item
  for item in "$@"; do
    if [[ "$item" = "$needle" ]]; then
      return 0
    fi
  done
  return 1
}

extract_source_surface_commands() {
  local source_file=$1
  local script_dir
  script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  "$script_dir/parse-rust-surface.pl" commands "$source_file"
}

extract_canonical_aliases() {
  local source_file=$1
  local script_dir
  script_dir=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
  "$script_dir/parse-rust-surface.pl" aliases "$source_file"
}

assert_exact_surface() {
  local source_file=$1
  local fixture_file=$2
  local parser=$3
  local label=$4

  [[ -r "$source_file" ]] || fail "missing source file: $source_file"
  [[ -r "$fixture_file" ]] || fail "missing expected fixture file: $fixture_file"

  local -a expected=()
  local -a actual=()
  local line

  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" ]] && continue
    expected+=("$line")
  done <"$fixture_file"

  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -z "$line" ]] && continue
    actual+=("$line")
  done < <("$parser" "$source_file")

  if [[ "${#actual[@]}" -ne "${#expected[@]}" ]]; then
    fail "$label mismatch for $source_file: expected ${#expected[@]} entries, got ${#actual[@]}"
  fi

  local i
  for i in "${!expected[@]}"; do
    if [[ "${actual[$i]}" != "${expected[$i]}" ]]; then
      fail "$label mismatch for $source_file: expected '${expected[$i]}', got '${actual[$i]}'"
    fi
  done
}

assert_source_surface_matches_fixture() {
  local source_file=$1
  local fixture_file=$2
  assert_exact_surface "$source_file" "$fixture_file" extract_source_surface_commands 'command surface'
}

assert_exact_alias_surface() {
  local source_file=$1
  local fixture_file=$2
  assert_exact_surface "$source_file" "$fixture_file" extract_canonical_aliases 'alias surface'
}

make_synthetic_source_surface() {
  local source_file=$1
  local fixture_file=$2

  mkdir -p "$(dirname -- "$source_file")"
  {
    printf '%s\n' 'pub(crate) const COMMANDS: &[CommandRow] = &['
    while IFS= read -r command; do
      [[ -z "$command" ]] && continue
      printf '    ("%s", None, &[], &[], false),\n' "$command"
    done <"$fixture_file"
    printf '%s\n' '];'
  } >"$source_file"
}

make_synthetic_canonical_alias_source() {
  local source_file=$1
  local fixture_file=$2

  mkdir -p "$(dirname -- "$source_file")"
  {
    printf '%s\n' 'fn canonical_command(value: &str) -> &str {'
    printf '%s\n' '  match value {'
    while IFS=$'\t' read -r alias canonical; do
      [[ -z "$alias" ]] && continue
      printf '    "%s" => "%s",\n' "$alias" "$canonical"
    done <"$fixture_file"
    printf '%s\n' '    other => other,'
    printf '%s\n' '  }'
    printf '%s\n' '}'
  } >"$source_file"
}

make_spoofed_command_surface() {
  local source_file=$1
  local fixture_file=$2
  "$script_dir/make-spoofed-surface.pl" commands "$fixture_file" "$source_file"
}

make_spoofed_alias_surface() {
  local source_file=$1
  local fixture_file=$2
  "$script_dir/make-spoofed-surface.pl" aliases "$fixture_file" "$source_file"
}

inject_text_before_literal() {
  local source_file=$1
  local needle=$2
  local replacement_file=$3
  env PYTHONDONTWRITEBYTECODE=1 "$script_dir/inject-text.py" "$source_file" "$needle" "$replacement_file"
}

make_id() {
  local prefix=$1
  printf '%s_%s_%s' "$prefix" "$$" "$RANDOM"
}

setup_fakebin() {
  local fakebin=$1
  mkdir -p "$fakebin"

  cat >"$fakebin/ssh" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_SSH_LOG:?}"
case "${FAKE_SSH_MODE:-execute}" in
  execute)
    if [[ "${1-}" = -- ]]; then
      shift
    fi
    target=${1:?}
    shift
    remote_cmd=$*
    remote_path=${FAKE_REMOTE_PATH:-$PATH}
    remote_hostname=${FAKE_REMOTE_HOSTNAME_VALUE:-${FAKE_HOSTNAME_VALUE:-}}
    remote_hostname_bin=${FAKE_REMOTE_HOSTNAME_BIN:-/bin/hostname}
    remote_cmd=${remote_cmd//\/bin\/hostname/$remote_hostname_bin}
    FAKE_HOSTNAME_VALUE="$remote_hostname" PATH="$remote_path" /bin/sh -c "$remote_cmd"
    ;;
  fail)
    printf 'ssh unexpectedly invoked: %s\n' "$*" >&2
    exit 99
    ;;
  *)
    printf 'unknown FAKE_SSH_MODE: %s\n' "${FAKE_SSH_MODE}" >&2
    exit 98
    ;;
esac
EOF

  cat >"$fakebin/hostname" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\n' "${FAKE_HOSTNAME_VALUE:?}"
EOF

  cat >"$fakebin/gitleaks" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_GITLEAKS_LOG:?}"
EOF

  cat >"$fakebin/kb" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_KB_LOG:?}"
EOF

  cat >"$fakebin/git" <<'EOF'
#!/bin/bash
set -euo pipefail
repo_root=${FAKE_GIT_REPO_ROOT:-}
if [[ "${1-}" = -C ]]; then
  repo_root=${2:?}
  shift 2
fi

command=${1:?}
shift
case "$command" in
  config)
    if [[ "${1-}" = --local ]]; then
      shift
    fi
    case "${1-}" in
      core.hooksPath)
        shift
        value=${1-}
        if [[ -n "${value-}" ]]; then
          if [[ -n "${FAKE_GIT_CONFIG_FILE:-}" ]]; then
            printf '%s\n' "$value" >"${FAKE_GIT_CONFIG_FILE:?}"
          fi
          exit 0
        fi
        ;;
      --get)
        shift
        key=${1:?}
        case "$key" in
          core.hooksPath)
            if [[ -n "${FAKE_GIT_CONFIG_FILE:-}" && -r "$FAKE_GIT_CONFIG_FILE" ]]; then
              /bin/cat "$FAKE_GIT_CONFIG_FILE"
            else
              printf '%s\n' "${FAKE_GIT_HOOKS_PATH:-}"
            fi
            ;;
          *)
            exit 1
            ;;
        esac
        ;;
      *)
        printf '%s\n' "${FAKE_GIT_CONFIG_MODE:-}" >/dev/null
        ;;
    esac
    ;;
  *)
    printf 'unexpected git command: %s\n' "$command" >&2
    exit 99
    ;;
esac
EOF

  chmod +x "$fakebin/ssh" "$fakebin/hostname" "$fakebin/gitleaks" "$fakebin/kb" "$fakebin/git"
}

run_expect_failure() {
  local output status
  set +e
  output=$("$@" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail "expected failure, got success with output: $output"
  fi
  printf '%s' "$output"
}

make_table() {
  local path=$1
  local board_id=$2
  local home_host=$3
  local ssh_target=$4
  local expected_remote=$5
  local kb_exec=$6
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$board_id" \
    "$home_host" \
    "$ssh_target" \
    "$expected_remote" \
    "$kb_exec" >"$path"
}

copy_public_package() {
  local target_root=$1
  mkdir -p "$target_root"
  cp -R "$package_dir"/. "$target_root"/
}

test_local_exec_injects_project_and_preserves_argv() {
  local fakebin="$tmp_dir/local/fakebin"
  local ssh_log="$tmp_dir/local/ssh.argv"
  local kb_log="$tmp_dir/local/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/local/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local empty=''
  local single_quote="one'two"
  local embedded_newline=$'line1\nline2'
  local trailing_newline=$'trail\n'
  local dollar_paren='literal $(touch)'
  local glob='literal *'

  FAKE_HOSTNAME_VALUE="$home_host" \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_SSH_MODE=fail \
  KB_HOSTS_TABLE="$table" \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-board" "$board_id" task ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"

  assert_log_clean "$ssh_log" 'local execution'
  assert_argv_file "$kb_log" --project "$board_id" task ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"
}

test_remote_exec_uses_ssh_and_preserves_argv() {
  local fakebin="$tmp_dir/remote/fakebin"
  local ssh_log="$tmp_dir/remote/ssh.argv"
  local kb_log="$tmp_dir/remote/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec
  board_id=$(make_id board)
  home_host=$(make_id home)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/remote/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local empty=''
  local single_quote="one'two"
  local embedded_newline=$'line1\nline2'
  local trailing_newline=$'trail\n'
  local dollar_paren='literal $(touch)'
  local glob='literal *'

  FAKE_HOSTNAME_VALUE=$(make_id current) \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  KB_HOSTS_TABLE="$table" \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-board" "$board_id" task ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"

  assert_argv_prefix "$ssh_log" -- "$ssh_target"
  assert_argv_file "$kb_log" --project "$board_id" task ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"
}

test_adjacent_hosts_table_is_used() {
  local fakebin="$tmp_dir/adjacent/fakebin"
  local ssh_log="$tmp_dir/adjacent/ssh.argv"
  local kb_log="$tmp_dir/adjacent/kb.argv"
  setup_fakebin "$fakebin"

  local install_root="$tmp_dir/adjacent/install"
  copy_public_package "$install_root"

  local board_id home_host ssh_target remote_host table kb_exec
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$install_root/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  FAKE_HOSTNAME_VALUE="$home_host" \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_SSH_MODE=fail \
  PATH="$fakebin:$PATH" \
  "$install_root/scripts/kb-board" "$board_id" task ls

  assert_log_clean "$ssh_log" 'adjacent hosts.tsv lookup'
  assert_argv_file "$kb_log" --project "$board_id" task ls
}

test_readme_example_table_is_accepted() {
  local fakebin="$tmp_dir/readme/fakebin"
  local ssh_log="$tmp_dir/readme/ssh.argv"
  local kb_log="$tmp_dir/readme/kb.argv"
  setup_fakebin "$fakebin"

  local example_kb
  example_kb=$(mktemp "$tmp_dir/readme-example-kb.XXXXXX")
  cat >"$example_kb" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_KB_LOG:?}"
EOF
  chmod +x "$example_kb"

  local table="$tmp_dir/readme/hosts.tsv"
  local board_id home_host ssh_target remote_host example_remote example_exec
  local readme_board readme_home readme_target readme_remote readme_exec
  local readme_text plan_file
  board_id=$(make_id board)
  home_host=$(make_id home)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  example_remote=$(/bin/hostname)
  example_exec="$example_kb"
  plan_file="$tmp_dir/readme-plan.md"
  printf '%s\n' '# plan' >"$plan_file"
  readme_text=$(/bin/cat "$package_dir/README.md")
  case "$readme_text" in
    *'--project NAME --body-file /tmp/plan.md'*) fail 'readme body-file example must not pass --project through kb-board' ;;
  esac
  assert_contains "$readme_text" 'scripts/kb-board board_identifier t new "Title" --body-file /tmp/plan.md --json' 'readme body-file example'
  readme_row=$(printf '%s\n' "$readme_text" | awk '
    /^```tsv$/ { in_block=1; next }
    in_block && NF { print; exit }
  ')
  IFS=$'\t' read -r readme_board readme_home readme_target readme_remote readme_exec <<<"$readme_row"
  assert_contains "$readme_board" 'board_identifier' 'readme board placeholder'
  assert_contains "$readme_home" 'board_home_host' 'readme home placeholder'
  assert_contains "$readme_target" 'ssh_target' 'readme ssh placeholder'
  assert_contains "$readme_remote" 'expected_remote_hostname' 'readme remote placeholder'
  assert_contains "$readme_exec" '<absolute_kb_binary_path>' 'readme executable placeholder'
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$board_id" \
    "$home_host" \
    "$ssh_target" \
    "$example_remote" \
    "$example_exec" >"$table"

  FAKE_HOSTNAME_VALUE=$(make_id current) \
  FAKE_REMOTE_HOSTNAME_VALUE="$example_remote" \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_REMOTE_PATH="$fakebin:$PATH" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  KB_HOSTS_TABLE="$table" \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-board" "$board_id" t new "Title" --body-file "$plan_file" --json

  assert_argv_prefix "$ssh_log" -- "$ssh_target"
  assert_argv_file "$kb_log" --project "$board_id" t new "Title" --body-file "$plan_file" --json
}

test_skill_parity_sections_are_present() {
  local skill="$package_dir/SKILL.md"
  local text
  text=$(/bin/cat "$skill")

  local -a required_headings=(
    '## Board home host is the execution boundary'
    '## Aliases'
    '## Addressing a board'
    '## Tag-scoped rules — what frames work'
    '## Attention — anything that needs the owner'
    '## Search and bounded RAG context'
    '## Sitreps — where a lane stands, cheaply'
    '## Handoffs — task and session'
    '## Working a task'
    '## Plans'
    '## Tags — which part of the system this is about'
    '## Provenance — where and when work happened'
    '## Reading'
    '## Watch — cursor-native live subscription'
    '## Subscription records and dispatcher delivery'
    '## Archival — bounded hot indexes, intact history'
    '## Deployment attempts — exact release receipts'
    '## The web view'
    '## As an MCP server'
    '## Refusals worth knowing'
    '## Reference'
  )

  local heading
  for heading in "${required_headings[@]}"; do
    assert_contains "$text" "$heading" "skill heading $heading"
  done

  local -a required_commands=(
    'scripts/kb-host BOARD_HOME_HOST r ls --json'
    'kb claim --next'
    'kb hb <id> --lease'
    'kb cp <id> --lease'
    'kb h new <task-id>'
    'kb search "resume the release handoff"'
    'kb t new "Title"'
    'kb att raise "<verdict-first'
    'kb r new "Universal rule."'
    'kb watch --project NAME'
    'kb subscription add --project NAME'
    'kb archive --older-than-days'
    'kb deploy start --repo'
    'kb mcp'
  )

  local command_snippet
  for command_snippet in "${required_commands[@]}"; do
    assert_contains "$text" "$command_snippet" "skill command $command_snippet"
  done

  local -a required_public_tokens=(
    '`kb` and `kanban` are the same binary'
    'kanban://BOARD/KIND/ID'
    'kanban://rules/rule/ID'
    'kanban-dispatcher'
    'kanban-codex-queue-adapter'
    'kanban serve'
    'kanban-serve.service'
    '/root/.local/bin/kanban-dispatcher'
    'system@cli'
  )

  local token_snippet
  for token_snippet in "${required_public_tokens[@]}"; do
    assert_contains "$text" "$token_snippet" "skill public token $token_snippet"
  done
}

test_selector_and_escape_flags_are_rejected_without_transport() {
  local fakebin="$tmp_dir/selectors/fakebin"
  local ssh_log="$tmp_dir/selectors/ssh.argv"
  local kb_log="$tmp_dir/selectors/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec output
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/selectors/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local -a selector_forms=(
    '--project'
    '--project=any'
    '--workspace'
    '--workspace=/tmp/any'
    '--db'
    '--db=/tmp/any.db'
  )
  local form
  for form in "${selector_forms[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=$(make_id current) \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-board" "$board_id" task ls "$form")
    assert_contains "$output" 'injects exactly one --project selector' "$form selector refusal"
    assert_log_clean "$ssh_log" "$form selector ssh"
    assert_log_clean "$kb_log" "$form selector kb"
  done

  local -a escape_forms=(
    '--registry'
    '--registry=all'
    '--all-boards'
    '--all-boards=all'
  )
  for form in "${escape_forms[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=$(make_id current) \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-board" "$board_id" task ls "$form")
    assert_contains "$output" 'cross-board scope flag' "$form escape refusal"
    assert_log_clean "$ssh_log" "$form escape ssh"
    assert_log_clean "$kb_log" "$form escape kb"
  done

  local -a command_escape_forms=(
    '--project'
    '--project=any'
    '--workspace'
    '--workspace=/tmp/any'
    '--db'
    '--db=/tmp/any.db'
    '--registry'
    '--registry=all'
    '--all-boards'
    '--all-boards=all'
  )
  for form in "${command_escape_forms[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=$(make_id current) \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-board" "$board_id" "$form" ls)
    assert_contains "$output" 'does not route registry or cross-board scope flag' "$form command refusal"
    assert_log_clean "$ssh_log" "$form command ssh"
    assert_log_clean "$kb_log" "$form command kb"
  done
}

test_registry_commands_are_rejected_without_transport() {
  local fakebin="$tmp_dir/registry/fakebin"
  local ssh_log="$tmp_dir/registry/ssh.argv"
  local kb_log="$tmp_dir/registry/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec output
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/registry/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local -a commands=(
    version
    v
    r
    rule
    init
    serve
    schema
    mcp
    doctor
    backup
    restore
    audit
    dashboard
    dash
    w
    ws
    workspace
  )

  local command
  for command in "${commands[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=$(make_id current) \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-board" "$board_id" "$command" ls)
    assert_contains "$output" 'kb-board' "$command registry refusal"
    assert_log_clean "$ssh_log" "$command registry ssh"
    assert_log_clean "$kb_log" "$command registry kb"
  done
}

test_host_surface_matches_source_allowlist() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file=${KB_SOURCE_RUST_FILE:-"$tmp_dir/source-surface/rust/lib.rs"}
  if [[ -n ${KB_SOURCE_RUST_FILE-} ]]; then
    [[ -r "$source_file" ]] || fail "missing source command surface file: $source_file"
  else
    make_synthetic_source_surface "$source_file" "$fixture_file"
  fi

  assert_source_surface_matches_fixture "$source_file" "$fixture_file"

  local allowed=(
    version v
    init
    workspace w ws
    dashboard dash
    doctor
    audit
    backup
    restore
    rule r
    serve
    schema
    mcp
  )
  local denied=(
    deploy
    import
    tag
    archive
    search
    search-rebuild
    task t
    story s
    handoff h
    attention att attn
    claim
    checkpoint cp
    heartbeat hb
    release rel
    note n
    context ctx
    events ev
    watch
    sitrep sr
    subscription
    todo
    stale
  )

  local command
  while IFS= read -r command; do
    [[ -z "$command" ]] && continue
    if contains_item "$command" "${allowed[@]}"; then
      continue
    fi
    if contains_item "$command" "${denied[@]}"; then
      continue
    fi
    fail "unexpected top-level command in $source_file: $command"
  done < <(extract_source_surface_commands "$source_file")
}

test_host_surface_external_source_override_is_accepted() {
  if [[ -z ${KB_SOURCE_RUST_FILE-} ]]; then
    return 0
  fi

  test_host_surface_matches_source_allowlist
}

test_host_surface_missing_command_drift_is_fail_closed() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file="$tmp_dir/source-missing/rust/lib.rs"
  local pruned_fixture="$tmp_dir/source-missing/expected.txt"
  mkdir -p "$(dirname -- "$pruned_fixture")"
  grep -vx '^schema$' "$fixture_file" >"$pruned_fixture"
  make_synthetic_source_surface "$source_file" "$pruned_fixture"

  local output status
  set +e
  output=$(assert_source_surface_matches_fixture "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'missing command drift should fail closed'
  fi
  assert_contains "$output" 'command surface mismatch' 'missing command drift'
}

test_host_surface_new_command_drift_is_fail_closed() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file="$tmp_dir/source-new/rust/lib.rs"
  make_synthetic_source_surface "$source_file" "$fixture_file"
  local needle=$'\n];\n'
  local replacement_file="$tmp_dir/source-new/replacement.txt"
  mkdir -p "$(dirname -- "$replacement_file")"
  "$script_dir/make-drift-replacement.pl" command >"$replacement_file"
  inject_text_before_literal "$source_file" "$needle" "$replacement_file"

  local output status
  set +e
  output=$(assert_source_surface_matches_fixture "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'new command drift should fail closed'
  fi
  assert_contains "$output" 'command surface mismatch' 'new command drift'
}

test_host_surface_spoofed_strings_are_ignored() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file="$tmp_dir/source-spoofed/rust/lib.rs"
  make_spoofed_command_surface "$source_file" "$fixture_file"
  assert_source_surface_matches_fixture "$source_file" "$fixture_file"
}

test_host_surface_duplicate_commands_are_fail_closed() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file="$tmp_dir/source-duplicate/rust/lib.rs"
  "$script_dir/make-spoofed-surface.pl" commands "$fixture_file" "$source_file" duplicate

  local output status
  set +e
  output=$(assert_source_surface_matches_fixture "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'duplicate command declarations should fail closed'
  fi
  assert_contains "$output" 'duplicate COMMANDS declaration' 'duplicate command declarations'
}

test_host_surface_missing_commands_table_is_fail_closed() {
  local fixture_file="$script_dir/kb-source-commands.txt"
  local source_file="$tmp_dir/source-missing-table/rust/lib.rs"
  mkdir -p "$(dirname -- "$source_file")"
  : >"$source_file"

  local output status
  set +e
  output=$(assert_source_surface_matches_fixture "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'missing COMMANDS table should fail closed'
  fi
  assert_contains "$output" 'missing COMMANDS table' 'missing COMMANDS table'
}

test_alias_surface_matches_fixture() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file=${KB_SOURCE_RUST_FILE:-"$tmp_dir/alias-surface/rust/lib.rs"}
  if [[ -n ${KB_SOURCE_RUST_FILE-} ]]; then
    [[ -r "$source_file" ]] || fail "missing source alias surface file: $source_file"
  else
    make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  fi

  assert_exact_alias_surface "$source_file" "$fixture_file"
}

test_alias_surface_missing_alias_drift_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-missing/rust/lib.rs"
  local pruned_fixture="$tmp_dir/alias-missing/expected.txt"
  mkdir -p "$(dirname -- "$pruned_fixture")"
  grep -vx $'^v\tversion$' "$fixture_file" >"$pruned_fixture"
  make_synthetic_canonical_alias_source "$source_file" "$pruned_fixture"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'missing alias drift should fail closed'
  fi
  assert_contains "$output" 'alias surface mismatch' 'missing alias drift'
}

test_alias_surface_new_alias_drift_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-new/rust/lib.rs"
  make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  local needle=$'\n    other => other,\n  }\n}\n'
  local replacement_file="$tmp_dir/alias-new/replacement.txt"
  mkdir -p "$(dirname -- "$replacement_file")"
  "$script_dir/make-drift-replacement.pl" alias >"$replacement_file"
  inject_text_before_literal "$source_file" "$needle" "$replacement_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'new alias drift should fail closed'
  fi
  assert_contains "$output" 'alias surface mismatch' 'new alias drift'
}

test_alias_surface_spoofed_strings_are_ignored() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-spoofed/rust/lib.rs"
  make_spoofed_alias_surface "$source_file" "$fixture_file"
  assert_exact_alias_surface "$source_file" "$fixture_file"
}

test_alias_surface_duplicate_functions_are_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-duplicate/rust/lib.rs"
  "$script_dir/make-spoofed-surface.pl" aliases "$fixture_file" "$source_file" duplicate

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'duplicate canonical_command functions should fail closed'
  fi
  assert_contains "$output" 'duplicate canonical_command function' 'duplicate canonical functions'
}

test_alias_surface_duplicate_alias_pairs_are_fail_closed() {
  local source_file="$tmp_dir/alias-dup-pair/rust/lib.rs"
  mkdir -p "$(dirname -- "$source_file")"
  {
    printf '%s\n' 'fn canonical_command(value: &str) -> &str {'
    printf '%s\n' '  match value {'
    printf '%s\n' '    "bk" => "backup",'
    printf '%s\n' '    "bk" => "backup",'
    printf '%s\n' '    other => other,'
    printf '%s\n' '  }'
    printf '%s\n' '}'
  } >"$source_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$script_dir/kb-canonical-aliases.txt" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'duplicate alias pair should fail closed'
  fi
  assert_contains "$output" 'duplicate canonical_command alias' 'duplicate alias pair'
}

test_alias_surface_duplicate_alias_target_is_fail_closed() {
  local source_file="$tmp_dir/alias-dup-target/rust/lib.rs"
  mkdir -p "$(dirname -- "$source_file")"
  {
    printf '%s\n' 'fn canonical_command(value: &str) -> &str {'
    printf '%s\n' '  match value {'
    printf '%s\n' '    "bk" => "backup",'
    printf '%s\n' '    "bk" => "restore",'
    printf '%s\n' '    other => other,'
    printf '%s\n' '  }'
    printf '%s\n' '}'
  } >"$source_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$script_dir/kb-canonical-aliases.txt" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'duplicate alias target should fail closed'
  fi
  assert_contains "$output" 'duplicate canonical_command alias' 'duplicate alias target'
}

test_alias_surface_block_rhs_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-block/rust/lib.rs"
  local replacement_file="$tmp_dir/alias-block/replacement.txt"
  make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  mkdir -p "$(dirname -- "$replacement_file")"
  printf '%s\n' '    "bk" => { let _ = "backup"; "surprise" },' >"$replacement_file"
  inject_text_before_literal "$source_file" $'\n    other => other,\n  }\n}\n' "$replacement_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'alias block rhs should fail closed'
  fi
  assert_contains "$output" 'canonical_command function is malformed' 'alias block rhs'
}

test_alias_surface_extra_rhs_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-extra-rhs/rust/lib.rs"
  local replacement_file="$tmp_dir/alias-extra-rhs/replacement.txt"
  make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  mkdir -p "$(dirname -- "$replacement_file")"
  printf '%s\n' '    "bk" => "backup" "surprise",' >"$replacement_file"
  inject_text_before_literal "$source_file" $'\n    other => other,\n  }\n}\n' "$replacement_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'alias extra rhs should fail closed'
  fi
  assert_contains "$output" 'canonical_command function is malformed' 'alias extra rhs'
}

test_alias_surface_guard_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-guard/rust/lib.rs"
  local replacement_file="$tmp_dir/alias-guard/replacement.txt"
  make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  mkdir -p "$(dirname -- "$replacement_file")"
  printf '%s\n' '    "bk" if false => "backup",' >"$replacement_file"
  inject_text_before_literal "$source_file" $'\n    other => other,\n  }\n}\n' "$replacement_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'alias guard should fail closed'
  fi
  assert_contains "$output" 'canonical_command function is malformed' 'alias guard'
}

test_alias_surface_call_is_fail_closed() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file="$tmp_dir/alias-call/rust/lib.rs"
  local replacement_file="$tmp_dir/alias-call/replacement.txt"
  make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  mkdir -p "$(dirname -- "$replacement_file")"
  printf '%s\n' '    "bk" => make_backup("backup"),' >"$replacement_file"
  inject_text_before_literal "$source_file" $'\n    other => other,\n  }\n}\n' "$replacement_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$fixture_file" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'alias call should fail closed'
  fi
  assert_contains "$output" 'canonical_command function is malformed' 'alias call'
}

test_alias_surface_duplicate_passthrough_is_fail_closed() {
  local source_file="$tmp_dir/alias-passthrough/rust/lib.rs"
  mkdir -p "$(dirname -- "$source_file")"
  {
    printf '%s\n' 'fn canonical_command(value: &str) -> &str {'
    printf '%s\n' '  match value {'
    printf '%s\n' '    other => other,'
    printf '%s\n' '    other => other,'
    printf '%s\n' '  }'
    printf '%s\n' '}'
  } >"$source_file"

  local output status
  set +e
  output=$(assert_exact_alias_surface "$source_file" "$script_dir/kb-canonical-aliases.txt" 2>&1)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    fail 'duplicate passthrough should fail closed'
  fi
  assert_contains "$output" 'duplicate canonical_command passthrough' 'duplicate passthrough'
}

test_alias_ownership_matches_wrappers() {
  local fixture_file="$script_dir/kb-canonical-aliases.txt"
  local source_file=${KB_SOURCE_RUST_FILE:-"$tmp_dir/alias-own/rust/lib.rs"}
  if [[ -n ${KB_SOURCE_RUST_FILE-} ]]; then
    [[ -r "$source_file" ]] || fail "missing source alias surface file: $source_file"
  else
    make_synthetic_canonical_alias_source "$source_file" "$fixture_file"
  fi

  assert_exact_alias_surface "$source_file" "$fixture_file"

  local fakebin="$tmp_dir/alias-own/fakebin"
  local ssh_log="$tmp_dir/alias-own/ssh.argv"
  local kb_log="$tmp_dir/alias-own/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/alias-own/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local alias canonical
  while IFS=$'\t' read -r alias canonical; do
    [[ -z "$alias" ]] && continue
    : >"$ssh_log"
    : >"$kb_log"
    case "$alias" in
      v|init|w|ws|dash|doctor|audit|backup|restore|r|serve|schema|mcp)
        output=$(run_expect_failure env \
          FAKE_HOSTNAME_VALUE=$(make_id current) \
          FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
          FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
          FAKE_REMOTE_PATH="$fakebin" \
          FAKE_SSH_LOG="$ssh_log" \
          FAKE_KB_LOG="$kb_log" \
          KB_HOSTS_TABLE="$table" \
          PATH="$fakebin:$PATH" \
          "$package_dir/scripts/kb-board" "$board_id" "$alias" ls)
        if [[ "$alias" = r ]]; then
          assert_contains "$output" 'board-owned commands only' "registry alias board refusal $alias"
        elif [[ "$alias" = v ]]; then
          assert_contains "$output" 'does not route registry command' "registry alias board refusal $alias"
        else
          assert_contains "$output" 'registry-wide command' "registry alias board refusal $alias"
        fi
        assert_log_clean "$ssh_log" "registry alias board ssh $alias"
        assert_log_clean "$kb_log" "registry alias board kb $alias"

        FAKE_HOSTNAME_VALUE="$home_host" \
        FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
        FAKE_REMOTE_PATH="$fakebin" \
        FAKE_SSH_LOG="$ssh_log" \
        FAKE_KB_LOG="$kb_log" \
        FAKE_SSH_MODE=fail \
        KB_HOSTS_TABLE="$table" \
        PATH="$fakebin:$PATH" \
        "$package_dir/scripts/kb-host" "$home_host" "$alias" ls
        assert_log_clean "$ssh_log" "registry alias host ssh $alias"
        assert_argv_file "$kb_log" "$alias" ls
        ;;
      *)
        FAKE_HOSTNAME_VALUE="$home_host" \
        FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
        FAKE_REMOTE_PATH="$fakebin" \
        FAKE_SSH_LOG="$ssh_log" \
        FAKE_KB_LOG="$kb_log" \
        FAKE_SSH_MODE=fail \
        KB_HOSTS_TABLE="$table" \
        PATH="$fakebin:$PATH" \
        "$package_dir/scripts/kb-board" "$board_id" "$alias" ls
        assert_log_clean "$ssh_log" "board alias board ssh $alias"
        assert_argv_file "$kb_log" --project "$board_id" "$alias" ls

        : >"$ssh_log"
        : >"$kb_log"
        output=$(run_expect_failure env \
          FAKE_HOSTNAME_VALUE=$(make_id current) \
          FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
          FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
          FAKE_REMOTE_PATH="$fakebin" \
          FAKE_SSH_LOG="$ssh_log" \
          FAKE_KB_LOG="$kb_log" \
          KB_HOSTS_TABLE="$table" \
          PATH="$fakebin:$PATH" \
          "$package_dir/scripts/kb-host" "$home_host" "$alias" ls)
        assert_contains "$output" 'registry-owned commands only' "board alias host refusal $alias"
        assert_log_clean "$ssh_log" "board alias host ssh $alias"
        assert_log_clean "$kb_log" "board alias host kb $alias"
        ;;
    esac
  done <"$fixture_file"
}

test_host_registry_commands_are_allowed_without_transport() {
  local fakebin="$tmp_dir/host-allow/fakebin"
  local ssh_log="$tmp_dir/host-allow/ssh.argv"
  local kb_log="$tmp_dir/host-allow/kb.argv"
  setup_fakebin "$fakebin"

  local host_id home_host ssh_target remote_host table kb_exec output
  host_id=$(make_id host)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-allow/hosts.tsv"
  make_table "$table" "$host_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local -a commands=(
    'version'
    'v'
    'init --name demo'
    'rule ls'
    'r ls'
    'workspace list'
    'w list'
    'ws list'
    'dashboard --json'
    'dash --json'
    'doctor --json'
    'audit verify --json'
    'backup --json'
    'restore --json'
    'serve --port 1234'
    'schema --json'
    'mcp'
  )

  local entry command_arg
  for entry in "${commands[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    # shellcheck disable=SC2086
    set -- $entry
    command_arg=$1
    shift || true
    output=$(env \
      FAKE_HOSTNAME_VALUE="$home_host" \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      FAKE_SSH_MODE=fail \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-host" "$home_host" "$command_arg" "$@")
    assert_log_clean "$ssh_log" "$entry allowed ssh"
    assert_argv_file "$kb_log" "$command_arg" "$@"
  done
}

test_host_local_exec_preserves_argv_and_uses_table_binary() {
  local fakebin="$tmp_dir/host-local/fakebin"
  local ssh_log="$tmp_dir/host-local/ssh.argv"
  local kb_log="$tmp_dir/host-local/kb.argv"
  setup_fakebin "$fakebin"

  local host_id home_host ssh_target remote_host table kb_exec
  host_id=$(make_id host)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-local/hosts.tsv"
  make_table "$table" "$host_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local empty=''
  local single_quote="one'two"
  local embedded_newline=$'line1\nline2'
  local trailing_newline=$'trail\n'
  local dollar_paren='literal $(touch)'
  local glob='literal *'

  FAKE_HOSTNAME_VALUE="$home_host" \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_SSH_MODE=fail \
  KB_HOSTS_TABLE="$table" \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-host" "$home_host" r ls --json "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"

  assert_log_clean "$ssh_log" 'host local execution'
  assert_argv_file "$kb_log" r ls --json "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"
}

test_host_remote_exec_uses_ssh_and_preserves_argv() {
  local fakebin="$tmp_dir/host-remote/fakebin"
  local ssh_log="$tmp_dir/host-remote/ssh.argv"
  local kb_log="$tmp_dir/host-remote/kb.argv"
  setup_fakebin "$fakebin"

  local host_id home_host ssh_target remote_host table kb_exec
  host_id=$(make_id host)
  home_host=$(make_id home)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-remote/hosts.tsv"
  make_table "$table" "$host_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local empty=''
  local single_quote="one'two"
  local embedded_newline=$'line1\nline2'
  local trailing_newline=$'trail\n'
  local dollar_paren='literal $(touch)'
  local glob='literal *'

  FAKE_HOSTNAME_VALUE=$(make_id current) \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  KB_HOSTS_TABLE="$table" \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-host" "$home_host" r ls --json "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"

  assert_argv_prefix "$ssh_log" -- "$ssh_target"
  assert_argv_file "$kb_log" r ls --json "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"
}

test_host_table_conflicts_are_fail_closed() {
  local fakebin="$tmp_dir/host-conflict/fakebin"
  local ssh_log="$tmp_dir/host-conflict/ssh.argv"
  local kb_log="$tmp_dir/host-conflict/kb.argv"
  setup_fakebin "$fakebin"

  local home_host ssh_target_a ssh_target_b remote_host table board_a board_b kb_exec output
  home_host=$(make_id home)
  ssh_target_a=$(make_id target-a)
  ssh_target_b=$(make_id target-b)
  remote_host=$(make_id remote)
  board_a=$(make_id board-a)
  board_b=$(make_id board-b)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-conflict/hosts.tsv"
  mkdir -p "$tmp_dir/host-conflict"
  make_table "$table" "$board_a" "$home_host" "$ssh_target_a" "$remote_host" "$kb_exec"
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$board_b" \
    "$home_host" \
    "$ssh_target_b" \
    "$remote_host" \
    "$kb_exec" >>"$table"

  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE=$(make_id current) \
    FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    KB_HOSTS_TABLE="$table" \
    PATH="$fakebin:$PATH" \
    "$package_dir/scripts/kb-host" "$home_host" r ls --json)
  assert_contains "$output" 'conflicting host mapping' 'host conflict refusal'
  assert_log_clean "$ssh_log" 'host conflict ssh'
  assert_log_clean "$kb_log" 'host conflict kb'
}

test_host_refuses_board_owned_commands_without_transport() {
  local fakebin="$tmp_dir/host-board/fakebin"
  local ssh_log="$tmp_dir/host-board/ssh.argv"
  local kb_log="$tmp_dir/host-board/kb.argv"
  setup_fakebin "$fakebin"

  local host_id home_host ssh_target remote_host table kb_exec output
  host_id=$(make_id host)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-board/hosts.tsv"
  make_table "$table" "$host_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  local -a commands=(
    deploy import tag archive search search-rebuild
    task t story s handoff h attention att attn claim checkpoint cp
    heartbeat hb release rel note n context ctx events ev watch sitrep sr
    subscription todo stale
  )

  local command
  for command in "${commands[@]}"; do
    : >"$ssh_log"
    : >"$kb_log"
    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=$(make_id current) \
      FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
      FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      KB_HOSTS_TABLE="$table" \
      PATH="$fakebin:$PATH" \
      "$package_dir/scripts/kb-host" "$home_host" "$command" ls)
    assert_contains "$output" 'registry-owned commands only' "host board-owned refusal $command"
    assert_log_clean "$ssh_log" "host board-owned ssh $command"
    assert_log_clean "$kb_log" "host board-owned kb $command"
  done
}

test_host_remote_hostname_mismatch_is_fail_closed() {
  local fakebin="$tmp_dir/host-mismatch/fakebin"
  local ssh_log="$tmp_dir/host-mismatch/ssh.argv"
  local kb_log="$tmp_dir/host-mismatch/kb.argv"
  setup_fakebin "$fakebin"

  local host_id home_host ssh_target remote_host table kb_exec output
  host_id=$(make_id host)
  home_host=$(make_id home)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/host-mismatch/hosts.tsv"
  make_table "$table" "$host_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE=$(make_id current) \
    FAKE_REMOTE_HOSTNAME_VALUE=$(make_id wrong) \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    KB_HOSTS_TABLE="$table" \
    PATH="$fakebin:$PATH" \
    "$package_dir/scripts/kb-host" "$home_host" r ls --json)
  assert_contains "$output" 'remote hostname mismatch' 'host remote hostname mismatch'
  assert_argv_prefix "$ssh_log" -- "$ssh_target"
  if [[ -s "$kb_log" ]]; then
    fail 'host remote hostname mismatch should prevent the remote kb stub from running'
  fi
}

test_binary_path_rules_are_enforced() {
  local fakebin="$tmp_dir/binary/fakebin"
  local ssh_log="$tmp_dir/binary/ssh.argv"
  local kb_log="$tmp_dir/binary/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table output
  board_id=$(make_id board)
  home_host=$(/bin/hostname)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  table="$tmp_dir/binary/hosts.tsv"

  local current_host=$(/bin/hostname)
  make_table "$table" "$board_id" "$current_host" "$ssh_target" "$remote_host" "$fakebin/kb"

  FAKE_HOSTNAME_VALUE="$current_host" \
  FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  KB_HOSTS_TABLE="$table" \
  KB_BIN=relative-kb \
  PATH="$fakebin:$PATH" \
  "$package_dir/scripts/kb-board" "$board_id" task ls --json
  assert_log_clean "$ssh_log" 'table absolute binary ssh'
  assert_argv_file "$kb_log" --project "$board_id" task ls --json

  : >"$ssh_log"
  : >"$kb_log"
  cat >"$table" <<EOF
$board_id	$current_host	$ssh_target	$remote_host	relative-kb
EOF
  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE="$current_host" \
    FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    KB_HOSTS_TABLE="$table" \
    PATH="$fakebin:$PATH" \
    "$package_dir/scripts/kb-board" "$board_id" task ls)
  assert_contains "$output" 'KB executable path must be absolute' 'relative table binary refusal'
  assert_log_clean "$ssh_log" 'relative table binary ssh'
  assert_log_clean "$kb_log" 'relative table binary kb'

  : >"$ssh_log"
  : >"$kb_log"
  cat >"$table" <<EOF
$board_id	$current_host	$ssh_target	$remote_host
EOF
  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE="$current_host" \
    FAKE_REMOTE_HOSTNAME_VALUE="$remote_host" \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    KB_HOSTS_TABLE="$table" \
    PATH="$fakebin:$PATH" \
    "$package_dir/scripts/kb-board" "$board_id" task ls)
  assert_contains "$output" 'malformed routing row' 'missing binary column refusal'
  assert_log_clean "$ssh_log" 'missing binary column ssh'
  assert_log_clean "$kb_log" 'missing binary column kb'
}

test_board_remote_hostname_mismatch_is_fail_closed() {
  local fakebin="$tmp_dir/mismatch/fakebin"
  local ssh_log="$tmp_dir/mismatch/ssh.argv"
  local kb_log="$tmp_dir/mismatch/kb.argv"
  setup_fakebin "$fakebin"

  local board_id home_host ssh_target remote_host table kb_exec output
  board_id=$(make_id board)
  home_host=$(make_id home)
  ssh_target=$(make_id target)
  remote_host=$(make_id remote)
  kb_exec="$fakebin/kb"
  table="$tmp_dir/mismatch/hosts.tsv"
  make_table "$table" "$board_id" "$home_host" "$ssh_target" "$remote_host" "$kb_exec"

  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE=$(make_id current) \
    FAKE_REMOTE_HOSTNAME_VALUE=$(make_id wrong) \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    KB_HOSTS_TABLE="$table" \
    PATH="$fakebin:$PATH" \
    "$package_dir/scripts/kb-board" "$board_id" task ls --json)
  assert_contains "$output" 'remote hostname mismatch' 'remote hostname mismatch'
  assert_argv_prefix "$ssh_log" -- "$ssh_target"
  if [[ -s "$kb_log" ]]; then
    fail 'remote hostname mismatch should prevent the remote kb stub from running'
  fi
}

test_denylist_and_hook_behaviour() {
  local clean_root="$tmp_dir/clean-root"
  local dirty_root="$tmp_dir/dirty-root"
  local git_meta_root="$tmp_dir/git-meta-root"
  mkdir -p "$clean_root" "$dirty_root" "$git_meta_root/.git"

  local token
  token=$(make_id token)
  printf 'safe\n' >"$clean_root/file.txt"
  printf '%s\n' "$token" >"$dirty_root/file.txt"
  printf '%s\n' "$token" >"$git_meta_root/.git/config"

  local fakebin="$tmp_dir/leak/fakebin"
  local gitleaks_log="$tmp_dir/leak/gitleaks.argv"
  setup_fakebin "$fakebin"

  local denylist="$tmp_dir/denylist.txt"
  printf '%s\n' "$token" >"$denylist"

  local output
  output=$("$package_dir/scripts/denylist-check" "$clean_root" "$denylist")
  assert_contains "$output" 'denylist-check: ok' 'clean denylist scan'

  local empty_denylist="$tmp_dir/empty-denylist.txt"
  : >"$empty_denylist"
  output=$(run_expect_failure "$package_dir/scripts/denylist-check" "$clean_root" "$empty_denylist")
  assert_contains "$output" 'denylist-check: denylist is empty' 'empty denylist scan'

  output=$("$package_dir/scripts/denylist-check" "$git_meta_root" "$denylist")
  assert_contains "$output" 'denylist-check: ok' 'git metadata ignored'

  output=$(run_expect_failure "$package_dir/scripts/denylist-check" "$dirty_root" "$denylist")
  assert_contains "$output" 'denylist-check: match' 'dirty denylist scan'

  output=$(run_expect_failure "$package_dir/scripts/denylist-check" "$clean_root")
  assert_contains "$output" 'missing denylist file path' 'denylist missing path'

  output=$(env \
    PATH="$fakebin:$PATH" \
    FAKE_GITLEAKS_LOG="$gitleaks_log" \
    "$package_dir/scripts/leak-gate" "$clean_root" "$denylist")
  assert_contains "$output" 'gitleaks completed' 'leak gate with gitleaks'
  assert_argv_file "$gitleaks_log" dir --no-banner "$clean_root"

  output=$(run_expect_failure env \
    PATH="/usr/bin:/bin" \
    "$package_dir/scripts/leak-gate" "$clean_root" "$denylist")
  assert_contains "$output" 'gitleaks unavailable' 'leak gate without gitleaks'

  output=$(run_expect_failure "$package_dir/scripts/leak-gate" "$clean_root")
  assert_contains "$output" 'missing denylist file path' 'leak gate missing denylist'

  output=$(run_expect_failure "$package_dir/scripts/commit-gate" "$clean_root")
  assert_contains "$output" 'KB_DENYLIST_FILE is required' 'commit gate missing denylist'

  local repo="$tmp_dir/hook-repo"
  local git_config_file="$tmp_dir/hook-git-config.txt"
  copy_public_package "$repo"
  (
    cd "$repo"
    git init >/dev/null
  )
  chmod +x "$repo/.githooks/pre-commit" "$repo/scripts/install-hooks" "$repo/scripts/check-hooks"

  output=$(env \
    FAKE_GIT_CONFIG_FILE="$git_config_file" \
    PATH="$fakebin:$PATH" \
    "$repo/scripts/install-hooks" "$repo")
  assert_contains "$output" 'install-hooks: set core.hooksPath=.githooks' 'hook install output'

  FAKE_GIT_CONFIG_FILE="$git_config_file" \
  PATH="$fakebin:$PATH" \
  "$repo/scripts/check-hooks" "$repo"

  output=$(run_expect_failure "$repo/.githooks/pre-commit")
  assert_contains "$output" 'KB_DENYLIST_FILE is required' 'hook missing denylist'

  output=$(env \
    KB_DENYLIST_FILE="$denylist" \
    PATH="$fakebin:$PATH" \
    FAKE_GITLEAKS_LOG="$gitleaks_log" \
    "$repo/.githooks/pre-commit")
  assert_contains "$output" 'gitleaks completed' 'hook success'
}

test_content_audit() {
  local scan_root="$package_dir"
  local synthetic_audit_root="$tmp_dir/synthetic-audit"
  local git_meta_root="$tmp_dir/git-audit"
  local denylist="$tmp_dir/content-audit-denylist.txt"
  local token_one token_two output
  token_one=$(make_id deny)
  token_two=$(make_id deny)
  mkdir -p "$synthetic_audit_root" "$git_meta_root/.git"
  printf '%s\n%s\n' "$token_one" "$token_two" >"$denylist"

  output=$("$package_dir/scripts/denylist-check" "$scan_root" "$denylist")
  assert_contains "$output" 'denylist-check: ok' 'content audit package scan'

  printf '%s\n' "$token_one" >"$synthetic_audit_root/file.txt"
  output=$(run_expect_failure "$package_dir/scripts/denylist-check" "$synthetic_audit_root" "$denylist")
  assert_contains "$output" 'denylist-check: match' 'content audit synthetic match'

  printf '%s\n' "$token_one" >"$git_meta_root/.git/config"
  output=$("$package_dir/scripts/denylist-check" "$git_meta_root" "$denylist")
  assert_contains "$output" 'denylist-check: ok' 'content audit git ignored'

  local empty_denylist="$tmp_dir/empty-denylist.txt"
  : >"$empty_denylist"
  output=$(run_expect_failure "$package_dir/scripts/denylist-check" "$scan_root" "$empty_denylist")
  assert_contains "$output" 'denylist-check: denylist is empty' 'content audit empty denylist'
}

assert_no_bytecode_artifacts() {
  local bytecode
  bytecode=$(find "$package_dir/tests" -type d -name '__pycache__' -o -type f -name '*.pyc')
  if [[ -n "$bytecode" ]]; then
    fail "bytecode artifact present after test run: $bytecode"
  fi
}

assert_test_wiring() {
  local source_file="$script_dir/kb-wrapper-tests.sh"
  local -a expected_tests=(
    test_local_exec_injects_project_and_preserves_argv
    test_remote_exec_uses_ssh_and_preserves_argv
    test_adjacent_hosts_table_is_used
    test_readme_example_table_is_accepted
    test_skill_parity_sections_are_present
    test_host_surface_matches_source_allowlist
    test_host_surface_external_source_override_is_accepted
    test_host_surface_missing_command_drift_is_fail_closed
    test_host_surface_new_command_drift_is_fail_closed
    test_host_surface_spoofed_strings_are_ignored
    test_host_surface_duplicate_commands_are_fail_closed
    test_host_surface_missing_commands_table_is_fail_closed
    test_alias_surface_matches_fixture
    test_alias_surface_missing_alias_drift_is_fail_closed
    test_alias_surface_new_alias_drift_is_fail_closed
    test_alias_surface_spoofed_strings_are_ignored
    test_alias_surface_duplicate_functions_are_fail_closed
    test_alias_surface_duplicate_alias_pairs_are_fail_closed
    test_alias_surface_duplicate_alias_target_is_fail_closed
    test_alias_surface_block_rhs_is_fail_closed
    test_alias_surface_extra_rhs_is_fail_closed
    test_alias_surface_guard_is_fail_closed
    test_alias_surface_call_is_fail_closed
    test_alias_surface_duplicate_passthrough_is_fail_closed
    test_alias_ownership_matches_wrappers
    test_selector_and_escape_flags_are_rejected_without_transport
    test_registry_commands_are_rejected_without_transport
    test_host_registry_commands_are_allowed_without_transport
    test_host_local_exec_preserves_argv_and_uses_table_binary
    test_host_remote_exec_uses_ssh_and_preserves_argv
    test_host_table_conflicts_are_fail_closed
    test_host_refuses_board_owned_commands_without_transport
    test_host_remote_hostname_mismatch_is_fail_closed
    test_binary_path_rules_are_enforced
    test_board_remote_hostname_mismatch_is_fail_closed
    test_denylist_and_hook_behaviour
    test_content_audit
  )

  local -a defined_tests=()
  local line name expected count
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ $line =~ ^(test_[A-Za-z0-9_]+)\(\)[[:space:]]*\{$ ]] || continue
    defined_tests+=("${BASH_REMATCH[1]}")
  done <"$source_file"

  for expected in "${expected_tests[@]}"; do
    if ! declare -F "$expected" >/dev/null; then
      fail "missing runnable test function: $expected"
    fi
    if ! contains_item "$expected" "${defined_tests[@]}"; then
      fail "missing test definition: $expected"
    fi
    count=$(grep -E "^${expected}\(\)[[:space:]]*\{" "$source_file" | wc -l | tr -d ' ')
    if [[ "$count" -ne 1 ]]; then
      fail "duplicate test definition: $expected"
    fi
  done

  for name in "${defined_tests[@]}"; do
    if ! contains_item "$name" "${expected_tests[@]}"; then
      fail "unexpected test definition: $name"
    fi
  done
}

main() {
  assert_test_wiring

  local -a tests=(
    test_local_exec_injects_project_and_preserves_argv
    test_remote_exec_uses_ssh_and_preserves_argv
    test_adjacent_hosts_table_is_used
    test_readme_example_table_is_accepted
    test_skill_parity_sections_are_present
    test_host_surface_matches_source_allowlist
    test_host_surface_external_source_override_is_accepted
    test_host_surface_missing_command_drift_is_fail_closed
    test_host_surface_new_command_drift_is_fail_closed
    test_host_surface_spoofed_strings_are_ignored
    test_host_surface_duplicate_commands_are_fail_closed
    test_host_surface_missing_commands_table_is_fail_closed
    test_alias_surface_matches_fixture
    test_alias_surface_missing_alias_drift_is_fail_closed
    test_alias_surface_new_alias_drift_is_fail_closed
    test_alias_surface_spoofed_strings_are_ignored
    test_alias_surface_duplicate_functions_are_fail_closed
    test_alias_surface_duplicate_alias_pairs_are_fail_closed
    test_alias_surface_duplicate_alias_target_is_fail_closed
    test_alias_surface_block_rhs_is_fail_closed
    test_alias_surface_extra_rhs_is_fail_closed
    test_alias_surface_guard_is_fail_closed
    test_alias_surface_call_is_fail_closed
    test_alias_surface_duplicate_passthrough_is_fail_closed
    test_alias_ownership_matches_wrappers
    test_selector_and_escape_flags_are_rejected_without_transport
    test_registry_commands_are_rejected_without_transport
    test_host_registry_commands_are_allowed_without_transport
    test_host_local_exec_preserves_argv_and_uses_table_binary
    test_host_remote_exec_uses_ssh_and_preserves_argv
    test_host_table_conflicts_are_fail_closed
    test_host_refuses_board_owned_commands_without_transport
    test_host_remote_hostname_mismatch_is_fail_closed
    test_binary_path_rules_are_enforced
    test_board_remote_hostname_mismatch_is_fail_closed
    test_denylist_and_hook_behaviour
    test_content_audit
  )

  local test_name
  for test_name in "${tests[@]}"; do
    "$test_name"
  done
  assert_no_bytecode_artifacts
  printf 'ok: public-kb-skill tests passed\n'
}

main "$@"
