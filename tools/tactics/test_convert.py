"""Focused conversion scenarios for source conventions and lifecycle holds."""

import unittest

from convert import CARD_NAMESPACE, PACK_NAMESPACE, Candidate, parse_content, reconcile


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
