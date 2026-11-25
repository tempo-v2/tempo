"""Exercise a built Lean candidate on disposable databases and local provider feeds."""
import argparse
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen


FEN = "4k3/8/8/3n4/8/8/8/3RK3 w - - 0 1"


def pgn(extra="", result="1/2-1/2", move="Ke2"):
    return (
        '[White "Learner"]\n[Black "Opponent"]\n[SetUp "1"]\n'
        f'[FEN "{FEN}"]\n{extra}\n1. {move} {result}\n\n'
    )


def wait_until(check, timeout=20):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        try:
            if result := check():
                return result
        except (ConnectionError, OSError) as error:
            if isinstance(error, HTTPError) and error.code != 503:
                raise
        time.sleep(.025)
    raise AssertionError("condition did not settle")


def sql(database, statement, values=()):
    with sqlite3.connect(database) as connection:
        return connection.execute(statement, values).fetchall()


@contextmanager
def serve(binary, database, worker=True):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    flags = [] if worker else ["--no-worker"]
    with subprocess.Popen([binary, "serve", database, str(port), *flags],
                          stdout=subprocess.DEVNULL) as service:
        def api(route, body=None):
            request = Request(
                f"http://127.0.0.1:{port}/api/{route}",
                data=None if body is None else json.dumps(body).encode(),
                headers={"Content-Type": "application/json"},
            )
            with urlopen(request, timeout=4) as response:
                return json.load(response)
        try:
            wait_until(lambda: api("health"))
            yield api
        finally:
            service.terminate()
            service.wait(timeout=5)


class Feed(BaseHTTPRequestHandler):
    def do_GET(self):
        link = {
            "/lichess": "https://lichess.org/fixture",
            "/chesscom": "https://www.chess.com/game/fixture",
        }[self.path]
        body = pgn(f'[Round "1"]\n[Link "{link}"]\n').encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def verify_games(binary, database):
    def job_status(job):
        return sql(database, "SELECT status FROM jobs WHERE id=?", [job])[0][0]

    def settle():
        return sql(database, "SELECT count(*) FROM jobs WHERE status IN ('running','queued')") == [(0,)]

    with serve(binary, database) as api:
        manual = wait_until(lambda: api("games/uploads", {"username": "Learner", "pgn": pgn('[Round "1"]\n')}))
        wait_until(settle)
        archive = api("games")
        assert len(archive) == len(api("tactics")) == 1, api("games/jobs")
        assert api("games/archive/" + archive[0]["id"]) == archive[0]
        assert job_status(manual["id"]) == "done"
        api("settings", {"lichess": "Learner", "chesscom": "Learner"})

    # Freeze each provider's discovery at a local fixture URL. Production
    # still downloads, stages and parses windows through the actual runner.
    with ThreadingHTTPServer(("127.0.0.1", 0), Feed) as feed:
        threading.Thread(target=feed.serve_forever, daemon=True).start()
        try:
            with serve(binary, database, worker=False) as api:
                for provider in ("lichess", "chesscom"):
                    job = api("games/jobs", {"provider": provider, "operation": "all"})["id"]
                    cursor = {"from": 0, "through": 1000, "next": 0,
                              "windows": [f"http://127.0.0.1:{feed.server_port}/{provider}"]}
                    sql(database, "UPDATE jobs SET cursor=? WHERE id=?", [json.dumps(cursor), job])
            with serve(binary, database) as api:
                wait_until(settle)
                assert [row["id"] for row in api("games")] == [archive[0]["id"]]
                assert sql(database, "SELECT added,existing FROM jobs WHERE provider IN ('lichess','chesscom')") == [(0, 1), (0, 1)]
                assert len(api("tactics")) == 1
                for round_number, result in [(2, "1/2-1/2"), (1, "1-0")]:
                    tags = f'[Round "{round_number}"]\n'
                    wait_until(lambda: api("games/uploads", {"username": "Learner", "pgn": pgn(tags, result)}))
                    wait_until(settle)
                assert len(api("games")) == len(api("tactics")) == 2
                assert sql(database, "SELECT sum(rejected) FROM jobs WHERE provider='pgn'") == [(1,)]

                def opening(name, move):
                    return api("openings", {"name": name, "color": "white",
                        "route": {"fen": FEN, "moves": [move]}, "destination": {"kind": "new"}})

                opening("Original analysis", "d1d5")
                sql(database, """CREATE TRIGGER fixture_comparison_failure BEFORE INSERT ON mismatches
                                  BEGIN SELECT RAISE(ABORT,'fixture comparison failure'); END""")
                scan = api("games/jobs", {"provider": "archive", "operation": "scan"})["id"]
                wait_until(lambda: job_status(scan) == "failed")
                wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE parent_id=? AND operation='capture_all'", [scan]) == [("done",)])
                assert sql(database, "SELECT detector_game IS NOT NULL,upload_file IS NOT NULL,window_offset FROM jobs WHERE parent_id=? AND operation='compare_all'", [scan]) == [(1, 1, 0)]
                opening("Added during retry", "e1d2")
                sql(database, "DROP TRIGGER fixture_comparison_failure")
                assert api(f"games/jobs/{scan}/retry", {}) == {"id": scan}
                wait_until(settle)
                assert sql(database, "SELECT status,added FROM jobs WHERE id=?", [scan]) == [("done", 2)]
                # First sealed game keeps its original facts. The second
                # game takes a later snapshot when it starts.
                assert sql(database, "SELECT count(*) FROM mismatches") == [(3,)]
                new_scan = api("games/jobs", {"provider": "archive", "operation": "scan"})["id"]
                wait_until(settle)
                assert sql(database, "SELECT status,added FROM jobs WHERE id=?", [new_scan]) == [("done", 2)]
                assert sql(database, "SELECT count(*) FROM mismatches") == [(4,)]
                assert len(api("tactics")) == 2
                for stage, expected in [("Semifinal", (1, 0)), ("SEMIFINAL", (0, 1)),
                                        ("Final", (1, 0)), ("?", (0, 1))]:
                    pgn_text = pgn(f'[Round "{stage}"]\n').replace("Opponent", "NamedRounds")
                    added = wait_until(lambda: api("games/uploads", {"username": "Learner", "pgn": pgn_text}))["id"]
                    wait_until(settle)
                    assert sql(database, "SELECT added,existing FROM jobs WHERE id=?", [added]) == [expected]
                assert sql(database, "SELECT count(*) FROM games WHERE black='NamedRounds'") == [(2,)]
                wait_until(lambda: not list(database.with_suffix(".uploads").iterdir()))
        finally:
            feed.shutdown()
    print("Manual and both provider turns share game identity. Detector failure, sealed retry, fresh rescan and Tactics verified.")


def verify_interrupted_upload(binary, predecessor, database):
    payload = pgn('[Round "11"]\n') + pgn('[Round "12"]\n', move='Kd2')
    with serve(predecessor, database, worker=False) as api:
        job = api("games/uploads", {"username": "Learner", "pgn": payload})["id"]
        sql(database, f"""CREATE TRIGGER fixture_stop_upload BEFORE UPDATE OF cursor ON jobs
                         WHEN OLD.id={job} AND OLD.cursor IS NOT NULL AND NEW.cursor IS NOT OLD.cursor
                         BEGIN SELECT RAISE(ABORT, 'pause predecessor upload'); END""")
        refused = subprocess.run([binary, "migrate-games", database], capture_output=True)
        assert refused.returncode != 0 and b"owns this database" in refused.stderr
    with serve(predecessor, database):
        wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE id=?", [job]) == [("failed",)])
    checkpoint = sql(database, "SELECT cursor,upload_file FROM jobs WHERE id=?", [job])
    assert checkpoint[0][0] == str(len(pgn('[Round "11"]\n').encode()))
    file = database.with_suffix(".uploads") / checkpoint[0][1]
    assert file.read_text() == payload
    assert sql(database, "SELECT count(*) FROM games") == [(1,)]
    sql(database, "DROP TRIGGER fixture_stop_upload")
    subprocess.run([binary, "migrate-games", database], check=True)
    assert sql(database, "SELECT cursor,upload_file FROM jobs WHERE id=?", [job]) == checkpoint
    with serve(binary, database) as api:
        assert api(f"games/jobs/{job}/retry", {}) == {"id": job}
        wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE id=?", [job]) == [("done",)])
        assert sql(database, "SELECT added,existing,rejected FROM jobs WHERE id=?", [job]) == [(2, 0, 0)]
        assert len(api("games")) == 2
        wait_until(lambda: not file.exists())
    print("An interrupted schema-6 upload kept its original ID and staged checkpoint across the offline upgrade.")


def verify_migration(binary, predecessor_database, destination):
    snapshot = destination.with_name("schema6-snapshot.sqlite3")
    with sqlite3.connect(predecessor_database.as_uri() + "?mode=ro", uri=True) as source:
        with sqlite3.connect(snapshot) as copy:
            source.backup(copy)
    with sqlite3.connect(snapshot) as source:
        with sqlite3.connect(destination) as copy:
            source.backup(copy)
    subprocess.run([binary, "migrate-games", destination], check=True)
    subprocess.run([binary, "migrate-games", destination], check=True)
    with sqlite3.connect(destination) as db:
        db.execute("ATTACH DATABASE ? AS previous", (snapshot.as_uri() + "?mode=ro",))
        assert db.execute("PRAGMA user_version").fetchone() == (7,)
        assert db.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        assert not db.execute("PRAGMA foreign_key_check").fetchall()
        tables = db.execute("SELECT name FROM previous.sqlite_schema WHERE type='table'").fetchall()
        for (table,) in tables:
            fields = [v[1] for v in db.execute("SELECT * FROM pragma_table_info(?, 'previous')", (table,))]
            if table == "games":
                fields.remove("route")
                fields.append("rowid")
            columns = ", ".join(f'"{field}"' for field in fields)
            for a, b in [("main", "previous"), ("previous", "main")]:
                sql_diff = f'SELECT {columns} FROM {a}."{table}" EXCEPT SELECT {columns} FROM {b}."{table}" LIMIT 1'
                assert not db.execute(sql_diff).fetchall(), table
        rows, byte_count = db.execute("SELECT count(*), sum(length(game_ir)) FROM games").fetchone()
        old_bytes = db.execute("SELECT sum(length(route)) FROM previous.games").fetchone()[0]
        assert byte_count < old_bytes
    with serve(binary, destination, worker=False) as api:
        assert all(api(route) is not None for route in ("health", "games", "games/jobs", "study/counts"))
    print(f"Schema 6 → 7 preserved {rows:,} games and all predecessor semantic rows. Route payloads {old_bytes:,} → {byte_count:,} bytes.")


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__)
    cli.add_argument("binary", type=Path)
    cli.add_argument("--predecessor-database", type=Path)
    cli.add_argument("--predecessor-binary", type=Path)
    options = cli.parse_args()
    with tempfile.TemporaryDirectory(prefix="tempo-game-ir-") as area:
        root = Path(area)
        if options.predecessor_database:
            verify_migration(options.binary.resolve(), options.predecessor_database.resolve(), root / "converted.sqlite3")
        if options.predecessor_binary:
            verify_interrupted_upload(options.binary.resolve(), options.predecessor_binary.resolve(), root / "unfinished.sqlite3")
        verify_games(options.binary.resolve(), root / "games.sqlite3")
