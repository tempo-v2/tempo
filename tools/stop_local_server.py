"""Replace a verified foreground Tempo server on the requested port."""

import errno
import os
from pathlib import Path
import signal
import subprocess
import time

from development_guard import Installation, database_files


def lsof(*args):
    result = subprocess.run(["lsof", "-nP", *args], capture_output=True, text=True)
    if result.returncode and (result.returncode != 1 or result.stderr):
        raise ValueError(result.stderr.strip() or "Unable to inspect the development listener")
    return result.stdout.splitlines()


def listeners(port):
    return {int(line[1:]) for line in lsof(f"-iTCP:{port}", "-sTCP:LISTEN", "-Fp")
            if line.startswith("p")}


def server_stamp(pid, port, installation):
    directories = []
    executables = []
    descriptor = ""
    for field in lsof("-a", "-p", str(pid), "-u", str(os.getuid()), "-d", "cwd,txt", "-Ffn"):
        if field.startswith("f"):
            descriptor = field[1:]
        elif field.startswith("n") and descriptor == "cwd":
            directories.append(Path(field[1:]))
        elif field.startswith("n") and descriptor == "txt":
            executables.append(Path(field[1:].removesuffix(" (deleted)")))
    if len(directories) != 1:
        return None
    workspace, = directories
    if not (workspace / "tempo/tools/local_development.py").is_file():
        return None
    ui = (workspace / "tempo-ui/dist").resolve()
    result = subprocess.run(
        ["ps", "-ww", "-p", str(pid), "-o", "lstart=", "-o", "command="],
        env={**os.environ, "LC_ALL": "C"}, capture_output=True, text=True,
    )
    if result.returncode:
        return None
    fields = result.stdout.split(None, 5)
    if len(fields) != 6:
        return None
    command = fields[5].rstrip("\n")
    for executable in executables:
        if executable.name != "tempo" or not executable.is_absolute():
            continue
        prefix = f"{executable} serve "
        suffix = f" {ui} {port}"
        if not (command.startswith(prefix) and command.endswith(suffix)):
            continue
        database = Path(command[len(prefix):-len(suffix)])
        if not database.is_absolute():
            continue
        owner_installation = Installation(workspace)
        if port in owner_installation.ports:
            raise ValueError(f"Port {port} is reserved for the managed installation")
        for path in (executable, ui, *database_files(database), *database_files(database.resolve())):
            installation.check(path)
            owner_installation.check(path)
        return pid, result.stdout
    return None


def find_server(port, installation):
    try:
        installation.check_port(port)
        return None
    except ValueError as error:
        if getattr(error.__cause__, "errno", None) != errno.EADDRINUSE:
            raise
    pids = listeners(port)
    if len(pids) == 1:
        pid, = pids
        stamp = server_stamp(pid, port, installation)
        if stamp:
            return stamp
    installation.check_port(port)
    return None


def stop_server(port, installation, expected):
    if expected is not None:
        current = find_server(port, installation)
        if current is not None:
            if current != expected:
                raise ValueError(f"The Tempo listener on port {port} changed during startup")
            pid, _ = current
            print(f"Stopping the existing Tempo server on port {port}.", flush=True)
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 5
            while pid in listeners(port):
                if time.monotonic() >= deadline:
                    raise ValueError(f"The existing Tempo server did not release port {port}")
                time.sleep(0.1)
    installation.check_port(port)
