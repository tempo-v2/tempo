"""Read-only, bounded candidate search for an authoring position."""

import json
import sys

import chess
import chess.engine

board = chess.Board(sys.argv[2])
if not board.is_valid():
    raise ValueError("Invalid authoring position")

with chess.engine.SimpleEngine.popen_uci(sys.argv[1], timeout=3) as engine:
    engine.configure({"Threads": 1, "Hash": 16})
    lines = engine.analyse(board, chess.engine.Limit(time=0.4, nodes=40_000), multipv=3)
    candidates = []
    for line in lines:
        move = line["pv"][0]
        relative = line["score"].relative
        candidates.append({
            "uci": move.uci(),
            "san": board.san(move),
            "centipawns": relative.score(),
            "mate": relative.mate(),
        })
    print(json.dumps({"engine": engine.id["name"], "candidates": candidates}))
