"""Read-only, bounded candidate search for an authoring position."""

import json
import sys

import chess
import chess.engine


def project(board, line):
    move = line["pv"][0]
    relative = line["score"].relative
    return {
        "uci": move.uci(),
        "san": board.san(move),
        "centipawns": relative.score(),
        "mate": relative.mate(),
    }


def compare(engine, board, roots):
    """Retain the deepest complete, exact MultiPV iteration for these roots."""
    depth = 0
    pending = {}
    complete = None
    limit = chess.engine.Limit(time=1.4, nodes=160_000)
    with engine.analysis(board, limit, multipv=len(roots), root_moves=roots) as results:
        for line in results:
            rank = line.get("multipv", 1)
            next_depth = line.get("depth", 0)
            if not next_depth or next_depth < depth or "score" not in line or not line.get("pv"):
                continue
            if next_depth != depth:
                pending.clear()
                depth = next_depth
            if line.get("lowerbound") or line.get("upperbound"):
                pending.pop(rank, None)
                continue
            pending[rank] = project(board, line)
            if len(pending) == len(roots) and len({mv["uci"] for mv in pending.values()}) == len(roots):
                complete = [pending[index] for index in sorted(pending)]
    return complete


board = chess.Board(sys.argv[2])
if not board.is_valid():
    raise ValueError("Invalid authoring position")

with chess.engine.SimpleEngine.popen_uci(sys.argv[1], timeout=3) as engine:
    engine.configure({"Threads": 1, "Hash": 16})
    lines = engine.analyse(board, chess.engine.Limit(time=0.4, nodes=40_000), multipv=min(int(sys.argv[3]), board.legal_moves.count()))
    candidates = [project(board, line) for line in lines]
    comparison = None
    if sys.argv[4:]:
        roots = list(dict.fromkeys(
            [line["pv"][0] for line in lines] + [board.parse_uci(uci) for uci in sys.argv[4:]]
        ))
        try:
            comparison = compare(engine, board, roots)
        except (chess.engine.EngineError, TimeoutError):
            pass
    print(json.dumps({
        "engine": engine.id["name"],
        "candidates": candidates,
        "comparison": comparison,
    }))
