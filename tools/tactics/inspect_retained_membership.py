#!/usr/bin/env python3
"""Report retained and independently verified source membership without writing SQLite."""

import argparse
from collections import Counter
from contextlib import closing
import json
from pathlib import Path
import sqlite3

from convert import MEMBERSHIP_PREFIX, THEME_LIST, read_catalog

ORIGIN_PREFIX = "tempo1:catalog:puzzle:"


def membership(entry, themes):
    try:
        fields = json.loads(entry[len(MEMBERSHIP_PREFIX):])
        theme, difficulty = fields["Motif"], fields["Difficulty"]
        deck, position = fields["DeckId"], fields["DeckPosition"]
        pack, raw_ordinal = deck.rsplit("-", 1)
        if theme not in themes or difficulty not in ("easy", "medium", "hard"):
            return None
        ordinal = int(raw_ordinal)
        if pack != f"{theme}-{difficulty}" or ordinal <= 0 or type(position) is not int or position <= 0:
            return None
        return theme, difficulty, ordinal, position
    except (AttributeError, KeyError, TypeError, ValueError):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path)
    parser.add_argument("catalog_dir", type=Path)
    args = parser.parse_args()

    themes = {entry["id"] for entry in json.loads(THEME_LIST.read_text())}
    catalog, _, _ = read_catalog(None, catalog_dir=args.catalog_dir)
    source_counts = Counter(entry.source_id for entry in catalog)
    verified = {
        item.source_id: {
            fact for entry in item.provenance if entry.startswith(MEMBERSHIP_PREFIX)
            and (fact := membership(entry, themes)) is not None
        }
        for item in catalog if source_counts[item.source_id] == 1 and item.status == "eligible"
    }
    counts = Counter()
    uri = args.database.resolve().as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as db:
        db.execute("BEGIN")
        dataset, = db.execute("SELECT dataset_id FROM store_state").fetchone()
        rows = db.execute(
            "SELECT target_id,item FROM current_content WHERE dataset_id=? AND repertoire IS NULL ORDER BY target_id",
            (dataset,),
        )
        for target, raw in rows:
            item = json.loads(raw)
            if item["status"] != "active":
                continue
            provenance = item["provenance"]
            retained = [
                membership(entry, themes) for entry in provenance if entry.startswith(MEMBERSHIP_PREFIX)
            ]
            known = set(retained) - {None}
            source_ids = {entry[len(ORIGIN_PREFIX):] for entry in provenance if entry.startswith(ORIGIN_PREFIX)}
            confirmed = set().union(*(verified.get(source, set()) for source in source_ids))
            pack_positions = Counter(fact[:3] for fact in known)
            if any(count > 1 for count in pack_positions.values()) or (confirmed and known - confirmed):
                state = "conflicting"
            elif None in retained:
                state = "malformed"
            elif known:
                state = "known"
            else:
                state = "missing"
            counts[state] += 1
            print(json.dumps({
                "target": target, "version": item["version"], "state": state,
                "retained": sorted(known), "source_ids": sorted(source_ids),
                "source_membership_not_retained": sorted(confirmed - known),
            }))
    print(json.dumps({"summary": {key: counts[key] for key in ("known", "missing", "malformed", "conflicting")}}))


if __name__ == "__main__":
    main()
