import csv
import io
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest

import chess

from build import build


def row(index, rating, themes="fork pin", moves="e2e4 e7e5 g1f3"):
    return {
        "PuzzleId": f"id{index:03}",
        "FEN": chess.STARTING_FEN,
        "Moves": moves,
        "Rating": rating,
        "Themes": themes,
        "GameUrl": "https://lichess.org/example",
    }


class CatalogTests(unittest.TestCase):
    def test_compressed_export_builds_complete_bands_and_one_final_pack(self):
        rows = [row(0, 999), row(1, 1000, moves="e2e4 0000")]
        rows += [row(i, 1099 if i % 2 else 1000) for i in range(2, 22)]
        rows += [row(i, 1100, "fork") for i in range(22, 27)]
        rows += [row(i, 1400, "fork") for i in range(27, 47)]
        rows += [row(i, 2700, "underPromotion") for i in range(47, 67)]
        rows += [row(i, 1010, "interference") for i in range(67, 86)]
        stream = io.StringIO(newline="")
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
        with tempfile.TemporaryDirectory() as tmp:
            source = Path(tmp) / "puzzles.csv.zst"
            output = Path(tmp) / "packs.sqlite3"
            subprocess.run(["zstd", "-q", "-o", source], input=stream.getvalue().encode(), check=True)
            build(source, output)
            with sqlite3.connect(output) as db:
                self.assertEqual(db.execute("PRAGMA integrity_check").fetchone(), ("ok",))
                self.assertEqual(db.execute("PRAGMA foreign_key_check").fetchall(), [])
                self.assertEqual(db.execute(
                    "SELECT theme, rating_min, rating_end FROM packs WHERE theme != 'mix' "
                    "ORDER BY theme, rating_min"
                ).fetchall(), [
                    ("fork", 1000, 1100), ("fork", 1100, None),
                    ("pin", 1000, 1100), ("underPromotion", 1000, None),
                ])
                self.assertEqual(db.execute(
                    "SELECT tail_candidates_capped FROM themes WHERE theme='interference'"
                ).fetchone(), (19,))
                tail = db.execute(
                    "SELECT rating FROM pack_puzzles JOIN puzzles USING (puzzle_id) "
                    "WHERE theme='fork' AND rating_min=1100 ORDER BY position"
                ).fetchall()
                self.assertEqual(tail, [(1100,)] * 5 + [(1400,)] * 15)
                self.assertEqual(db.execute(
                    "SELECT COUNT(*) FROM pack_puzzles WHERE puzzle_id='id002'"
                ).fetchone(), (3,))
                board = chess.Board()
                board.push_uci("e2e4")
                self.assertEqual(db.execute(
                    "SELECT solver_fen, source_moves FROM puzzles WHERE puzzle_id='id002'"
                ).fetchone(), (board.fen(), "e2e4 e7e5 g1f3"))
                self.assertEqual(db.execute(
                    "SELECT COUNT(*) FROM puzzles WHERE puzzle_id IN ('id000','id001')"
                ).fetchone(), (0,))
            with self.assertRaises(FileExistsError):
                build(source, output)


if __name__ == "__main__":
    unittest.main()
