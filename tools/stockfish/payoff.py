"""Return a bounded best-defense line for an already selected puzzle root."""
import json
import sys

import chess
import chess.engine

board = chess.Board(sys.argv[2])
root = board.parse_uci(sys.argv[3])
line = []
depth = 0
with chess.engine.SimpleEngine.popen_uci(sys.argv[1], timeout=5) as engine:
    engine.configure({"Threads": 1, "Hash": 32})
    limit = chess.engine.Limit(depth=18, nodes=500_000)
    with engine.analysis(board, limit, root_moves=[root]) as analysis:
        for info in analysis:
            seen_depth = info.get("depth", 0)
            pv = info.get("pv", [])
            if (seen_depth < max(12, depth) or not pv or pv[0] != root
                    or info.get("upperbound") or info.get("lowerbound")):
                continue
            depth = seen_depth
            line = pv
if not line:
    raise RuntimeError("incomplete payoff search")
print(json.dumps([move.uci() for move in line]))
