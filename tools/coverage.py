"""Batch forecast. Input is an immutable Openings snapshot; no Tempo database access."""
import hashlib
import heapq
import itertools
import json
import math
import pathlib
import signal
import sqlite3
import sys

import chess

DIGEST = "65aae8465eed5e65df66a24ea7370715579f9e5435098d06fe18bdb1e267e997"


def key(board):
    return " ".join(board.fen().split()[:4])


def maia(root):
    import torch
    from maia2 import inference, model
    from maia2.utils import mirror_move

    weights = root / "weights" / "rapid_model.pt"
    if hashlib.sha256(weights.read_bytes()).hexdigest() != DIGEST:
        raise ValueError("Maia Rapid weights failed verification")
    torch.set_num_threads(2)
    network = model.from_pretrained("rapid", device="cpu", save_root=str(weights.parent))
    network.eval()
    moves, ratings, decoded = inference.prepare()
    cache = sqlite3.connect(root / ("coverage-1500-" + DIGEST + ".sqlite3"))
    cache.execute("create table if not exists prediction(position text primary key, moves text not null)")

    def predict(board):
        position = key(board)
        row = cache.execute("select moves from prediction where position=?", (position,)).fetchone()
        if row:
            return json.loads(row[0])
        tensor, own, opponent, mask = inference.preprocessing(board.fen(), 1500, 1500, ratings, moves)
        with torch.inference_mode():
            logits, _, _ = network(tensor.unsqueeze(0), torch.tensor([own]), torch.tensor([opponent]))
            legal = mask.nonzero().flatten()
            probabilities = torch.softmax(logits[0, legal].double(), dim=0).tolist()
        distribution = {}
        for index, probability in zip(legal.tolist(), probabilities):
            move = decoded[index]
            distribution[move if board.turn else mirror_move(move)] = probability
        cache.execute("insert or replace into prediction values (?,?)", (position, json.dumps(distribution)))
        cache.commit()
        return distribution
    return predict


def forecast(data, predict, limit=3000):
    start = chess.Board(data["route"]["fen"])
    for move in data["route"]["moves"]:
        start.push_uci(move)
    learner = data["color"] == "white"
    horizon = data["through"]
    if start.fullmove_number > horizon or (start.fullmove_number == horizon and learner and not start.turn):
        raise ValueError("Choose a horizon that includes another move of yours")
    cards = {key(chess.Board(card["fen"])): card for card in data["cards"]}
    sequence = itertools.count()
    pending = [(-1., next(sequence), start, (), ())]
    gaps, reach, completed = {}, {}, {}
    ended = conflict = 0.
    for _ in range(limit):
        if not pending:
            break
        negative, _, board, line, path = heapq.heappop(pending)
        mass = -negative
        if board.is_game_over():
            ended += mass
            continue
        if board.turn == learner:
            card = cards.get(key(board))
            if card is None:
                gap = gaps.setdefault(key(board), dict(fen=board.fen(), moves=line, probability=0.))
                gap["probability"] += mass
                continue
            if card["quarantined"] or len(card["answers"]) != 1:
                conflict += mass
                continue
            identity = card["id"]
            if identity not in path:
                reach[identity] = reach.get(identity, 0.) + mass
                path = (*path, identity)
            if board.fullmove_number == horizon:
                completed[path] = completed.get(path, 0.) + mass
                continue
            choices = {card["answers"][0]: 1.}
        else:
            choices = predict(board)
            if (set(choices) != {m.uci() for m in board.legal_moves}
                    or not all(math.isfinite(v) and v >= 0 for v in choices.values())
                    or not math.isclose(sum(choices.values()), 1.)):
                raise ValueError("Incomplete legal move distribution")
        for move, probability in choices.items():
            if probability > 0:
                child = board.copy(stack=True)
                child.push_uci(move)
                heapq.heappush(pending, (-mass * probability, next(sequence), child, (*line, move), path))
    return dict(gaps=sorted(gaps.values(), key=lambda gap: -gap["probability"]),
                reach=reach, paths=list(completed.items()), ended=ended, conflict=conflict,
                unresolved=conflict + sum(-entry[0] for entry in pending))


if __name__ == "__main__":
    signal.alarm(300)
    root = pathlib.Path(sys.argv[1])
    print(json.dumps(forecast(json.load(sys.stdin), maia(root)), allow_nan=False))
