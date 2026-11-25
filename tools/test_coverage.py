"""Coverage arithmetic with explicit conditional opponent distributions."""
import unittest
import chess
from coverage import forecast


class CoverageTest(unittest.TestCase):
    def test_transposed_first_gap_and_studied_paths(self):
        start = chess.Board()
        cards = []

        def response(moves, identity, answer):
            board = start.copy()
            for move in moves:
                board.push_uci(move)
            cards.append(dict(fen=board.fen(), id=identity, answers=[answer], quarantined=False))

        response(["g1f3"], "nf3", "d7d5")
        response(["g2g3"], "g3", "d7d5")
        data = dict(route=dict(fen=start.fen(), moves=[]), color="black", through=2, cards=cards)

        def predict(board):
            moves = {m.uci(): 0. for m in board.legal_moves}
            if not board.move_stack:
                moves.update(g1f3=.3, g2g3=.7)
            else:
                moves["g2g3" if board.piece_at(chess.F3) else "g1f3"] = 1.
            return moves

        report = forecast(data, predict)
        self.assertEqual([g["probability"] for g in report["gaps"]], [1.])
        response(["g1f3", "d7d5", "g2g3"], "shared", "g8f6")
        report = forecast(data, predict)
        self.assertEqual(report["reach"]["shared"], 1.)
        self.assertEqual(dict(report["paths"]), {("g3", "shared"): .7, ("nf3", "shared"): .3})
        bounded = forecast(data, predict, limit=4)
        self.assertAlmostEqual(sum(m for _, m in bounded["paths"]) + bounded["unresolved"], 1.)

    def test_white_horizon_conflict_and_terminal(self):
        board = chess.Board()
        data = dict(route=dict(fen=board.fen(), moves=[]), color="white", through=1,
                    cards=[dict(fen=board.fen(), id="e4", answers=["e2e4"], quarantined=False)])
        no_opponent = lambda board: self.fail("unexpected opponent turn")
        self.assertEqual(forecast(data, no_opponent)["paths"], [(("e4",), 1.)])
        data["cards"][0]["quarantined"] = True
        self.assertEqual(forecast(data, no_opponent)["conflict"], 1.)
        data["route"]["fen"] = "7k/6Q1/5K2/8/8/8/8/8 b - - 0 1"
        data["through"] = 2
        self.assertEqual(forecast(data, no_opponent)["ended"], 1.)


if __name__ == "__main__":
    unittest.main()
