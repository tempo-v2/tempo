"""Return a bounded best-defense line for an already selected puzzle root."""
import json
import sys

import chess
import chess.engine

board = chess.Board(sys.argv[2])
root = board.parse_uci(sys.argv[3])
with chess.engine.SimpleEngine.popen_uci(sys.argv[1], timeout=5) as engine:
    engine.configure({"Threads": 1, "Hash": 32})
    info = engine.analyse(board, chess.engine.Limit(depth=18, nodes=500_000), root_moves=[root])
line = info.get("pv", [])
if info.get("depth", 0) < 12 or info.get("upperbound") or info.get("lowerbound"):
    raise RuntimeError("incomplete payoff search")
print(json.dumps([move.uci() for move in line[:16]]))
