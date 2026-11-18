# Transfer Tempo 1 tactics into a private V2 batch

## Migrate installed content only

Find the active Tempo 1 PostgreSQL and web containers and the database path
opened by the running Tempo 2 process. From the assembled V2 workspace, set
`PG_CONTAINER`, `PG_DATABASE`, `PG_USER`, `WEB_CONTAINER`, and `V2_DB` to
those verified values. Use a new private directory and target the existing
database. Use SQLite's online backup before importing.

```sh
umask 077
work=$(mktemp -d .local/tempo1-transfer.XXXXXX)
docker exec -i -e PGOPTIONS='-c default_transaction_read_only=on' "$PG_CONTAINER" \
  psql -XqAt -v ON_ERROR_STOP=1 -v content_only=true -U "$PG_USER" -d "$PG_DATABASE" \
  < tempo/tools/tactics/export_personal.sql > "$work/source.json"
docker cp "$WEB_CONTAINER:/usr/share/nginx/html/data" "$work/installed-data"
cargo build --locked -p tempo --bin tempo

python3 tempo/tools/tactics/convert.py personal \
  --snapshot "$work/source.json" --catalog-dir "$work/installed-data" \
  --output "$work/batch"
sqlite3 -readonly "$V2_DB" ".backup $work/before.sqlite"
target/debug/tempo import-tactics "$V2_DB" "$work/batch/normalized.json" \
  > "$work/import.json"
python3 tempo/tools/tactics/reconcile.py "$work/batch/manifest.json" "$work/import.json" \
  > "$work/reconciliation.json"
```

If conversion exits with status 2, inspect the manifest for rejected records
or missing activated assets. Run the same conversion with a fresh output
directory and `--valid-only` to accept the recorded exclusions. Stop if any
other command fails. The import publishes one revision-fenced SQLite
transaction via the app and Store. If a concurrent write changes the revision
before commit, retry the import against the current database. Validate the
result through the running server using `POST /api/study` with
`{"command":"study_status"}`.

The content-only export contains tactical board and line records plus the
links required to recover or attribute them. The converter includes all
installed packs and recognizes exact copies among the older deck assets.
Chess applies the setup move in pack rows and validates the complete
learner-first solution of every candidate. Captured and stored cards begin
at the learner's turn. The import deduplicates by canonical solver position
and full ordered line. Legal distinct lines remain distinct. A reused source
ID with different content fails the batch before publication. An exact match
to an existing tactic retains its item, version, and learning. Its new source
association is available in the reconciliation ledger.

Keep the raw export, batch, backup, and per-record report private. See the
[installed migration results](MIGRATION_2026-11-17.md) for the first run.

## Pinned packs and earlier personal exports

The SQL export reads PostgreSQL schema 65 or newer in a single read-only,
repeatable-read transaction. Use the established read-only connection to the
identified Tempo 1 database. Run with `psql -XqAt -v ON_ERROR_STOP=1` and the
SQL file on stdin. Redirect stdout to a new private file. A successful export
contains one JSON object with the source database/schema, exact row values,
table counts and transaction settings. It includes generic capture, discovery,
activation and linked card-review operation receipts. This export covers server-side
records. Browser-only history and activity absent from the server snapshot
remain outside its scope.

The converter uses Python 3.10+ and Cargo from the assembled V2 workspace.
Give it a local read-only checkout of the pinned Tempo repository and a new
private output directory. It reads pack records from the pinned commit.

```sh
python3 tempo/tools/tactics/convert.py personal \
  --legacy-repo /path/to/legacy-tempo \
  --snapshot /private/path/raw-personal.json \
  --output /private/path/personal-transfer

python3 tempo/tools/tactics/convert.py packaged \
  --legacy-repo /path/to/legacy-tempo \
  --pack fork-focused-01 \
  --output /private/path/disposable-pack
```

Personal conversion starts with every tactic card, capture association,
admission/progress link and retained tactical queue origin. It also includes
observed puzzle IDs from introductions and discovery attempts. Stored cards
supply the current exercise when a catalog row links to them. Pack-only
conversion supports disposable practice databases. Keep that standalone
batch out of a transferred personal dataset, since it contains no personal
lifecycle evidence. Omit `--pack` to inspect every pinned pack.

The private output contains byte-for-byte raw inputs and a SHA-256 inventory
in `manifest.json`. Pack evidence for personal history keeps each referenced
original row, source path, row offset and pinned file digest. The manifest
accounts for eligible candidates, links to current cards, exclusions, holds
and rejections. When a rejection or hold remains, the default exit status
is 2 and no normalized file appears. Inspect those rows, then use
`--valid-only` and a fresh output directory to explicitly emit a partial batch.

`normalized.json` uses schema version 1 and a `puzzles` array with
`namespace`, `source_id`, `solver_fen`, `solution` (all UCI plies),
`label` and `provenance`. Stable IDs use `tempo1-card` for current DB
cards, `tempo1-puzzle` for legacy puzzle table rows and `lichess-puzzle`
for catalog IDs. Source filenames, checksums and board edits do not
change the source identity. Pack conversion applies its first move once
with the Chess helper. Solver-first cards and puzzle rows retain the full line.

The conversion manifest precedes import. Use
`python3 tempo/tools/tactics/reconcile.py CONVERSION_MANIFEST IMPORT_REPORT`
to join it to the app's retained targets, aliases, and quarantines. A catalog
entry with `linked_card_id` follows the corresponding `tempo1-card`
disposition. Keep raw files and reports outside the public UI directory.
The selected history rows preserve scheduler payloads, recorded reviews, and
linked operation receipts. Conversion maps zero historical learning states
into V2.
