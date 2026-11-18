"""Stop this checkout's Tempo servers before launching a fresh build."""

import argparse
import os
from pathlib import Path
import signal
import subprocess
import time


def lsof(*args):
    result = subprocess.run(
        ["lsof", "-nP", *args], capture_output=True, text=True
    )
    # lsof returns 1 with no output when no files match.
    if result.returncode and (result.returncode != 1 or result.stderr):
        raise RuntimeError(result.stderr.strip() or "Unable to inspect Tempo server processes")
    return result.stdout.splitlines()


def listeners(*filters):
    fields = lsof(*filters, "-sTCP:LISTEN", "-Fp")
    return {int(field[1:]) for field in fields if field.startswith("p")}


def tempo_processes(pids, workspace, executable):
    if not pids:
        return set()
    fields = lsof(
        "-a", "-p", ",".join(map(str, sorted(pids))), "-u", str(os.getuid()),
        "-d", "txt", "-Fpn",
    )
    matches = set()
    for field in fields:
        if field.startswith("p"):
            pid = int(field[1:])
        elif field.startswith("n"):
            path = Path(field[1:].removesuffix(" (deleted)"))
            if path == executable or (path.name == "tempo" and path.is_relative_to(workspace)):
                matches.add(pid)
    return matches


def stop_local_servers(port, workspace, executable):
    workspace = workspace.resolve()
    executable = executable.resolve()
    owned = listeners("-a", "-u", str(os.getuid()), "-iTCP")
    targets = tempo_processes(owned, workspace, executable)
    destination = (f"-iTCP:{port}",)

    def check_port(allowed):
        blockers = listeners(*destination) - allowed if port else set()
        if blockers:
            identifiers = ", ".join(map(str, sorted(blockers)))
            raise RuntimeError(f"Port {port} is held by another process (PID {identifiers}).")

    check_port(targets)
    if targets:
        print(f"Stopping {len(targets)} existing Tempo server(s) for this checkout.", flush=True)

    for sig in (signal.SIGTERM, signal.SIGKILL):
        # Check executable identity again before signaling, including at escalation.
        targets = tempo_processes(targets, workspace, executable)
        if not targets:
            break
        for pid in targets:
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 5
        while targets and time.monotonic() < deadline:
            time.sleep(0.1)
            targets = tempo_processes(targets, workspace, executable)
    remaining = tempo_processes(
        listeners("-a", "-u", str(os.getuid()), "-iTCP"), workspace, executable
    )
    if remaining:
        raise RuntimeError("A Tempo server is still running for this checkout.")
    check_port(set())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", type=int)
    parser.add_argument("workspace", type=Path)
    parser.add_argument("executable", type=Path)
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("PORT must be between 0 and 65535")
    try:
        stop_local_servers(args.port, args.workspace, args.executable)
    except (OSError, RuntimeError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
