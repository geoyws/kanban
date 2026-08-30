#!/bin/bash
set -euo pipefail

repo_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../../.." && pwd)
hax_kb="$repo_root/skills/kb/scripts/hax-kb"
kb_board="$repo_root/skills/kb/scripts/kb-board"
actual_hostname=$(/bin/hostname)

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/kb-wrapper-tests.XXXXXX")
trap 'rm -rf "$tmp_dir"' EXIT

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  exit 1
}

assert_eq() {
  local expected=$1
  local actual=$2
  local label=$3
  if [ "$expected" != "$actual" ]; then
    fail "$label: expected '$expected', got '$actual'"
  fi
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

assert_first_argv() {
  local file=$1
  local expected=$2
  local actual=
  IFS= read -r -d '' actual <"$file" || true
  assert_eq "$expected" "$actual" "$file first argv"
}

assert_argv_prefix() {
  local file=$1
  shift
  local -a expected=("$@")
  local -a actual=()

  while IFS= read -r -d '' item; do
    actual+=("$item")
  done <"$file"

  if [ "${#actual[@]}" -lt "${#expected[@]}" ]; then
    fail "$file: expected at least ${#expected[@]} argv items, got ${#actual[@]}"
  fi

  local i
  for i in "${!expected[@]}"; do
    if [ "${actual[$i]}" != "${expected[$i]}" ]; then
      fail "$file argv[$i] mismatch: expected '${expected[$i]}', got '${actual[$i]}'"
    fi
  done
}

assert_log_clean() {
  local file=$1
  local label=$2
  if [ -e "$file" ] && [ -s "$file" ]; then
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

  if [ "${#actual[@]}" -ne "${#expected[@]}" ]; then
    fail "argv length mismatch: expected ${#expected[@]}, got ${#actual[@]}"
  fi

  local i
  for i in "${!expected[@]}"; do
    if [ "${actual[$i]}" != "${expected[$i]}" ]; then
      fail "argv[$i] mismatch: expected '${expected[$i]}', got '${actual[$i]}'"
    fi
  done
}

setup_fakebin() {
  local fakebin=$1
  mkdir -p "$fakebin"

  cat >"$fakebin/hostname" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\n' "${FAKE_HOSTNAME_VALUE:?}"
EOF

  cat >"$fakebin/ssh" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_SSH_LOG:?}"
case "${FAKE_SSH_MODE:-execute}" in
  execute)
    if [ "${1-}" = -- ]; then
      shift
    fi
    host=${1:?}
    shift
    remote_cmd=$*
    remote_hostname=${FAKE_REMOTE_HOSTNAME_VALUE:-${FAKE_HOSTNAME_VALUE:?}}
    remote_hostname_bin=${FAKE_REMOTE_HOSTNAME_BIN:-/bin/hostname}
    remote_cmd=${remote_cmd//\/bin\/hostname/$remote_hostname_bin}
    FAKE_HOSTNAME_VALUE="$remote_hostname" PATH="${FAKE_REMOTE_PATH:-$PATH}" /bin/sh -c "$remote_cmd"
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

  cat >"$fakebin/kb" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\0' "$@" >"${FAKE_KB_LOG:?}"
EOF

  chmod +x "$fakebin/hostname" "$fakebin/ssh" "$fakebin/kb"
}

run_expect_failure() {
  local output status
  set +e
  output=$("$@" 2>&1)
  status=$?
  set -e
  if [ "$status" -eq 0 ]; then
    fail "expected failure, got success with output: $output"
  fi
  printf '%s' "$output"
}

test_raw_hax_kb_remote_transport_preserves_argv() {
  local fakebin="$tmp_dir/raw/fakebin"
  local ssh_log="$tmp_dir/raw/ssh.argv"
  local kb_log="$tmp_dir/raw/kb.argv"
  setup_fakebin "$fakebin"

  FAKE_HOSTNAME_VALUE=client-host \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_REMOTE_HOSTNAME_VALUE=hax \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_SSH_MODE=execute \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$hax_kb" r ls --json --tag queuer 'literal $HOME *'

  assert_argv_prefix "$ssh_log" -- hax
  assert_argv_file "$kb_log" r ls --json --tag queuer 'literal $HOME *'
}

test_hax_kb_execs_locally_on_hax_without_ssh() {
  local fakebin="$tmp_dir/local/fakebin"
  local ssh_log="$tmp_dir/local/ssh.argv"
  local kb_log="$tmp_dir/local/kb.argv"
  setup_fakebin "$fakebin"

  FAKE_HOSTNAME_VALUE=hax \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_SSH_MODE=fail \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$hax_kb" r ls --json

  assert_argv_file "$kb_log" r ls --json
  assert_log_clean "$ssh_log" 'direct-on-HAX execution'
}

test_hax_kb_remote_transport_preserves_argv() {
  local fakebin="$tmp_dir/remote/fakebin"
  local ssh_log="$tmp_dir/remote/ssh.argv"
  local kb_log="$tmp_dir/remote/kb.argv"
  setup_fakebin "$fakebin"

  FAKE_HOSTNAME_VALUE=client-host \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_REMOTE_HOSTNAME_VALUE=hax \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_SSH_MODE=execute \
  HAX_SSH_HOST=ssh-alias \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$hax_kb" rule cat r-123 --json 'spaced arg' 'literal $(touch)' '*'

  assert_argv_prefix "$ssh_log" -- ssh-alias
  assert_argv_file "$kb_log" rule cat r-123 --json 'spaced arg' 'literal $(touch)' '*'
}

test_hax_kb_remote_round_trips_weird_argv() {
  local fakebin="$tmp_dir/weird/fakebin"
  local ssh_log="$tmp_dir/weird/ssh.argv"
  local kb_log="$tmp_dir/weird/kb.argv"
  setup_fakebin "$fakebin"

  local empty=''
  local single_quote="one'two"
  local embedded_newline=$'line1\nline2'
  local trailing_newline=$'trail\n'
  local dollar_paren='literal $(touch)'
  local glob='literal *'

  FAKE_HOSTNAME_VALUE=client-host \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_REMOTE_HOSTNAME_VALUE=hax \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_SSH_MODE=execute \
  HAX_SSH_HOST=ssh-alias \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$hax_kb" r ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"

  assert_argv_prefix "$ssh_log" -- ssh-alias
  assert_argv_file "$kb_log" r ls "$empty" "$single_quote" "$embedded_newline" "$trailing_newline" "$dollar_paren" "$glob"
}

test_hax_kb_remote_rejects_non_hax_remote_hostname() {
  local fakebin="$tmp_dir/bad-remote/fakebin"
  local ssh_log="$tmp_dir/bad-remote/ssh.argv"
  local kb_log="$tmp_dir/bad-remote/kb.argv"
  setup_fakebin "$fakebin"

  local output
  output=$(run_expect_failure env \
    FAKE_HOSTNAME_VALUE=not-hax \
    FAKE_SSH_LOG="$ssh_log" \
    FAKE_KB_LOG="$kb_log" \
    FAKE_REMOTE_PATH="$fakebin" \
    FAKE_REMOTE_HOSTNAME_VALUE=not-hax \
    FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
    FAKE_SSH_MODE=execute \
    HAX_SSH_HOST=ssh-alias \
    HAX_KB_BIN="$fakebin/kb" \
    PATH="$fakebin:$PATH" \
    "$hax_kb" r ls --json)

  assert_contains "$output" 'remote host is not hax' 'remote hostname guard'
  if [ -e "$kb_log" ] && [ -s "$kb_log" ]; then
    fail 'remote hostname guard should prevent the kb stub from running'
  fi
}

test_kb_board_execs_directly_on_hax() {
  local fakebin="$tmp_dir/board/fakebin"
  local ssh_log="$tmp_dir/board/ssh.argv"
  local kb_log="$tmp_dir/board/kb.argv"
  setup_fakebin "$fakebin"

  FAKE_HOSTNAME_VALUE=hax \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_REMOTE_HOSTNAME_VALUE=hax \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_SSH_MODE=fail \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$kb_board" alpha task ls --json 'spaced arg' 'literal $(touch)' '*'

  assert_log_clean "$ssh_log" 'direct board helper on hax'
  assert_argv_file "$kb_log" --project alpha task ls --json 'spaced arg' 'literal $(touch)' '*'
}

test_kb_board_remote_transport_preserves_literals() {
  local fakebin="$tmp_dir/option/fakebin"
  local ssh_log="$tmp_dir/option/ssh.argv"
  local kb_log="$tmp_dir/option/kb.argv"
  setup_fakebin "$fakebin"

  FAKE_HOSTNAME_VALUE=client-host \
  FAKE_SSH_LOG="$ssh_log" \
  FAKE_KB_LOG="$kb_log" \
  FAKE_REMOTE_PATH="$fakebin" \
  FAKE_REMOTE_HOSTNAME_VALUE=hax \
  FAKE_REMOTE_HOSTNAME_BIN="$fakebin/hostname" \
  FAKE_SSH_MODE=execute \
  HAX_SSH_HOST=-oProxyJump=jump.example \
  HAX_KB_BIN="$fakebin/kb" \
  PATH="$fakebin:$PATH" \
  "$kb_board" alpha task ls --json

  assert_argv_prefix "$ssh_log" -- -oProxyJump=jump.example
  assert_argv_file "$kb_log" --project alpha task ls --json
}

test_kb_board_refuses_missing_project_or_command() {
  local output
  output=$(run_expect_failure "$kb_board")
  assert_contains "$output" 'usage: kb-board PROJECT KB_COMMAND [ARGS...]' 'missing both'

  output=$(run_expect_failure "$kb_board" '')
  assert_contains "$output" 'usage: kb-board PROJECT KB_COMMAND [ARGS...]' 'empty project'

  output=$(run_expect_failure "$kb_board" alpha)
  assert_contains "$output" 'usage: kb-board PROJECT KB_COMMAND [ARGS...]' 'missing command'

  output=$(run_expect_failure "$kb_board" alpha '')
  assert_contains "$output" 'usage: kb-board PROJECT KB_COMMAND [ARGS...]' 'empty command'
}

test_kb_board_rejects_forbidden_selectors_everywhere() {
  local fakebin="$tmp_dir/refuse/fakebin"
  local ssh_log="$tmp_dir/refuse/ssh.argv"
  local kb_log="$tmp_dir/refuse/kb.argv"
  setup_fakebin "$fakebin"

  local -a forms=(
    '--project'
    '--project=alpha'
    '--workspace'
    '--workspace=/tmp/board'
    '--db'
    '--db=/tmp/board.db'
  )
  local -a labels=(
    '--project'
    '--project='
    '--workspace'
    '--workspace='
    '--db'
    '--db='
  )
  local form label output
  local -a project_case
  local -a command_case
  local -a trailing_case

  for i in "${!forms[@]}"; do
    form=${forms[$i]}
    label=${labels[$i]}

    project_case=("$form" task ls)
    command_case=(alpha "$form" ls)
    trailing_case=(alpha task ls "$form")

    : >"$ssh_log"
    : >"$kb_log"

    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=client-host \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_REMOTE_HOSTNAME_VALUE=hax \
      FAKE_SSH_MODE=execute \
      HAX_SSH_HOST=ssh-alias \
      HAX_KB_BIN="$fakebin/kb" \
      PATH="$fakebin:$PATH" \
      "$kb_board" "${project_case[@]}")
    assert_contains "$output" 'injects exactly one project selector' "$label project slot"
    assert_log_clean "$ssh_log" "$label project slot ssh"
    assert_log_clean "$kb_log" "$label project slot kb"

    : >"$ssh_log"
    : >"$kb_log"

    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=client-host \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_REMOTE_HOSTNAME_VALUE=hax \
      FAKE_SSH_MODE=execute \
      HAX_SSH_HOST=ssh-alias \
      HAX_KB_BIN="$fakebin/kb" \
      PATH="$fakebin:$PATH" \
      "$kb_board" "${command_case[@]}")
    assert_contains "$output" 'injects exactly one project selector' "$label command slot"
    assert_log_clean "$ssh_log" "$label command slot ssh"
    assert_log_clean "$kb_log" "$label command slot kb"

    : >"$ssh_log"
    : >"$kb_log"

    output=$(run_expect_failure env \
      FAKE_HOSTNAME_VALUE=client-host \
      FAKE_SSH_LOG="$ssh_log" \
      FAKE_KB_LOG="$kb_log" \
      FAKE_REMOTE_PATH="$fakebin" \
      FAKE_REMOTE_HOSTNAME_VALUE=hax \
      FAKE_SSH_MODE=execute \
      HAX_SSH_HOST=ssh-alias \
      HAX_KB_BIN="$fakebin/kb" \
      PATH="$fakebin:$PATH" \
      "$kb_board" "${trailing_case[@]}")
    assert_contains "$output" 'injects exactly one project selector' "$label trailing slot"
    assert_log_clean "$ssh_log" "$label trailing slot ssh"
    assert_log_clean "$kb_log" "$label trailing slot kb"
  done
}

test_kb_board_refuses_registry_commands() {
  local output
  output=$(run_expect_failure "$kb_board" alpha r ls)
  assert_contains "$output" 'use hax-kb directly for registry rules' 'r command'

  output=$(run_expect_failure "$kb_board" alpha rule ls)
  assert_contains "$output" 'use hax-kb directly for registry rules' 'rule command'
}

main() {
  if [ "$actual_hostname" = hax ]; then
    test_hax_kb_execs_locally_on_hax_without_ssh
    test_kb_board_execs_directly_on_hax
  else
    test_raw_hax_kb_remote_transport_preserves_argv
    test_hax_kb_remote_transport_preserves_argv
    test_hax_kb_remote_round_trips_weird_argv
    test_hax_kb_remote_rejects_non_hax_remote_hostname
    test_kb_board_remote_transport_preserves_literals
  fi
  test_kb_board_refuses_missing_project_or_command
  test_kb_board_rejects_forbidden_selectors_everywhere
  test_kb_board_refuses_registry_commands
  printf 'ok: kb wrapper tests passed\n'
}

main "$@"
