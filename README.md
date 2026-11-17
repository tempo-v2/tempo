# tempo

Tempo 2's composition root and application commands. Domain decisions produce
values; this crate connects them and commits the complete operation through Store.

## Public boundary

`Tempo` exposes `next_exercise`, `resume`, `submit`, `abandon`, `again`,
`import_pgn`, `ingest_games`, `export` and `restore`. `Components` injects the
queue/learning/grading/content policies, clock, and attempt-ID source. The
application owns the adapter implementing Study's question-source interface.

Submission receipts are checked before current content validation. A new accepted
response saves progress, assessment, learning effect and receipt in one optimistic
transaction. Game feedback also carries the compared content version. Imports
preserve progress except for actual semantic changes; restore is a separate
operation into an empty dataset.

Game providers implement `GameProvider::fetch` outside the Study-owning object.
Already-fetched games enter `ingest_games`, then the same `again` command used by
other identified external feedback. Network failure cannot become a prerequisite
for ordinary Study.

## Run the wired fixture

From the local workspace root:

```sh
cargo build -p tempo
node tempo/demo/run.mjs
cargo test -p tempo --test contracts
node --test tempo/tests/ui_boundary.mjs
```

`cargo run -p tempo -- stdio` accepts one JSON command per line. Start with
`{"command":"next"}`. The UI contract is versioned in `src/wire.rs` and the
UI package's public declarations. The stdio adapter lives here; the UI imports no
process or backend code. The application tests use the same implementation as the
demo, including a lost reply after commit.

Real now: typed composition, attempt/learning rules, content-change coordination,
memory transaction invariants, replay, stale checks and UI serialization.
Fixtures: PGN/canonicalization/comparison inputs, interval scheduler, ordering,
counter IDs, fixed time and process-local storage. The JSON snapshot codec proves
the export/restore port; enriched PGN is a replacement codec. There is no production
HTTP server, SQL store or chess engine in this increment.

The counter ID adapter must be replaced by identities unique across restarts
before using durable storage. Current snapshots read the complete dataset; a
future performance-driven bounded-read API may be justified by measured data size.
Cross-repository details live in `.planning/INTERFACES.md`.
