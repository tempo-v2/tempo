"""Build the workspace pair and restart the registered Tempo installation."""

import contextlib
import fcntl
import hashlib
import http.client
import json
import os
from pathlib import Path
import plistlib
import pwd
import re
import selectors
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.request
from urllib.error import URLError

from local_development import WORKSPACE, UI, build_backend, build_ui
from development_guard import Installation, LABEL, RELEASE, check_ui, loaded_resources, plist_resources
from stop_local_server import listeners, lsof, server_stamp, stop_server

DOMAIN = f"gui/{os.getuid()}"
SERVICE = f"{DOMAIN}/{LABEL}"
PHONE_VARIABLES = ("TEMPO_PHONE_ORIGIN", "TEMPO_PHONE_TAILSCALE_LOGIN")


def launch(*args):
    subprocess.run(["/bin/launchctl", *args], check=True)


def database_status(path):
    with contextlib.closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)) as database:
        dataset, = database.execute("SELECT dataset_id FROM store_state").fetchone()
        schema, = database.execute("PRAGMA user_version").fetchone()
    return dataset, schema


def backup(source, destination):
    with (
        contextlib.closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as current,
        contextlib.closing(sqlite3.connect(destination)) as copy,
    ):
        current.backup(copy)
        if (copy.execute("PRAGMA quick_check").fetchall() != [("ok",)]
                or copy.execute("PRAGMA foreign_key_check").fetchone()):
            raise ValueError(f"Tempo backup failed integrity checks: {destination}")


def context_status(port, dataset, schema):
    url = f"http://127.0.0.1:{port}/api/context"
    with urllib.request.urlopen(url, timeout=1) as reply:
        status = json.load(reply)
    if (status.get("status") != "ready"
            or status.get("dataset_id") != dataset
            or status.get("schema_version") != schema):
        raise ValueError("Tempo did not report the expected dataset and schema readiness")


def desktop_assets(port, ui):
    with urllib.request.urlopen(f"http://127.0.0.1:{port}/assets/app.js", timeout=1) as reply:
        if reply.read() != (ui / "assets/app.js").read_bytes():
            raise ValueError("Tempo did not serve the expected UI")


def study_smoke(port, dataset):
    url = f"http://127.0.0.1:{port}"
    request = urllib.request.Request(
        url + "/api/study", data=b'{"command":"study_status"}',
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=1) as reply:
        status = json.load(reply)
    if status.get("dataset_id") != dataset or status.get("status") != "empty":
        raise ValueError("Tempo did not report Study status for the registered dataset")


def phone_environment(job):
    registered = job.get("EnvironmentVariables", {})
    overriding = any(key in os.environ for key in PHONE_VARIABLES)
    environment = os.environ if overriding else registered
    phone = {key: environment[key] for key in PHONE_VARIABLES if key in environment}
    if phone and (len(phone) != len(PHONE_VARIABLES) or not all(phone.values())):
        raise ValueError("Set both TEMPO_PHONE_ORIGIN and TEMPO_PHONE_TAILSCALE_LOGIN")
    if overriding:
        job["EnvironmentVariables"] = {**registered, **phone}
    return phone


def phone_status(port, dataset, ui, phone):
    origin, login = (phone[key] for key in PHONE_VARIABLES)
    host = origin.removeprefix("https://")
    headers = {"Host": host, "X-Forwarded-Host": host, "X-Forwarded-Proto": "https",
               "Tailscale-User-Login": login, "Origin": origin, "Content-Type": "application/json"}
    with contextlib.closing(http.client.HTTPConnection("127.0.0.1", port, timeout=2)) as connection:
        connection.request("GET", "/", headers=headers)
        response = connection.getresponse()
        if response.status != 200 or response.read() != (ui / "phone/index.html").read_bytes():
            raise ValueError("Tempo did not serve the expected phone UI")
        connection.request("POST", "/api/study", b'{"command":"study_status"}', headers=headers)
        response = connection.getresponse()
        if response.status != 200 or json.load(response).get("dataset_id") != dataset:
            raise ValueError("Tempo phone Study did not report the registered dataset")


def rehearse(binary, ui, database, dataset, scratch, phone):
    copy = scratch / "rehearsal.sqlite"
    backup(database, copy)
    with subprocess.Popen([binary, "serve", copy, ui, "0"], stdout=subprocess.PIPE,
                          env={**os.environ, **phone}) as server:
        try:
            with selectors.DefaultSelector() as ready:
                ready.register(server.stdout, selectors.EVENT_READ)
                if not ready.select(15):
                    raise ValueError("The candidate did not start on the disposable database")
                banner = server.stdout.readline().decode().strip()
            prefix = "Tempo study at http://127.0.0.1:"
            if not banner.startswith(prefix):
                raise ValueError("The candidate did not start on the disposable database")
            port = int(banner[len(prefix):])
            _, schema = database_status(copy)
            context_status(port, dataset, schema)
            desktop_assets(port, ui)
            study_smoke(port, dataset)
            if phone:
                phone_status(port, dataset, ui, phone)
            return schema
        finally:
            if server.poll() is None:
                server.terminate()
            server.wait(timeout=5)


def artifact_hash(binary, ui):
    digest = hashlib.sha256()
    for file in (binary, *sorted(path for path in ui.rglob("*") if path.is_file())):
        name = "tempo" if file == binary else str(file.relative_to(ui))
        with file.open("rb") as contents:
            digest.update(name.encode() + b"\0" + hashlib.file_digest(contents, "sha256").digest())
    return digest.hexdigest()


def other_servers(port, installation):
    """Identify the owner's Tempo development listeners on all other ports."""
    found = {}
    for line in lsof("-a", "-u", str(os.getuid()), "-c", "/^tempo$/", "-iTCP", "-sTCP:LISTEN", "-Fpn"):
        if line.startswith("p"):
            pid = int(line[1:])
        elif line.startswith("n"):
            candidate_port = int(line.rsplit(":", 1)[1])
            if candidate_port == port:
                continue
            stamp = server_stamp(pid, candidate_port, installation)
            if not stamp:
                if pid in listeners(candidate_port):
                    raise ValueError(f"Cannot verify the Tempo listener on port {candidate_port} (PID {pid})")
                continue
            found[candidate_port] = stamp
    return found


def registration(config, resources):
    running = loaded_resources()
    if running:
        actual, program, loaded_config = running
        if (actual, program) != resources or loaded_config.resolve() != config:
            raise ValueError("Loaded Tempo and its archived registration disagree")
    return running


def managed_pid():
    result = subprocess.run(["/bin/launchctl", "print", SERVICE], capture_output=True, text=True)
    match = re.search(r"^\s*pid = (\d+)$", result.stdout, re.M) if not result.returncode else None
    return int(match[1]) if match else None


def check_port_owner(port):
    if listeners(port) - {managed_pid()}:
        raise ValueError(f"Another application is using the registered Tempo port {port}")


def stop_managed(port):
    launch("bootout", SERVICE)
    for _ in range(50):
        if not listeners(port):
            return
        time.sleep(0.1)
    raise ValueError(f"Managed Tempo did not release port {port}")


def verify_running_binary(pid, binary):
    images = lsof("-a", "-p", str(pid), "-d", "txt", "-Fn")
    expected = binary.resolve()
    if not any(line.startswith("n") and Path(line[1:]).resolve() == expected
               for line in images):
        raise ValueError("Tempo listener did not load the expected binary")


def start_managed(config, port, dataset, schema, *, previous_release=False):
    resources = plist_resources(config)
    (binary, _, ui, _), _ = resources
    job = plistlib.loads(config.read_bytes())
    launch("bootstrap", DOMAIN, str(config))
    if not job.get("RunAtLoad"):
        launch("kickstart", SERVICE)
    for _ in range(50):
        pid = managed_pid()
        if pid and listeners(port) == {pid}:
            try:
                if not previous_release:
                    context_status(port, dataset, schema)
                desktop_assets(port, ui)
                verify_running_binary(pid, binary)
                if registration(config, resources):
                    return
            except (ConnectionError, TimeoutError, URLError):
                pass
        time.sleep(0.1)
    raise ValueError(f"Managed Tempo did not respond on port {port}")


def write_plist(config, contents):
    with tempfile.NamedTemporaryFile(dir=config.parent, delete=False) as temporary:
        path = Path(temporary.name)
        try:
            temporary.write(contents)
            temporary.flush()
            path.replace(config)
        finally:
            path.unlink(missing_ok=True)


def restart(config, keep_development=False):
    release = config.parent
    if not config.is_file():
        raise ValueError("No managed Tempo registration. Use make start-dev for a separate workspace.")
    original = config.read_bytes()
    job = plistlib.loads(original)
    initial_environment = job.get("EnvironmentVariables", {})
    phone = phone_environment(job)
    environment_changed = initial_environment != job.get("EnvironmentVariables", {})
    resources = plist_resources(config)
    (old_binary, database, old_ui, port), _ = resources
    dataset, schema = database_status(database)
    registration(config, resources)
    check_port_owner(port)
    installation = Installation(WORKSPACE)
    check_ui(installation, UI)
    target = Path(os.environ.get("CARGO_TARGET_DIR") or "target").resolve()
    installation.check_cargo_output(target)
    if not keep_development:
        other_servers(port, installation)
    build_backend(target, "release")
    build_ui()

    with tempfile.TemporaryDirectory(prefix="start-", dir=release) as directory:
        scratch = Path(directory)
        stage = scratch / "artifacts"
        stage.mkdir()
        shutil.copy2(target / "release/tempo", stage / "tempo")
        shutil.copytree(UI / "dist", stage / "ui")
        checksum = artifact_hash(stage / "tempo", stage / "ui")
        changed = checksum != artifact_hash(old_binary, old_ui)
        candidate_schema = schema
        if changed or environment_changed:
            candidate_schema = rehearse(
                stage / "tempo", stage / "ui", database, dataset, scratch, phone
            )
        if changed:
            checkpoint = release / "checkpoints" / f"workspace-{checksum[:20]}"
            if checkpoint.exists():
                if checksum != artifact_hash(checkpoint / "tempo", checkpoint / "ui"):
                    raise ValueError(f"Existing Tempo checkpoint differs from the build: {checkpoint}")
            else:
                stage.rename(checkpoint)
            job["ProgramArguments"] = [str(checkpoint / "tempo"), "serve", str(database),
                                       str(checkpoint / "ui"), str(port)]
            if "Program" in job:
                job["Program"] = str(checkpoint / "tempo")
            job["RunAtLoad"] = True
            for key, suffix in (("StandardOutPath", "log"), ("StandardErrorPath", "err")):
                job[key] = str(release / f"server-{checkpoint.name}.{suffix}")

        running = registration(config, resources)
        check_port_owner(port)
        if not keep_development:
            for other_port, stamp in other_servers(port, installation).items():
                stop_server(other_port, installation, stamp)
        try:
            if running:
                stop_managed(port)
            if listeners(port):
                raise ValueError(f"Another application is using the registered Tempo port {port}")
            if changed or environment_changed:
                recovery = Path(tempfile.mkdtemp(prefix="recovery-start-", dir=release))
                (recovery / config.name).write_bytes(original)
                backup(database, recovery / "study.sqlite")
                print(f"Saved Tempo backup in {recovery}", flush=True)
                write_plist(config, plistlib.dumps(job))
            start_managed(config, port, dataset, candidate_schema)
        except (OSError, ValueError, subprocess.CalledProcessError, sqlite3.Error, KeyboardInterrupt):
            # The live database may already have accepted writes. Never restore a backup here.
            try:
                if database_status(database)[1] == schema and not listeners(port) - {managed_pid()}:
                    if loaded_resources():
                        stop_managed(port)
                    write_plist(config, original)
                    start_managed(config, port, dataset, schema, previous_release=True)
            except (OSError, ValueError, subprocess.CalledProcessError, sqlite3.Error) as recovery_error:
                print(f"Could not restart the previous Tempo service: {recovery_error}", file=sys.stderr)
            raise
        if not keep_development:
            remaining = other_servers(port, Installation(WORKSPACE))
            if remaining:
                raise ValueError(f"Another Tempo session started during restart on ports {sorted(remaining)}")
    print(f"Tempo is running at http://127.0.0.1:{port}", flush=True)


def main():
    if sys.platform != "darwin":
        raise ValueError("Managed Tempo requires macOS. Use make start-dev for a separate workspace.")
    if os.environ.get("TEMPO_DB") or os.environ.get("TEMPO_PORT"):
        raise ValueError("Use make start-dev to select a database or port.")
    release = Path(pwd.getpwuid(os.getuid()).pw_dir) / "Developer/tempo-v2" / RELEASE
    config = (release / f"{LABEL}.plist").resolve()
    if not config.is_file():
        raise ValueError("No managed Tempo registration. Use make start-dev for a separate workspace.")
    with (release / "start.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        restart(config, keep_development=os.environ.get("TEMPO_KEEP_DEV_SERVERS") == "1")


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError, RuntimeError, TypeError, subprocess.CalledProcessError, sqlite3.Error) as error:
        sys.exit(f"Tempo start: {error}")
