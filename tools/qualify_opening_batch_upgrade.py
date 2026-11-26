"""Run with the Coverage Python environment and candidate/predecessor Tempo binaries."""

from contextlib import contextmanager
import json
from pathlib import Path
import random
import socket
import sqlite3
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen

import chess
import chess.pgn


@contextmanager
def serving(binary, database):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    process = subprocess.Popen(
        [binary, "serve", database, str(port), "--no-worker"],
        stdout=subprocess.DEVNULL,
    )

    def api(path, body=None):
        payload = json.dumps(body).encode() if body is not None else None
        request = Request(
            f"http://127.0.0.1:{port}/api/{path}", data=payload,
            headers={"Content-Type": "application/json"},
        )
        try:
            with urlopen(request, timeout=15) as response:
                return response.status, json.load(response)
        except HTTPError as error:
            return error.code, json.load(error)

    try:
        for _ in range(100):
            try:
                if api("health")[0] == 200:
                    break
            except OSError:
                time.sleep(0.035)
        else:
            raise RuntimeError("Tempo fixture did not start")
        yield api
    finally:
        process.terminate()
        process.wait(timeout=5)


def two_line_pgn():
    game = chess.pgn.Game()
    first = game.add_variation(chess.Move.from_uci("g1f3"))
    first.add_variation(chess.Move.from_uci("g8f6")).add_variation(chess.Move.from_uci("g2g3"))
    variation = first.add_variation(chess.Move.from_uci("d7d5"))
    board = variation.board()
    random_moves = random.Random(20261125)
    seen = {board.epd()}
    for _ in range(90):
        choices = []
        for move in board.legal_moves:
            if board.is_capture(move) or move.promotion:
                continue
            board.push(move)
            if board.epd() not in seen and not board.is_game_over():
                choices.append(move)
            board.pop()
        move = random_moves.choice(choices)
        variation = variation.add_variation(move)
        board.push(move)
        seen.add(board.epd())
    return game.accept(chess.pgn.StringExporter(headers=False, comments=False))


def stop_predecessor_batch(predecessor, database):
    start = chess.STARTING_FEN
    with serving(predecessor, database) as api:
        status, saved = api("openings", {
            "name": "Primary title", "color": "white",
            "route": {"fen": start, "moves": ["g1f3", "g8f6", "g2g3"]},
        })
        assert status == 200, saved
        repertoire = saved["id"]
        scan = {"repertoire": repertoire, "color": "white", "route": {"fen": start, "moves": []}, "through": 6}
        gaps = [{"fen": start, "moves": [], "probability": 1}]
        with sqlite3.connect(database) as store:
            revision, = store.execute("SELECT revision FROM coverage_revision WHERE repertoire=?", (repertoire,)).fetchone()
            store.execute("INSERT INTO coverage_report VALUES(?,?,?,?,?)", ("fixture", repertoire, revision, json.dumps(scan), "{}"))
            store.execute("INSERT INTO coverage_gap_page VALUES(?,?,?)", ("fixture", 0, json.dumps(gaps)))
            store.execute("INSERT INTO coverage_view VALUES(?,?,?,?)", ("fixture", '{"total":1}', '{"cards":[],"paths":[]}', '"maia3-5m-1500-history-v1"'))
            store.executescript("""
                CREATE TRIGGER suspend_batch_update BEFORE UPDATE ON gap_batch
                WHEN OLD.line=1 AND OLD.card>=40
                BEGIN SELECT RAISE(ABORT,'fixture stop after the completed prefix and partial line'); END;
                CREATE TRIGGER suspend_batch_delete BEFORE DELETE ON gap_batch
                WHEN OLD.line=1 AND OLD.card>=40
                BEGIN SELECT RAISE(ABORT,'fixture stop after the completed prefix and partial line'); END;
            """)
        draft = {"report": "fixture", "gap": 0, "pgn": two_line_pgn()}
        status, preview = api("coverage/preview", draft)
        assert status == 200 and preview["batched"] and not preview["conflicts"], preview
        request = {"draft": draft, "command": preview["command"]}
        status, failed = api("coverage/save", request)
        assert status == 500, failed
        with sqlite3.connect(database) as store:
            checkpoint = store.execute("SELECT revision,line,card,total FROM gap_batch").fetchone()
            assert checkpoint[1:] == (1, 40, 2), checkpoint
            prefix_id, = store.execute("SELECT id FROM openings WHERE name='Gap continuation'").fetchone()
            store.executescript("DROP TRIGGER suspend_batch_update; DROP TRIGGER suspend_batch_delete;")
    return repertoire, request, checkpoint, prefix_id


def qualify(candidate, predecessor, database):
    repertoire, request, checkpoint, prefix_id = stop_predecessor_batch(predecessor, database)
    subprocess.run([candidate, "migrate-games", database], check=True)
    with sqlite3.connect(database) as store:
        assert store.execute("SELECT alias_of FROM openings WHERE id=?", (prefix_id,)).fetchone() == (repertoire,)
        assert store.execute("SELECT revision,line,card,total,resolved_lines FROM gap_batch").fetchone() == (*checkpoint, None)
    with serving(candidate, database) as api:
        status, reply = api("coverage/save", request)
        assert status == 200 and reply["command"] == request["command"], reply
        assert len(reply["lines"]) == 2 and reply["lines"][0] == prefix_id, reply
        assert api("coverage/save", request) == (200, reply)
        with sqlite3.connect(database) as store:
            assert store.execute("SELECT count(*) FROM gap_batch").fetchone() == (0,)
            assert store.execute("SELECT count(*) FROM gap_save_receipt").fetchone() == (1,)
            assert store.execute("SELECT count(*) FROM authored_openings").fetchone() == (2,)
            assert store.execute("SELECT count(*) FROM authored_openings WHERE id=?", (reply["lines"][1],)).fetchone() == (1,)
    print("Predecessor Gaps batch resumed after one completed line and 40 responses. The old alias ID and new resolved ID replayed exactly.")


if __name__ == "__main__":
    candidate, predecessor = [Path(path).resolve() for path in sys.argv[1:]]
    with TemporaryDirectory(prefix="to-batch-", dir="/tmp") as temporary:
        qualify(candidate, predecessor, Path(temporary) / "fixture.sqlite3")
