#!/usr/bin/env python3
"""Join conversion dispositions to the targets actually retained by V2."""
import argparse
from collections import Counter
import json
from pathlib import Path


def reconcile(conversion, imported):
    if conversion["schema_version"] != 1 or imported["schema_version"] != 1:
        raise ValueError("Expected version 1 manifests")
    targets = {}
    for source in imported["sources"]:
        identity = (source["namespace"], source["source_id"])
        if identity in targets:
            raise ValueError("The import report contains a duplicate source identity")
        targets[identity] = source

    candidates = conversion["candidates"]
    if conversion["candidate_count"] != len(candidates):
        raise ValueError("The conversion candidate count is inconsistent")
    conversion_ids = {(row["namespace"], row["source_id"]) for row in candidates}
    if len(conversion_ids) != len(candidates):
        raise ValueError("The conversion contains a duplicate source identity")
    expected = {
        (row["namespace"], row["source_id"])
        for row in candidates
        if row["disposition"] == "eligible"
    }
    if expected != targets.keys():
        raise ValueError("The import report and eligible conversion batch do not match")

    rows = []
    for row in candidates:
        role = row["disposition"]
        source_identity = (row["namespace"], row["source_id"])
        if role == "linked_card":
            source_identity = ("tempo1-card", row["linked_card_id"])
        target = targets.get(source_identity) if role in {"eligible", "linked_card"} else None
        if role in {"eligible", "linked_card"} and target is None:
            raise ValueError("A linked card is missing from the import report")
        rows.append({
            **row,
            "final_status": target["status"] if target else role,
            "retained_target": target["target"] if target else None,
            "retained_version": target["version"] if target else None,
            "alias": (role == "linked_card" or target["alias"]) if target else False,
        })

    return {
        "schema_version": 1,
        "batch": conversion["batch"],
        "dataset": imported["dataset"],
        "inventory": conversion["inventory"],
        "historical_learning_mapped": imported["historical_learning_mapped"],
        "candidate_count": len(rows),
        "unique_retained_targets": len({row["target"] for row in targets.values()}),
        "source_disposition_counts": dict(Counter(row["final_status"] for row in rows)),
        "candidates": rows,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("conversion", type=Path)
    parser.add_argument("imported", type=Path)
    args = parser.parse_args()
    output = reconcile(json.loads(args.conversion.read_text()), json.loads(args.imported.read_text()))
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
