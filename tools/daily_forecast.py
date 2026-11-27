"""Estimate first encounters along authored game histories for Daily Queue."""

import contextlib
import json
import math
import pathlib
import signal
import sys
import types
from dataclasses import dataclass, field

import chess


HORIZON = 200
PROFILE = "maia3-5m-1500-authored-history-v1"


@dataclass
class Prefix:
    children: dict = field(default_factory=dict)
    card: str | None = None
    beyond_horizon: set = field(default_factory=set)


@dataclass
class Scenario:
    root: Prefix = field(default_factory=Prefix)
    replies: dict = field(default_factory=dict)


def position_key(board):
    return " ".join(board.fen().split()[:4])


def add_route(scenario, route):
    decisions = {}
    for decision in route["decisions"]:
        ply, card = decision["ply"], decision["card"]
        if not 0 <= ply < len(route["moves"]) or ply in decisions:
            raise ValueError("Invalid daily opening decision")
        decisions[ply] = card
    node = scenario.root
    board = chess.Board()
    for ply, move in enumerate(route["moves"][:HORIZON]):
        if ply in decisions:
            card = decisions.pop(ply)
            if board.turn != (route["color"] == "white"):
                raise ValueError("Daily opening decision at an opponent turn")
            reply = (move, card)
            if scenario.replies.setdefault(position_key(board), reply) != reply:
                raise ValueError("Inconsistent daily opening card identity")
            node.card = card
        board.push_uci(move)
        node = node.children.setdefault(move, Prefix())
    node.beyond_horizon.update(decisions.values())


def mark_uncertain(node, seen, uncertain):
    if node.card is not None and node.card not in seen:
        uncertain.add(node.card)
    uncertain.update(node.beyond_horizon - seen)
    for child in node.children.values():
        mark_uncertain(child, seen, uncertain)


def traverse(node, board, learner, mass, predict, replies, seen, reach, uncertain):
    if board.is_game_over():
        return
    reply = replies.get(position_key(board)) if board.turn == learner else None
    if board.ply() == HORIZON:
        mark_uncertain(node, seen, uncertain)
        if reply is not None and reply[1] not in seen:
            uncertain.add(reply[1])
        return
    if board.turn == learner:
        if reply is None and len(node.children) > 1:
            mark_uncertain(node, seen, uncertain)
            return
        if reply is not None:
            move, card = reply
            if card in reach and card not in seen:
                reach[card] += mass
                seen = seen | {card}
            for other in node.children.keys() - {move}:
                mark_uncertain(node.children[other], seen, uncertain)
            choices = {move: 1.}
        else:
            choices = dict.fromkeys(node.children, 1.)
    else:
        if node.children and mass > 0.:
            choices = predict(board)
            if (set(choices) != {m.uci() for m in board.legal_moves}
                    or not all(math.isfinite(v) and v >= 0 for v in choices.values())
                    or not math.isclose(sum(choices.values()), 1.)):
                raise ValueError("Incomplete legal move distribution")
        else:
            choices = dict.fromkeys(node.children, 0.)
    for move, child in sorted(node.children.items()):
        if move in choices:
            board.push_uci(move)
            traverse(child, board, learner, mass * choices[move], predict, replies, seen, reach, uncertain)
            board.pop()


def forecast(data, predict):
    cards = set(data["cards"])
    if len(cards) != len(data["cards"]):
        raise ValueError("Duplicate daily opening cards")
    reach = dict.fromkeys(cards, 0.)
    supported, uncertain = set(), set()
    by_color = {"white": {}, "black": {}}
    for route in data["routes"]:
        scenario = by_color[route["color"]].setdefault(route["repertoire"], Scenario())
        present = {entry["card"] for entry in route["decisions"] if entry["card"] in cards}
        if route["fen"] == chess.STARTING_FEN:
            supported.update(present)
            add_route(scenario, route)
        else:
            uncertain.update(present)
    for color, repertoires in by_color.items():
        for _, scenario in sorted(repertoires.items()):
            traverse(scenario.root, chess.Board(), color == "white", .5 / len(repertoires),
                     predict, scenario.replies, set(), reach, uncertain)
    return {
        card: {"kind": "estimated", "probability": min(1., reach[card])}
        if card in supported and card not in uncertain else {"kind": "unscored"}
        for card in sorted(cards)
    }


if __name__ == "__main__":
    signal.alarm(300)
    tools = types.ModuleType("tempo_coverage")
    exec(compile(sys.argv[2], "<embedded coverage model>", "exec"), vars(tools))
    root = pathlib.Path(sys.argv[1])
    request = json.load(sys.stdin)
    if request["profile"] != PROFILE:
        raise ValueError("Incompatible daily forecast profile")
    with tools.model_slot(root), contextlib.redirect_stdout(sys.stderr):
        scores = forecast(request, tools.maia(root))
    print(json.dumps({"profile": PROFILE, "basis": request["basis"],
                      "succeeded": True, "scores": scores},
                     allow_nan=False))
