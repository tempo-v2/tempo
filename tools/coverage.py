"""Batch forecast. Input is an immutable Openings snapshot; no Tempo database access."""
import contextlib
import fcntl
import hashlib
import heapq
import itertools
import json
import math
import os
import pathlib
import signal
import sqlite3
import sys

import chess

MODEL = "maia3-5m-1500-history-v1"
WEIGHTS_SHA256 = "ba14208b2992d85502f5fb501934abf6aaaeb355e9f3fdf90e326911f562524f"
RATING = 1500


@contextlib.contextmanager
def model_slot(root):
    """Keep one physical model active across both service processes."""
    fd = os.open(root / ".model.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise SystemExit(75) from None
        yield
    finally:
        os.close(fd)


def key(board):
    return " ".join(board.fen().split()[:4])


def recent_positions(board, count):
    """Current position and its predecessors, oldest first, without changing board."""
    recent = board.copy(stack=count - 1)
    positions = [recent.copy(stack=False)]
    while recent.move_stack:
        recent.pop()
        positions.append(recent.copy(stack=False))
    return positions[::-1]


def maia(root):
    import torch
    from maia3.dataset import get_historical_tokens, tokenize_board
    from maia3.uci import load_model, parse_args
    from maia3.utils import get_all_possible_moves, mirror_move

    weights = root / "weights" / "maia3-5m.pt"
    if hashlib.sha256(weights.read_bytes()).hexdigest() != WEIGHTS_SHA256:
        raise ValueError("Maia-3 5M weights failed verification")
    torch.set_num_threads(2)
    cfg = parse_args(["--model", "5m", "--device", "cpu", "--checkpoint-path", str(weights)])
    network = load_model(cfg)
    ratings = torch.tensor([RATING])
    indices = {move: index for index, move in enumerate(get_all_possible_moves())}
    cache = sqlite3.connect(root / f"coverage-5m-{RATING}-{WEIGHTS_SHA256}-v1.sqlite3")
    cache.execute("create table if not exists prediction(history text primary key, moves text not null)")

    def predict(board):
        positions = recent_positions(board, cfg.history)
        history = "|".join(key(position) for position in positions)
        row = cache.execute("select moves from prediction where history=?", (history,)).fetchone()
        if row:
            return json.loads(row[0])
        moves = sorted(move.uci() for move in board.legal_moves)
        legal = [indices[move if board.turn else mirror_move(move)] for move in moves]
        with torch.inference_mode():
            tokens = get_historical_tokens([tokenize_board(pos) for pos in positions], cfg, 0., 0., 0., 0.)
            logits, _, _ = network(tokens.unsqueeze(0), ratings, ratings)
            probabilities = torch.softmax(logits[0, legal].double(), dim=0).tolist()
        distribution = dict(zip(moves, probabilities))
        cache.execute("insert or replace into prediction values (?,?)", (history, json.dumps(distribution)))
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
    pending = [(-1., next(sequence), (), ())]
    gaps, reach, completed = {}, {}, {}
    ended = conflict = 0.
    for _ in range(limit):
        if not pending:
            break
        negative, _, line, path = heapq.heappop(pending)
        board = start.copy()
        for move in line:
            board.push_uci(move)
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
                heapq.heappush(pending, (-mass * probability, next(sequence), (*line, move), path))
    return dict(gaps=sorted(gaps.values(), key=lambda gap: -gap["probability"]),
                reach=reach, paths=list(completed.items()), ended=ended, conflict=conflict,
                unresolved=conflict + sum(-entry[0] for entry in pending))


if __name__ == "__main__":
    signal.alarm(300)
    root = pathlib.Path(sys.argv[1])
    request = json.load(sys.stdin)
    with model_slot(root), contextlib.redirect_stdout(sys.stderr):
        report = forecast(request, maia(root))
    report["model"] = MODEL
    print(json.dumps(report, allow_nan=False))
