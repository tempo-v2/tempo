"""Run with python -m unittest discover -s tools/stockfish -p test_hanging.py.

Set TEMPO_TEST_STOCKFISH to an installed binary for the real JSONL integration.
"""

import io
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

import chess
import chess.engine

import hanging


def request_for(board, *, start=None, moves=None):
    return {
        "start_fen": board.fen() if start is None else start,
        "moves": [] if moves is None else moves,
        "fen": board.fen(),
        "legal_moves": sorted(move.uci() for move in board.legal_moves),
    }


def info(board, uci, depth, rank, cp=25, **extra):
    return {
        "depth": depth,
        "multipv": rank,
        "score": chess.engine.PovScore(chess.engine.Cp(cp), board.turn),
        "pv": [chess.Move.from_uci(uci)],
        "nodes": depth * 100 + rank,
        **extra,
    }


class ReplayContractTests(unittest.TestCase):
    def test_repetition_history_and_move_counters_survive_validation(self):
        board = chess.Board()
        moves = ["g1f3", "g8f6", "f3g1", "f6g8"] * 2
        for move in moves:
            board.push_uci(move)
        restored = hanging.position_from_request(
            request_for(board, start=chess.STARTING_FEN, moves=moves)
        )
        self.assertEqual(len(restored.move_stack), 8)
        self.assertTrue(restored.is_repetition(3))
        self.assertEqual(restored.halfmove_clock, 8)
        self.assertEqual(restored.fullmove_number, 5)
        self.assertFalse(chess.Board(restored.fen()).is_repetition(3))

    def test_counter_mismatch_and_illegal_prefix_are_rejected(self):
        request = request_for(chess.Board())
        request["fen"] = request["fen"].rsplit(" ", 1)[0] + " 2"
        with self.assertRaisesRegex(ValueError, "does not match"):
            hanging.position_from_request(request)
        request = request_for(chess.Board())
        request["moves"] = ["e2e5"]
        with self.assertRaisesRegex(ValueError, "illegal move"):
            hanging.position_from_request(request)
        request["moves"] = ["0000"]
        with self.assertRaisesRegex(ValueError, "illegal move"):
            hanging.position_from_request(request)

    def test_cozy_counter_saturation_does_not_discard_real_history(self):
        board = chess.Board()
        moves = ["g1f3", "g8f6", "f3g1", "f6g8"] * 30
        for move in moves:
            board.push_uci(move)
        request = request_for(board, start=chess.STARTING_FEN, moves=moves)
        fields = request["fen"].split()
        fields[4] = "100"
        request["fen"] = " ".join(fields)
        restored = hanging.position_from_request(request)
        self.assertEqual(restored.halfmove_clock, 120)
        self.assertEqual(len(restored.move_stack), 120)

    def test_equivalent_uncapturable_ep_serialization_is_irrelevant(self):
        board = chess.Board()
        board.push_uci("e2e4")
        request = request_for(board, start=chess.STARTING_FEN, moves=["e2e4"])
        request["fen"] = board.fen(en_passant="fen")
        restored = hanging.position_from_request(request)
        self.assertEqual(restored.fen(), board.fen())

    def test_complete_legal_root_set_is_required(self):
        request = request_for(chess.Board())
        request["legal_moves"].pop()
        with self.assertRaisesRegex(ValueError, "each legal root exactly once"):
            hanging.position_from_request(request)
        request = request_for(chess.Board())
        request["legal_moves"].append(request["legal_moves"][0])
        with self.assertRaisesRegex(ValueError, "each legal root exactly once"):
            hanging.position_from_request(request)

    def test_legal_ep_and_promotion_roots_keep_exact_uci(self):
        for fen, expected in (
            ("7k/8/8/3pP3/8/8/8/7K w - d6 0 1", "e5d6"),
            ("r6k/1P6/8/8/8/8/8/7K w - - 0 1", "b7a8n"),
        ):
            board = hanging.position_from_request(request_for(chess.Board(fen)))
            self.assertIn(expected, {move.uci() for move in board.legal_moves})


class ExactIterationTests(unittest.TestCase):
    def setUp(self):
        # Two legal king moves give a small complete root set.
        self.board = chess.Board("7k/8/8/8/5b2/8/8/7K w - - 0 1")
        self.roots = sorted(move.uci() for move in self.board.legal_moves)
        self.assertEqual(len(self.roots), 2)

    def test_new_partial_depth_cannot_borrow_an_old_root_score(self):
        collected = hanging.ExactIterations(self.board)
        for rank, root in enumerate(self.roots, 1):
            collected.observe(info(self.board, root, 12, rank))
        collected.observe(info(self.board, self.roots[0], 13, 1, cp=99))
        collected.observe(info(self.board, self.roots[1], 12, 2, cp=-999))
        self.assertEqual(collected.complete[0], 12)
        self.assertEqual({root["score"]["value"] for root in collected.complete[1]}, {25})

    def test_bounds_and_duplicate_roots_do_not_make_a_complete_iteration(self):
        collected = hanging.ExactIterations(self.board)
        collected.observe(info(self.board, self.roots[0], 12, 1))
        collected.observe(info(self.board, self.roots[1], 12, 2, lowerbound=True))
        self.assertIsNone(collected.complete)
        collected.observe(info(self.board, self.roots[0], 12, 2))
        self.assertIsNone(collected.complete)
        collected.observe(info(self.board, self.roots[1], 12, 2))
        self.assertEqual(collected.complete[0], 12)

    def test_bound_invalidates_the_same_rank_in_pending_iteration(self):
        collected = hanging.ExactIterations(self.board)
        collected.observe(info(self.board, self.roots[0], 12, 1))
        collected.observe(info(self.board, self.roots[0], 12, 1, upperbound=True))
        collected.observe(info(self.board, self.roots[1], 12, 2))
        self.assertIsNone(collected.complete)

    def test_equal_scores_keep_stockfish_rank_order(self):
        collected = hanging.ExactIterations(self.board)
        for rank, root in enumerate(reversed(self.roots), 1):
            collected.observe(info(self.board, root, 12, rank))
        self.assertEqual(
            [root["uci"] for root in collected.complete[1]], list(reversed(self.roots))
        )

    def test_black_perspective_and_mate_are_typed(self):
        board = chess.Board("r6k/8/8/8/8/8/N7/7K b - - 0 1")
        line = info(board, "a8a2", 12, 1)
        line["score"] = chess.engine.PovScore(chess.engine.Cp(-400), chess.WHITE)
        self.assertEqual(
            hanging.exact_root_score(board, line)["score"],
            {"kind": "centipawns", "value": 400},
        )
        line["score"] = chess.engine.PovScore(chess.engine.Mate(-3), chess.WHITE)
        self.assertEqual(
            hanging.exact_root_score(board, line)["score"],
            {"kind": "mate", "value": 3},
        )
        line["score"] = chess.engine.PovScore(chess.engine.Mate(0), chess.BLACK)
        self.assertIsNone(hanging.exact_root_score(board, line))

    def test_illegal_engine_root_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "illegal root"):
            hanging.ExactIterations(self.board).observe(info(self.board, "h1h8", 12, 1))


class JsonLinesTests(unittest.TestCase):
    def test_bad_request_does_not_discard_the_following_request(self):
        class StubEvaluator:
            engine_name = "test"

            def evaluate(self, request, nodes, min_depth):
                hanging.position_from_request(request)
                return {"status": "incomplete", "reason": "test budget"}

        request = request_for(chess.Board()) | {"request_id": 42, "nodes": 500_000, "min_depth": 12}
        source = io.BytesIO(b"not json\n" + json.dumps(request).encode() + b"\n")
        output = io.StringIO()
        hanging.serve(StubEvaluator(), source, output)
        responses = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual([response["status"] for response in responses], ["ready", "failed", "incomplete"])
        self.assertEqual(responses[-1]["request_id"], 42)
        self.assertNotIn("evaluation", responses[-1])

    def test_resource_limits_are_explicit(self):
        for kwargs in ({"nodes": 0}, {"nodes": True}, {"min_depth": 0}, {"hash_mb": -1}):
            with self.assertRaises(ValueError):
                hanging.SearchConfiguration(**kwargs)


@unittest.skipUnless(os.environ.get("TEMPO_TEST_STOCKFISH"), "set TEMPO_TEST_STOCKFISH for real engine coverage")
class RealStockfishTests(unittest.TestCase):
    def worker(self, requests, *, nodes=500_000, min_depth=12):
        command = [sys.executable, str(Path(hanging.__file__)), os.environ["TEMPO_TEST_STOCKFISH"]]
        requests = [
            request | {"request_id": index, "nodes": nodes, "min_depth": min_depth}
            for index, request in enumerate(requests)
        ]
        result = subprocess.run(
            command,
            input="".join(json.dumps(request) + "\n" for request in requests),
            text=True,
            capture_output=True,
            timeout=40,
            check=True,
        )
        return [json.loads(line) for line in result.stdout.splitlines()]

    def test_persistent_worker_covers_every_root_for_both_colours(self):
        board = chess.Board("7k/n7/8/8/8/8/8/R6K w - - 0 1")
        requests = [request_for(board), request_for(board.mirror())]
        replies = self.worker(requests)
        self.assertEqual(len(replies), 3)
        self.assertEqual(replies[0]["status"], "ready")
        self.assertEqual(replies[0]["protocol"], 1)
        self.assertEqual(replies[0]["python_chess_version"], chess.__version__)
        for index, (request, reply, capture) in enumerate(zip(requests, replies[1:], ["a1a7", "a8a2"])):
            self.assertEqual(reply["status"], "complete", reply)
            self.assertEqual(reply["request_id"], index)
            self.assertEqual(reply["engine"], replies[0]["engine"])
            self.assertGreaterEqual(reply["depth"], 12)
            self.assertGreater(reply["nodes"], 0)
            roots = {entry["uci"]: entry for entry in reply["roots"]}
            self.assertEqual(set(roots), set(request["legal_moves"]))
            self.assertTrue(all(entry["bound"] == "exact" for entry in roots.values()))
            self.assertGreater(roots[capture]["score"]["value"], 0)

    def test_budget_exhaustion_has_no_fabricated_scores(self):
        replies = self.worker([request_for(chess.Board())], nodes=1)
        self.assertEqual(replies[1]["status"], "incomplete")
        self.assertNotIn("roots", replies[1])

    def test_mate_scores_survive_real_protocol(self):
        board = chess.Board("7k/8/5KQ1/8/8/8/8/8 w - - 0 1")
        replies = self.worker([request_for(board)])
        self.assertEqual(replies[1]["status"], "complete", replies[1])
        roots = {entry["uci"]: entry for entry in replies[1]["roots"]}
        self.assertEqual(roots["g6g7"]["score"], {"kind": "mate", "value": 1})


if __name__ == "__main__":
    unittest.main()
