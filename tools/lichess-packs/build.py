#!/usr/bin/env python3
"""Build the standalone Tempo Lichess pack catalog."""
import argparse
from collections import defaultdict
import csv
from dataclasses import dataclass
import hashlib
import io
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile

import chess


THEME_REVISION = "6f91abdf23132a7e7f141c9cbaaf7c493f2b7d32"
THEMES = frozenset("""
    mix opening middlegame endgame rookEndgame bishopEndgame pawnEndgame
    knightEndgame queenEndgame queenRookEndgame advancedPawn attackingF2F7
    capturingDefender discoveredAttack doubleCheck exposedKing fork hangingPiece
    kingsideAttack pin queensideAttack sacrifice skewer trappedPiece attraction
    clearance collinearMove discoveredCheck defensiveMove deflection interference
    intermezzo quietMove xRayAttack zugzwang mate mateIn1 mateIn2 mateIn3 mateIn4
    mateIn5 anastasiaMate arabianMate backRankMate balestraMate blindSwineMate
    bodenMate cornerMate doubleBishopMate dovetailMate epauletteMate hookMate
    killBoxMate pillsburysMate morphysMate operaMate swallowstailMate triangleMate
    vukovicMate smotheredMate castling enPassant promotion underPromotion
    equality advantage crushing oneMove short long veryLong master masterVsMaster
    superGM
""".split())
PACK_SIZE = 20
MIN_RATING = 1000
BAND_WIDTH = 100

SCHEMA = """
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE themes (
    theme TEXT PRIMARY KEY,
    first_short_band INTEGER NOT NULL,
    tail_candidates_capped INTEGER NOT NULL
);
CREATE TABLE puzzles (
    puzzle_id TEXT PRIMARY KEY,
    rating INTEGER NOT NULL CHECK (rating >= 1000),
    source_fen TEXT NOT NULL,
    source_moves TEXT NOT NULL,
    solver_fen TEXT NOT NULL,
    tags TEXT NOT NULL,
    game_url TEXT NOT NULL
);
CREATE TABLE packs (
    theme TEXT NOT NULL REFERENCES themes(theme),
    rating_min INTEGER NOT NULL,
    rating_end INTEGER,
    PRIMARY KEY (theme, rating_min)
);
CREATE TABLE pack_puzzles (
    theme TEXT NOT NULL,
    rating_min INTEGER NOT NULL,
    position INTEGER NOT NULL CHECK (position BETWEEN 1 AND 20),
    puzzle_id TEXT NOT NULL REFERENCES puzzles(puzzle_id),
    PRIMARY KEY (theme, rating_min, position),
    UNIQUE (theme, puzzle_id),
    FOREIGN KEY (theme, rating_min) REFERENCES packs(theme, rating_min)
);
"""


@dataclass(frozen=True)
class Puzzle:
    puzzle_id: str
    rating: int
    source_fen: str
    source_moves: str
    solver_fen: str
    tags: str
    game_url: str


def checked_puzzle(row, rating):
    moves = row["Moves"].split()
    if len(moves) < 2 or "0000" in moves:
        return None
    try:
        board = chess.Board(row["FEN"])
        if not board.is_valid():
            return None
        board.push_uci(moves[0])
        solver_fen = board.fen()
        for move in moves[1:]:
            board.push_uci(move)
    except ValueError:
        return None
    return Puzzle(row["PuzzleId"], rating, row["FEN"], " ".join(moves),
                  solver_fen, row["Themes"], row["GameUrl"])


def collect(rows, themes):
    pools = defaultdict(dict)
    for row in rows:
        rating = int(row["Rating"])
        if rating < MIN_RATING:
            continue
        band = rating // BAND_WIDTH * BAND_WIDTH
        puzzle_id = row["PuzzleId"]
        tags = set(row["Themes"].split()).intersection(themes)
        if "mix" in themes:
            tags.add("mix")
        available = [
            pool for theme in tags
            if len(pool := pools[theme, band]) < PACK_SIZE and puzzle_id not in pool
        ]
        if available and (puzzle := checked_puzzle(row, rating)):
            for pool in available:
                pool[puzzle_id] = puzzle
    return pools


def packs_for(pools, theme):
    band = MIN_RATING
    while len(pool := pools.get((theme, band), {})) == PACK_SIZE:
        yield band, band + BAND_WIDTH, list(pool.values())
        band += BAND_WIDTH

    tail = list(pool.values())
    for higher in sorted(start for key, start in pools if key == theme and start > band):
        if len(tail) == PACK_SIZE:
            break
        tail.extend(list(pools[theme, higher].values())[:PACK_SIZE - len(tail)])
    yield band, None, tail


def write_catalog(db, pools, themes, digest):
    db.execute("PRAGMA foreign_keys = ON")
    db.executescript(SCHEMA)
    with db:
        db.executemany("INSERT INTO metadata VALUES (?, ?)", {
            "format": "1",
            "source": "https://database.lichess.org/lichess_db_puzzle.csv.zst",
            "source_sha256": digest,
            "source_license": "CC0-1.0",
            "theme_revision": THEME_REVISION,
        }.items())
        stored = set()
        for theme in sorted(themes):
            packs = list(packs_for(pools, theme))
            short_band, _, tail = packs[-1]
            db.execute("INSERT INTO themes VALUES (?, ?, ?)", (theme, short_band, len(tail)))
            for start, end, members in packs:
                if len(members) < PACK_SIZE:
                    continue
                db.execute("INSERT INTO packs VALUES (?, ?, ?)", (theme, start, end))
                for position, puzzle in enumerate(
                    sorted(members, key=lambda puzzle: (puzzle.rating, puzzle.puzzle_id)), 1
                ):
                    if puzzle.puzzle_id not in stored:
                        db.execute("INSERT INTO puzzles VALUES (?, ?, ?, ?, ?, ?, ?)", (
                            puzzle.puzzle_id, puzzle.rating, puzzle.source_fen, puzzle.source_moves,
                            puzzle.solver_fen, puzzle.tags, puzzle.game_url,
                        ))
                        stored.add(puzzle.puzzle_id)
                    db.execute("INSERT INTO pack_puzzles VALUES (?, ?, ?, ?)",
                               (theme, start, position, puzzle.puzzle_id))


def build(source, output):
    if output.exists():
        raise FileExistsError(output)
    # Use one descriptor for hashing and decompression.
    with source.open("rb") as compressed:
        digest = hashlib.file_digest(compressed, "sha256").hexdigest()
        compressed.seek(0)
        with subprocess.Popen(["zstd", "-dc"], stdin=compressed, stdout=subprocess.PIPE) as zstd:
            with io.TextIOWrapper(zstd.stdout, encoding="utf-8", newline="") as stream:
                pools = collect(csv.DictReader(stream), THEMES)
            if zstd.wait():
                raise RuntimeError("Lichess export decompression failed")

    with tempfile.TemporaryDirectory(dir=output.parent) as temporary:
        database = Path(temporary) / "catalog.sqlite3"
        db = sqlite3.connect(database)
        try:
            write_catalog(db, pools, THEMES, digest)
        finally:
            db.close()
        os.link(database, output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("output", type=Path)
    arguments = parser.parse_args()
    build(arguments.source, arguments.output)
