"""Exercise a built Lean candidate on disposable databases and local provider feeds."""
import argparse
from contextlib import contextmanager
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import socket
import sqlite3
import struct
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


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def serve(binary, database, worker=True, port=None):
    port = port or unused_port()
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


def verify_browse(binary, database):
    with serve(binary, database) as api:
        archive = pgn(
            '[Round "1"]\n[Date "' + 'x' * 80 + '"]\n'
            '[Link "https://lichess.org/' + 'x' * 2050 + '"]\n'
        ).replace('Opponent', 'x' * 270)
        job = wait_until(lambda: api("games/uploads", {"username": "Learner", "pgn": archive}))
        wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE id=?", [job["id"]]) == [("done",)])
        game, = api("games")
        assert game == api("games/archive/" + game["id"])
        assert len(game["black"]) == 256 and len(game["played_on"]) == 64
        assert game["source_url"] is None and game["route"]["moves"]
        assert sql(database, "SELECT length(black),length(played_on),length(source_url) FROM games") == [(270, 80, 2070)]
        assert api("games?after=" + game["id"]) == []
        assert api("openings") == [] and api("openings/repertoires") == []
        assert api("settings") == {} and api("settings/new-openings")
        assert api("study") is None
        assert sql(database, "SELECT count(*) FROM attempts") == [(0,)]
    print("Interactive reads retained pagination, addressed replay, and original oversized archive metadata.")


def verify_preview_admission(binary, database):
    port = unused_port()
    preview = {"pgn": "1. c4 e5 1-0"}
    with serve(binary, database, worker=False, port=port) as api:
        def post(route, body, chunked=False):
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
            try:
                if chunked:
                    body = [body[offset:offset + 1024] for offset in range(0, len(body), 1024)]
                connection.request("POST", "/api/" + route, body,
                                   headers={"Content-Type": "application/json"}, encode_chunked=chunked)
                response = connection.getresponse()
                return response.status, response.read()
            finally:
                connection.close()

        body = json.dumps(preview).encode()
        with socket.create_connection(("127.0.0.1", port), timeout=5) as held:
            held.sendall((
                f"POST /api/openings/pgn HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
                f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n"
                "Connection: close\r\n\r\n"
            ).encode() + body[:1])

            def saturated():
                status, reply = post("openings/pgn", body)
                if status == 503:
                    assert json.loads(reply)["code"] == "view_unavailable"
                    return True
                assert status == 200, (status, reply)
                return False

            wait_until(saturated, timeout=2)
            assert sql(database, "SELECT count(*) FROM jobs") == [(0,)]
            assert sql(database, "SELECT count(*) FROM cards") == [(0,)]
            assert not list(database.with_suffix(".uploads").glob("*"))
            assert api("tactics", {"name": "Independent puzzle", "pgn": "1. d4 d5 1-0"})["id"]
            assert api("games/uploads", {"username": "Learner", "pgn": pgn()})["id"]
            assert api("openings", {"name": "Independent opening", "color": "white",
                "route": {"fen": FEN, "moves": ["d1d5"]}})["id"]
            attempt = api("study/next", {"after": None})
            assert api("study/answer", {
                "attempt": attempt["attempt"], "dataset": attempt["dataset"], "played": None,
            })["rating"] == "Again"
            held.sendall(body[1:])
            response = http.client.HTTPResponse(held)
            response.begin()
            assert response.status == 200
            assert json.loads(response.read())["moves"] == ["c2c4", "e7e5"]

        command = b'{"provider":"archive","operation":"scan"}'
        for chunked in (False, True):
            for size, expected in [(16 * 1024, 200), (16 * 1024 + 1, 413)]:
                assert post("games/jobs", command.ljust(size), chunked)[0] == expected
        pgn_limit = 64 * 1024 * 1024
        for size, expected in [(pgn_limit, 200), (pgn_limit + 1, 400)]:
            line = b'1. c4 e5 1-0'.ljust(size)
            assert post("openings/pgn", b'{"pgn":"' + line + b'"}')[0] == expected
            status, reply = post("games/uploads", b'{"username":"Learner","pgn":"' + line + b'"}')
            assert status == expected, (status, reply)
            if status == 200:
                (file,), = sql(database, "SELECT upload_file FROM jobs WHERE id=?", (json.loads(reply)["id"],))
                assert (database.with_suffix(".uploads") / file).stat().st_size == pgn_limit
    print("Preview reception stayed isolated from durable intake and Study, with the existing JSON and PGN limits.")


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
    previous_version, = sql(database, "PRAGMA user_version")[0]
    subprocess.run([binary, "migrate-games", database], check=True)
    assert sql(database, "SELECT cursor,upload_file FROM jobs WHERE id=?", [job]) == checkpoint
    with serve(binary, database) as api:
        assert api(f"games/jobs/{job}/retry", {}) == {"id": job}
        wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE id=?", [job]) == [("done",)])
        assert sql(database, "SELECT added,existing,rejected FROM jobs WHERE id=?", [job]) == [(2, 0, 0)]
        assert len(api("games")) == 2
        wait_until(lambda: not file.exists())
    print(f"An interrupted schema-{previous_version} upload kept its original ID and staged checkpoint across the offline upgrade.")
    return previous_version


def verify_archive_resume(binary, predecessor, database):
    def game_ir(fen, played):
        def square(text):
            return ord(text[0]) - ord("a") + 8 * (int(text[1]) - 1)
        move = square(played[:2]) | (square(played[2:]) << 6)
        return struct.pack("<BHH", 1, len(fen), 1) + fen.encode() + struct.pack("<H", move)

    neutral = game_ir("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1", "e2e4")
    compared = game_ir(FEN, "e1e2")
    prior_ids = [f"a{number:03d}" for number in range(64)]
    # The last game by ID sits before the sealed game in ordinal order.
    records = [(game, neutral) for game in prior_ids] + [("z", compared), ("m", compared)]
    insert = """INSERT INTO games(id,white,black,learner_color,game_ir,occurrence,outcome)
                VALUES (?,'Learner','Opponent','white',?,'[null,null,null,null]','1-0')"""
    with serve(predecessor, database, worker=False) as api:
        for number in range(118):
            api("openings", {"name": f"Archived reference {number}", "color": "white",
                "route": {"fen": FEN, "moves": ["d1d5"]}, "destination": {"kind": "new"}})
        with sqlite3.connect(database) as db:
            db.executemany(insert, records)
            db.execute("""CREATE TRIGGER fixture_stop_sealed BEFORE UPDATE OF detector_game ON jobs
                          WHEN OLD.detector_game='m' AND NEW.detector_game IS NULL
                            AND OLD.operation='compare_all'
                          BEGIN SELECT RAISE(ABORT, 'pause final sealed batch'); END""")
        parent = api("games/jobs", {"provider": "archive", "operation": "scan"})["id"]

    def lane(op):
        return sql(database, """SELECT id,status,cursor,detector_game,upload_file,window_offset
                                FROM jobs WHERE parent_id=? AND operation=?""", (parent, op))[0]

    with serve(predecessor, database):
        wait_until(lambda: lane("compare_all")[1] == "failed" and lane("capture_all")[1] == "done")
    compare, _, stored, game, file, offset = lane("compare_all")
    capture = lane("capture_all")[0]
    assert json.loads(stored) == {"through": len(records), "after": prior_ids[-1]}
    assert game == "m" and file and offset > 0
    sealed = database.with_suffix(".uploads") / file
    sealed_bytes = sealed.read_bytes()
    assert sql(database, "SELECT count(*) FROM mismatches") == [(117,)]

    with sqlite3.connect(database) as db:
        db.execute("DROP TRIGGER fixture_stop_sealed")
        db.execute("""CREATE TRIGGER fixture_stop_after_page BEFORE UPDATE OF detector_game ON jobs
                      WHEN NEW.detector_game='z' AND OLD.operation='compare_all'
                      BEGIN SELECT RAISE(ABORT, 'pause after committed compatibility page'); END""")
        db.executemany(insert, ((f"b{number:06d}", neutral) for number in range(100_000)))
    old_checkpoints = (lane("compare_all"), lane("capture_all"))
    subprocess.run([binary, "migrate-games", database], check=True)
    assert (lane("compare_all"), lane("capture_all")) == old_checkpoints
    assert sealed.read_bytes() == sealed_bytes
    refused = subprocess.run([predecessor, "serve", database, "0", "--no-worker"],
                             capture_output=True, timeout=5)
    assert refused.returncode != 0 and b"requires a newer release" in refused.stderr

    with serve(binary, database) as api:
        api(f"games/jobs/{parent}/retry", {})
        attempt = api("study/next", {"after": None})
        answer = {"attempt": attempt["attempt"], "dataset": attempt["dataset"], "played": None}
        assert api("study/answer", answer)["rating"] == "Again"
        assert api("study/next", {"after": attempt["attempt"], "dataset": attempt["dataset"]})
        wait_until(lambda: lane("compare_all")[1] == "failed")
        checkpoint = json.loads(lane("compare_all")[2])
        assert checkpoint == {"v2": {"through": len(records), "after_ordinal": 64, "legacy_after_id": "m"}}
        assert sql(database, "SELECT count(*) FROM mismatches") == [(118,)]
        assert lane("capture_all") == old_checkpoints[1]
    sql(database, "DROP TRIGGER fixture_stop_after_page")

    with serve(binary, database) as api:
        api(f"games/jobs/{parent}/retry", {})
        wait_until(lambda: sql(database, "SELECT status FROM jobs WHERE id=?", [parent]) == [("done",)])
        for job in (compare, capture):
            completed = sql(database, "SELECT game_id FROM job_games WHERE job_id=? AND scanned=1 ORDER BY game_id", [job])
            assert completed == [(game,) for game in sorted(game for game, _ in records)]
        assert sql(database, "SELECT added FROM jobs WHERE id=?", [parent]) == [(len(records),)]
        assert sql(database, "SELECT game_id,count(*) FROM mismatches GROUP BY game_id ORDER BY game_id") == [("m", 118), ("z", 118)]
        assert sql(database, "SELECT count(*) FROM attempts WHERE result IS NOT NULL") == [(1,)]
    print("Schema-7 rescan resumed a partial seal and restarted after a completed-prefix page. "
          "Both lanes kept exact membership, findings and parent counts across 100,000 later games. "
          "Study Next and Answer completed during recovery.")


def verify_migration(binary, predecessor_database, destination):
    snapshot = destination.with_name("predecessor-snapshot.sqlite3")
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
        previous_version, = db.execute("PRAGMA previous.user_version").fetchone()
        assert db.execute("PRAGMA user_version").fetchone() == (8,)
        assert db.execute("PRAGMA integrity_check").fetchone() == ("ok",)
        assert not db.execute("PRAGMA foreign_key_check").fetchall()
        tables = db.execute("SELECT name FROM previous.sqlite_schema WHERE type='table'").fetchall()
        for (table,) in tables:
            fields = [v[1] for v in db.execute("SELECT * FROM pragma_table_info(?, 'previous')", (table,))]
            if table == "games" and previous_version < 7:
                fields.remove("route")
                fields.append("rowid")
            columns = ", ".join(f'"{field}"' for field in fields)
            for a, b in [("main", "previous"), ("previous", "main")]:
                sql_diff = f'SELECT {columns} FROM {a}."{table}" EXCEPT SELECT {columns} FROM {b}."{table}" LIMIT 1'
                assert not db.execute(sql_diff).fetchall(), table
        rows, = db.execute("SELECT count(*) FROM games").fetchone()
        if previous_version < 7:
            byte_count, = db.execute("SELECT sum(length(game_ir)) FROM games").fetchone()
            old_bytes = db.execute("SELECT sum(length(route)) FROM previous.games").fetchone()[0]
            assert byte_count < old_bytes
    with serve(binary, destination, worker=False) as api:
        assert all(api(route) is not None for route in ("health", "games", "games/jobs", "study/counts"))
    print(f"Schema {previous_version} → 8 preserved {rows:,} games and all {len(tables)} predecessor tables.")


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
            predecessor = options.predecessor_binary.resolve()
            previous_version = verify_interrupted_upload(options.binary.resolve(), predecessor, root / "unfinished.sqlite3")
            if previous_version == 7:
                verify_archive_resume(options.binary.resolve(), predecessor, root / "rescan.sqlite3")
        verify_games(options.binary.resolve(), root / "games.sqlite3")
        verify_browse(options.binary.resolve(), root / "browse.sqlite3")
        verify_preview_admission(options.binary.resolve(), root / "preview.sqlite3")
