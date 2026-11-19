"""One pinned, CPU-only, read-only Maia subprocess for Tempo's local adapter."""

import hashlib
import json
import sys
from pathlib import Path
from contextlib import redirect_stdout

import chess
import torch
from maia2 import inference, model
from maia2.utils import mirror_move

DIGEST = "65aae8465eed5e65df66a24ea7370715579f9e5435098d06fe18bdb1e267e997"
weights_dir = Path(sys.argv[1])
with (weights_dir / "rapid_model.pt").open("rb") as weights:
    if hashlib.file_digest(weights, "sha256").hexdigest() != DIGEST:
        raise ValueError("The Rapid checkpoint failed checksum verification")

torch.set_num_threads(1)
with redirect_stdout(sys.stderr):
    network = model.from_pretrained("rapid", device="cpu", save_root=str(weights_dir))
network.eval()
move_ids, ratings, move_by_id = inference.prepare()
print(json.dumps({"ready": True}), flush=True)

for line in sys.stdin:
    try:
        fen = json.loads(line)
        board, active, opponent, mask = inference.preprocessing(fen, 1500, 1500, ratings, move_ids)
        ids = mask.nonzero().flatten()
        with torch.inference_mode():
            logits, _, _ = network(board.unsqueeze(0), torch.tensor([active]), torch.tensor([opponent]))
            probabilities = torch.softmax(logits[0, ids].to(torch.float64), dim=0).tolist()
        black = fen.split()[1] == "b"
        moves = [move_by_id[index] for index in ids.tolist()]
        result = dict(zip((mirror_move(move) if black else move for move in moves), probabilities))
        if set(result) != {move.uci() for move in chess.Board(fen).legal_moves}:
            raise ValueError("Maia did not return a full legal distribution")
        print(json.dumps(result, allow_nan=False), flush=True)
    except (ValueError, KeyError, RuntimeError) as error:
        print(json.dumps({"error": str(error)}), flush=True)
