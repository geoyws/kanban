#!/usr/bin/env python3
"""Transport benchmark for the representative agent loop against a kanban board home.

Contract: docs/testing/graphql-agent-loop-benchmark.md. This script is the
reproducible half of that contract: it reads the version-controlled fixture,
runs each transport arm with its own cold setup, warmups and measured
iterations, and writes one receipt JSON.

Two halves, measured separately, reported side by side.

The READ half is the twelve reads of one loop. It writes nothing: every read in
the fixture is a read-only CLI operation, and the MCP arms call the same
operations through `kb mcp`, which runs the same binary per tool call -- the
`mcp-batch` arm carrying every batchable read of a loop in one `tools/call
batch` instead of one call each.

The WRITE half (fixture v3 and later) is the four board writes around one unit
of work -- claim, checkpoint under that lease, note, release -- as ONE `kanban
transact` and as the same four commands one at a time. It never writes the
frozen fixture board: every arm runs against a disposable copy this driver makes
per iteration (`mktemp -d`, `cp -r`, `chmod -R u+w`) and removes afterwards,
addressed with `--db` because the copied registry still names the frozen board
by absolute path. The copy and the removal are measured separately and are
excluded from the loop time.

Stdlib only. One command from a clean checkout:

    python3 docs/testing/bench/agent-loop-bench.py --fixture docs/testing/bench/fixture-v3.json --out docs/testing/<receipt>.json

And, against no host at all, the exact remote command lines every arm issues:

    python3 docs/testing/bench/agent-loop-bench.py --fixture docs/testing/bench/fixture-v3.json --dry-run
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import platform
import re
import shlex
import statistics
import subprocess
import sys
import time
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
RECEIPT_SCHEMA = "kanban-agent-loop-benchmark-receipt/1"
WRITES_SCHEMA = "kanban-agent-loop-benchmark-writes/1"
# What `--dry-run` substitutes for the four values a real run learns from the
# host: the claimed task, the lease token, the scratch directory `mktemp -d`
# answers with, and the board file inside the fixture's data root.
DRY_RUN = {
    "task": "t-dry",
    "token": "dry-lease-token",
    "scratch": "/tmp/kb-bench-scratch-DRYRUN00",
    "board_file": "<board-uuid>.db",
}
# OpenSSH 9 prints `debug1: Authenticated to ...`; OpenSSH 10 drops the prefix.
AUTHENTICATED = re.compile(r"^(?:debug1: )?Authenticated to ", re.M)
# Shapes a committed receipt must never contain: env-style assignments of
# credential-named variables, bearer/webhook material, private key blocks.
SECRET_SHAPE = re.compile(
    r"(?i)(?:[A-Z0-9_]*(?:api[_-]?key|secret|token|password|passwd|webhook)[A-Z0-9_]*=\S)"
    r"|(?:-----BEGIN [A-Z ]*PRIVATE KEY-----)"
    r"|(?:https://discord\.com/api/webhooks/)"
)


def now_iso() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="milliseconds")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def canonical_digest(body: bytes, drop_keys: list[str]) -> tuple[str, str | None]:
    """(raw sha256, normalized sha256 or None when the body is not JSON).

    Normalization: parse, drop the fixture's declared volatile keys (top level,
    and per element of a top-level array),
    re-serialize with sorted keys and no whitespace. The same body over any
    transport yields the same normalized digest; a body that differs in
    anything but the dropped keys does not.
    """
    raw = sha256(body)
    try:
        value = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw, None
    # Volatile keys are dropped at the top level and from every element of a
    # top-level array: `workspace list` answers with an array of workspace rows,
    # and selecting a board stamps each row's lastUsedAt, so the reading itself
    # moves that field on every iteration.
    if isinstance(value, dict):
        for key in drop_keys:
            value.pop(key, None)
    elif isinstance(value, list):
        for element in value:
            if isinstance(element, dict):
                for key in drop_keys:
                    element.pop(key, None)
    canonical = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return raw, sha256(canonical.encode("utf-8"))


def nearest_rank(sorted_values: list[float], percentile: float) -> float | None:
    """Nearest-rank percentile: the ceil(P/100 * n)-th smallest value (1-indexed)."""
    if not sorted_values:
        return None
    rank = max(1, math.ceil(percentile / 100.0 * len(sorted_values)))
    return sorted_values[rank - 1]


def summarize(values: list[float]) -> dict:
    if not values:
        return {"n": 0}
    ordered = sorted(values)
    return {
        "n": len(ordered),
        "min": round(ordered[0], 3),
        "p50": round(nearest_rank(ordered, 50), 3),
        "p95": round(nearest_rank(ordered, 95), 3),
        "p99": round(nearest_rank(ordered, 99), 3),
        "max": round(ordered[-1], 3),
        "mean": round(statistics.fmean(ordered), 3),
        "stdev": round(statistics.pstdev(ordered), 3) if len(ordered) > 1 else 0.0,
    }


def run(cmd: list[str], timeout: float, stdin: bytes | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, input=stdin, capture_output=True, timeout=timeout, check=False)


# --------------------------------------------------------------------------- probes


def build_processes() -> dict:
    """Count local cargo/rustc processes by process NAME only.

    Never `pgrep -f`: matching full command lines pulls in unrelated processes
    whose argv embeds an environment, and a receipt is committed. Names only.
    """
    names: list[str] = []
    try:
        proc = run(["ps", "-axo", "comm="], 10)
        for line in proc.stdout.decode(errors="replace").splitlines():
            base = os.path.basename(line.strip())
            if base in ("cargo", "rustc", "cargo-clippy", "rust-analyzer", "rustdoc"):
                names.append(base)
    except (OSError, subprocess.TimeoutExpired):
        return {"count": None, "names": ["<ps unavailable>"]}
    return {"count": len(names), "names": sorted(set(names))}


def local_probe() -> dict:
    load1, load5, load15 = os.getloadavg()
    ssh_v = run(["ssh", "-V"], 10)
    return {
        "at": now_iso(),
        "hostname": platform.node(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "ssh_client": (ssh_v.stderr or ssh_v.stdout).decode().strip(),
        "loadavg": [round(load1, 2), round(load5, 2), round(load15, 2)],
        "build_processes": build_processes(),
    }


def rtt_probe(host: str) -> dict:
    """ICMP round trip to the resolved host: the number every fresh SSH handshake pays about ten times."""
    try:
        proc = run(["ping", "-c", "5", "-i", "0.2", host], 30)
    except (OSError, subprocess.TimeoutExpired):
        return {"host": host, "error": "ping unavailable"}
    text = proc.stdout.decode(errors="replace")
    match = re.search(r"= ([\d.]+)/([\d.]+)/([\d.]+)/([\d.]+) ms", text)
    if not match:
        return {"host": host, "error": text.strip()[-300:]}
    return {"host": host, "min_ms": float(match.group(1)), "avg_ms": float(match.group(2)), "max_ms": float(match.group(3)), "stdev_ms": float(match.group(4))}


def effective_ssh_options(target: str) -> dict:
    wanted = {
        "hostname", "user", "port", "identityfile", "compression", "controlmaster",
        "controlpath", "controlpersist", "addressfamily", "gssapiauthentication",
        "batchmode", "proxycommand", "proxyjump", "canonicalizehostname",
    }
    out = run(["ssh", "-G", target], 20).stdout.decode()
    found: dict[str, list[str]] = {}
    for line in out.splitlines():
        key, _, value = line.partition(" ")
        if key in wanted:
            found.setdefault(key, []).append(value)
    return {k: (v[0] if len(v) == 1 else v) for k, v in sorted(found.items())}


REMOTE_PROBE = r"""
set -u
K=%(kb)s
printf 'hostname=%%s\n' "$(hostname)"
printf 'loadavg=%%s\n' "$(cat /proc/loadavg)"
printf 'uptime=%%s\n' "$(uptime)"
printf 'nproc=%%s\n' "$(nproc)"
printf 'kb_version=%%s\n' "$("$K" version 2>&1)"
R=$(readlink -f "$K")
printf 'kb_resolved=%%s\n' "$R"
printf 'kb_sha256=%%s\n' "$(sha256sum "$R" | cut -d' ' -f1)"
printf 'kb_mtime=%%s\n' "$(stat -c %%y "$R")"
printf 'ssh_server=%%s\n' "$(ssh -V 2>&1)"
printf 'os=%%s\n' "$(. /etc/os-release && printf '%%s' "$PRETTY_NAME")"
DB=$("$K" workspace list --json 2>/dev/null | jq -r --arg n %(board)s '.[] | select(.name == $n) | .boardPath' | head -n 1)
printf 'board_path=%%s\n' "$DB"
if [ -n "$DB" ] && [ -f "$DB" ]; then
  printf 'board_bytes=%%s\n' "$(stat -c %%s "$DB")"
  printf 'board_mtime=%%s\n' "$(stat -c %%y "$DB")"
  printf 'board_wal=%%s\n' "$([ -f "$DB-wal" ] && stat -c %%s "$DB-wal" || printf absent)"
fi
printf 'meminfo=%%s\n' "$(grep -E '^(MemTotal|MemFree|Cached|Buffers)' /proc/meminfo | tr -s ' ' | tr '\n' ';')"
"""


def remote_probe(fx: dict) -> dict:
    script = REMOTE_PROBE % {"kb": shlex.quote(fx["kb_exec"]), "board": shlex.quote(fx["board"])}
    cmd = [
        "ssh", "-o", "BatchMode=yes", "-o", "ControlMaster=no", "-o", "ControlPath=none",
        "--", fx["ssh_target"], "bash", "-s",
    ]
    proc = run(cmd, 60, stdin=script.encode())
    result: dict = {"at": now_iso(), "exit": proc.returncode}
    for line in proc.stdout.decode(errors="replace").splitlines():
        key, sep, value = line.partition("=")
        if sep:
            result[key] = value
    if proc.returncode != 0:
        result["stderr"] = proc.stderr.decode(errors="replace")[-2000:]
    load = result.get("loadavg", "")
    parts = load.split()
    if len(parts) >= 3:
        result["load1"] = float(parts[0])
        result["load5"] = float(parts[1])
        result["load15"] = float(parts[2])
    return result



def rpc_frame(method: str, params: dict | None, request_id: int | None) -> bytes:
    """One newline-delimited JSON-RPC frame, byte for byte as it goes on the pipe.

    Shared by the live arms and by `--dry-run`: a printed frame that is not the
    frame the arm sends would be a worse lie than printing nothing.
    """
    frame: dict = {"jsonrpc": "2.0", "method": method}
    if params is not None:
        frame["params"] = params
    if request_id is not None:
        frame["id"] = request_id
    return (json.dumps(frame, separators=(",", ":")) + "\n").encode()


# --------------------------------------------------------------------------- writes


def pointer(value, path: str):
    """RFC 6901 JSON pointer, in the one spelling `transact`'s `$ref` accepts.

    Same escapes, same "empty names the whole document". It has to be the same
    spelling: the token a per-command arm pulls out of its claim with
    `/leaseToken` is the value `transact` resolves on the host from
    `{"$ref": {"item": 0, "path": "/leaseToken"}}`, and a fixture that meant
    two different things by one pointer would not be comparing the same write.
    """
    if path == "":
        return value
    if not path.startswith("/"):
        raise KeyError(f"{path!r} is not a JSON pointer: it is empty or starts with /")
    current = value
    for raw in path[1:].split("/"):
        token = raw.replace("~1", "/").replace("~0", "~")
        if isinstance(current, list):
            try:
                current = current[int(token)]
            except (ValueError, IndexError) as error:
                raise KeyError(f"{path} does not resolve: {error}") from error
        elif isinstance(current, dict):
            if token not in current:
                raise KeyError(f"{path} does not resolve: no key {token!r}")
            current = current[token]
        else:
            raise KeyError(f"{path} does not resolve: {token!r} is not inside a container")
    return current


def substitute(value, mapping: dict):
    """Replace the fixture's placeholders wherever they appear in a value.

    Substring, not whole-token: `TASK` appears alone in an argv element and
    inside nothing else, while a future note body may want to name the task it
    is about. Keys are never substituted -- a placeholder is a value.
    """
    if isinstance(value, str):
        for name, replacement in mapping.items():
            value = value.replace(name, replacement)
        return value
    if isinstance(value, list):
        return [substitute(item, mapping) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item, mapping) for key, item in value.items()}
    return value


def produced(entry: dict, body: bytes) -> dict:
    """The placeholders one per-command write hands to the writes after it.

    `claim` declares `{"TOKEN": "/leaseToken"}`, so the token is read out of
    the claim's own answer rather than guessed at, and a release that would be
    issued with an empty lease fails the loop here instead of on the host.
    """
    produces = entry.get("produces")
    if not produces:
        return {}
    parsed = json.loads(body.decode("utf-8"))
    resolved = {}
    for name, path in produces.items():
        value = pointer(parsed, path)
        if not isinstance(value, str) or not value:
            raise ValueError(f"{path} answered {value!r}, which is not a token")
        resolved[name] = value
    return resolved


def projected_state(body: bytes, projection: dict) -> dict:
    """The readback's answer reduced to the fixture's declared state.

    Absolute counts rather than deltas, because every iteration starts from a
    fresh copy of the same frozen board: the baseline is a constant, so
    `notes` and `checkpoints` are directly comparable across arms.
    """
    parsed = json.loads(body.decode("utf-8"))
    state = {}
    for name, spec in projection.items():
        value = pointer(parsed, spec["pointer"])
        kind = spec["as"]
        if kind == "value":
            state[name] = value
        elif kind == "is-null":
            state[name] = value is None
        elif kind == "length":
            if not isinstance(value, (list, dict, str)):
                raise ValueError(f"{spec['pointer']} answered {type(value).__name__}, which has no length")
            state[name] = len(value)
        else:
            raise ValueError(f"unknown state projection {kind!r}")
    return state


def transact_envelope_error(body: bytes, items: int, exited_ok: bool | None = None) -> str | None:
    """Why this `transact` answer is not four writes that landed, or None.

    ADR-041: the envelope is `{batchId, ok, failedIndex, rolledBack,
    results[]}`, one result per item, and the exit status is non-zero exactly
    when `ok` is false. All of it is checked, the exit status included, because
    a rolled-back batch that reported success would leave the readback state
    equal across arms -- equal and wrong.
    """
    try:
        envelope = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        return f"unreadable transact envelope: {error}: {body[:300]!r}"
    ok = bool(envelope.get("ok"))
    if exited_ok is not None and ok != exited_ok:
        return (
            f"the envelope says ok={envelope.get('ok')!r} and the CLI exited "
            f"{'0' if exited_ok else 'non-zero'}; transact must exit non-zero exactly when ok is false"
        )
    if not ok:
        return (
            f"transact failed at index {envelope.get('failedIndex')} "
            f"(rolledBack={envelope.get('rolledBack')}): {json.dumps(envelope.get('results', []))[:400]}"
        )
    results = envelope.get("results")
    if not isinstance(results, list):
        return f"transact answered no results array: {json.dumps(envelope)[:300]}"
    if len(results) != items:
        return f"transact answered {len(results)} results for {items} items"
    refused = [result for result in results if not result.get("ok")]
    if refused:
        return "transact reported ok with a refused item: " + json.dumps(refused)[:400]
    return None


class WritePlan:
    """One iteration's disposable copy of the frozen fixture, and the task in it."""

    __slots__ = ("task", "scratch", "db")

    def __init__(self, task: str, scratch: str, db: str):
        self.task = task
        self.scratch = scratch
        self.db = db


class WriteStep:
    """One write of the write half: one command, or one whole `transact`."""

    __slots__ = ("id", "ok", "ms", "bytes_sent", "bytes_received", "body", "error")

    def __init__(self, id: str, ok: bool, ms: float, bytes_sent: int, bytes_received: int, body: bytes, error: str | None):
        self.id = id
        self.ok = ok
        self.ms = ms
        self.bytes_sent = bytes_sent
        self.bytes_received = bytes_received
        self.body = body
        self.error = error

    def json(self) -> dict:
        return {
            "id": self.id,
            "ok": self.ok,
            "ms": round(self.ms, 3),
            "bytes_sent": self.bytes_sent,
            "bytes_received": self.bytes_received,
            "body_bytes": len(self.body),
            "error": self.error,
        }


def cold_ssh_argv(fx: dict) -> list[str]:
    """A fresh, unmultiplexed ssh, whatever the arm's own transport is.

    The scratch lifecycle -- the copy, the readback and the removal -- travels
    here on every arm, for two reasons. It is not what is being measured. And
    an arm's reuse proof counts requests: a readback sent down the MCP pipe
    would make `requests_answered` disagree with `requests_per_write_loop` and
    turn a correct arm's proof into a failure.
    """
    return [
        "ssh", "-o", "BatchMode=yes", "-o", f"Compression={fx['ssh_compression']}",
        "-o", "ControlMaster=no", "-o", "ControlPath=none", "--", fx["ssh_target"],
    ]


def cold_script(fx: dict, script: str) -> subprocess.CompletedProcess:
    return run(cold_ssh_argv(fx) + ["bash", "-s"], float(fx["per_read_timeout_s"]), stdin=script.encode())


def cold_exec(fx: dict, command: str, stdin: bytes | None = None) -> subprocess.CompletedProcess:
    return run(cold_ssh_argv(fx) + [command], float(fx["per_read_timeout_s"]), stdin=stdin)


def scratch_prefix(fx: dict) -> str:
    """The fixed head of the fixture's mktemp template: `/tmp/kb-bench-scratch-`.

    One string, used in two places that must agree: the remote `rm -rf`
    refuses a path that does not start with it, and this driver refuses to send
    one. A destructive command on a production-bearing host does not get to
    trust a variable.
    """
    template = fx["writes"]["scratch"]["mktemp_template"]
    prefix = template.rstrip("X")
    if not prefix or prefix == template:
        raise RuntimeError(f"mktemp_template {template!r} must end in the X's mktemp replaces")
    return prefix


def scratch_prepare_script(fx: dict) -> str:
    """`mktemp -d`, copy the frozen fixture into it, make it writable, print it.

    `cp -r SOURCE/. DEST/` rather than `cp -r SOURCE DEST`: the copy must be
    the data root's contents (registry.db beside boards/), not a directory
    nested inside it. `chmod -R u+w` because the fixture is frozen 0400 and a
    copy of a read-only file is a read-only file.
    """
    scratch = fx["writes"]["scratch"]
    template = shlex.quote(scratch["mktemp_template"])
    source = shlex.quote(scratch["source"])
    return (
        "set -eu\n"
        f"scratch=$(mktemp -d {template})\n"
        f'cp -r {source}/. "$scratch/"\n'
        'chmod -R u+w "$scratch"\n'
        "printf '%s\\n' \"$scratch\"\n"
    )


def prepare_scratch(fx: dict, task: str, board_file: str) -> tuple[WritePlan | None, dict]:
    """One iteration's copy, timed on its own so the loop is not charged for it."""
    started = time.perf_counter_ns()
    try:
        proc = cold_script(fx, scratch_prepare_script(fx))
    except subprocess.TimeoutExpired:
        return None, {"ms": round((time.perf_counter_ns() - started) / 1e6, 3), "error": "timeout"}
    ms = round((time.perf_counter_ns() - started) / 1e6, 3)
    if proc.returncode != 0:
        return None, {"ms": ms, "error": f"rc={proc.returncode} stderr={proc.stderr.decode(errors='replace')[-300:]}"}
    lines = proc.stdout.decode(errors="replace").strip().splitlines()
    scratch = lines[-1].strip() if lines else ""
    prefix = scratch_prefix(fx)
    if not scratch.startswith(prefix) or scratch == prefix:
        return None, {"ms": ms, "error": f"mktemp -d answered {scratch!r}, which is not a path under {prefix}"}
    return WritePlan(task, scratch, f"{scratch}/boards/{board_file}"), {"ms": ms, "error": None, "scratch": scratch}


def scratch_guard(fx: dict, plan: WritePlan, exit_on_refusal: bool) -> str:
    """`rm -rf`, and only ever under the fixture's own mktemp prefix.

    The check is on the host as well as in `prepare_scratch`, because this is a
    destructive command on a production-bearing box and a shell variable is not
    a promise. A path that fails the pattern is reported, not removed.
    """
    prefix = scratch_prefix(fx)
    path = shlex.quote(plan.scratch)
    refusal = f"printf 'refusing to remove %s: not under {prefix}\\n' {path} >&2"
    if exit_on_refusal:
        refusal += " ; exit 1"
    return (
        f"case {path} in\n"
        f"  {prefix}?*) rm -rf {path} ;;\n"
        f"  *) {refusal} ;;\n"
        "esac\n"
    )


def readback_script(fx: dict, plan: WritePlan) -> str:
    """Read the task's state off the copy, then remove the copy.

    One script, so the state is read from a directory that is about to stop
    existing and a second connection is not a second chance to leave one
    behind. The kb exit status survives the removal in `$rc`; a refused removal
    still prints the state and says so on stderr, where the caller reads it as
    a leak.
    """
    readback = fx["writes"]["readback"]
    argv = [fx["kb_exec"], "--db", plan.db]
    argv += substitute(list(readback["argv"]), {"TASK": plan.task}) + ["--json"]
    return (
        "set -u\n"
        f"state=$({shlex.join(argv)})\n"
        "rc=$?\n"
        + scratch_guard(fx, plan, exit_on_refusal=False)
        + "printf '%s' \"$state\"\n"
        'exit "$rc"\n'
    )


def scratch_remove_script(fx: dict, plan: WritePlan) -> str:
    """The removal alone, for the copy the task selection ran against."""
    return "set -u\n" + scratch_guard(fx, plan, exit_on_refusal=True)


def readback_and_cleanup(fx: dict, plan: WritePlan) -> dict:
    """Read the task's state off the copy, then remove the copy. One cold ssh.

    Both in one command because the state is only worth reading from a copy
    that is about to stop existing, and because a second connection is a second
    chance to leave one behind.
    """
    started = time.perf_counter_ns()
    try:
        proc = cold_script(fx, readback_script(fx, plan))
    except subprocess.TimeoutExpired:
        ms = round((time.perf_counter_ns() - started) / 1e6, 3)
        return {"ok": False, "ms": ms, "state": None, "error": "timeout", "scratch_leaked": plan.scratch}
    ms = round((time.perf_counter_ns() - started) / 1e6, 3)
    stderr = proc.stderr.decode(errors="replace")[-300:]
    if proc.returncode != 0:
        return {
            "ok": False, "ms": ms, "state": None,
            "error": f"rc={proc.returncode} stderr={stderr}",
            "scratch_leaked": plan.scratch if "refusing to remove" in stderr else None,
        }
    try:
        state = projected_state(proc.stdout, fx["writes"]["readback"]["state_projection"])
    except (UnicodeDecodeError, json.JSONDecodeError, KeyError, ValueError) as error:
        return {"ok": False, "ms": ms, "state": None, "error": f"unreadable readback: {error}"}
    return {"ok": True, "ms": ms, "state": state, "error": None}


def remove_scratch(fx: dict, plan: WritePlan) -> dict:
    script = scratch_remove_script(fx, plan)
    try:
        proc = cold_script(fx, script)
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": "timeout", "scratch_leaked": plan.scratch}
    if proc.returncode != 0:
        return {"ok": False, "error": proc.stderr.decode(errors="replace")[-300:], "scratch_leaked": plan.scratch}
    return {"ok": True}


def per_command_write(arm, plan: WritePlan) -> list[WriteStep]:
    """The write half as four separate requests, threading the lease by hand.

    Which is the comparison: a per-command arm cannot know the token before the
    claim has answered, so the three writes after it are three more round
    trips over the same link. `transact` resolves that same value on the host
    from a `$ref` and pays for one. A failed step stops the loop -- the writes
    after it are about a lease that was never granted.
    """
    mapping = {"TASK": plan.task}
    steps: list[WriteStep] = []
    for entry in arm.fx["writes"]["per_command"]:
        step = arm.issue_write(plan, substitute(entry, mapping))
        steps.append(step)
        if not step.ok:
            break
        try:
            mapping.update(produced(entry, step.body))
        except (UnicodeDecodeError, json.JSONDecodeError, KeyError, ValueError) as error:
            step.ok = False
            step.error = f"{entry['id']} answered nothing to thread onward: {error}"
            break
    return steps


# --------------------------------------------------------------------------- arms


class ReadResult:
    __slots__ = ("ok", "ms", "bytes_sent", "bytes_received", "body_bytes", "body", "error")

    def __init__(self, ok: bool, ms: float, bytes_sent: int, bytes_received: int, body: bytes, error: str | None):
        self.ok = ok
        self.ms = ms
        self.bytes_sent = bytes_sent
        self.bytes_received = bytes_received
        self.body_bytes = len(body)
        self.body = body
        self.error = error


class Arm:
    """What every arm shares: how a loop's reads are issued, and how many
    requests that takes.

    An arm answers one logical read per fixture entry whatever it does on the
    wire, so the equivalence check can compare read for read. Lazily, as a
    generator, so a loop that loses its transport stops instead of retrying
    eleven more times against a closed pipe.
    """

    def read_all(self, reads: list[dict]):
        for read in reads:
            yield self.read(read)

    def requests_per_loop(self, reads: int) -> int:
        return reads


class SshArm(Arm):
    name = "ssh"
    description = "one fresh ssh connection per logical read, exactly what kb-board/kb-remote do today"

    def __init__(self, fx: dict):
        self.fx = fx
        self.timeout = float(fx["per_read_timeout_s"])

    def ssh_base(self) -> list[str]:
        return [
            "ssh", "-o", "BatchMode=yes", "-o", f"Compression={self.fx['ssh_compression']}",
            "-o", "ControlMaster=no", "-o", "ControlPath=none", "--", self.fx["ssh_target"],
        ]

    def remote_command(self, read: dict) -> str:
        argv = [self.fx["kb_exec"]]
        if read.get("board_scoped", True):
            argv += ["--project", self.fx["board"]]
        argv += list(read["argv"]) + ["--json"]
        return shlex.join(argv)

    def transport(self) -> dict:
        return {
            "kind": "ssh-per-read",
            "ssh_argv_prefix": self.ssh_base(),
            "compression": self.fx["ssh_compression"],
            "note": "the deployed kb-remote wrapper additionally wraps every call in `bash -c` with a hostname guard (kb-remote:290); the bench checks the hostname once in setup and runs kb directly",
        }

    def setup(self) -> dict:
        started = time.perf_counter_ns()
        probe = run(self.ssh_base() + [shlex.join([self.fx["kb_exec"], "version"]) + " && hostname"], self.timeout)
        ms = (time.perf_counter_ns() - started) / 1e6
        out = probe.stdout.decode(errors="replace").strip().splitlines()
        hostname = out[-1] if out else ""
        if probe.returncode != 0 or hostname != self.fx["expected_remote_hostname"]:
            raise RuntimeError(f"setup probe failed rc={probe.returncode} hostname={hostname!r} stderr={probe.stderr.decode(errors='replace')[-500:]}")
        return {"cold_probe_ms": round(ms, 3), "kb_version": out[0] if out else "", "remote_hostname": hostname}

    def read(self, read: dict) -> ReadResult:
        command = self.remote_command(read)
        started = time.perf_counter_ns()
        try:
            proc = run(self.ssh_base() + [command], self.timeout)
        except subprocess.TimeoutExpired:
            return ReadResult(False, (time.perf_counter_ns() - started) / 1e6, len(command.encode()), 0, b"", "timeout")
        ms = (time.perf_counter_ns() - started) / 1e6
        ok = proc.returncode == 0
        error = None if ok else f"rc={proc.returncode} stderr={proc.stderr.decode(errors='replace')[-500:]}"
        return ReadResult(ok, ms, len(command.encode()), len(proc.stdout), proc.stdout, error)

    def prove_reuse(self, phase: str) -> dict:
        """Run one fixture read with -vv and count authentications: a fresh connection per read authenticates every time."""
        read = self.fx["reads"][0]
        proc = run(self.ssh_base()[:1] + ["-vv"] + self.ssh_base()[1:] + [self.remote_command(read)], self.timeout)
        err = proc.stderr.decode(errors="replace")
        return {
            "phase": phase,
            "method": "ssh -vv on one fixture read; count `Authenticated to` lines and mux client lines",
            "authenticated_lines": len(AUTHENTICATED.findall(err)),
            "mux_lines": len(re.findall(r"mux_client", err)),
        }

    def reuse_summary(self, before: dict, after: dict, measured_reads: int, teardown: dict) -> dict:
        fresh = all(p["authenticated_lines"] == 1 and p["mux_lines"] == 0 for p in (before, after))
        return {
            "reused": False,
            "fresh_connection_proven": fresh,
            "how_proven": "every read spawns a new ssh with ControlMaster=no ControlPath=none; the -vv probes before and after authenticated exactly once per invocation and used no mux client",
            "connections_per_loop": len(self.fx["reads"]),
            "evidence": [before, after],
        }

    def teardown(self) -> dict:
        return {}

    # ---- write half ------------------------------------------------------

    def requests_per_write_loop(self) -> int:
        return len(self.fx["writes"]["per_command"])

    def write_command(self, plan: WritePlan, argv: list[str]) -> str:
        """One write of the loop, addressed at the iteration's copy.

        `--db` and never `--project`: the copied registry still names the
        frozen board by absolute path, so the board name would open the file
        this half exists not to touch. The fixture's `scratch` block carries
        the whole argument.
        """
        return shlex.join([self.fx["kb_exec"], "--db", plan.db] + list(argv) + ["--json"])

    def issue_write(self, plan: WritePlan, entry: dict) -> WriteStep:
        command = self.write_command(plan, entry["argv"])
        sent = len(command.encode())
        started = time.perf_counter_ns()
        try:
            proc = run(self.ssh_base() + [command], self.timeout)
        except subprocess.TimeoutExpired:
            return WriteStep(entry["id"], False, (time.perf_counter_ns() - started) / 1e6, sent, 0, b"", "timeout")
        ms = (time.perf_counter_ns() - started) / 1e6
        ok = proc.returncode == 0
        error = None if ok else f"rc={proc.returncode} stderr={proc.stderr.decode(errors='replace')[-500:]}"
        return WriteStep(entry["id"], ok, ms, sent, len(proc.stdout), proc.stdout, error)

    def write_loop(self, plan: WritePlan) -> list[WriteStep]:
        return per_command_write(self, plan)

    def read_dry_run(self) -> list[str]:
        return [shlex.join(self.ssh_base() + [self.remote_command(read)]) for read in self.fx["reads"]]

    def write_dry_run(self, plan: WritePlan) -> list[str]:
        mapping = {"TASK": plan.task, "TOKEN": DRY_RUN["token"]}
        return [
            shlex.join(self.ssh_base() + [self.write_command(plan, substitute(entry, mapping)["argv"])])
            for entry in self.fx["writes"]["per_command"]
        ]

    def write_reuse_summary(self, before: dict, after: dict, loops: int, teardown: dict) -> dict:
        fresh = all(p["authenticated_lines"] == 1 and p["mux_lines"] == 0 for p in (before, after))
        return {
            "reused": False,
            "fresh_connection_proven": fresh,
            "how_proven": "every write spawns a new ssh with ControlMaster=no ControlPath=none; the -vv probes before and after authenticated exactly once per invocation and used no mux client",
            "connections_per_write_loop": self.requests_per_write_loop(),
            "loops_counted": loops,
            "evidence": [before, after],
        }


class ControlMasterArm(SshArm):
    name = "ssh-controlmaster"
    description = "one ssh connection per logical read multiplexed over a per-invocation ControlMaster (flags only, no ~/.ssh/config change)"

    def ssh_base(self) -> list[str]:
        return [
            "ssh", "-o", "BatchMode=yes", "-o", f"Compression={self.fx['ssh_compression']}",
            "-o", "ControlMaster=auto", "-o", f"ControlPath={self.fx['controlmaster_path']}",
            "-o", f"ControlPersist={self.fx['controlmaster_persist']}", "--", self.fx["ssh_target"],
        ]

    def control(self, op: str) -> subprocess.CompletedProcess:
        return run(["ssh", "-o", f"ControlPath={self.fx['controlmaster_path']}", "-O", op, self.fx["ssh_target"]], 30)

    def transport(self) -> dict:
        t = super().transport()
        t["kind"] = "ssh-per-read-over-controlmaster"
        t["ssh_argv_prefix"] = self.ssh_base()
        return t

    def master_pid(self) -> int | None:
        check = self.control("check")
        text = (check.stderr + check.stdout).decode(errors="replace")
        match = re.search(r"Master running \(pid=(\d+)\)", text)
        return int(match.group(1)) if match else None

    def setup(self) -> dict:
        # Cold: make sure no master from an earlier run is alive, then let the probe create one.
        if self.master_pid() is not None:
            self.control("exit")
            time.sleep(0.2)
        if self.master_pid() is not None:
            raise RuntimeError("a ControlMaster for this ControlPath survived `-O exit`; refusing a warm cold-setup")
        result = super().setup()
        pid = self.master_pid()
        if pid is None:
            raise RuntimeError("the cold probe did not leave a ControlMaster running")
        result["master_pid"] = pid
        return result

    def prove_reuse(self, phase: str) -> dict:
        proof = super().prove_reuse(phase)
        proof["master_pid"] = self.master_pid()
        return proof

    def reuse_summary(self, before: dict, after: dict, measured_reads: int, teardown: dict) -> dict:
        same = before.get("master_pid") is not None and before.get("master_pid") == after.get("master_pid")
        return {
            "reused": same and after["authenticated_lines"] == 0 and after["mux_lines"] > 0,
            "how_proven": "`ssh -O check` reports the same master pid before and after the measured iterations, and a -vv read through it shows mux_client lines and zero `Authenticated to` lines",
            "connections_per_loop": 0,
            "master_connections": 1,
            "evidence": [before, after],
        }

    def write_reuse_summary(self, before: dict, after: dict, loops: int, teardown: dict) -> dict:
        same = before.get("master_pid") is not None and before.get("master_pid") == after.get("master_pid")
        return {
            "reused": same and after["authenticated_lines"] == 0 and after["mux_lines"] > 0,
            "how_proven": "`ssh -O check` reports the same master pid before and after the measured write iterations, and a -vv read through it shows mux_client lines and zero `Authenticated to` lines",
            "connections_per_write_loop": 0,
            "master_connections": 1,
            "evidence": [before, after],
        }

    def teardown(self) -> dict:
        exit_ = self.control("exit")
        text = (exit_.stderr + exit_.stdout).decode(errors="replace").strip()
        time.sleep(0.2)
        return {"control_exit": text, "master_after_exit": self.master_pid()}


class McpArm(Arm):
    name = "mcp-over-ssh"
    description = "one persistent `ssh <target> kb mcp` child; every logical read is a tools/call on that stdio pipe"

    def __init__(self, fx: dict):
        self.fx = fx
        self.timeout = float(fx["per_read_timeout_s"])
        self.proc: subprocess.Popen | None = None
        self.next_id = 0
        self.requests = 0
        self.stderr_path = os.path.join(HERE, ".mcp-ssh-stderr.log")

    def ssh_argv(self) -> list[str]:
        return [
            "ssh", "-v", "-o", "BatchMode=yes", "-o", f"Compression={self.fx['ssh_compression']}",
            "-o", "ControlMaster=no", "-o", "ControlPath=none", "--", self.fx["ssh_target"],
            shlex.join([self.fx["kb_exec"], "mcp"]),
        ]

    def transport(self) -> dict:
        return {
            "kind": "mcp-stdio-over-persistent-ssh",
            "ssh_argv": self.ssh_argv(),
            "compression": self.fx["ssh_compression"],
            "protocol": "JSON-RPC 2.0, newline-delimited, MCP protocolVersion 2024-11-05 (rust/mcp.rs)",
            "note": "the -v flag is on the one long-lived ssh so its stderr proves a single authentication; -v prints nothing per tools/call",
        }

    def _rpc(self, method: str, params: dict | None, notification: bool = False) -> tuple[bytes, bytes | None]:
        assert self.proc is not None and self.proc.stdin is not None and self.proc.stdout is not None
        if not notification:
            self.next_id += 1
        line = rpc_frame(method, params, None if notification else self.next_id)
        self.proc.stdin.write(line)
        self.proc.stdin.flush()
        if notification:
            return line, None
        response = self.proc.stdout.readline()
        if not response:
            raise RuntimeError("mcp server closed the pipe")
        return line, response

    def setup(self) -> dict:
        started = time.perf_counter_ns()
        self.stderr_file = open(self.stderr_path, "wb")
        self.proc = subprocess.Popen(self.ssh_argv(), stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr_file)
        _, response = self._rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "agent-loop-bench", "version": "1"},
        })
        self._rpc("notifications/initialized", None, notification=True)
        ms = (time.perf_counter_ns() - started) / 1e6
        init = json.loads(response)
        if "error" in init:
            raise RuntimeError(f"initialize failed: {init['error']}")
        # The hostname guard, through the same pipe: `stale` is the cheapest board read and refuses a wrong board.
        return {
            "cold_probe_ms": round(ms, 3),
            "server_info": init["result"].get("serverInfo"),
            "protocol_version": init["result"].get("protocolVersion"),
            "ssh_pid": self.proc.pid,
        }

    def _tool_call(self, tool: str, arguments: dict) -> ReadResult:
        """One `tools/call` on this arm's pipe, timed and unwrapped.

        Both halves go through here so a read and a write are measured at the
        same two points and counted in the same `self.requests`, which is what
        the reuse proof compares against `requests_per_loop`.
        """
        started = time.perf_counter_ns()
        try:
            line, response = self._rpc("tools/call", {"name": tool, "arguments": arguments})
        except RuntimeError as error:
            return ReadResult(False, (time.perf_counter_ns() - started) / 1e6, 0, 0, b"", str(error))
        ms = (time.perf_counter_ns() - started) / 1e6
        self.requests += 1
        assert response is not None
        try:
            parsed = json.loads(response)
        except json.JSONDecodeError as error:
            return ReadResult(False, ms, len(line), len(response), b"", f"invalid frame: {error}")
        if "error" in parsed:
            return ReadResult(False, ms, len(line), len(response), b"", f"jsonrpc error: {parsed['error']}")
        result = parsed.get("result", {})
        text = "".join(part.get("text", "") for part in result.get("content", []))
        body = text.encode("utf-8")
        if result.get("isError"):
            return ReadResult(False, ms, len(line), len(response), body, f"isError: {text[:500]}")
        return ReadResult(True, ms, len(line), len(response), body, None)

    def read(self, read: dict) -> ReadResult:
        args = dict(read["args"])
        if read.get("board_scoped", True):
            args["project"] = self.fx["board"]
        return self._tool_call(read["tool"], args)

    def prove_reuse(self, phase: str) -> dict:
        assert self.proc is not None
        return {
            "phase": phase,
            "method": "the ssh child pid and liveness, and the count of JSON-RPC requests answered on its pipe so far",
            "ssh_pid": self.proc.pid,
            "alive": self.proc.poll() is None,
            "requests_answered": self.requests,
        }

    def reuse_summary(self, before: dict, after: dict, measured_reads: int, teardown: dict) -> dict:
        one_pipe = before["ssh_pid"] == after["ssh_pid"] and after["alive"] and after["requests_answered"] - before["requests_answered"] == measured_reads
        return {
            "reused": one_pipe and teardown.get("authenticated_lines") == 1,
            "how_proven": "same ssh child pid alive before and after, every measured read answered on that one pipe, and the child's -v stderr contains exactly one `Authenticated to` line (checked at teardown)",
            "connections_per_loop": 0,
            "persistent_connections": 1,
            "evidence": [before, after],
        }

    # ---- write half ------------------------------------------------------

    def first_request_id(self) -> int:
        """The id of this arm's first measured request: `initialize` took 1.

        Only `--dry-run` needs it, and it needs it to be right: a printed frame
        with an invented id is a frame this arm never sends.
        """
        return 2

    def requests_per_write_loop(self) -> int:
        return len(self.fx["writes"]["per_command"])

    def write_arguments(self, plan: WritePlan, entry: dict) -> dict:
        return {**entry["args"], "db": plan.db}

    def issue_write(self, plan: WritePlan, entry: dict) -> WriteStep:
        result = self._tool_call(entry["tool"], self.write_arguments(plan, entry))
        return WriteStep(entry["id"], result.ok, result.ms, result.bytes_sent, result.bytes_received, result.body, result.error)

    def write_loop(self, plan: WritePlan) -> list[WriteStep]:
        return per_command_write(self, plan)

    def read_dry_run(self) -> list[str]:
        lines = [shlex.join(self.ssh_argv())]
        for index, read in enumerate(self.fx["reads"]):
            args = dict(read["args"])
            if read.get("board_scoped", True):
                args["project"] = self.fx["board"]
            frame = rpc_frame("tools/call", {"name": read["tool"], "arguments": args}, self.first_request_id() + index)
            lines.append(frame.decode().rstrip())
        return lines

    def write_dry_run(self, plan: WritePlan) -> list[str]:
        mapping = {"TASK": plan.task, "TOKEN": DRY_RUN["token"]}
        lines = [shlex.join(self.ssh_argv())]
        for index, entry in enumerate(self.fx["writes"]["per_command"]):
            resolved = substitute(entry, mapping)
            frame = rpc_frame(
                "tools/call",
                {"name": resolved["tool"], "arguments": self.write_arguments(plan, resolved)},
                self.first_request_id() + index,
            )
            lines.append(frame.decode().rstrip())
        return lines

    def write_reuse_summary(self, before: dict, after: dict, loops: int, teardown: dict) -> dict:
        expected = loops * self.requests_per_write_loop()
        answered = after["requests_answered"] - before["requests_answered"]
        one_pipe = before["ssh_pid"] == after["ssh_pid"] and after["alive"] and answered == expected
        return {
            "reused": one_pipe and teardown.get("authenticated_lines") == 1,
            "how_proven": "same ssh child pid alive before and after, every write of every warmup and measured loop answered on that one pipe, and the child's -v stderr contains exactly one `Authenticated to` line (checked at teardown)",
            "connections_per_write_loop": 0,
            "requests_per_write_loop": self.requests_per_write_loop(),
            "loops_counted": loops,
            "requests_expected": expected,
            "requests_answered": answered,
            "persistent_connections": 1,
            "evidence": [before, after],
        }

    def teardown(self) -> dict:
        assert self.proc is not None
        try:
            self.proc.stdin.close()  # type: ignore[union-attr]
            self.proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.stderr_file.close()
        with open(self.stderr_path, "rb") as handle:
            err = handle.read().decode(errors="replace")
        os.unlink(self.stderr_path)
        return {
            "ssh_exit": self.proc.returncode,
            "authenticated_lines": len(AUTHENTICATED.findall(err)),
            "total_requests": self.requests,
        }


class McpBatchArm(McpArm):
    name = "mcp-batch"
    description = (
        "one persistent `ssh <target> kb mcp` child; every batchable logical read of the loop is carried in a single "
        "`tools/call batch` per iteration, the rest issued as ordinary tools/call on the same pipe. Per-read `ms` for a "
        "batched read is the WHOLE batch's wall time attributed to that read: inside one call there is no per-read "
        "timing to observe, so those numbers sum to far more than `loop_ms` and only `loop_ms` is comparable. Per-read "
        "`bytes_sent`/`bytes_received` are that entry's slice of the request and response frames, so they sum to the "
        "frame minus its envelope rather than to a per-read frame."
    )

    def __init__(self, fx: dict):
        super().__init__(fx)
        self.batchable: list[str] = []
        self.unbatchable: list[str] = []

    def transport(self) -> dict:
        transport = super().transport()
        transport["kind"] = "mcp-stdio-batch-over-persistent-ssh"
        transport["batch_tool"] = "batch (rust/mcp.rs): up to 32 read-only tool calls per request, validated whole before any entry runs, results returned in order"
        transport["note"] = (
            "the -v flag is on the one long-lived ssh so its stderr proves a single authentication; -v prints nothing "
            "per tools/call. A batch carries only tools whose tools/list entry is readOnlyHint true, which is read off "
            "the server at setup rather than assumed here"
        )
        return transport

    def setup(self) -> dict:
        result = super().setup()
        # Which reads may travel in a batch is the server's answer, not this
        # driver's guess: `batch` refuses any tool whose `tools/list` entry is
        # not read-only, and `claim --candidates` is read-only in the CLI while
        # its command row covers every `claim` invocation, plain `claim`
        # included. Asking makes the partition move on its own if a future
        # release splits that row.
        _, response = self._rpc("tools/list", {})
        listed = json.loads(response)
        if "error" in listed:
            raise RuntimeError(f"tools/list failed: {listed['error']}")
        tools = {tool["name"]: tool for tool in listed["result"]["tools"]}
        if "batch" not in tools:
            raise RuntimeError("the served kb has no `batch` tool; this arm needs a build that carries it")
        for read in self.fx["reads"]:
            tool = tools.get(read["tool"])
            if tool is None:
                raise RuntimeError(f"the served kb has no `{read['tool']}` tool")
            if tool.get("annotations", {}).get("readOnlyHint") is True:
                self.batchable.append(read["id"])
            else:
                self.unbatchable.append(read["id"])
        if not self.batchable:
            raise RuntimeError("no fixture read is batchable; there is nothing for this arm to measure")
        result["batchable_reads"] = list(self.batchable)
        result["unbatchable_reads"] = list(self.unbatchable)
        result["unbatchable_reason"] = "the tool's tools/list entry is not readOnlyHint true, so `batch` refuses it; issued as its own tools/call on the same pipe"
        return result

    def requests_per_loop(self, reads: int) -> int:
        return 1 + len(self.unbatchable)

    def arguments_for(self, read: dict) -> dict:
        args = dict(read["args"])
        if read.get("board_scoped", True):
            args["project"] = self.fx["board"]
        return args

    def read_all(self, reads: list[dict]):
        batched = [read for read in reads if read["id"] in self.batchable]
        answers = self._batch(batched)
        for read in reads:
            if read["id"] in answers:
                yield answers[read["id"]]
            else:
                yield self.read(read)

    def _batch(self, reads: list[dict]) -> dict[str, ReadResult]:
        """One `tools/call batch`, split back into one ReadResult per read."""
        calls = [{"name": read["tool"], "arguments": self.arguments_for(read)} for read in reads]
        entry_sent = [len(json.dumps(call, separators=(",", ":")).encode()) for call in calls]
        started = time.perf_counter_ns()
        try:
            line, response = self._rpc("tools/call", {"name": "batch", "arguments": {"calls": calls}})
        except RuntimeError as error:
            ms = (time.perf_counter_ns() - started) / 1e6
            return {read["id"]: ReadResult(False, ms, sent, 0, b"", str(error)) for read, sent in zip(reads, entry_sent)}
        ms = (time.perf_counter_ns() - started) / 1e6
        self.requests += 1
        assert response is not None

        def failed(message: str) -> dict[str, ReadResult]:
            return {read["id"]: ReadResult(False, ms, sent, 0, b"", message) for read, sent in zip(reads, entry_sent)}

        try:
            parsed = json.loads(response)
        except json.JSONDecodeError as error:
            return failed(f"invalid frame: {error}")
        if "error" in parsed:
            return failed(f"jsonrpc error: {parsed['error']}")
        envelope = parsed.get("result", {})
        text = "".join(part.get("text", "") for part in envelope.get("content", []))
        if envelope.get("isError"):
            # The whole batch was refused, so no entry ran: every read in it
            # fails with the server's reason rather than with silence.
            return failed(f"batch refused: {text[:500]}")
        try:
            entries = json.loads(text)["results"]
        except (json.JSONDecodeError, KeyError, TypeError) as error:
            return failed(f"unreadable batch result: {error}: {text[:500]}")
        if len(entries) != len(reads):
            return failed(f"batch answered {len(entries)} results for {len(reads)} calls")
        answers = {}
        for read, sent, entry in zip(reads, entry_sent, entries):
            received = len(json.dumps(entry, separators=(",", ":")).encode())
            result = entry.get("result") if entry.get("ok") else entry.get("error")
            body = "".join(part.get("text", "") for part in (result or {}).get("content", [])).encode("utf-8")
            error = None if entry.get("ok") else f"isError: {body.decode(errors='replace')[:500]}"
            answers[read["id"]] = ReadResult(bool(entry.get("ok")), ms, sent, received, body, error)
        return answers

    def first_request_id(self) -> int:
        # initialize took 1, this arm's setup tools/list took 2.
        return 3

    def read_dry_run(self) -> list[str]:
        calls = [{"name": read["tool"], "arguments": self.arguments_for(read)} for read in self.fx["reads"]]
        frame = rpc_frame("tools/call", {"name": "batch", "arguments": {"calls": calls}}, self.first_request_id())
        return [
            shlex.join(self.ssh_argv()),
            frame.decode().rstrip(),
            "  # printed as if every read were batchable. The real partition is read off tools/list at setup:",
            "  # a read whose tool is not readOnlyHint true leaves the batch and is issued as its own tools/call.",
        ]

    def reuse_summary(self, before: dict, after: dict, measured_reads: int, teardown: dict) -> dict:
        loops = measured_reads // len(self.fx["reads"]) if self.fx["reads"] else 0
        expected = loops * self.requests_per_loop(len(self.fx["reads"]))
        one_pipe = (
            before["ssh_pid"] == after["ssh_pid"]
            and after["alive"]
            and after["requests_answered"] - before["requests_answered"] == expected
        )
        return {
            "reused": one_pipe and teardown.get("authenticated_lines") == 1,
            "how_proven": "same ssh child pid alive before and after, one batch plus one call per unbatchable read answered on that one pipe for every measured loop, and the child's -v stderr contains exactly one `Authenticated to` line (checked at teardown)",
            "connections_per_loop": 0,
            "requests_per_loop": self.requests_per_loop(len(self.fx["reads"])),
            "requests_expected": expected,
            "persistent_connections": 1,
            "evidence": [before, after],
        }


class McpTransactArm(McpArm):
    name = "mcp-transact"
    description = (
        "one persistent `ssh <target> kb mcp` child; the whole write half of a loop carried in a single "
        "`tools/call transact` -- claim, checkpoint, note, release as one ordered, all-or-nothing batch, with the "
        "lease threaded on the host by a `$ref` instead of by a round trip. Write half only: `transact` writes, so "
        "the read-only `batch` refuses it, and the read half is already measured by mcp-batch."
    )

    def transport(self) -> dict:
        transport = super().transport()
        transport["kind"] = "mcp-stdio-transact-over-persistent-ssh"
        transport["transact_tool"] = (
            "transact (rust/mcp.rs:710): up to 32 ordered calls in one transaction, the whole list pre-flighted "
            "before any of it runs, `$ref` resolving an earlier item's answer on the host, every item rolled back "
            "together on any failure (ADR-041)"
        )
        transport["note"] = (
            "the -v flag is on the one long-lived ssh so its stderr proves a single authentication; -v prints "
            "nothing per tools/call. The board selector travels on the transact call itself and never on an item: "
            "an item naming a board of its own is refused (rust/lib.rs:4856)"
        )
        return transport

    def first_request_id(self) -> int:
        # initialize took 1, this arm's setup tools/list took 2.
        return 3

    def setup(self) -> dict:
        result = super().setup()
        # Asked of the server, not assumed: an arm that measures `transact`
        # against a release that does not carry it must say so at setup rather
        # than report a failed iteration thirty times.
        _, response = self._rpc("tools/list", {})
        listed = json.loads(response)
        if "error" in listed:
            raise RuntimeError(f"tools/list failed: {listed['error']}")
        tools = {tool["name"]: tool for tool in listed["result"]["tools"]}
        if "transact" not in tools:
            raise RuntimeError(
                "the served kb has no `transact` tool; this arm needs a build that carries it (kanban 2815693 or newer)"
            )
        if tools["transact"].get("annotations", {}).get("readOnlyHint") is not False:
            raise RuntimeError(
                "the served kb advertises `transact` as read-only, which contradicts ADR-041; refusing to measure a write against it"
            )
        missing = [item["name"] for item in self.fx["writes"]["transact_items"] if item["name"] not in tools]
        if missing:
            raise RuntimeError(f"the served kb has no {missing} tool(s), which this fixture's write half names")
        result["transact_items"] = [item["name"] for item in self.fx["writes"]["transact_items"]]
        return result

    def requests_per_write_loop(self) -> int:
        return 1

    def write_loop(self, plan: WritePlan) -> list[WriteStep]:
        items = substitute(self.fx["writes"]["transact_items"], {"TASK": plan.task})
        result = self._tool_call("transact", {"items": items, "db": plan.db})
        error = result.error or transact_envelope_error(result.body, len(items))
        return [WriteStep("transact", error is None, result.ms, result.bytes_sent, result.bytes_received, result.body, error)]

    def write_dry_run(self, plan: WritePlan) -> list[str]:
        items = substitute(self.fx["writes"]["transact_items"], {"TASK": plan.task})
        frame = rpc_frame(
            "tools/call",
            {"name": "transact", "arguments": {"items": items, "db": plan.db}},
            self.first_request_id(),
        )
        return [shlex.join(self.ssh_argv()), frame.decode().rstrip()]


class SshTransactArm(SshArm):
    name = "ssh-transact"
    description = (
        "one fresh ssh connection carrying the whole write half: `kanban transact --items-file /dev/stdin` with the "
        "item list on stdin. One handshake, one process, one transaction. The list travels on stdin rather than in "
        "`--items` because an argv string has a size limit a batch does not (ADR-041 section 8). Write half only."
    )

    def transport(self) -> dict:
        transport = super().transport()
        transport["kind"] = "ssh-transact-per-write-loop"
        transport["note"] = (
            "one exec per write loop, ControlMaster=no ControlPath=none, the item list on stdin as "
            "`--items-file /dev/stdin`; the board selector travels on the transact command itself and never on an "
            "item (rust/lib.rs:4856)"
        )
        return transport

    def requests_per_write_loop(self) -> int:
        return 1

    def transact_command(self, plan: WritePlan) -> str:
        return shlex.join([self.fx["kb_exec"], "--db", plan.db, "transact", "--items-file", "/dev/stdin", "--json"])

    def write_loop(self, plan: WritePlan) -> list[WriteStep]:
        items = substitute(self.fx["writes"]["transact_items"], {"TASK": plan.task})
        payload = json.dumps(items, separators=(",", ":")).encode()
        command = self.transact_command(plan)
        # The command string and the item list both crossed the link, so both
        # are what this arm sent.
        sent = len(command.encode()) + len(payload)
        started = time.perf_counter_ns()
        try:
            proc = run(self.ssh_base() + [command], self.timeout, stdin=payload)
        except subprocess.TimeoutExpired:
            return [WriteStep("transact", False, (time.perf_counter_ns() - started) / 1e6, sent, 0, b"", "timeout")]
        ms = (time.perf_counter_ns() - started) / 1e6
        error = transact_envelope_error(proc.stdout, len(items), proc.returncode == 0)
        if error is None and proc.returncode != 0:
            error = f"rc={proc.returncode} stderr={proc.stderr.decode(errors='replace')[-500:]}"
        return [WriteStep("transact", error is None, ms, sent, len(proc.stdout), proc.stdout, error)]

    def write_dry_run(self, plan: WritePlan) -> list[str]:
        items = substitute(self.fx["writes"]["transact_items"], {"TASK": plan.task})
        return [
            shlex.join(self.ssh_base() + [self.transact_command(plan)]),
            "  # on stdin: " + json.dumps(items, separators=(",", ":")),
        ]


# The read half's arms, unchanged from v2.
ARMS = {
    SshArm.name: SshArm,
    ControlMasterArm.name: ControlMasterArm,
    McpArm.name: McpArm,
    McpBatchArm.name: McpBatchArm,
}

# The write half's arms. The three transports the read half shares issue the
# four writes one at a time, threading the lease token by hand; the two
# transact arms issue the same unit of work as one request. `mcp-batch` is
# absent on purpose: `batch` refuses an entry whose tools/list row is not
# readOnlyHint true, so it has no write half to measure.
WRITE_ARMS = {
    SshArm.name: SshArm,
    ControlMasterArm.name: ControlMasterArm,
    McpArm.name: McpArm,
    McpTransactArm.name: McpTransactArm,
    SshTransactArm.name: SshTransactArm,
}

# Derived from the two tables rather than written a second time: an arm that
# exists is an arm `--arms` offers, and neither half can gain or lose one
# silently.
READ_ARM_NAMES = tuple(ARMS)
WRITE_ARM_NAMES = tuple(WRITE_ARMS)
ARM_NAMES = READ_ARM_NAMES + tuple(name for name in WRITE_ARM_NAMES if name not in READ_ARM_NAMES)


# --------------------------------------------------------------------------- loop


def run_loop(arm, reads: list[dict]) -> dict:
    loop_started = time.perf_counter_ns()
    results = []
    for read, result in zip(reads, arm.read_all(reads)):
        drop = read.get("normalize_drop_keys", [])
        raw, normalized = canonical_digest(result.body, drop) if result.ok else (None, None)
        results.append({
            "id": read["id"],
            "ok": result.ok,
            "ms": round(result.ms, 3),
            "bytes_sent": result.bytes_sent,
            "bytes_received": result.bytes_received,
            "body_bytes": result.body_bytes,
            "sha256_raw": raw,
            "sha256_normalized": normalized,
            "error": result.error,
        })
        if not result.ok and isinstance(arm, McpArm) and result.error and "closed the pipe" in result.error:
            break
    loop_ms = (time.perf_counter_ns() - loop_started) / 1e6
    return {
        "ok": all(r["ok"] for r in results) and len(results) == len(reads),
        "loop_ms": round(loop_ms, 3),
        "bytes_sent": sum(r["bytes_sent"] for r in results),
        "bytes_received": sum(r["bytes_received"] for r in results),
        "body_bytes": sum(r["body_bytes"] for r in results),
        "reads": results,
    }


def run_arm(name: str, fx: dict, warmups: int, iterations: int, log) -> dict:
    arm = ARMS[name](fx)
    reads = fx["reads"]
    receipt: dict = {
        "arm": name,
        "description": arm.description,
        "transport": arm.transport(),
        "started_at": now_iso(),
        "reads_per_loop": len(reads),
    }
    log(f"[{name}] remote probe (before)")
    receipt["host_before"] = remote_probe(fx)
    receipt["client_before"] = local_probe()
    log(f"[{name}] cold setup")
    try:
        receipt["setup"] = arm.setup()
    except Exception as error:  # noqa: BLE001 - the receipt must carry the reason
        receipt["setup"] = {"error": str(error)}
        receipt["aborted"] = f"setup failed: {error}"
        receipt["finished_at"] = now_iso()
        try:
            receipt["teardown"] = arm.teardown()
        except Exception as teardown_error:  # noqa: BLE001
            receipt["teardown"] = {"error": str(teardown_error)}
        return receipt
    # After setup, because an arm may only learn how many requests one loop
    # takes by asking the server what it will accept.
    receipt["requests_per_loop"] = arm.requests_per_loop(len(reads))
    receipt["reuse_before"] = arm.prove_reuse("before")
    warm = []
    for i in range(warmups):
        loop = run_loop(arm, reads)
        warm.append(loop)
        log(f"[{name}] warmup {i + 1}/{warmups} {loop['loop_ms']:.0f} ms ok={loop['ok']}")
    measured = []
    aborted = None
    for i in range(iterations):
        loop = run_loop(arm, reads)
        measured.append(loop)
        log(f"[{name}] measured {i + 1}/{iterations} {loop['loop_ms']:.0f} ms ok={loop['ok']}")
        if not loop["ok"] and isinstance(arm, McpArm) and any(r["error"] and "closed the pipe" in r["error"] for r in loop["reads"]):
            aborted = f"mcp pipe closed during measured iteration {i + 1}"
            break
    receipt["reuse_after"] = arm.prove_reuse("after")
    receipt["host_after"] = remote_probe(fx)
    receipt["client_after"] = local_probe()
    try:
        receipt["teardown"] = arm.teardown()
    except Exception as error:  # noqa: BLE001
        receipt["teardown"] = {"error": str(error)}
    receipt["finished_at"] = now_iso()
    if aborted:
        receipt["aborted"] = aborted

    ok_loops = [m for m in measured if m["ok"]]
    failures = [
        {"iteration": i + 1, "reads": [r for r in m["reads"] if not r["ok"]]}
        for i, m in enumerate(measured) if not m["ok"]
    ]
    per_read: dict[str, dict] = {}
    for read in reads:
        samples = [r for m in ok_loops for r in m["reads"] if r["id"] == read["id"]]
        digests = sorted({r["sha256_normalized"] for r in samples if r["sha256_normalized"]})
        first_change = None
        if len(digests) > 1:
            initial = samples[0]["sha256_normalized"]
            for idx, r in enumerate(samples):
                if r["sha256_normalized"] != initial:
                    first_change = idx + 1
                    break
        per_read[read["id"]] = {
            "ms": summarize([r["ms"] for r in samples]),
            "bytes_sent": samples[0]["bytes_sent"] if samples else None,
            "bytes_received": summarize([float(r["bytes_received"]) for r in samples]),
            "body_bytes": summarize([float(r["body_bytes"]) for r in samples]),
            "sha256_normalized": digests,
            "sha256_raw": sorted({r["sha256_raw"] for r in samples if r["sha256_raw"]}),
            "drift": {"distinct_digests": len(digests), "first_changed_iteration": first_change},
        }
    measured_reads = sum(len(m["reads"]) for m in measured)
    receipt.update({
        "warmups": {"count": len(warm), "loop_ms": [w["loop_ms"] for w in warm], "ok": sum(1 for w in warm if w["ok"])},
        "measured": {
            "iterations_requested": iterations,
            "iterations_run": len(measured),
            "iterations_ok": len(ok_loops),
            "failures": len(failures),
            "loop_ms": summarize([m["loop_ms"] for m in ok_loops]),
            "loop_ms_samples": [m["loop_ms"] for m in measured],
            "bytes_sent_per_loop": summarize([float(m["bytes_sent"]) for m in ok_loops]),
            "bytes_received_per_loop": summarize([float(m["bytes_received"]) for m in ok_loops]),
            "body_bytes_per_loop": summarize([float(m["body_bytes"]) for m in ok_loops]),
            "per_read": per_read,
        },
        "failure_detail": failures,
        "connection_reuse": arm.reuse_summary(receipt["reuse_before"], receipt["reuse_after"], measured_reads, receipt["teardown"]),
        "host_load": {
            "before": receipt["host_before"].get("load1"),
            "after": receipt["host_after"].get("load1"),
        },
        "client_load": {
            "before": receipt["client_before"]["loadavg"][0],
            "after": receipt["client_after"]["loadavg"][0],
        },
        "compression": fx["ssh_compression"],
        "valid": aborted is None and len(failures) == 0 and len(ok_loops) == iterations,
    })
    return receipt


def equivalence(arms: list[dict], reads: list[dict]) -> dict:
    per_read = []
    all_equivalent = True
    for read in reads:
        by_arm = {}
        for arm in arms:
            if "measured" not in arm:
                continue
            by_arm[arm["arm"]] = arm["measured"]["per_read"][read["id"]]["sha256_normalized"]
        sets = list(by_arm.values())
        singleton = all(len(s) == 1 for s in sets)
        equal = len({tuple(s) for s in sets}) == 1 if sets else False
        equivalent = singleton and equal
        all_equivalent = all_equivalent and equivalent
        per_read.append({"id": read["id"], "digests_by_arm": by_arm, "equivalent": equivalent, "drift_within_an_arm": not singleton})
    return {"per_read": per_read, "all_equivalent": all_equivalent}


def verdict(arms: list[dict], equiv: dict, warmups: int, iterations: int, fx: dict) -> dict:
    reasons = []
    if warmups < fx["warmups"]:
        reasons.append(f"warmups {warmups} < fixture minimum {fx['warmups']}")
    if iterations < fx["iterations"]:
        reasons.append(f"iterations {iterations} < fixture minimum {fx['iterations']}")
    for arm in arms:
        if arm.get("aborted"):
            reasons.append(f"{arm['arm']}: aborted ({arm['aborted']})")
        elif not arm.get("valid"):
            reasons.append(f"{arm['arm']}: {arm['measured']['failures']} failed iteration(s)")
        for probe in ("host_before", "host_after"):
            if arm.get(probe, {}).get("hostname") != fx["expected_remote_hostname"]:
                reasons.append(f"{arm['arm']}: {probe} hostname {arm.get(probe, {}).get('hostname')!r} != {fx['expected_remote_hostname']!r}")
    versions = {(a.get("host_before", {}).get("kb_sha256"), a.get("host_after", {}).get("kb_sha256")) for a in arms}
    if len(versions) != 1 or any(None in v or v[0] != v[1] for v in versions):
        reasons.append("served kb binary changed between probes or across arms")
    boards = {(a.get("host_before", {}).get("board_path"), a.get("host_before", {}).get("board_bytes")) for a in arms}
    if len({b[0] for b in boards}) != 1:
        reasons.append("board path differed across arms")
    if not equiv["all_equivalent"]:
        reasons.append("response equivalence failed: " + ", ".join(r["id"] for r in equiv["per_read"] if not r["equivalent"]))
    return {"comparable": not reasons, "reasons": reasons}


# --------------------------------------------------------------------------- write loop


def frozen_board_check(before: dict, after: dict) -> dict:
    """The frozen fixture board, before and after one arm's write half.

    The scratch rule is an argument; this is the evidence. Same path, same
    size, same mtime across the arm means every write landed on the copy.
    """
    keys = ("board_path", "board_bytes", "board_mtime")
    return {
        "before": {key: before.get(key) for key in keys},
        "after": {key: after.get(key) for key in keys},
        "unchanged": all(before.get(key) is not None and before.get(key) == after.get(key) for key in keys),
    }


def select_write_task(fx: dict, board_file: str, log) -> dict:
    """Choose the task the write half claims, at run time, off a scratch copy.

    Never a hardcoded id: a frozen id may already be claimed or done, and a
    claim that refuses measures nothing. `claim --candidates` is read-only
    (ADR-026, rust/store.rs:4381), and it still runs against a throwaway copy
    that is removed immediately, so the selection is not the one thing that
    reaches the frozen board.
    """
    selection = fx["writes"]["task_selection"]
    plan, copy = prepare_scratch(fx, "t-unselected", board_file)
    if plan is None:
        raise RuntimeError(f"could not copy the fixture to select a task: {copy['error']}")
    command = shlex.join([fx["kb_exec"], "--db", plan.db] + list(selection["argv"]) + ["--json"])
    log(f"[writes] selecting a task: {shlex.join(selection['argv'])}")
    try:
        proc = cold_exec(fx, command)
        if proc.returncode != 0:
            raise RuntimeError(
                f"candidate lookup failed: rc={proc.returncode} stderr={proc.stderr.decode(errors='replace')[-300:]}"
            )
        try:
            candidates = json.loads(proc.stdout.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"unreadable candidate list: {error}") from error
    finally:
        removal = remove_scratch(fx, plan)
    if not isinstance(candidates, list):
        raise RuntimeError(f"candidate lookup answered {type(candidates).__name__}, not a list")
    required = selection.get("require_status")
    chosen = None
    for index, candidate in enumerate(candidates):
        if required is not None and candidate.get("status") != required:
            continue
        chosen = (index, pointer(candidate, selection["id_pointer"]))
        break
    if chosen is None:
        raise RuntimeError(
            f"{selection['on_empty']} ({len(candidates)} candidate(s) answered, none with status {required!r})"
        )
    index, task = chosen
    log(f"[writes] claiming {task} (candidate {index} of {len(candidates)})")
    return {
        "at": now_iso(),
        "task": task,
        "selected_index": index,
        "required_status": required,
        "candidates": [
            {"id": c.get("id"), "status": c.get("status"), "lane": c.get("lane"), "priority": c.get("priority")}
            for c in candidates
        ],
        "command": command,
        "scratch_copy_ms": copy["ms"],
        "scratch_removed": removal.get("ok"),
        "how": selection["run"],
    }


def run_write_iteration(arm, fx: dict, task: str, board_file: str) -> dict:
    """One write iteration: copy, write, read back, remove.

    Only the middle is `write_loop_ms`. The copy and the readback are reported
    beside it as their own numbers, so that they were excluded is checkable
    rather than promised.
    """
    plan, copy = prepare_scratch(fx, task, board_file)
    if plan is None:
        return {
            "ok": False, "write_loop_ms": None, "scratch_copy_ms": copy["ms"], "readback_ms": None,
            "scratch": None, "bytes_sent": 0, "bytes_received": 0, "steps": [], "state": None,
            "error": f"scratch copy failed: {copy['error']}",
        }
    started = time.perf_counter_ns()
    steps = arm.write_loop(plan)
    loop_ms = (time.perf_counter_ns() - started) / 1e6
    back = readback_and_cleanup(fx, plan)
    state = back["state"]
    errors = [step.error for step in steps if not step.ok]
    if len(steps) != arm.requests_per_write_loop():
        errors.append(f"{len(steps)} request(s) issued where {arm.requests_per_write_loop()} was expected")
    if not back["ok"]:
        errors.append(f"readback failed: {back['error']}")
    if state is not None:
        wrong = {
            name: {"expected": value, "got": state.get(name)}
            for name, value in fx["writes"]["readback"].get("expected", {}).items()
            if state.get(name) != value
        }
        if wrong:
            errors.append("the write half did not leave the state the fixture expects: " + json.dumps(wrong))
    return {
        "ok": not errors,
        "write_loop_ms": round(loop_ms, 3),
        "scratch_copy_ms": copy["ms"],
        "readback_ms": back["ms"],
        "scratch": plan.scratch,
        "scratch_leaked": back.get("scratch_leaked"),
        "bytes_sent": sum(step.bytes_sent for step in steps),
        "bytes_received": sum(step.bytes_received for step in steps),
        "steps": [step.json() for step in steps],
        "state": state,
        "error": "; ".join(error for error in errors if error) or None,
    }


def run_write_arm(name: str, fx: dict, warmups: int, iterations: int, task: str, board_file: str, log) -> dict:
    """One arm's write half: its own cold setup, warmups and measured iterations.

    A separate pass from the read half rather than a second phase inside it, so
    a read receipt is byte-shaped exactly as v2 left it and an arm that has
    only one half needs no special case.
    """
    arm = WRITE_ARMS[name](fx)
    receipt: dict = {
        "arm": name,
        "half": "writes",
        "description": arm.description,
        "transport": arm.transport(),
        "started_at": now_iso(),
        "writes_per_loop": len(fx["writes"]["transact_items"]),
        "requests_per_write_loop": arm.requests_per_write_loop(),
        "task": task,
    }
    log(f"[{name}/writes] remote probe (before)")
    receipt["host_before"] = remote_probe(fx)
    receipt["client_before"] = local_probe()
    log(f"[{name}/writes] cold setup")
    try:
        receipt["setup"] = arm.setup()
    except Exception as error:  # noqa: BLE001 - the receipt must carry the reason
        receipt["setup"] = {"error": str(error)}
        receipt["aborted"] = f"setup failed: {error}"
        receipt["finished_at"] = now_iso()
        try:
            receipt["teardown"] = arm.teardown()
        except Exception as teardown_error:  # noqa: BLE001
            receipt["teardown"] = {"error": str(teardown_error)}
        return receipt
    receipt["reuse_before"] = arm.prove_reuse("before")
    warm = []
    for index in range(warmups):
        iteration = run_write_iteration(arm, fx, task, board_file)
        warm.append(iteration)
        log(f"[{name}/writes] warmup {index + 1}/{warmups} {iteration['write_loop_ms']} ms ok={iteration['ok']}")
    measured = []
    aborted = None
    for index in range(iterations):
        iteration = run_write_iteration(arm, fx, task, board_file)
        measured.append(iteration)
        log(f"[{name}/writes] measured {index + 1}/{iterations} {iteration['write_loop_ms']} ms ok={iteration['ok']}")
        if not iteration["ok"] and isinstance(arm, McpArm) and iteration["error"] and "closed the pipe" in iteration["error"]:
            aborted = f"mcp pipe closed during measured write iteration {index + 1}"
            break
    receipt["reuse_after"] = arm.prove_reuse("after")
    receipt["host_after"] = remote_probe(fx)
    receipt["client_after"] = local_probe()
    try:
        receipt["teardown"] = arm.teardown()
    except Exception as error:  # noqa: BLE001
        receipt["teardown"] = {"error": str(error)}
    receipt["finished_at"] = now_iso()
    if aborted:
        receipt["aborted"] = aborted

    ok_loops = [m for m in measured if m["ok"]]
    failures = [
        {"iteration": index + 1, "error": m["error"], "steps": [s for s in m["steps"] if not s["ok"]]}
        for index, m in enumerate(measured) if not m["ok"]
    ]
    step_ids: list[str] = []
    for m in measured:
        for step in m["steps"]:
            if step["id"] not in step_ids:
                step_ids.append(step["id"])
    per_step = {}
    for step_id in step_ids:
        samples = [s for m in ok_loops for s in m["steps"] if s["id"] == step_id]
        per_step[step_id] = {
            "ms": summarize([s["ms"] for s in samples]),
            "bytes_sent": samples[0]["bytes_sent"] if samples else None,
            "bytes_received": summarize([float(s["bytes_received"]) for s in samples]),
            "body_bytes": summarize([float(s["body_bytes"]) for s in samples]),
        }
    # One canonical string per distinct end state. More than one means the arm
    # disagreed with itself, which no cross-arm comparison can repair.
    states = sorted({json.dumps(m["state"], sort_keys=True) for m in ok_loops if m["state"] is not None})
    leaked = [m["scratch"] for m in measured if m.get("scratch_leaked")]
    receipt.update({
        "warmups": {
            "count": len(warm),
            "write_loop_ms": [w["write_loop_ms"] for w in warm],
            "ok": sum(1 for w in warm if w["ok"]),
        },
        "measured": {
            "iterations_requested": iterations,
            "iterations_run": len(measured),
            "iterations_ok": len(ok_loops),
            "failures": len(failures),
            "write_loop_ms": summarize([m["write_loop_ms"] for m in ok_loops]),
            "write_loop_ms_samples": [m["write_loop_ms"] for m in measured],
            "scratch_copy_ms": summarize([m["scratch_copy_ms"] for m in measured if m["scratch_copy_ms"] is not None]),
            "readback_ms": summarize([m["readback_ms"] for m in measured if m["readback_ms"] is not None]),
            "bytes_sent_per_write_loop": summarize([float(m["bytes_sent"]) for m in ok_loops]),
            "bytes_received_per_write_loop": summarize([float(m["bytes_received"]) for m in ok_loops]),
            "per_step": per_step,
            "state": {"distinct": states, "value": json.loads(states[0]) if len(states) == 1 else None},
        },
        "failure_detail": failures,
        # warmups + measured: the "before" snapshot is taken before the
        # warmups on purpose -- it proves the transport survived them -- so the
        # request count it is compared against has to include them.
        "connection_reuse": arm.write_reuse_summary(
            receipt["reuse_before"], receipt["reuse_after"], len(warm) + len(measured), receipt["teardown"]
        ),
        "frozen_board": frozen_board_check(receipt["host_before"], receipt["host_after"]),
        "scratch_leaked": leaked,
        "host_load": {"before": receipt["host_before"].get("load1"), "after": receipt["host_after"].get("load1")},
        "client_load": {
            "before": receipt["client_before"]["loadavg"][0],
            "after": receipt["client_after"]["loadavg"][0],
        },
        "compression": fx["ssh_compression"],
        "valid": aborted is None and len(failures) == 0 and len(ok_loops) == iterations,
    })
    return receipt


def write_equivalence(arms: list[dict], fx: dict) -> dict:
    """The write half is comparable iff every arm left the same state.

    Two questions kept apart, because they have different answers: did an
    arm's own iterations agree with each other, and did the arms agree with one
    another. An arm that disagrees with itself cannot be compared with
    anything, so it is named rather than folded into one boolean.
    """
    by_arm = {}
    for arm in arms:
        if "measured" not in arm:
            continue
        by_arm[arm["arm"]] = arm["measured"]["state"]["distinct"]
    sets = list(by_arm.values())
    singleton = all(len(states) == 1 for states in sets)
    equal = len({tuple(states) for states in sets}) == 1 if sets else False
    return {
        "state_by_arm": by_arm,
        "projection": fx["writes"]["readback"]["state_projection"],
        "all_equivalent": bool(sets) and singleton and equal,
        "drift_within_an_arm": [name for name, states in by_arm.items() if len(states) != 1],
    }


def write_reasons(arms: list[dict], equiv: dict, warmups: int, iterations: int, fx: dict) -> list[str]:
    """Every reason this run's write numbers may not be compared."""
    reasons = []
    if warmups < fx["warmups"]:
        reasons.append(f"writes: warmups {warmups} < fixture minimum {fx['warmups']}")
    if iterations < fx["iterations"]:
        reasons.append(f"writes: iterations {iterations} < fixture minimum {fx['iterations']}")
    for arm in arms:
        if arm.get("aborted"):
            reasons.append(f"writes/{arm['arm']}: aborted ({arm['aborted']})")
        elif not arm.get("valid"):
            reasons.append(f"writes/{arm['arm']}: {arm['measured']['failures']} failed iteration(s)")
        for probe in ("host_before", "host_after"):
            if arm.get(probe, {}).get("hostname") != fx["expected_remote_hostname"]:
                reasons.append(
                    f"writes/{arm['arm']}: {probe} hostname {arm.get(probe, {}).get('hostname')!r} "
                    f"!= {fx['expected_remote_hostname']!r}"
                )
        if "measured" in arm and not arm["frozen_board"]["unchanged"]:
            reasons.append(
                f"writes/{arm['arm']}: the frozen fixture board changed during the write half "
                f"({json.dumps(arm['frozen_board'])})"
            )
        if arm.get("scratch_leaked"):
            reasons.append(f"writes/{arm['arm']}: scratch copies left on the host: {arm['scratch_leaked']}")
        reuse = arm.get("connection_reuse", {})
        if reuse.get("requests_expected") is not None and reuse.get("requests_answered") != reuse["requests_expected"]:
            reasons.append(
                f"writes/{arm['arm']}: {reuse.get('requests_answered')} request(s) answered on the pipe where "
                f"{reuse['requests_expected']} were expected for {reuse.get('loops_counted')} loop(s)"
            )
    versions = {(a.get("host_before", {}).get("kb_sha256"), a.get("host_after", {}).get("kb_sha256")) for a in arms}
    if len(versions) != 1 or any(None in v or v[0] != v[1] for v in versions):
        reasons.append("writes: served kb binary changed between probes or across arms")
    if not equiv["all_equivalent"]:
        reasons.append("writes: state equivalence failed: " + json.dumps(equiv["state_by_arm"]))
    return reasons


def dry_run(fx: dict, read_names: list[str], write_names: list[str]) -> int:
    """Print what one iteration of every selected arm would issue, then stop.

    No host is contacted. The four values a measured run learns from the
    remote -- the claimed task, the lease token, the `mktemp -d` directory and
    the board file inside the data root -- come from `DRY_RUN`; every other
    byte is built by the same code the measured run uses, which is the only
    reason a printed line is worth reading.
    """
    plan = WritePlan(
        DRY_RUN["task"], DRY_RUN["scratch"], f"{DRY_RUN['scratch']}/boards/{DRY_RUN['board_file']}"
    )
    print(f"# dry run: fixture {fx['fixture_id']}, target {fx['ssh_target']}, one iteration per arm, nothing sent")
    if write_names:
        print(f"# substituted: TASK={plan.task} TOKEN={DRY_RUN['token']} scratch={plan.scratch} board={DRY_RUN['board_file']}")
        print("# a measured run reads TASK from `claim --candidates` on a scratch copy, TOKEN from the claim's own")
        print("# answer, the scratch path from `mktemp -d` on the remote, and the board file name for --db from")
        print(f"# the frozen registry ({fx['board']} in {fx['writes']['scratch']['source']}).")
    else:
        print("# read half only: this fixture declares no `writes` section, or the write half was not asked for.")
    for name in read_names:
        arm = ARMS[name](fx)
        requests = arm.requests_per_loop(len(fx["reads"]))
        print(f"\n## {name} -- read half: {len(fx['reads'])} reads, {requests} request(s) per loop")
        for line in arm.read_dry_run():
            print("  " + line)
    for name in write_names:
        arm = WRITE_ARMS[name](fx)
        print(
            f"\n## {name} -- write half: {len(fx['writes']['transact_items'])} board writes, "
            f"{arm.requests_per_write_loop()} request(s) per loop"
        )
        print("  # 1. the disposable copy, on its own cold ssh, excluded from write_loop_ms:")
        print("  " + shlex.join(cold_ssh_argv(fx) + ["bash", "-s"]))
        for line in scratch_prepare_script(fx).strip().splitlines():
            print("  | " + line)
        print("  # 2. the write half itself, on this arm's transport, measured:")
        for line in arm.write_dry_run(plan):
            print("  " + line)
        print("  # 3. the readback and the removal, on their own cold ssh, excluded from write_loop_ms:")
        print("  " + shlex.join(cold_ssh_argv(fx) + ["bash", "-s"]))
        for line in readback_script(fx, plan).strip().splitlines():
            print("  | " + line)
    return 0


ARMS_HELP = """arms (--arms takes any comma-separated subset; the default is all of them):
  ssh                reads + writes  one fresh ssh connection per request
  ssh-controlmaster  reads + writes  one master per arm; one request per multiplexed channel
  mcp-over-ssh       reads + writes  one persistent `kb mcp` child; one tools/call per request
  mcp-batch          reads only      every batchable read of a loop in one `tools/call batch`
  mcp-transact       writes only     the whole write half in one `tools/call transact`
  ssh-transact       writes only     the whole write half in one cold ssh, `transact --items-file /dev/stdin`

halves:
  --reads-only   the twelve reads and nothing else
  --writes-only  the write half and nothing else; the fixture must declare `writes`
  (neither)      both halves, reads first, each with its own cold setup per arm

The write half never touches the frozen fixture board: every iteration runs
against its own `mktemp -d` copy, addressed with --db, and removes it
afterwards. The copy and the readback are timed separately and excluded from
write_loop_ms.
"""


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, epilog=ARMS_HELP, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--fixture", default=os.path.join(HERE, "fixture.json"))
    parser.add_argument("--out", help="receipt JSON path; required unless --dry-run")
    parser.add_argument("--arms", default=",".join(ARM_NAMES), help="comma-separated subset of " + ",".join(ARM_NAMES))
    parser.add_argument("--reads-only", action="store_true", help="run the read half only")
    parser.add_argument("--writes-only", action="store_true", help="run the write half only")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the exact remote command lines one iteration of every selected arm would issue, then exit 0 without contacting any host",
    )
    parser.add_argument("--warmups", type=int, help="override the fixture (a smaller value disqualifies the receipt)")
    parser.add_argument("--iterations", type=int, help="override the fixture (a smaller value disqualifies the receipt)")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    if args.reads_only and args.writes_only:
        parser.error("--reads-only and --writes-only ask for different halves; pass one, or neither for both")
    if not args.dry_run and not args.out:
        parser.error("--out is required unless --dry-run")

    with open(args.fixture, encoding="utf-8") as handle:
        fx = json.load(handle)
    warmups = fx["warmups"] if args.warmups is None else args.warmups
    iterations = fx["iterations"] if args.iterations is None else args.iterations
    names = [n.strip() for n in args.arms.split(",") if n.strip()]
    unknown = [n for n in names if n not in ARMS and n not in WRITE_ARMS]
    if unknown:
        parser.error(f"unknown arm(s) {unknown}; choose from {list(ARM_NAMES)}")

    # A fixture older than v3 has no write half. That is not an error unless
    # the write half is what was asked for.
    has_writes = isinstance(fx.get("writes"), dict)
    if args.writes_only and not has_writes:
        parser.error(f"{args.fixture} declares no `writes` section, so there is no write half to run")
    read_names = [n for n in names if n in ARMS] if not args.writes_only else []
    write_names = [n for n in names if n in WRITE_ARMS] if (not args.reads_only and has_writes) else []
    if not read_names and not write_names:
        parser.error(
            f"nothing to run: {names} has no arm with the requested half "
            f"(reads: {list(READ_ARM_NAMES)}, writes: {list(WRITE_ARM_NAMES)})"
        )

    def log(message: str) -> None:
        if not args.quiet:
            print(message, file=sys.stderr, flush=True)

    if args.dry_run:
        return dry_run(fx, read_names, write_names)

    with open(args.fixture, "rb") as handle:
        fixture_sha = sha256(handle.read())
    ssh_options = effective_ssh_options(fx["ssh_target"])
    receipt = {
        "schema": RECEIPT_SCHEMA,
        "fixture": {**fx, "path": os.path.relpath(args.fixture, os.getcwd()), "sha256": fixture_sha},
        "run": {
            "started_at": now_iso(),
            "command": shlex.join([os.path.relpath(sys.argv[0], os.getcwd())] + (argv if argv is not None else sys.argv[1:])),
            "warmups": warmups,
            "iterations": iterations,
            "halves": {"reads": read_names, "writes": write_names},
            "percentile_method": "nearest-rank over successful measured loops: value at ceil(P/100*n), 1-indexed",
            "timing": "wall clock, time.perf_counter_ns on the client, per read and per loop. On mcp-batch a batched read's `ms` is the whole batch call's wall time attributed to that read, because one call has no per-read timing to observe; those per-read numbers therefore sum to far more than loop_ms and only loop_ms is comparable across arms. `write_loop_ms` is the write half's requests alone: the per-iteration scratch copy (`scratch_copy_ms`) and the readback that also removes it (`readback_ms`) are timed on their own cold ssh and excluded.",
            "bytes": "application-layer: request = the exec command string (ssh arms) or the JSON-RPC frame (mcp); response = stdout bytes (ssh arms) or the JSON-RPC frame (mcp); body_bytes = the kb --json output either way. On mcp-batch a batched read's request and response bytes are that entry's slice of the one frame, so they sum to the frame minus its envelope. On ssh-transact the request is the command string plus the item list it sent on stdin. Not wire bytes.",
            "client": local_probe(),
            "ssh_effective_options": ssh_options,
            "rtt": rtt_probe(str(ssh_options.get("hostname", fx["ssh_target"]))),
        },
        "arms": [],
    }
    for name in read_names:
        receipt["arms"].append(run_arm(name, fx, warmups, iterations, log))
    reasons: list[str] = []
    if read_names:
        receipt["equivalence"] = equivalence(receipt["arms"], fx["reads"])
        reasons += verdict(receipt["arms"], receipt["equivalence"], warmups, iterations, fx)["reasons"]
    else:
        receipt["equivalence"] = {"per_read": [], "all_equivalent": None, "not_run": "the read half was not part of this run"}

    if not write_names:
        receipt["writes"] = {
            "writes_schema": WRITES_SCHEMA,
            "not_run": (
                f"{args.fixture} declares no `writes` section" if not has_writes
                else "the write half was not part of this run"
            ),
        }
    else:
        writes: dict = {
            "writes_schema": WRITES_SCHEMA,
            "started_at": now_iso(),
            "scratch_rule": fx["writes"]["scratch"]["rule"],
            "excluded_from_write_loop_ms": ["scratch_copy_ms", "readback_ms"],
            "arms": [],
        }
        receipt["writes"] = writes
        try:
            probe = remote_probe(fx)
            board_path = probe.get("board_path")
            if not board_path:
                raise RuntimeError(f"the remote probe found no board file for {fx['board']}: {json.dumps(probe)[:300]}")
            board_file = os.path.basename(board_path)
            writes["board_file"] = board_file
            writes["frozen_source"] = fx["writes"]["scratch"]["source"]
            writes["task"] = select_write_task(fx, board_file, log)
        except (RuntimeError, OSError) as error:
            writes["error"] = str(error)
            reasons.append(f"writes: {error}")
        else:
            for name in write_names:
                writes["arms"].append(run_write_arm(name, fx, warmups, iterations, writes["task"]["task"], board_file, log))
            writes["equivalence"] = write_equivalence(writes["arms"], fx)
            reasons += write_reasons(writes["arms"], writes["equivalence"], warmups, iterations, fx)
        writes["finished_at"] = now_iso()

    receipt["verdict"] = {"comparable": not reasons, "reasons": reasons}
    receipt["run"]["finished_at"] = now_iso()
    text = json.dumps(receipt, indent=2, ensure_ascii=False) + "\n"
    # A receipt is committed. It carries hostnames, paths, hashes and numbers;
    # it must never carry a credential, so anything that looks like one refuses
    # the write rather than trusting a reviewer to notice.
    leak = SECRET_SHAPE.search(text)
    if leak:
        log(f"refusing to write {args.out}: receipt text matches a credential shape near {leak.group(0)[:16]!r}; fix the probe, not the guard")
        return 2
    with open(args.out, "w", encoding="utf-8") as handle:
        handle.write(text)
    for arm in receipt["arms"]:
        if "measured" in arm:
            m = arm["measured"]["loop_ms"]
            log(f"{arm['arm']} reads: p50={m.get('p50')} p95={m.get('p95')} p99={m.get('p99')} ms, ok={arm['measured']['iterations_ok']}/{arm['measured']['iterations_run']}, reused={arm['connection_reuse']['reused']}")
        else:
            log(f"{arm['arm']} reads: aborted: {arm.get('aborted')}")
    for arm in receipt["writes"].get("arms", []):
        if "measured" in arm:
            m = arm["measured"]["write_loop_ms"]
            log(
                f"{arm['arm']} writes: p50={m.get('p50')} p95={m.get('p95')} p99={m.get('p99')} ms, "
                f"{arm['requests_per_write_loop']} request(s)/loop, copy p50={arm['measured']['scratch_copy_ms'].get('p50')} ms, "
                f"ok={arm['measured']['iterations_ok']}/{arm['measured']['iterations_run']}, frozen board unchanged={arm['frozen_board']['unchanged']}"
            )
        else:
            log(f"{arm['arm']} writes: aborted: {arm.get('aborted')}")
    log(f"comparable={receipt['verdict']['comparable']} reasons={receipt['verdict']['reasons']}")
    return 0 if receipt["verdict"]["comparable"] else 1


if __name__ == "__main__":
    sys.exit(main())
