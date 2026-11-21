#!/usr/bin/env python3
"""Collect bounded, read-only host evidence beside Tempo's structured logs.

No packages, database access, HTTP commands, process arguments, environment or
hostnames are collected. Only this tool's short-lived ps/vm_stat children can
be stopped when they exceed their deadline or output budget.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import math
import os
from pathlib import Path
import platform
import re
import selectors
import stat
import subprocess
import time
from typing import Any

MAX_PROCESS_OUTPUT = 2 * 1024 * 1024
MAX_PROCESSES = 20
MAX_LOG_FILES = 5
MAX_LOG_TAIL = 512 * 1024
MAX_LOG_EVENTS = 2000
MAX_LOG_LINE = 8192
TOKEN = re.compile(r"^[A-Za-z0-9_.:-]{1,128}$")
DECIMAL_ID = re.compile(r"^[0-9]{1,20}$")
LOG_FILE = re.compile(r"^tempo-[A-Za-z0-9_-]+\.jsonl(?:\.[1-9][0-9]?)?$")

# This is an export allowlist, not a general JSON-file copier. Additional runtime
# fields must be reviewed here before they appear in a diagnostic bundle.
LOG_NUMBERS = frozenset({
    "schema_version", "timestamp_ms", "monotonic_ms", "elapsed_ms",
    "pid", "status", "status_code", "cpu_percent", "rss_bytes", "peak_rss_bytes", "load1",
    "load5", "load15", "cpu_count", "dropped_events", "active_spans",
    "scheduler_delay_ms", "queue_wait_ms", "store_span_id",
    "bytes", "request_bytes", "response_bytes", "child_pid", "timeout_ms",
    "sqlite_code", "sqlite_extended_code", "error_code", "queue_depth", "count", "listen_port",
})
LOG_TOKENS = frozenset({
    "event", "component", "operation", "phase", "level", "run_id",
    "request_id", "error_code", "outcome", "method", "thread", "route",
})


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="milliseconds")


def bounded_command(command: list[str], *, timeout: float = 2.0,
                    max_bytes: int = MAX_PROCESS_OUTPUT) -> tuple[str, str | None]:
    """Read a fixed command without an unbounded pipe or an unbounded wait."""
    try:
        child = subprocess.Popen(command, stdout=subprocess.PIPE,
                                 stderr=subprocess.DEVNULL, stdin=subprocess.DEVNULL)
    except OSError:
        return "", "unavailable"
    chunks: list[bytes] = []
    size = 0
    deadline = time.monotonic() + timeout
    failure = None
    try:
        assert child.stdout is not None
        with selectors.DefaultSelector() as ready:
            ready.register(child.stdout, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    failure = "timeout"
                    break
                events = ready.select(remaining)
                if not events:
                    failure = "timeout"
                    break
                block = os.read(child.stdout.fileno(), min(65536, max_bytes - size + 1))
                if not block:
                    break
                size += len(block)
                if size > max_bytes:
                    failure = "output_limit"
                    break
                chunks.append(block)
        if failure is None:
            try:
                if child.wait(timeout=max(0.001, deadline - time.monotonic())):
                    failure = "command_failed"
            except subprocess.TimeoutExpired:
                failure = "timeout"
    finally:
        if child.poll() is None:
            child.kill()  # This collector's own child, never the observed PID.
        try:
            child.wait(timeout=0.2)
        except subprocess.TimeoutExpired:
            # SIGKILL has already been sent; never let OS teardown block Study's
            # diagnostic capture. This is not a timeout of the observed process.
            failure = failure or "cleanup_timeout"
        if child.stdout is not None:
            child.stdout.close()
    return (b"".join(chunks).decode("utf-8", errors="replace"), failure)


def process_kind(name: str) -> str:
    lower = name.lower()
    if lower == "tempo":
        return "tempo"
    if lower in {"rustc", "cargo", "rust-analyzer", "clippy-driver", "cc",
                 "gcc", "g++", "clang", "clang++", "ld", "lld", "cc1", "cc1plus"}:
        return "compiler_or_build_tool"
    if lower in {"stockfish", "lc0"} or lower.startswith("stockfish-"):
        return "chess_engine"
    if lower in {"node", "nodejs", "npm", "npx"}:
        return "javascript_runtime"
    if re.fullmatch(r"python(?:[0-9]+(?:\.[0-9]+)*)?", lower):
        return "python_runtime"
    return "other"


def parse_processes(text: str, observed_pid: int | None = None) -> dict[str, Any]:
    rows = []
    for line in text.splitlines():
        parts = line.split(None, 4)
        if len(parts) != 5:
            continue
        try:
            pid, parent = int(parts[0]), int(parts[1])
            cpu, rss = float(parts[2]), int(parts[3])
        except ValueError:
            continue
        if pid <= 0 or parent < 0 or not math.isfinite(cpu) or cpu < 0 or rss < 0:
            continue
        # comm can be an absolute executable path on macOS. Never export it.
        name = parts[4].rstrip("/").rsplit("/", 1)[-1]
        name = "".join(c for c in name if c.isprintable())[:80] or "unknown"
        rows.append({"pid": pid, "parent_pid": parent, "executable": name,
                     "kind_hint": process_kind(name), "cpu_percent_ps": cpu,
                     "rss_bytes": rss * 1024})
    rows.sort(key=lambda row: (-row["cpu_percent_ps"], -row["rss_bytes"], row["pid"]))
    return {"top_processes": rows[:MAX_PROCESSES],
            "observed_process": next((row for row in rows if row["pid"] == observed_pid), None),
            "processes_seen": len(rows)}


def parse_linux_memory(text: str) -> dict[str, int]:
    fields = {"MemTotal": "total_bytes", "MemAvailable": "available_bytes",
              "SwapTotal": "swap_total_bytes", "SwapFree": "swap_free_bytes"}
    result = {}
    for line in text.splitlines():
        match = re.fullmatch(r"([A-Za-z_]+):\s+(\d+)\s+kB", line.strip())
        if match and match[1] in fields:
            result[fields[match[1]]] = int(match[2]) * 1024
    return result


def parse_macos_memory(text: str) -> dict[str, int]:
    size = re.search(r"page size of (\d+) bytes", text)
    if not size or not 1 <= int(size[1]) <= 1024 * 1024:
        return {}
    page_size = int(size[1])
    bytes_fields = {"Pages free": "free_bytes", "Pages inactive": "inactive_bytes",
                    "Pages speculative": "speculative_bytes",
                    "Pages wired down": "wired_bytes",
                    "Pages occupied by compressor": "compressor_bytes"}
    counters = {"Swapins": "swapin_pages_total", "Swapouts": "swapout_pages_total"}
    result = {"page_size_bytes": page_size}
    for line in text.splitlines():
        match = re.fullmatch(r"([^:]+):\s*(\d+)\.?", line.strip())
        if not match:
            continue
        if match[1] in bytes_fields:
            result[bytes_fields[match[1]]] = int(match[2]) * page_size
        elif match[1] in counters:
            result[counters[match[1]]] = int(match[2])
    return result


def host_sample(observed_pid: int | None) -> dict[str, Any]:
    result: dict[str, Any] = {"timestamp": utc_now()}
    try:
        result["load_average"] = list(os.getloadavg())
    except (AttributeError, OSError):
        result["load_average"] = None
    output, failure = bounded_command(["ps", "-axo", "pid=,ppid=,%cpu=,rss=,comm="])
    result.update(parse_processes(output, observed_pid))
    if failure:
        result["process_sample_error"] = failure
    system = platform.system()
    if system == "Linux":
        try:
            with open("/proc/meminfo", encoding="ascii") as source:
                result["memory"] = parse_linux_memory(source.read(32768))
        except (OSError, UnicodeError):
            result["memory_sample_error"] = "unavailable"
    elif system == "Darwin":
        output, failure = bounded_command(["vm_stat"], max_bytes=32768)
        result["memory"] = parse_macos_memory(output)
        if failure:
            result["memory_sample_error"] = failure
    else:
        result["memory_sample_error"] = "unsupported_platform"
    return result


def sanitized_log_event(value: Any) -> dict[str, Any] | None:
    if (not isinstance(value, dict) or type(value.get("schema_version")) is not int
            or value.get("schema_version") != 1
            or not isinstance(value.get("event"), str)):
        return None
    result = {}
    for key, item in value.items():
        is_number = ((type(item) is int and -(2 ** 63) <= item < 2 ** 64)
                     or (type(item) is float and math.isfinite(item) and abs(item) < 1e20))
        if key in LOG_NUMBERS and is_number:
            result[key] = item
        elif key in ("span_id", "parent_span_id") and isinstance(item, str) and DECIMAL_ID.fullmatch(item):
            result[key] = item
        elif key in LOG_TOKENS and isinstance(item, str) and TOKEN.fullmatch(item):
            result[key] = item
        elif key == "provider" and item in ("lichess", "chess_com"):
            result[key] = item
    return result if "event" in result else None


def collect_logs(directory: Path | None) -> dict[str, Any]:
    """Export only a bounded tail of known diagnostic JSONL, with no raw text."""
    if directory is None:
        return {"status": "not_requested", "events": []}
    candidates = []
    skipped = 0
    tail_truncations = 0
    event_truncations = 0
    directory_truncated = False
    try:
        with os.scandir(directory) as entries:
            for index, entry in enumerate(entries):
                if index >= 256:
                    directory_truncated = True
                    break
                if LOG_FILE.fullmatch(entry.name) and entry.is_file(follow_symlinks=False):
                    candidates.append((entry.stat(follow_symlinks=False).st_mtime_ns, entry.path))
    except OSError:
        return {"status": "unavailable", "events": []}
    candidates.sort(reverse=True)
    events = []
    files_read = 0
    for _, filename in reversed(candidates[:MAX_LOG_FILES]):
        try:
            flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
            fd = os.open(filename, flags)
            with os.fdopen(fd, "rb") as source:
                # Read from the existing open file after re-checking its type.
                metadata = os.fstat(source.fileno())
                if not stat.S_ISREG(metadata.st_mode):
                    continue
                offset = max(0, metadata.st_size - MAX_LOG_TAIL)
                source.seek(offset)
                data = source.read(MAX_LOG_TAIL)
                if offset:
                    tail_truncations += 1
                    data = data.partition(b"\n")[2]
        except OSError:
            skipped += 1
            continue
        files_read += 1
        for line in data.splitlines():
            if not line or len(line) > MAX_LOG_LINE:
                skipped += 1
                continue
            try:
                value = sanitized_log_event(json.loads(line))
            except (ValueError, UnicodeError, RecursionError):
                value = None
            if value is None:
                skipped += 1
            else:
                events.append(value)
                if len(events) > MAX_LOG_EVENTS:
                    excess = len(events) - MAX_LOG_EVENTS
                    event_truncations += excess
                    del events[:excess]
    return {"status": "captured" if files_read else "no_matching_logs",
            "files_read": files_read, "events": events, "skipped_records_or_files": skipped,
            "truncated_file_tails": tail_truncations,
            "truncated_events": event_truncations,
            "omitted_matching_files": max(0, len(candidates) - MAX_LOG_FILES),
            "directory_scan_truncated": directory_truncated,
            "bounds": {"files": MAX_LOG_FILES, "tail_bytes_per_file": MAX_LOG_TAIL,
                       "events": MAX_LOG_EVENTS}}


def collect(duration: float, interval: float, observed_pid: int | None,
            log_dir: Path | None) -> dict[str, Any]:
    started = time.monotonic()
    deadline = started + duration
    next_sample = started
    result: dict[str, Any] = {
        "schema_version": 1, "kind": "tempo_host_diagnostics", "started_at": utc_now(),
        "host": {"platform": platform.system(), "cpu_count": os.cpu_count()},
        "requested_duration_seconds": duration, "interval_seconds": interval,
        "observed_pid": observed_pid, "interrupted": False, "samples": [],
        "interpretation": [
            "Process categories are executable-name hints, not workload attribution.",
            "ps CPU percentages use platform-specific averaging, not this sample interval.",
            "A missing observed PID may mean it exited, was hidden, or ps was unavailable.",
            "High load with low Tempo CPU suggests competing work; it does not prove causation.",
            "A request timeout does not establish whether a command was accepted.",
        ],
    }
    try:
        while time.monotonic() < deadline:
            now = time.monotonic()
            sample = host_sample(observed_pid)
            sample["elapsed_ms"] = round((now - started) * 1000, 3)
            sample["schedule_delay_ms"] = round(max(0, now - next_sample) * 1000, 3)
            result["samples"].append(sample)
            next_sample += interval
            # Missed ticks are evidence, not a reason to launch a catch-up burst.
            if next_sample < time.monotonic():
                next_sample = time.monotonic() + interval
            time.sleep(max(0, min(next_sample, deadline) - time.monotonic()))
    except KeyboardInterrupt:
        result["interrupted"] = True
    result["actual_duration_seconds"] = round(time.monotonic() - started, 3)
    result["finished_at"] = utc_now()
    result["diagnostic_logs"] = collect_logs(log_dir)
    return result


def bounded_number(low: float, high: float):
    def parse(value: str) -> float:
        try:
            number = float(value)
        except ValueError as error:
            raise argparse.ArgumentTypeError("expected a number") from error
        if not math.isfinite(number) or not low <= number <= high:
            raise argparse.ArgumentTypeError(f"must be between {low:g} and {high:g}")
        return number
    return parse


def positive_pid(value: str) -> int:
    try:
        number = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a positive process ID") from error
    if number <= 0:
        raise argparse.ArgumentTypeError("expected a positive process ID")
    return number


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=positive_pid, help="also retain this exact Tempo process")
    parser.add_argument("--duration", type=bounded_number(1, 300), default=30.0,
                        help="sample for 1–300 seconds (default: 30)")
    parser.add_argument("--interval", type=bounded_number(1, 30), default=2.0,
                        help="seconds between samples, 1–30 (default: 2)")
    parser.add_argument("--log-dir", type=Path, help="include sanitized tails of Tempo JSONL logs")
    parser.add_argument("--output", type=Path, help="new JSON file, or an existing directory")
    args = parser.parse_args()
    filename = "tempo-diagnostics-" + dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + ".json"
    output = args.output or Path(filename)
    if output.is_dir():
        output /= filename
    if output.exists() or output.is_symlink():
        parser.error("output already exists; select a new filename")
    if not output.parent.is_dir():
        parser.error("output directory does not exist")
    print("Collecting host samples. Reproduce the wait in Tempo; Ctrl+C saves a partial capture.")
    result = collect(args.duration, args.interval, args.pid, args.log_dir)
    try:
        fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as target:
            json.dump(result, target, indent=2, allow_nan=False)
            target.write("\n")
    except OSError as error:
        parser.error(f"could not create output ({type(error).__name__})")
    print(f"Saved {len(result['samples'])} host samples to {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
