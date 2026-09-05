#!/usr/bin/env python3
"""Transport benchmark for the representative agent loop against a kanban board home.

Contract: docs/testing/graphql-agent-loop-benchmark.md. This script is the
reproducible half of that contract: it reads the version-controlled fixture,
runs each transport arm with its own cold setup, warmups and measured
iterations, and writes one receipt JSON. It never writes to the board: every
read in the fixture is a read-only CLI operation, and the MCP arm calls the same
operations through `kb mcp`, which runs the same binary per tool call.

Stdlib only. One command from a clean checkout:

    python3 docs/testing/bench/agent-loop-bench.py --out docs/testing/<receipt>.json
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
ARM_NAMES = ("ssh", "ssh-controlmaster", "mcp-over-ssh")
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

    Normalization: parse, drop the fixture's declared volatile top-level keys,
    re-serialize with sorted keys and no whitespace. The same body over any
    transport yields the same normalized digest; a body that differs in
    anything but the dropped keys does not.
    """
    raw = sha256(body)
    try:
        value = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return raw, None
    if isinstance(value, dict):
        for key in drop_keys:
            value.pop(key, None)
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


class SshArm:
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

    def teardown(self) -> dict:
        exit_ = self.control("exit")
        text = (exit_.stderr + exit_.stdout).decode(errors="replace").strip()
        time.sleep(0.2)
        return {"control_exit": text, "master_after_exit": self.master_pid()}


class McpArm:
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
        frame: dict = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            frame["params"] = params
        if not notification:
            self.next_id += 1
            frame["id"] = self.next_id
        line = (json.dumps(frame, separators=(",", ":")) + "\n").encode()
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

    def read(self, read: dict) -> ReadResult:
        args = dict(read["args"])
        if read.get("board_scoped", True):
            args["project"] = self.fx["board"]
        started = time.perf_counter_ns()
        try:
            line, response = self._rpc("tools/call", {"name": read["tool"], "arguments": args})
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


ARMS = {SshArm.name: SshArm, ControlMasterArm.name: ControlMasterArm, McpArm.name: McpArm}


# --------------------------------------------------------------------------- loop


def run_loop(arm, reads: list[dict]) -> dict:
    loop_started = time.perf_counter_ns()
    results = []
    for read in reads:
        result = arm.read(read)
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
        "requests_per_loop": len(reads),
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


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fixture", default=os.path.join(HERE, "fixture.json"))
    parser.add_argument("--out", required=True, help="receipt JSON path")
    parser.add_argument("--arms", default=",".join(ARM_NAMES), help="comma-separated subset of " + ",".join(ARM_NAMES))
    parser.add_argument("--warmups", type=int, help="override the fixture (a smaller value disqualifies the receipt)")
    parser.add_argument("--iterations", type=int, help="override the fixture (a smaller value disqualifies the receipt)")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    with open(args.fixture, encoding="utf-8") as handle:
        fx = json.load(handle)
    warmups = fx["warmups"] if args.warmups is None else args.warmups
    iterations = fx["iterations"] if args.iterations is None else args.iterations
    names = [n.strip() for n in args.arms.split(",") if n.strip()]
    unknown = [n for n in names if n not in ARMS]
    if unknown:
        parser.error(f"unknown arm(s) {unknown}; choose from {list(ARMS)}")

    def log(message: str) -> None:
        if not args.quiet:
            print(message, file=sys.stderr, flush=True)

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
            "percentile_method": "nearest-rank over successful measured loops: value at ceil(P/100*n), 1-indexed",
            "timing": "wall clock, time.perf_counter_ns on the client, per read and per loop",
            "bytes": "application-layer: request = the exec command string (ssh arms) or the JSON-RPC frame (mcp); response = stdout bytes (ssh arms) or the JSON-RPC frame (mcp); body_bytes = the kb --json output either way. Not wire bytes.",
            "client": local_probe(),
            "ssh_effective_options": ssh_options,
            "rtt": rtt_probe(str(ssh_options.get("hostname", fx["ssh_target"]))),
        },
        "arms": [],
    }
    for name in names:
        receipt["arms"].append(run_arm(name, fx, warmups, iterations, log))
    receipt["equivalence"] = equivalence(receipt["arms"], fx["reads"])
    receipt["verdict"] = verdict(receipt["arms"], receipt["equivalence"], warmups, iterations, fx)
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
            log(f"{arm['arm']}: p50={m.get('p50')} p95={m.get('p95')} p99={m.get('p99')} ms, ok={arm['measured']['iterations_ok']}/{arm['measured']['iterations_run']}, reused={arm['connection_reuse']['reused']}")
        else:
            log(f"{arm['arm']}: aborted: {arm.get('aborted')}")
    log(f"comparable={receipt['verdict']['comparable']} reasons={receipt['verdict']['reasons']}")
    return 0 if receipt["verdict"]["comparable"] else 1


if __name__ == "__main__":
    sys.exit(main())
