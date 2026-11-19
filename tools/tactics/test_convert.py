"""Focused conversion scenarios for source conventions and lifecycle holds."""

import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from convert import CARD_NAMESPACE, PACK_NAMESPACE, Candidate, MEMBERSHIP_PREFIX, parse_content, read_catalog, reconcile


class ConvertTests(unittest.TestCase):
    def test_source_convention_is_explicit(self):
        pack = Candidate(PACK_NAMESPACE, "sample", [])
        card = Candidate(CARD_NAMESPACE, "sample", [])
        fen = "source"
        parse_content(pack, fen, "a2a3 a7a6 h2h3", setup_first=True)
        parse_content(card, fen, ["a7a6", "h2h3"])
        self.assertEqual(pack.setup, "a2a3")
        self.assertEqual(pack.moves, card.moves)
        self.assertIsNone(card.setup)

    def test_directory_and_pinned_catalog_read_the_same_verified_memberships(self):
        path = "public/data/tactics-catalog.json"
        packs = [
            {"id": f"{theme}-easy-01", "theme": theme, "difficulty": "easy", "ordinal": 1,
             "asset": f"data/tactics-packs/{theme}.json", "count": 1}
            for theme in ("fork", "pin", "skewer")
        ]
        files = {path: json.dumps({"version": 1, "packs": packs + [packs[0]]}).encode()}
        for pack in packs:
            row = {"PuzzleId": "sample", "FEN": "source", "Moves": ["a2a3", "a7a6"],
                   "Motif": pack["theme"], "Difficulty": "easy", "DeckId": pack["id"],
                   "DeckPosition": 2 if pack["theme"] == "skewer" else 1}
            files["public/" + pack["asset"]] = json.dumps([row]).encode()
        with TemporaryDirectory() as directory:
            root = Path(directory)
            for name, data in files.items():
                absolute = root / name.removeprefix("public/data/")
                absolute.parent.mkdir(parents=True, exist_ok=True)
                absolute.write_bytes(data)
            expected, _, _ = read_catalog(None, catalog_dir=root)
            with patch("convert.git_blob", side_effect=lambda _repo, path: files[path]):
                actual, _, _ = read_catalog(Path("ignored"))
        self.assertEqual(actual, expected)
        self.assertEqual(len(actual), 1)
        memberships = [json.loads(entry.removeprefix(MEMBERSHIP_PREFIX))
                       for entry in actual[0].provenance if entry.startswith(MEMBERSHIP_PREFIX)]
        self.assertEqual({member["Motif"] for member in memberships}, {"fork", "pin"})
        self.assertEqual(len(memberships), 2)
        self.assertIn("conflicts", actual[0].membership_issues[0]["reason"])

    def test_archived_same_definition_cannot_be_restored_as_a_catalog_copy(self):
        definition = {"practice_start": "position", "solution": ["a2a3"]}
        card = Candidate(CARD_NAMESPACE, "card", [], status="excluded", checked=definition)
        pack = Candidate(PACK_NAMESPACE, "pack", [], checked=definition)
        self.assertEqual(reconcile([card, pack]), [])
        self.assertEqual(card.status, "excluded")
        self.assertEqual(pack.status, "held")

    def test_duplicate_pack_identity_still_rejects_even_with_a_current_card(self):
        definition = {"practice_start": "position", "solution": ["a2a3"]}
        card = Candidate(CARD_NAMESPACE, "card", [], checked=definition)
        pack = Candidate(PACK_NAMESPACE, "pack", [], checked=definition, card_id="card")
        duplicate = Candidate(PACK_NAMESPACE, "pack", [], checked=definition, card_id="card")
        self.assertEqual(reconcile([card, pack, duplicate]), [card])
        self.assertEqual((pack.status, duplicate.status), ("rejected", "rejected"))


if __name__ == "__main__":
    unittest.main()
