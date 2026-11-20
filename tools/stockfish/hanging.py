"""Bounded, read-only all-root evaluation for Tempo's tactical detector.

Run with ``python hanging.py /path/to/stockfish``. One ready message is followed
by one response per JSONL request. The parent owns cancellation and the process
group; Stockfish deliberately inherits that group. The process is reused, but
each request starts a new engine game to clear search history. No tactical
classification, move tolerance, puzzle generation, or persistence lives here.

Requests contain request_id, nodes, min_depth, start_fen, the actual UCI move
prefix in moves, the resulting fen, and the complete legal_moves set supplied
by Chess. Results contain every legal root, in engine rank order, at the
deepest completed, exact, same-depth MultiPV iteration.
An incomplete search has no evaluation, rather than fabricated zero scores.

Dependency used by the focused tests: chess==1.11.2 (python-chess module).
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import sys
from typing import Any, BinaryIO, TextIO

import chess
import chess.engine


MAX_REQUEST_BYTES = 1_048_576


@dataclass(frozen=True)
class SearchConfiguration:
    nodes: int = 500_000
    min_depth: int = 12
    hash_mb: int = 64

    def __post_init__(self) -> None:
        for name, value, maximum in (
            ("nodes", self.nodes, 10_000_000),
            ("min_depth", self.min_depth, 128),
            ("hash_mb", self.hash_mb, 1_024),
        ):
            if type(value) is not int or not 1 <= value <= maximum:
                raise ValueError(f"{name} must be an integer from 1 to {maximum}")


def position_from_request(request: Any) -> chess.Board:
    """Validate the caller's replay and complete legal set before engine input."""
    if not isinstance(request, dict):
        raise ValueError("request must be an object")
    if set(request) != {"start_fen", "moves", "fen", "legal_moves"}:
        raise ValueError("request requires start_fen, moves, fen, and legal_moves")
    if not all(isinstance(request[key], str) for key in ("start_fen", "fen")):
        raise ValueError("FEN values must be strings")
    for key in ("moves", "legal_moves"):
        if not isinstance(request[key], list) or not all(
            isinstance(move, str) for move in request[key]
        ):
            raise ValueError(f"{key} must be an array of UCI move strings")

    board = chess.Board(request["start_fen"])
    declared = chess.Board(request["fen"])
    if not board.is_valid() or not declared.is_valid():
        raise ValueError("invalid chess position")
    for uci in request["moves"]:
        # push_uci validates legality; null moves are not legal game moves.
        move = chess.Move.from_uci(uci)
        if move not in board.legal_moves:
            raise ValueError("move prefix contains an illegal move")
        board.push(move)
    # An irrelevant uncapturable EP field may differ between FEN serializers.
    # Chess's cozy-chess timeline saturates counters at 100 and u16::MAX.
    # Compare that declared representation, but retain unsaturated replay
    # counters and the full move stack on the board passed to Stockfish.
    replayed_fields = board.fen(en_passant="legal").split()
    declared_fields = declared.fen(en_passant="legal").split()
    replayed_fields[4] = str(min(board.halfmove_clock, 100))
    replayed_fields[5] = str(min(board.fullmove_number, 65_535))
    if replayed_fields != declared_fields:
        raise ValueError("current FEN does not match the replayed game prefix")
    supplied = request["legal_moves"]
    legal = {move.uci() for move in board.legal_moves}
    if len(supplied) != len(set(supplied)) or set(supplied) != legal:
        raise ValueError("legal_moves must contain each legal root exactly once")
    return board


def exact_root_score(board: chess.Board, info: dict[str, Any]) -> dict[str, Any] | None:
    """Project an exact UCI score to the unchanged root player's perspective."""
    if info.get("lowerbound") or info.get("upperbound"):
        return None
    pv = info.get("pv")
    if not pv or "score" not in info:
        return None
    move = pv[0]
    if move not in board.legal_moves:
        raise ValueError("engine returned an illegal root move")
    relative = info["score"].pov(board.turn)
    cp = relative.score()
    if cp is not None:
        score = {"kind": "centipawns", "value": cp}
    else:
        mate = relative.mate()
        # A legal nonterminal root cannot already be checkmate. Signed zero
        # cannot represent both MateGiven and Mate(0) in the public score type.
        if mate is None or mate == 0:
            return None
        score = {"kind": "mate", "value": mate}
    return {"uci": move.uci(), "score": score, "bound": "exact"}


class ExactIterations:
    """Never combine old-depth MultiPV entries with a newer partial iteration."""

    def __init__(self, board: chess.Board) -> None:
        self.board = board
        self.expected = {move.uci() for move in board.legal_moves}
        self.depth = 0
        self.nodes = 0
        self.pending: dict[int, dict[str, Any]] = {}
        self.complete: tuple[int, list[dict[str, Any]]] | None = None

    def observe(self, info: dict[str, Any]) -> None:
        nodes = info.get("nodes")
        if type(nodes) is int and nodes >= 0:
            self.nodes = max(self.nodes, nodes)
        depth = info.get("depth")
        if type(depth) is not int or depth <= 0 or depth < self.depth:
            return
        rank = info.get("multipv", 1)
        if type(rank) is not int or not 1 <= rank <= len(self.expected):
            return
        if depth != self.depth:
            self.depth = depth
            self.pending.clear()
        projected = exact_root_score(self.board, info)
        if projected is None:
            self.pending.pop(rank, None)
            return
        self.pending[rank] = projected
        if len(self.pending) == len(self.expected) and {
            line["uci"] for line in self.pending.values()
        } == self.expected:
            self.complete = (
                depth,
                [self.pending[rank] for rank in sorted(self.pending)],
            )


class Evaluator:
    def __init__(
        self,
        engine: chess.engine.SimpleEngine,
        configuration: SearchConfiguration,
    ) -> None:
        self.engine = engine
        self.configuration = configuration
        name = engine.id.get("name", "")
        if not name.lower().startswith("stockfish"):
            raise ValueError("the tactical evaluator requires Stockfish")
        self.engine_name = name
        options: dict[str, Any] = {
            "Threads": 1,
            "Hash": configuration.hash_mb,
            "UCI_LimitStrength": False,
            "Skill Level": 20,
        }
        if "SyzygyPath" in engine.options:
            options["SyzygyPath"] = ""
        engine.configure(options)

    def evaluate(
        self, request: Any, nodes: int, min_depth: int
    ) -> dict[str, Any]:
        configuration = SearchConfiguration(nodes, min_depth, self.configuration.hash_mb)
        board = position_from_request(request)
        roots = sorted(board.legal_moves, key=lambda move: move.uci())
        if not roots:
            return {"status": "incomplete", "reason": "position has no legal moves"}
        multipv_option = self.engine.options.get("MultiPV")
        if multipv_option is None or (
            multipv_option.max is not None and multipv_option.max < len(roots)
        ):
            return {"status": "incomplete", "reason": "engine cannot cover all legal roots"}
        iterations = ExactIterations(board)
        # A distinct game object makes python-chess send ucinewgame/isready;
        # the actual full board move stack is still sent with position/moves.
        with self.engine.analysis(
            board,
            chess.engine.Limit(nodes=configuration.nodes),
            multipv=len(roots),
            root_moves=roots,
            game=object(),
        ) as analysis:
            for info in analysis:
                iterations.observe(info)
        if iterations.complete is None or iterations.complete[0] < configuration.min_depth:
            reached = iterations.complete[0] if iterations.complete is not None else 0
            return {
                "status": "incomplete",
                "reason": "minimum complete exact depth was not reached",
                "completed_depth": reached,
                "nodes": iterations.nodes,
            }
        depth, scores = iterations.complete
        return {
            "status": "complete",
            "engine": self.engine_name,
            "depth": depth,
            "nodes": iterations.nodes,
            "roots": scores,
        }


def emit(response: dict[str, Any], output: TextIO) -> None:
    output.write(json.dumps(response, separators=(",", ":")) + "\n")
    output.flush()


def serve(evaluator: Evaluator, source: BinaryIO, output: TextIO) -> None:
    emit(
        {"protocol": 1, "status": "ready", "engine": evaluator.engine_name,
         "python_chess_version": chess.__version__},
        output,
    )
    while raw := source.readline(MAX_REQUEST_BYTES + 1):
        request_id = None
        if len(raw) > MAX_REQUEST_BYTES:
            emit({"request_id": None, "status": "failed", "reason": "request exceeds size limit"}, output)
            return
        try:
            request = json.loads(raw)
            if not isinstance(request, dict):
                raise ValueError("request must be an object")
            request_id = request.pop("request_id")
            if type(request_id) is not int or not 0 <= request_id < 2**64:
                request_id = None
                raise ValueError("request_id must be an unsigned 64-bit integer")
            nodes = request.pop("nodes")
            min_depth = request.pop("min_depth")
            response = evaluator.evaluate(request, nodes, min_depth)
        except (ValueError, TypeError, KeyError, UnicodeError) as error:
            response = {"status": "failed", "reason": str(error)[:500]}
        except (chess.engine.EngineError, TimeoutError, OSError) as error:
            # A failed engine protocol cannot be reused safely. The parent can
            # start a fresh worker, but must not grade this request as a miss.
            emit({"request_id": request_id, "status": "unavailable", "reason": str(error)[:500]}, output)
            return
        response["request_id"] = request_id
        emit(response, output)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stockfish", type=Path)
    parser.add_argument("--hash-mb", type=int, default=64)
    args = parser.parse_args()
    try:
        configuration = SearchConfiguration(hash_mb=args.hash_mb)
        with chess.engine.SimpleEngine.popen_uci(
            str(args.stockfish), timeout=5.0, setpgrp=False
        ) as engine:
            evaluator = Evaluator(engine, configuration)
            serve(evaluator, sys.stdin.buffer, sys.stdout)
    except (ValueError, OSError, chess.engine.EngineError, TimeoutError) as error:
        emit({"status": "unavailable", "reason": str(error)[:500]}, sys.stdout)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
