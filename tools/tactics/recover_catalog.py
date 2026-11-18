"""Build a private SQLite catalog of Tempo 1 metadata linked to imported V2 puzzles."""

import argparse
import json
import sqlite3
import tempfile
from contextlib import closing
from pathlib import Path


PREFIX = "tempo1:catalog-metadata:"
SCHEMA = """
CREATE TABLE catalog_dataset (
    dataset_id TEXT PRIMARY KEY,
    source_catalog TEXT NOT NULL
);
CREATE TABLE puzzle (
    target_id TEXT PRIMARY KEY,
    content_version INTEGER NOT NULL
);
CREATE TABLE pack (
    pack_id TEXT PRIMARY KEY,
    theme TEXT NOT NULL,
    theme_name TEXT NOT NULL,
    theme_group TEXT NOT NULL,
    difficulty TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    was_active INTEGER NOT NULL CHECK (was_active IN (0, 1))
);
CREATE INDEX pack_theme_difficulty ON pack(theme, difficulty, ordinal);
CREATE TABLE pack_puzzle (
    source_puzzle_id TEXT PRIMARY KEY,
    target_id TEXT NOT NULL REFERENCES puzzle,
    pack_id TEXT NOT NULL REFERENCES pack,
    position INTEGER NOT NULL,
    rating INTEGER NOT NULL,
    metadata TEXT NOT NULL,
    UNIQUE (pack_id, position)
);
CREATE INDEX pack_puzzle_target ON pack_puzzle(target_id);
CREATE TABLE puzzle_tag (
    source_puzzle_id TEXT NOT NULL REFERENCES pack_puzzle,
    tag TEXT NOT NULL,
    PRIMARY KEY (source_puzzle_id, tag)
) WITHOUT ROWID;
CREATE INDEX puzzle_tag_lookup ON puzzle_tag(tag, source_puzzle_id);
CREATE VIEW puzzle_by_pack AS
SELECT pack.*, member.position, member.rating, member.source_puzzle_id,
       member.target_id, puzzle.content_version
FROM pack
JOIN pack_puzzle AS member USING (pack_id)
JOIN puzzle USING (target_id);
CREATE VIEW unclassified_puzzle AS
SELECT * FROM puzzle WHERE NOT EXISTS (
    SELECT 1 FROM pack_puzzle WHERE pack_puzzle.target_id = puzzle.target_id
);
"""


def read_json(path):
    return json.loads(path.read_bytes())


def read_imported_metadata(path):
    puzzles = []
    metadata = {}
    with closing(sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)) as db:
        db.execute("BEGIN")
        dataset, = db.execute("SELECT dataset_id FROM store_state").fetchone()
        for target, raw in db.execute(
            "SELECT target_id, item FROM current_content WHERE dataset_id = ? AND repertoire IS NULL",
            (dataset,),
        ):
            item = json.loads(raw)
            if item["status"] != "active":
                continue
            puzzles.append((target, item["version"]))
            for entry in item["provenance"]:
                if entry.startswith(PREFIX):
                    fields = json.loads(entry[len(PREFIX):])
                    source_id = fields["PuzzleId"]
                    if source_id in metadata:
                        raise ValueError(f"multiple imported matches for {source_id}")
                    metadata[source_id] = (target, fields)
    return dataset, puzzles, metadata


def populate(db, catalog_dir, active_packs, imported):
    dataset, puzzles, metadata = imported
    catalog = read_json(catalog_dir / "tactics-catalog.json")
    if catalog["version"] != 1:
        raise ValueError("unsupported Tempo 1 catalog version")
    themes = {theme["id"]: theme for theme in catalog["themes"]}
    if active_packs - {pack["id"] for pack in catalog["packs"]}:
        raise ValueError("the snapshot contains an active pack missing from the catalog")

    db.execute("INSERT INTO catalog_dataset VALUES (?, ?)", (dataset, json.dumps(catalog["source"])))
    db.executemany("INSERT INTO puzzle VALUES (?, ?)", puzzles)
    seen = set()
    for pack in catalog["packs"]:
        pack_id, theme, difficulty = pack["id"], pack["theme"], pack["difficulty"]
        if not pack_id or "/" in pack_id or pack_id in {".", ".."}:
            raise ValueError("invalid pack ID")
        db.execute("INSERT INTO pack VALUES (?, ?, ?, ?, ?, ?, ?)", (
            pack_id, theme, themes[theme]["name"], pack["group"], difficulty, pack["ordinal"],
            pack_id in active_packs,
        ))
        rows = read_json(catalog_dir / "tactics-packs" / f"{pack_id}.json")
        if len(rows) != pack["count"]:
            raise ValueError(f"wrong puzzle count for {pack_id}")
        for position, row in enumerate(rows, start=1):
            source_id = row["PuzzleId"]
            if (row["DeckId"], row["Motif"], row["Difficulty"], row["DeckPosition"]) != (
                pack_id, theme, difficulty, position,
            ):
                raise ValueError(f"inconsistent pack membership for {source_id}")
            fields = {key: value for key, value in row.items() if key not in {"FEN", "Moves"}}
            if source_id in seen or source_id not in metadata or metadata[source_id][1] != fields:
                raise ValueError(f"imported metadata does not match the source for {source_id}")
            seen.add(source_id)
            db.execute("INSERT INTO pack_puzzle VALUES (?, ?, ?, ?, ?, ?)", (
                source_id, metadata[source_id][0], pack_id, position, row["Rating"],
                json.dumps(fields, sort_keys=True),
            ))
            db.executemany("INSERT INTO puzzle_tag VALUES (?, ?)", (
                (source_id, tag) for tag in row["Themes"]
            ))
    if seen != metadata.keys():
        raise ValueError("some imported catalog records have no source pack")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("study_database", type=Path)
    parser.add_argument("catalog_directory", type=Path)
    parser.add_argument("tempo1_snapshot", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output already exists")
    imported = read_imported_metadata(args.study_database)
    snapshot = read_json(args.tempo1_snapshot)
    active_packs = {
        row["pack_id"] for row in snapshot["rows"]["tactic_pack_activation"] if row["active"]
    }
    with tempfile.TemporaryDirectory(dir=args.output.parent) as temp:
        staged = Path(temp) / "catalog.sqlite"
        staged.touch(mode=0o600)
        with closing(sqlite3.connect(staged)) as db:
            db.execute("PRAGMA foreign_keys = ON")
            db.executescript(SCHEMA)
            with db:
                populate(db, args.catalog_directory, active_packs, imported)
            if db.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                raise ValueError("catalog SQLite integrity check failed")
            if db.execute("PRAGMA foreign_key_check").fetchone() is not None:
                raise ValueError("catalog SQLite foreign key check failed")
        args.output.hardlink_to(staged)


if __name__ == "__main__":
    main()
