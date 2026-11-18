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

    added = {change["target"] for change in imported["changes"] if change["kind"] == "added"}
    by_target = {}
    for source in targets.values():
        target = source["target"]
        if target not in by_target or not source["alias"]:
            by_target[target] = (source["namespace"], source["source_id"])
    rows = []
    for row in candidates:
        role = row["disposition"]
        source_identity = (row["namespace"], row["source_id"])
        if role == "linked_card":
            source_identity = ("tempo1-card", row["linked_card_id"])
        target = targets.get(source_identity) if role in {"eligible", "linked_card"} else None
        if role in {"eligible", "linked_card"} and target is None:
            raise ValueError("A linked card is missing from the import report")
        if target is None:
            outcome = role
        elif target["status"] != "active":
            outcome = "unresolved"
        elif source_identity != by_target[target["target"]] or role == "linked_card":
            outcome = "duplicate"
        else:
            outcome = "imported" if target["target"] in added else "already_present"
        rows.append({
            **row,
            "final_status": target["status"] if target else role,
            "outcome": outcome,
            "reason": (f"existing Tempo 2 target has status {target['status']}"
                       if outcome == "unresolved" else row["reason"]),
            "retained_target": target["target"] if target else None,
            "retained_version": target["version"] if target else None,
            "alias": (role == "linked_card" or target["alias"]) if target else False,
        })

    by_source = {(row["namespace"], row["source_id"]): row for row in rows}
    references = []
    for reference in conversion.get("references", []):
        row = by_source.get((reference["namespace"], reference["source_id"]))
        if row is None:
            raise ValueError(f"Unaccounted source reference: {reference}")
        references.append({
            **reference,
            "outcome": "duplicate" if row["final_status"] == "active" else row["outcome"],
            "reason": row["reason"],
            "retained_target": row["retained_target"],
        })

    return {
        "schema_version": 1,
        "batch": conversion["batch"],
        "dataset": imported["dataset"],
        "inventory": conversion["inventory"],
        "historical_learning_mapped": imported["historical_learning_mapped"],
        "candidate_count": len(rows),
        "unique_retained_targets": len(by_target),
        "unique_valid_source_puzzles": conversion["unique_eligible_definitions"],
        "final_tactic_count": imported.get("final_tactic_count"),
        "already_present_tactic_count": imported.get("already_present_tactic_count"),
        "outcome_counts": dict(Counter(row["outcome"] for row in rows)),
        "reference_counts": {relation: dict(counts) for relation, counts in
                             count_references(references).items()},
        "references": references,
        "source_disposition_counts": dict(Counter(row["final_status"] for row in rows)),
        "candidates": rows,
    }


def count_references(references):
    counts = {}
    for reference in references:
        counts.setdefault(reference["relation"], Counter())[reference["outcome"]] += 1
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("conversion", type=Path)
    parser.add_argument("imported", type=Path)
    args = parser.parse_args()
    output = reconcile(json.loads(args.conversion.read_text()), json.loads(args.imported.read_text()))
    print(json.dumps(output, indent=2))


if __name__ == "__main__":
    main()
