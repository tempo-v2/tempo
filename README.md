# Tempo

Tempo is a local chess trainer. Games imports and replays games. Openings
stores authored lines. Tactics owns a standalone puzzle library. Study
reviews published opening and tactic cards using Good, Again, and FSRS.

## Operation contract

| Operation | Owner, inputs and effect |
| --- | --- |
| Import games | Games accepts public provider account or PGN plus learner identity. The same canonical game and learner side reuse an archive record. The worker saves each canonical game and queues its scan in the same transaction. Import completes after archive acceptance. |
| Scan | The scanner compares legal played moves against each matching repertoire independently and saves answer-versioned observations. Missed-capture findings wait for the [standalone tactic worker](stockfish-analysis/README.md#generated-tactic-publication-worker). Tactics publishes a finding only when a proposed capture is the unique best move at depth 10, with every alternative outside the 30-centipawn window. |
| Save opening | Openings validates legal routes and publishes one response card at each learner decision. The destination selects a repertoire. A matching reply reuses the card, and a different reply at the same position returns a conflict. Existing conflicts are quarantined. |
| Publish tactic | Tactics validates every move of the supplied PGN and publishes the solver position and accepted moves. Stable content identity makes imports and worker replay idempotent. |
| Review | Study issues one durable attempt with a fixed answer set for the published card. The first legal answer or reveal accepts one Good/Again, writes FSRS and the repeat placement atomically, and saves a replayable receipt. Next is a separate operation. Source details are read separately after acceptance. |
| Import old openings | One-time offline command takes a consistent read-only snapshot. It verifies and copies current repertoire content and starts schedules fresh. The new runtime cannot open a legacy database. |

Position IDs hash a canonical orthodox position with side to move,
castling rights and legal en passant. Games and the scanner share that
representation with Openings. The two game observations are addressed by
game, ply and, for mismatch, repertoire and answer version. Evaluation,
move probability and explorer statistics are future read-only interfaces
over the same position input.

The previous runtime is available in repository history. The current
runtime and browser application are GPL-3.0-or-later. The original license
notice is retained in LICENSE. The vendored Core 0.2.2 artifact contains its
third-party notices.

## Run locally

With Rust and Node installed:

```sh
npm --prefix ui ci
npm --prefix ui run build
cargo build --locked
mkdir -p .local
target/debug/tempo import-openings /path/to/old.sqlite3 .local/tempo.sqlite3
target/debug/tempo serve .local/tempo.sqlite3
```

Omit the import command to start with an empty library. The migration
prints reconciled routes and response counts per repertoire. Any
unreconciled repertoire rolls back the whole import. Repeating the
migration reuses existing lines and does not reset their new schedules.

Open **http://127.0.0.1:41823**. Settings stores public player names. Games
can import an entire provider archive, resume a failed import, or scan all
stored games against the repertoire as it exists today. The service owns
one background worker with bounded network calls and short SQLite writes.
`serve DATABASE PORT --no-worker` runs the same browser and Study paths
with no provider or scanner activity.

Each submitted Again places the card into the persisted queue at the
drawn offset, or at the end of a shorter queue. Subsequent Again results
insert their own cards into that same queue. A replay leaves the recorded
insertion untouched.

Study shows the submitted move after Good and the solution after Again.
After a brief pause, a newly accepted Good loads the next card. If that
load fails, Retry sends only Next. Returning to a saved answer shows its
result without advancing. Board sounds are browser-local and can be
changed in Settings. Game, opening, and puzzle PGN uploads accept up
to 64 MiB. Games parses archives one game at a time.

Opening reviews replay the saved lead-in from its first position, then
stop at the scheduled response. Playback is ungraded, and **Skip to
your move** starts the issued decision immediately.

A saved opening order can prioritize new opening cards after the existing
due work check. This is a static list of card identities. Study never
runs a model or imports game evidence to decide that order.

A local ranker can read a consistent catalogue snapshot with
`tempo opening-rank-input DATABASE`. Return a JSON file with the same
`dataset` and `revision` and an ordered array of unique opening `card_ids`.
Run `tempo opening-rank-apply DATABASE SNAPSHOT.json` to replace the saved order.
The command rejects a changed catalogue, foreign dataset, or unknown card
before writing. Cards omitted from the snapshot use the ordinary unranked
queue rules. Snapshots contain repertoire answers and are intended for use
by local tooling.

## Verification

Run `tools/check.sh` from a checkout. CI uses the same command with Rust
1.98.1, Node 26.12.0, uv 0.13.4 and `zstd`. The script runs locked Rust
workspace tests, builds the UI from `npm ci` and the vendored Core package, and
runs the Coverage and pack-builder Python tests on locked Python 3.12.12.
The native Stockfish test is opt-in as described in
[Stockfish analysis](stockfish-analysis/README.md).

For mounted Chromium qualification, install the locked harness and its browser.
On a new Linux host, add `-- --with-deps` to the browser install command if its
system libraries are missing. The harness also uses the `sqlite3` CLI.

```sh
npm --prefix tools ci
npm --prefix tools run browser:install
npm --prefix ui ci
npm --prefix ui run build
cargo build --locked --release --bin tempo
node tools/qualify_gap_resolution.cjs --candidate-only target/release/tempo
node tools/qualify_answer_ir.cjs --candidate-only target/release/tempo /path/to/lichess-packs.sqlite3
```

Use the qualified pack artifact containing puzzle `03N75` and its pack,
identified in the Lean assembly manifest. Each Tempo binary must have the
matching built `ui/dist` in the checkout path used to compile it. Set
`TEMPO_QUALIFY_CHROME` only to exercise an explicitly chosen Chrome binary.
Set `TEMPO_QUALIFY_STOCKFISH` to a Stockfish 19 binary to include native gap
review in the Gaps suite.

Run installed-predecessor replay separately. Use the actual previous
installation's binary and assets and take a read-only SQLite backup of its
database. The harness copies the schema-18 snapshot before starting either
server, and uses temporary databases and loopback ports.

```sh
tools/qualify-predecessor.sh /path/to/candidate/tempo /path/to/predecessor/tempo \
  /path/to/predecessor-snapshot.sqlite3 /path/to/lichess-packs.sqlite3
```

For historical qualification, `tools/qualify_answer_ir.cjs --legacy-puzzle
CANDIDATE PREDECESSOR SNAPSHOT PACK` targets the old per-tab puzzle journal.
`--legacy-packs CANDIDATE PREDECESSOR SNAPSHOT PACK LEGACY_STUDY_CLIENT`
targets the older pack and receipt contracts. Keep the schema-18
`tools/qualify_opening_batch_upgrade.py` migration check with its schema-17
predecessor and the `tools/qualify_game_ir.py` checks with their schema-8
candidate.

## Offline large content saves

If the browser reports that an opening save exceeds its row budget, stop the
service and run `tempo import-content DATABASE opening INPUT.json` with the same
opening save payload (name, color, route and destination). The `tactic` form
takes the same PGN and name input as the browser. These commands hold the
service's database lock and use the same atomic validation and conflict checks.

Openings → Gaps saves large continuations online in bounded batches. Tempo
validates the full PGN first. Each batch's content and checkpoint commit together.
Retry uses the original previewed command and resumes at the last checkpoint.
If another edit changes the repertoire between batches, Tempo reports the
committed progress and asks for a fresh scan. Older exported gap files can
still be imported with `tempo import-content DATABASE gap tempo-gap.json`
while Tempo is stopped.

A service backup that must resume queued PGN uploads includes the SQLite
database and its adjacent `.uploads` directory. Stop the service before
copying both for a coordinated release or rollback. The `opening-rank-apply`
and legacy opening import commands also require exclusive database ownership.

## Archive conversion and recovery

With the service stopped, run `tempo migrate-games DATABASE` on the existing
Lean database before starting this release. The command acquires the service
lock and converts archived routes to the versioned packed format in one
transaction. The schema-18 migration also assigns a canonical identity to each
authored opening. Equivalent saved lines in the same repertoire become aliases
of one active line. Their original IDs, titles and routes remain stored.
Game IDs, staged uploads, Study receipts and Learning are preserved. Keep the
stopped database and adjacent `.uploads` directory together for recovery, with
the executable and UI that served that schema.

Reload active Study and Tactics tabs during cutover. Tempo imports surviving
predecessor pending commands into its browser journals. An accepted Study result
can be recovered independently of loading the next card.

## Provider turns and repertoire choices

A provider job freezes its window inventory, stages each nonempty download
(up to 32 MiB), and accepts at most one record per turn. The file reference,
byte offset, accepted game or rejected record, and ready order are durable.
Restart and Retry continue the staged window at its saved offset. The
account checkpoint advances after all windows finish. Every Games job turn
uses one 60-second deadline for new work and writer admission. Short recovery
and claim attempts use separate two-second admission budgets. A claimed write
resolves before another turn can use its result. The next bounded claim records
a failed turn together with claiming the next job. Local blocking filesystem
operations retain the worker until they physically return. Unfinished
older manual uploads with a prior rejection replay in bounded turns from
the staged file because their old cursor can point inside a rejected game.
Already accepted records are deduplicated. Completion uses the replay's
physical rejection count.

The Openings editor requests up to 50 destination choices by repertoire
identity and practice color. Each choice keeps the first authored line as
its display example. The picker follows the same indexed lookup through
migration and newly saved content. Choice paging has its own read slot and
the same SQL budget as other optional reads.

### Repertoire coverage

On the Apple silicon Tempo host, run `tools/setup-coverage.sh` with uv 0.13.4.
It synchronizes the locked Python 3.12.12 Maia-3 5M CPU environment and verifies
the weights checksum. Pass a disposable runtime directory as the first argument
to rehearse provisioning and synchronization away from the serving runtime. Scans use 1500/1500 Elo and include the available recent
move history. In Openings → Gaps, choose a repertoire, starting PGN and horizon.
The forecast follows your saved responses and branches on Maia's opponent move
probabilities. Authored coverage counts complete paths through your chosen move.
Personally studied coverage requires a completed review on each current response.
Both are conditional on the starting line. Early game endings are shown separately.
A scan expands up to 3,000 positions, most probable first. Unexplored probability
and conflicting replies contribute to the displayed uncertainty range.

Open a gap in Lichess, extend the line and paste its exported PGN in Tempo.
Preview shows the learner replies below the selected position, including legal
transpositions and variations. Save commits those lines together. Scanning and
previewing leave the repertoire and learning history untouched.

After a scan, **Apply to opening order** ranks reached opening cards by their
estimated chance of occurring. An older Maia-2 report can still be read and
its PGN edits retried. Rescan before applying its order. New cards use the saved
order when Settings →
New openings uses Maia. Within an applied group, the due selector chooses the
highest-ranked eligible review when the next due opening belongs to that
group. The command preserves committed attempts, Again placement and FSRS due
times. Rank preparation runs outside the writer and activates in one commit
after bounded staging turns.

### Repertoire move similarity

Export one repertoire's active replies with
`tempo repertoire-similarity-input DATABASE REPERTOIRE COLOR`.
Pass that JSON as `repertoire` in a request with the complete position `fen`
to `tempo repertoire-similarity` on stdin. Optional `options.count` accepts
1–10 and defaults to 5. The reply gives the closest saved example for each
distinct legal move, using the repertoire's practice side. Distance compares
piece placement, castling rights and legally available en passant. Zero
distance means the same chess position. A ranked result can be empty.
Terminal and opponent-turn results have their own statuses. Errors are JSON
and exit nonzero. These commands leave the repertoire and Study data untouched.
