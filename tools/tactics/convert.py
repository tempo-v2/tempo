#!/usr/bin/env python3
"""Convert private legacy Tempo exports and pinned public packs for Tempo V2."""

import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass, field
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys

LEGACY_REVISION = "366cc346976570cb7ab7de518957d2cd9eee46e1"
PACK_NAMESPACE = "lichess-puzzle"
CARD_NAMESPACE = "tempo1-card"
MEMBERSHIP_PREFIX = "tempo1:catalog-metadata:"
SOURCE_DIFFICULTIES = {"easy", "medium", "hard", "focused", "advanced"}
THEME_LIST = Path(__file__).resolve().parents[2] / "src/application/tactic_themes.json"


@dataclass
class Candidate:
    namespace: str
    source_id: str
    locations: list
    fen: str | None = None
    moves: list = field(default_factory=list)
    setup: str | None = None
    label: str = "Tactic"
    provenance: list = field(default_factory=list)
    status: str = "eligible"
    reason: str | None = None
    card_id: str | None = None
    checked: dict | None = None
    membership_issues: list = field(default_factory=list)
    legacy_game_captures: list = field(default_factory=list)

    def manifest(self):
        return {
            "namespace": self.namespace,
            "source_id": self.source_id,
            "disposition": self.status,
            "reason": self.reason,
            "raw_locations": self.locations,
            "linked_card_id": self.card_id,
            "membership_issues": self.membership_issues,
            "incoming_target": self.checked.get("target") if self.checked else None,
        }

    def normalized(self):
        return {
            "namespace": self.namespace,
            "source_id": self.source_id,
            "solver_fen": self.checked["solver_fen"],
            "solution": self.checked["solution"],
            "label": self.label,
            "provenance": self.provenance,
            **({"legacy_game_captures": self.legacy_game_captures}
               if self.legacy_game_captures else {}),
            **({"opponent_lead_ins": [{"before_fen": self.fen, "uci": self.setup}]}
               if self.setup is not None else {}),
        }


def digest(raw):
    return hashlib.sha256(raw).hexdigest()


def json_bytes(data):
    return (json.dumps(data, ensure_ascii=False, indent=2) + "\n").encode()


def git_blob(repo, path):
    return subprocess.run(
        ["git", "-C", str(repo), "show", f"{LEGACY_REVISION}:{path}"],
        check=True, stdout=subprocess.PIPE,
    ).stdout


def normalized_membership(pack, row, position, known_themes):
    """Use explicit pack and row facts; the legacy spelling is checked at ingress."""
    theme, difficulty, ordinal = (pack.get(field) for field in ("theme", "difficulty", "ordinal"))
    if not isinstance(theme, str) or theme not in known_themes:
        return None, "unknown pack theme"
    if difficulty not in SOURCE_DIFFICULTIES or type(ordinal) is not int or ordinal <= 0:
        return None, "invalid pack difficulty or ordinal"
    deck = pack.get("id")
    if deck != f"{theme}-{difficulty}-{ordinal:02d}":
        return None, "pack ID conflicts with its declared membership"
    expected = {"Motif": theme, "Difficulty": difficulty, "DeckId": deck, "DeckPosition": position}
    if any(key not in row for key in expected):
        return None, "source row is missing membership fields"
    if type(row["DeckPosition"]) is not int or any(row[key] != val for key, val in expected.items()):
        return None, "source row conflicts with its catalog position or pack"
    metadata = {key: value for key, value in row.items() if key not in {"FEN", "Moves"}}
    return MEMBERSHIP_PREFIX + json.dumps(metadata, sort_keys=True), None


def read_catalog(repo, selected_packs=None, observed=None, catalog_dir=None):
    def read(path):
        if catalog_dir is not None:
            return (catalog_dir / path.removeprefix("public/data/")).read_bytes()
        return git_blob(repo, path)

    path = "public/data/tactics-catalog.json"
    index = read(path)
    catalog = json.loads(index)
    files = {path: index}
    candidates = []
    evidence = []
    asset_hashes = {}
    definitions = {}
    memberships_by_source = {}
    known_themes = {entry["id"] for entry in json.loads(THEME_LIST.read_text())}
    if catalog["version"] != 1:
        raise ValueError("unsupported pinned tactics-catalog version")
    known_packs = {pack["id"] for pack in catalog["packs"]}
    if selected_packs and set(selected_packs) - known_packs:
        raise ValueError("unrecognized pack ID")
    for pack in catalog["packs"]:
        if selected_packs and pack["id"] not in selected_packs:
            continue
        asset = PurePosixPath(pack["asset"])
        if asset.parts[:2] != ("data", "tactics-packs") or ".." in asset.parts:
            raise ValueError("unexpected catalog asset path")
        path = "public/" + str(asset)
        raw = read(path)
        rows = json.loads(raw)
        if not isinstance(rows, list) or len(rows) != pack["count"]:
            raise ValueError(f"catalog count mismatch for {pack['id']}")
        if observed is None:
            files[path] = raw
        for index_in_pack, row in enumerate(rows):
            source_id = row.get("PuzzleId")
            if observed is not None and source_id not in observed:
                continue
            display_id = source_id if isinstance(source_id, str) else ""
            provenance = [f"tempo1:catalog:puzzle:{display_id}", f"tempo1:pack:{pack['id']}"]
            if row.get("GameUrl"):
                provenance.append(row["GameUrl"])
            definitions[display_id] = (row.get("FEN"), row.get("Moves"))
            membership, issue = normalized_membership(pack, row, index_in_pack + 1, known_themes)
            if membership is not None:
                provenance.append(membership)
            if observed is None:
                location = f"raw-packaged/{path}#{index_in_pack}"
            else:
                location = f"raw-catalog-evidence.json#/rows/{len(evidence)}"
                if path not in asset_hashes:
                    asset_hashes[path] = digest(raw)
                evidence.append({"asset": path, "index": index_in_pack, "record": row})
            candidate = Candidate(PACK_NAMESPACE, display_id, [location], label=f"{pack['theme']} · {display_id}",
                                  provenance=provenance)
            parse_content(candidate, row.get("FEN"), row.get("Moves"), setup_first=True)
            if issue:
                candidate.membership_issues.append({"location": location, "reason": issue})
            previous = memberships_by_source.get(display_id)
            same_definition = previous and (
                previous.fen, previous.moves, previous.setup
            ) == (candidate.fen, candidate.moves, candidate.setup)
            if display_id and same_definition and previous.status == candidate.status == "eligible":
                previous.locations.extend(candidate.locations)
                previous.provenance = sorted(set(previous.provenance + candidate.provenance))
                previous.membership_issues.extend(candidate.membership_issues)
            else:
                if previous:
                    conflict = {"location": location, "reason": "conflicting definitions for one source puzzle"}
                    previous.membership_issues.append(conflict)
                    candidate.membership_issues.append(conflict)
                memberships_by_source[display_id] = candidate
                candidates.append(candidate)
    if observed is not None:
        files = {
            "raw-catalog-evidence.json": json_bytes({
                "revision": LEGACY_REVISION,
                "index_sha256": digest(index),
                "asset_sha256": asset_hashes,
                "rows": evidence,
            }),
        }
    else:
        files = {f"raw-packaged/{name}": raw for name, raw in files.items()}
    lineage = []
    if catalog_dir is not None:
        legacy_path = "public/data/tactics-decks.json"
        if (catalog_dir / "tactics-decks.json").is_file():
            raw = read(legacy_path)
            raw_path = f"raw-packaged/{legacy_path}"
            files[raw_path] = raw
            for offset, row in enumerate(json.loads(raw)):
                source_id = row["PuzzleId"]
                location = f"{raw_path}#{offset}"
                if definitions.get(source_id) == (row["FEN"], row["Moves"]):
                    lineage.append({"relation": "legacy_decks", "record_id": str(offset),
                                    "namespace": PACK_NAMESPACE, "source_id": source_id,
                                    "raw_location": location})
                else:
                    candidate = Candidate("lichess-legacy-deck", source_id, [location],
                                          label=f"{row['Motif']} · {source_id}",
                                          provenance=[f"tempo1:legacy-deck:{row['DeckId']}", row["GameUrl"]])
                    parse_content(candidate, row["FEN"], row["Moves"], setup_first=True)
                    candidates.append(candidate)
    return candidates, files, lineage


def parse_content(candidate, fen, moves, setup_first=False):
    if not isinstance(fen, str):
        candidate.status, candidate.reason = "rejected", "missing FEN"
        return
    if isinstance(moves, str):
        moves = moves.split()
    if not isinstance(moves, list) or any(not isinstance(move, str) for move in moves):
        candidate.status, candidate.reason = "rejected", "moves must be UCI strings"
        return
    if len(moves) < (2 if setup_first else 1):
        candidate.status, candidate.reason = "rejected", "missing learner move"
        return
    candidate.fen = fen
    if setup_first:
        candidate.setup, *candidate.moves = moves
    else:
        candidate.moves = moves


def read_move_json(value):
    try:
        return json.loads(value)
    except (TypeError, ValueError):
        return None


def read_personal(snapshot):
    raw = snapshot.read_bytes()
    bundle = json.loads(raw)
    source = bundle["source"]
    if (bundle["schema_version"] != 1 or source["format"] != "tempo-postgresql-tactics"
            or source["transaction_read_only"] != "on"
            or source["transaction_isolation"] != "repeatable read"):
        raise ValueError("expected a read-only Tempo PostgreSQL tactics snapshot")
    rows = bundle["rows"]
    if source["migration_version"] < 65:
        raise ValueError("expected Tempo schema version 65 or newer")
    if bundle["counts"] != {table: len(content) for table, content in rows.items()}:
        raise ValueError("raw snapshot row counts do not reconcile")
    return raw, source, rows


def by_id(rows, key):
    result = {row[key]: row for row in rows}
    if len(result) != len(rows):
        raise ValueError(f"duplicate {key} in snapshot relation")
    return result


def captured_game_request(capture):
    """Read the game claim, board and line from one authoritative capture."""
    if capture.get("source_kind") != "game":
        return None
    request = capture.get("request_json")
    if isinstance(request, str):
        request = read_move_json(request)
    if (not isinstance(request, dict)
            or request.get("capture_id") != capture["id"]
            or request.get("source_kind") != "game"
            or not isinstance(request.get("starting_fen"), str)
            or not isinstance(request.get("moves"), list)
            or not all(isinstance(move, str) for move in request["moves"])):
        return None
    position = request["starting_fen"].split()
    # Tempo 1 persisted clockless position keys on capture requests.
    if len(position) == 4:
        position.extend(["0", "1"])
    if len(position) != 6:
        return None
    return {
        "capture_id": capture["id"],
        "solver_fen": " ".join(position),
        "solution": request["moves"],
    }


def card_candidates(rows):
    cards = by_id(rows["cards"], "id")
    deletions = by_id(rows["deleted_cards"], "card_id")
    candidate_offsets = {row["id"]: index for index, row in enumerate(rows["candidate_card_ids"])}
    card_offsets = {row["id"]: index for index, row in enumerate(rows["cards"])}
    deletion_offsets = {row["card_id"]: index for index, row in enumerate(rows["deleted_cards"])}
    retained_boards = defaultdict(set)
    for origin in rows["queue_attempt_origins"]:
        retained_boards[origin["card_id"]].add((origin["start_fen"], origin["moves_json"]))
    links = defaultdict(list)
    game_captures = defaultdict(list)
    progress = {}
    captured_ids = {capture["card_id"] for capture in rows["tactic_captures"]}
    for capture in rows["tactic_captures"]:
        attribution = links[capture["card_id"]]
        attribution.append(f"tempo1:capture:{capture['id']}")
        for key in ("source_kind", "source_ref"):
            if capture.get(key):
                attribution.append(f"tempo1:{key}:{capture[key]}")
        if capture.get("source_url"):
            attribution.append(capture["source_url"])
        proof = captured_game_request(capture)
        if proof is not None:
            game_captures[capture["card_id"]].append(proof)
    for row in rows["tactic_progress"]:
        progress[row["puzzle_id"]] = row["card_id"]
        if row["card_id"]:
            links[row["card_id"]].append(f"tempo1:progress:{row['puzzle_id']}")
    result = []
    for card_id in sorted(candidate_offsets):
        card = cards.get(card_id)
        candidate = Candidate(CARD_NAMESPACE, card_id, [f"raw-personal.json#/rows/candidate_card_ids/{candidate_offsets[card_id]}"],
                              label="Captured tactic" if card_id in captured_ids else "Studied tactic",
                              provenance=[f"tempo1:card:{card_id}", *sorted(links[card_id])],
                              legacy_game_captures=sorted(game_captures[card_id], key=lambda row: row["capture_id"]))
        if card_id in deletions:
            candidate.locations.append(f"raw-personal.json#/rows/deleted_cards/{deletion_offsets[card_id]}")
        if card is None:
            # Retained origin can identify a content alias, but it cannot
            # replace the missing current card or restore deleted content.
            boards = retained_boards[card_id]
            if len(boards) == 1:
                fen, moves = next(iter(boards))
                parse_content(candidate, fen, read_move_json(moves))
            if card_id in deletions:
                candidate.status, candidate.reason = "excluded", "deleted card tombstone"
            else:
                candidate.status, candidate.reason = "held", "linked card row is missing"
        else:
            candidate.locations.append(f"raw-personal.json#/rows/cards/{card_offsets[card_id]}")
            parse_content(candidate, card["start_fen"], read_move_json(card["moves_json"]))
            if card.get("source_ref"):
                candidate.provenance.append(f"tempo1:card-source:{card['source_ref']}")
            if card_id in deletions:
                candidate.status, candidate.reason = "held", "current card conflicts with deletion tombstone"
            elif card["archived"] or card["superseded_by"]:
                candidate.status, candidate.reason = "excluded", "archived or superseded card"
            elif card["pending_validation"]:
                candidate.status, candidate.reason = "held", "legacy card pending validation"
        result.append(candidate)
    return result, progress


def personal_candidates(snapshot, repo, catalog_dir=None):
    raw, source, rows = read_personal(snapshot)
    cards, progress = card_candidates(rows)
    observed = set(progress)
    for relation in ("tactic_discovery_attempts", "tactic_introductions", "observed_puzzle_ids"):
        observed.update(row["puzzle_id"] for row in rows.get(relation, []))
    stored_puzzles = by_id(rows["puzzles"], "id")
    catalog, files, references = read_catalog(
        repo, observed=None if catalog_dir is not None else observed | set(stored_puzzles),
        catalog_dir=catalog_dir,
    )
    missing = observed - stored_puzzles.keys() - {item.source_id for item in catalog}
    for source_id in sorted(missing):
        catalog.append(Candidate(
            PACK_NAMESPACE, source_id, ["raw-personal.json#tactic_history"],
            status="held", reason="observed puzzle missing from the pinned catalog and legacy puzzles",
        ))
    # A stored puzzle already begins at the learner's turn. The pack is
    # checked as evidence where it exists, while the stored row supplies play.
    for offset, row in enumerate(rows["puzzles"]):
        source_id = row["id"]
        candidate = Candidate(
            "tempo1-puzzle", source_id, [f"raw-personal.json#/rows/puzzles/{offset}"],
            provenance=[f"tempo1:puzzle:{source_id}"],
        )
        parse_content(candidate, row["start_fen"], read_move_json(row["moves_json"]))
        candidate.card_id = progress.get(source_id)
        cards.append(candidate)
    if catalog_dir is not None:
        for relation, id_field, namespace, target_field in (
            ("tactic_captures", "id", CARD_NAMESPACE, "card_id"),
            ("tactic_progress", "puzzle_id", PACK_NAMESPACE, "puzzle_id"),
            ("observed_puzzle_ids", "puzzle_id", PACK_NAMESPACE, "puzzle_id"),
        ):
            for offset, row in enumerate(rows.get(relation, [])):
                source_id = row[target_field]
                target_namespace = namespace
                if namespace == PACK_NAMESPACE and source_id in stored_puzzles:
                    target_namespace = "tempo1-puzzle"
                references.append({
                    "relation": relation, "record_id": row[id_field],
                    "namespace": target_namespace, "source_id": source_id,
                    "raw_location": f"raw-personal.json#/rows/{relation}/{offset}",
                })
    else:
        for candidate in catalog:
            candidate.card_id = progress.get(candidate.source_id)
            if candidate.source_id in stored_puzzles and not candidate.card_id:
                candidate.status, candidate.reason = "reference", "personal legacy puzzle supplies its current definition"
    files["raw-personal.json"] = raw
    operation_states = defaultdict(Counter)
    for receipt in rows.get("operation_receipts", []):
        operation_states[receipt["command_name"]][receipt["state"]] += 1
    inventory = {
        "snapshot": source,
        "raw_relation_counts": {table: len(content) for table, content in rows.items()},
        "operation_receipt_states": operation_states,
        "distinct_capture_cards": len({row["card_id"] for row in rows["tactic_captures"]}),
        "distinct_observed_puzzles": len(observed),
        "learning_state_mapped": 0,
    }
    if catalog_dir is not None:
        installed_packs = {pack["id"] for pack in json.loads(
            files["raw-packaged/public/data/tactics-catalog.json"]
        )["packs"]}
        active = {row["pack_id"] for row in rows["tactic_pack_activation"] if row["active"]}
        inventory.update(installed_packs=len(installed_packs), active_packs=len(active),
                         missing_active_packs=sorted(active - installed_packs))
    return cards + catalog, files, inventory, references


def check_candidates(candidates):
    parsed = [item for item in candidates if item.fen is not None and item.source_id.strip()]
    manifest_path = Path(__file__).resolve().parent / "chess-helper" / "Cargo.toml"
    checked = subprocess.run(
        ["cargo", "run", "--locked", "--quiet", "--manifest-path", str(manifest_path)],
        input=b"".join(json_bytes_compact({
            "namespace": item.namespace, "source_id": item.source_id,
            "fen": item.fen, "moves": item.moves, "setup": item.setup,
        }) for item in parsed),
        stdout=subprocess.PIPE, check=True,
    ).stdout.splitlines()
    if len(checked) != len(parsed):
        raise ValueError("chess validator did not return one result per input")
    for item, result in zip(parsed, map(json.loads, checked)):
        if "error" in result:
            if item.status == "eligible":
                item.status, item.reason = "rejected", result["error"]
        else:
            item.checked = result


def json_bytes_compact(data):
    return (json.dumps(data, ensure_ascii=False) + "\n").encode()


def reconcile(candidates):
    by_source = defaultdict(list)
    by_definition = defaultdict(list)
    card_by_id = {item.source_id: item for item in candidates if item.namespace == CARD_NAMESPACE}
    for item in candidates:
        by_source[(item.namespace, item.source_id)].append(item)
        if not item.source_id.strip():
            item.status, item.reason = "rejected", "blank source ID"
        if item.checked:
            definition = (item.checked["practice_start"], tuple(item.checked["solution"]))
            by_definition[definition].append(item)

    for group in by_source.values():
        if len(group) > 1:
            for item in group:
                item.status, item.reason = "rejected", "duplicate source identity"

    for group in by_definition.values():
        if any(item.status in {"excluded", "held"} for item in group):
            for item in group:
                if item.status == "eligible":
                    item.status, item.reason = "held", "same definition has conflicting legacy lifecycle evidence"
    for item in candidates:
        if not item.card_id or item.reason == "duplicate source identity":
            continue
        card = card_by_id[item.card_id]
        if item.status in {"eligible", "rejected", "held"}:
            if card.status == "eligible":
                card.provenance.extend(p for p in item.provenance if p not in card.provenance)
                item.status, item.reason = "linked_card", "stored current card supplies the playable definition"
            else:
                item.status, item.reason = "held", "linked card is excluded, invalid or held"

    eligible = [item for item in candidates if item.status == "eligible"]
    if any(item.checked is None for item in eligible):
        raise ValueError("internal error: unvalidated eligible candidate")
    return eligible


def write_bundle(output, files, manifest, eligible, emit):
    # Exact source bytes and their checksums are written once to a fresh
    # private directory. The normalized batch appears last, if authorized.
    output.mkdir(mode=0o700)
    for name, raw in files.items():
        path = output / name
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        path.write_bytes(raw)
    manifest["raw_files"] = {name: digest(raw) for name, raw in sorted(files.items())}
    (output / "manifest.json").write_bytes(json_bytes(manifest))
    if emit:
        normalized = {"schema_version": 1, "puzzles": [item.normalized() for item in eligible]}
        staged = output / ".normalized.json.tmp"
        staged.write_bytes(json_bytes(normalized))
        staged.rename(output / "normalized.json")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    for name in ("packaged", "personal"):
        mode = sub.add_parser(name)
        mode.add_argument("--legacy-repo", type=Path)
        mode.add_argument("--output", type=Path, required=True)
        mode.add_argument("--valid-only", action="store_true",
                          help="explicitly emit eligible rows when rejections or holds remain")
    sub.choices["packaged"].add_argument("--pack", action="append", help="select a pinned pack ID (default all)")
    sub.choices["personal"].add_argument("--snapshot", type=Path, required=True)
    sub.choices["personal"].add_argument(
        "--catalog-dir", type=Path,
        help="include every puzzle from the installed Tempo 1 web data directory",
    )
    args = parser.parse_args()

    if args.legacy_repo is None and getattr(args, "catalog_dir", None) is None:
        parser.error("provide --legacy-repo or --catalog-dir")
    os.umask(0o077)
    if args.mode == "personal":
        candidates, files, inventory, references = personal_candidates(
            args.snapshot, args.legacy_repo, args.catalog_dir,
        )
    else:
        candidates, files, references = read_catalog(args.legacy_repo, args.pack)
        inventory = {}
    check_candidates(candidates)
    eligible = reconcile(candidates)
    counts = dict(Counter(item.status for item in candidates))
    partial = bool(counts.get("held") or counts.get("rejected") or inventory.get("missing_active_packs"))
    emit = not partial or args.valid_only
    if emit and partial:
        batch = "valid_only"
    elif emit:
        batch = "all_eligible"
    else:
        batch = "withheld"
    manifest = {
        "schema_version": 1, "scope": args.mode, "legacy_catalog_revision": None if getattr(args, "catalog_dir", None) else LEGACY_REVISION,
        "batch": batch, "inventory": inventory, "candidate_count": len(candidates),
        "unique_eligible_definitions": len({(item.checked["practice_start"], tuple(item.checked["solution"])) for item in eligible}),
        "disposition_counts": counts, "candidates": [item.manifest() for item in candidates],
        "references": references,
    }
    write_bundle(args.output, files, manifest, eligible, emit)
    print(json.dumps({"batch": batch, "candidate_count": len(candidates), "dispositions": counts}))
    return 0 if emit else 2


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"tempo tactics conversion failed: {error}", file=sys.stderr)
        sys.exit(1)
