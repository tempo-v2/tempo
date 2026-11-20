#!/usr/bin/env python3
"""Add explicit setup moves from a retained Tempo 1 migration bundle.

Write a version-1 transfer to stdout. Use it with backfill-study-lead-ins;
the backfill matches the source and complete solution against saved cards.
"""
import hashlib
import json
import sys
from pathlib import Path


def enrich(bundle):
    manifest = json.loads((bundle / "manifest.json").read_bytes())
    transfer = json.loads((bundle / "normalized.json").read_bytes())
    if manifest["schema_version"] != 1 or transfer["schema_version"] != 1:
        raise ValueError("unsupported bundle version")
    candidates = {
        (row["namespace"], row["source_id"]): row
        for row in manifest["candidates"]
        if row["disposition"] == "eligible"
    }
    sources = {}

    def read_source(name):
        if name not in sources:
            path = (bundle / name).resolve()
            if not path.is_relative_to(bundle):
                raise ValueError("source is outside the retained bundle")
            data = path.read_bytes()
            if hashlib.sha256(data).hexdigest() != manifest["raw_files"][name]:
                raise ValueError(f"source checksum mismatch: {name}")
            sources[name] = json.loads(data)
        return sources[name]

    for row in transfer["puzzles"]:
        candidate = candidates.get((row["namespace"], row["source_id"]))
        if not candidate:
            continue
        setups = set()
        for location in candidate["raw_locations"]:
            name, _, pointer = location.partition("#")
            if name.startswith("raw-packaged/public/data/tactics-packs/") and pointer.isdecimal():
                source = read_source(name)[int(pointer)]
            elif name == "raw-catalog-evidence.json" and pointer.startswith("/rows/"):
                source = read_source(name)["rows"][int(pointer.removeprefix("/rows/"))]["record"]
            else:
                continue
            moves = source.get("Moves")
            if isinstance(moves, str):
                moves = moves.split()
            if (source.get("PuzzleId") == row["source_id"] and isinstance(source.get("FEN"), str)
                    and isinstance(moves, list) and moves and isinstance(moves[0], str)
                    and moves[1:] == row["solution"]):
                setups.add((source["FEN"], moves[0]))
        if len(setups) == 1:
            before, uci = setups.pop()
            row["opponent_lead_in"] = {"before_fen": before, "uci": uci}
        elif setups:
            row.pop("opponent_lead_in", None)
    return transfer


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("Usage: recover_setup_moves.py RETAINED_BUNDLE")
    json.dump(enrich(Path(sys.argv[1]).resolve()), sys.stdout, separators=(",", ":"))
    print()
