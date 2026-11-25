"""Coverage arithmetic with explicit conditional opponent distributions."""
import unittest
import chess
from coverage import forecast, key, recent_positions


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

    def test_prediction_history_distinguishes_transpositions(self):
        def play(moves):
            board = chess.Board()
            for move in moves:
                board.push_uci(move)
            return board

        left = play(["g1f3", "d7d5", "g2g3"])
        right = play(["g2g3", "d7d5", "g1f3"])
        self.assertEqual(key(left), key(right))
        self.assertNotEqual(
            [key(position) for position in recent_positions(left, 8)],
            [key(position) for position in recent_positions(right, 8)],
        )
        self.assertEqual(
            [key(position) for position in recent_positions(left, 2)],
            [key(play(["g1f3", "d7d5"])), key(left)],
        )
        self.assertEqual(len(left.move_stack), 3)

    def test_retreat_uses_conditional_move_probability(self):
        board = chess.Board()
        board.push_san("e4")
        card = board.copy()
        card.push_san("Nc6")
        data = dict(route=dict(fen=chess.STARTING_FEN, moves=["e2e4"]),
                    color="white", through=3,
                    cards=[dict(fen=card.fen(), id="nf3", answers=["g1f3"], quarantined=False)])

        def predict(position):
            moves = dict.fromkeys((move.uci() for move in position.legal_moves), 0.)
            if len(position.move_stack) == 1:
                moves.update(b8c6=.2, e7e5=.8)
            else:
                moves.update(c6b8=.1, e7e5=.9)
            return moves

        report = forecast(data, predict)
        gaps = {tuple(gap["moves"]): gap["probability"] for gap in report["gaps"]}
        self.assertAlmostEqual(gaps["b8c6", "g1f3", "c6b8"], .02)
        self.assertAlmostEqual(sum(gaps.values()) + report["unresolved"], 1.)

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
