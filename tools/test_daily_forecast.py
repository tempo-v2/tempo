"""First encounters along authored routes, with explicit opponent probabilities."""

import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import chess

from coverage import model_slot
from daily_forecast import forecast


def route(repertoire, color, moves, decisions, fen=chess.STARTING_FEN):
    return dict(repertoire=repertoire, color=color, fen=fen, moves=moves.split(),
                decisions=[dict(ply=ply, card=card) for ply, card in decisions])


def distribution(board, **probabilities):
    return {move.uci(): probabilities.get(move.uci(), 0.) for move in board.legal_moves}


class DailyForecastTest(unittest.TestCase):
    def test_sums_distinct_authored_histories_without_duplicate_encounters(self):
        first = route("a", "black", "g1f3 d7d5 g2g3 g8f6 f3g1 f6g8 g1f3 g8f6",
                      [(1, "nf3"), (3, "shared"), (7, "shared")])
        second = route("a", "black", "g2g3 d7d5 g1f3 g8f6", [(3, "shared")])
        other = route("b", "black", "e2e4 e7e5", [(1, "zero")])
        data = dict(routes=[first, first, second, other], cards=["nf3", "shared", "zero"])

        def predict(board):
            match board.ply():
                case 0:
                    return distribution(board, g1f3=.4, g2g3=.5, d2d4=.1)
                case 2:
                    move = "g2g3" if board.piece_at(chess.F3) else "g1f3"
                    return distribution(board, **{move: 1.})
                case 4:
                    return distribution(board, f3g1=1.)
                case 6:
                    return distribution(board, g1f3=1.)
                case _:
                    self.fail("unexpected forecast node")

        scores = forecast(data, predict)
        self.assertEqual({"kind": "estimated", "probability": .1}, scores["nf3"])
        self.assertAlmostEqual(.225, scores["shared"]["probability"])
        self.assertEqual({"kind": "estimated", "probability": 0.}, scores["zero"])

    def test_explicit_saved_reply_resolves_context_ambiguity(self):
        compatible = route("a", "white", "e2e4 e7e5 g1f3", [(2, "followup")])
        conflicting = route("a", "white", "d2d4 d7d5 c2c4", [(2, "other")])
        authority = route("a", "white", "e2e4", [(0, "root")])
        unsupported = route("b", "black", "e7e5", [(0, "custom")],
                            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1")
        cards = ["followup", "other", "root", "custom", "missing"]
        predict = lambda board: distribution(board, e7e5=.6, c7c5=.4)
        ambiguous = forecast(dict(routes=[compatible, conflicting], cards=cards), predict)
        self.assertEqual({"kind": "unscored"}, ambiguous["followup"])
        scores = forecast(dict(routes=[compatible, conflicting, authority, unsupported], cards=cards), predict)
        self.assertEqual({"kind": "estimated", "probability": .5}, scores["root"])
        self.assertEqual({"kind": "estimated", "probability": .3}, scores["followup"])
        self.assertEqual({"kind": "unscored"}, scores["other"])
        self.assertEqual({"kind": "unscored"}, scores["custom"])
        self.assertEqual({"kind": "unscored"}, scores["missing"])

    def test_horizon_and_shared_physical_admission(self):
        line = route("a", "white", "e2e4 e7e5 g1f3", [(0, "root"), (2, "later")])
        with mock.patch("daily_forecast.HORIZON", 2):
            scores = forecast(dict(routes=[line], cards=["root", "later"]),
                              lambda board: distribution(board, e7e5=1.))
        self.assertEqual({"kind": "estimated", "probability": .5}, scores["root"])
        self.assertEqual({"kind": "unscored"}, scores["later"])

        with tempfile.TemporaryDirectory() as temporary:
            coverage = pathlib.Path(__file__).with_name("coverage.py").read_text()
            daily = pathlib.Path(__file__).with_name("daily_forecast.py").read_text()
            with model_slot(pathlib.Path(temporary)):
                for call in ([sys.executable, "-c", coverage, temporary],
                             [sys.executable, "-c", daily, temporary, coverage]):
                    result = subprocess.run(call, input=b"{}", capture_output=True,
                                            timeout=3, check=False)
                    self.assertEqual(result.returncode, 75)
                    self.assertEqual(result.stdout, b"")
            with model_slot(pathlib.Path(temporary)):
                pass


if __name__ == "__main__":
    unittest.main()
