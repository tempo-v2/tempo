"""Build and serve Tempo in the foreground using development resources."""

import json
import os
from pathlib import Path
import subprocess
import sys

WORKSPACE = Path.cwd().resolve()
UI = WORKSPACE / "tempo-ui"
sys.path.insert(0, str(UI / "tools"))

from development_guard import Installation, check_ui, contains, database_files, identity

DEFAULT_DB = ".local/development/study.sqlite"


def build_ui():
    dependencies = [*UI.glob("vendor/*.tgz"), UI / "package.json", UI / "package-lock.json"]
    stamp = UI / "node_modules/.tempo-installed"
    if not stamp.is_file() or any(dep.stat().st_mtime > stamp.stat().st_mtime for dep in dependencies):
        subprocess.run(["npm", "ci", "--no-audit"], cwd=UI, check=True)
        stamp.touch()
    subprocess.run(["npm", "run", "build"], cwd=UI, check=True)


def main(operation):
    installation = Installation(WORKSPACE)
    if operation in ("start", "build-ui"):
        check_ui(installation, UI)
    if operation == "build-ui":
        build_ui()
        return

    # Use one checked output directory for both final and intermediate Cargo artifacts.
    target = Path(os.environ.get("CARGO_TARGET_DIR") or "target").resolve()
    installation.check_cargo_output(target)
    executable = target / "debug/tempo"
    explicit_db = os.environ.get("TEMPO_DB")
    database = Path(DEFAULT_DB if explicit_db is None else explicit_db).resolve()
    if operation == "start":
        port = int(os.environ["TEMPO_PORT"])
        installation.check_port(port)
    for path in database_files(database):
        installation.check(path)
        if any(contains(output, path) for output in (target, UI / "dist", UI / "node_modules")):
            raise ValueError(f"Development database overlaps build output: {path}")

    existing = identity(database) is not None
    if operation == "init-dev":
        if explicit_db is None or existing:
            raise ValueError("init-dev requires TEMPO_DB naming a new development database")
    elif existing and not database.is_file():
        raise ValueError(f"Development database is not a file: {database}")
    elif not existing and explicit_db is not None:
        raise ValueError("Selected database not found. Use make init-dev TEMPO_DB=PATH to create it.")

    if operation == "start":
        build_ui()
    cargo_env = dict(os.environ)
    cargo_env.pop("MAKEFLAGS", None)
    cargo_env.pop("MFLAGS", None)
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "tempo", "--target-dir", str(target),
         "--config", f"build.build-dir={json.dumps(str(target), ensure_ascii=False)}"],
        env=cargo_env, check=True,
    )
    if not existing:
        os.umask(0o077)
        database.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run([executable, "init", database], check=True)
    if operation == "start":
        os.execv(executable, [str(executable), "serve", str(database), str((UI / "dist").resolve()), str(port)])


if __name__ == "__main__":
    try:
        main(sys.argv[1])
    except KeyboardInterrupt:
        sys.exit(130)
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode)
    except (OSError, RuntimeError, ValueError, TypeError) as error:
        sys.exit(f"Tempo development preflight: {error}")
