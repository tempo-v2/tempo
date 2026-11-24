# Tempo

Tempo is a local chess trainer. Games imports and replays games. Openings
stores authored lines. Tactics owns a standalone puzzle library. Study
reviews published opening and tactic cards using Good, Again, and FSRS.

## Operation contract

| Operation | Owner, inputs and effect |
| --- | --- |
| Import games | Games accepts public provider account or PGN plus learner identity. The same canonical game and learner side reuse an archive record. The worker records resumable progress and commits one game at a time. |
| Scan | Chess compares legal played moves to saved repertoire answers and finds clear missed captures. Games records immutable observations. Tactics accepts a complete puzzle independently through its catalog intake. Neither operation can grade, schedule, or replace an issued card. |
| Save opening | Openings validates legal routes and publishes one response card at each learner decision. The same named line, practice color, position and moves reuse the line. Display and source notes carry no grade or scheduling authority. |
| Publish tactic | Tactics validates every move of the supplied PGN and publishes the solver position and accepted moves. Stable content identity makes imports and worker replay idempotent. |
| Review | Study issues one durable attempt against one immutable published card. The first legal answer or reveal accepts one Good/Again, writes FSRS and the repeat placement atomically, and saves a replayable receipt. Next is a separate operation. Source details are read separately after acceptance. |
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
