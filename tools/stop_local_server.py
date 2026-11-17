"""Release a requested port only when its listener belongs to this checkout."""

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
        raise RuntimeError(result.stderr.strip() or "Unable to inspect the port")
    return result.stdout.splitlines()


def listeners(port):
    fields = lsof(f"-iTCP:{port}", "-sTCP:LISTEN", "-Fp")
    return {int(field[1:]) for field in fields if field.startswith("p")}


def stop_local_server(port, executable):
    if port == 0:
        return
    pids = listeners(port)
    if not pids:
        return
    if len(pids) != 1:
        raise RuntimeError(f"Port {port} has multiple listeners")
    pid, = pids
    files = lsof("-a", "-p", str(pid), "-u", str(os.getuid()), "-d", "txt", "-Fn")
    expected = f"n{executable.resolve()}"
    if expected not in files and f"{expected} (deleted)" not in files:
        raise RuntimeError(
            f"Port {port} is held by another process (PID {pid}). "
            "Choose another port with TEMPO_PORT."
        )

    print(f"Stopping the existing Tempo server on port {port}.", flush=True)
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 5
    while listeners(port):
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Port {port} did not become available")
        time.sleep(0.1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("port", type=int, metavar="PORT")
    parser.add_argument("executable", type=Path)
    args = parser.parse_args()
    if not 0 <= args.port <= 65535:
        parser.error("PORT must be between 0 and 65535")
    try:
        stop_local_server(args.port, args.executable)
    except (OSError, RuntimeError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
