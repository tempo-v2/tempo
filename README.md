# tempo

The application connects the domain crates and commits complete operations. Start
at [src/lib.rs](src/lib.rs) for the public outline, then open the file named for
the concept you need. A file contains its purpose, usage guidance, implementation,
and a `#[cfg(test)] mod tests` example where the concept has its own public behavior.
The longer cross-component journeys stay in [tests/contracts.rs](tests/contracts.rs).

## Choose the entry point

| When you need to… | Reach for | Source and nearby example |
| --- | --- | --- |
| Assemble policies and adapters | `Components`, then `Tempo::new` | [components.rs](src/components.rs) replaces only the queue; [application/mod.rs](src/application/mod.rs) owns private application state and constructor checks. |
| Begin study, or continue the active attempt | `Tempo::next_exercise` | [next_exercise.rs](src/application/next_exercise.rs) demonstrates that another Next resumes without issuing a competing attempt. |
| Reload persisted progress | `Tempo::resume` | [resume.rs](src/application/resume.rs) demonstrates a read with no new assessment or write. |
| Answer the displayed learner decision | `SubmitCommand` → `Tempo::submit` | [submit_command.rs](src/commands/submit_command.rs) constructs a retryable action; [submit.rs](src/application/submit.rs) shows progress, learning, assessment and receipt saved together. |
| Stop an attempt | `AbandonCommand` → `Tempo::abandon` | [abandon_command.rs](src/commands/abandon_command.rs) supplies revision/identity without an answer; [abandon.rs](src/application/abandon.rs) clears the active pointer. |
| Report an externally observed miss | `AgainCommand` → `Tempo::again` | [again_command.rs](src/commands/again_command.rs) rejects analysis against the wrong definition; [again.rs](src/application/again.rs) updates learning without fabricating a human attempt. |
| Update repertoire or tactics content | `Tempo::import_pgn` | [import_pgn.rs](src/application/import_pgn.rs) preserves progress for an unchanged repertoire. Metadata and semantic-change journeys live in `tests/contracts.rs`. |
| Process games already fetched by a refresh job | `Tempo::ingest_games` | [ingest_games.rs](src/application/ingest_games.rs) demonstrates replay-aware ingestion. The count includes processed deviations that were already committed. |
| Transfer content and learning to an empty dataset | `Tempo::export`, `Tempo::restore` | [export.rs](src/application/export.rs) shows a read-only export; [restore.rs](src/application/restore.rs) restores under a new dataset identity. |
| Dispatch one decoded UI intent | `wire::StudyCommand` → `Tempo::handle` | [study_command.rs](src/commands/study_command.rs) decodes the versioned envelope; [handle.rs](src/application/handle.rs) returns typed replies even on application failure. |

The existing root imports remain available, for example `tempo::SubmitCommand`
and `tempo::Components`. Grouped modules add navigable paths without requiring
callers to adopt them. `tempo::wire::{StudyCommand, StudyView}` remains the public
transport boundary. Your `src/views/` files remain the homes of those view concepts.

## Values and their direction

| Value | Produced by | Consumed by | What to notice |
| --- | --- | --- | --- |
| `SubmitCommand` / `AbandonCommand` | UI controller or another caller | Application operation | Copy the latest **attempt revision**, allocate one submission ID per user action, reuse the whole command for retries. |
| `AgainCommand` | Game comparison or another feedback adapter | Application operation | Include the content version actually compared, provider namespace and stable event identity. |
| [`Game`](src/game.rs) | `GameProvider` | `Tempo::ingest_games` | Stable provider/game IDs make rescans replayable; the PGN is still raw input for the comparator. |
| [`GameRange`](src/game_range.rs) | Refresh caller | `GameProvider::fetch` | Inclusive observation-time endpoints; the provider validates their order. |
| [`PortableSnapshot`](src/portable_snapshot.rs) | Export or a codec | Codec or restore | Content, learning and policy only. It omits source dataset ID, attempts, active pointer and receipts. |
| [`AppError`](src/app_error.rs) | Application error mapping | Caller or error view | Stable code plus message and retryability; the caller need not parse a domain error string. |
| [`AttemptView`](src/views/attempt_view.rs) | Application result mapping | UI | Prompt, feedback, identities and lifetime assessment; no answer key or scheduler state. |
| [`EmptyView`](src/views/empty_view.rs) | Next with no eligible item | UI | Empty **now**, so another request can be useful after facts change. |
| [`ErrorView`](src/views/error_view.rs) | `AppError` conversion | UI | Recovery information; never a learning assessment. |
| [`StudyView`](src/views/study_view.rs) | `Tempo::handle` | Transport and UI | The three reply shapes. A resumed correction still displays its earlier `again` grade. |

The application owns the adaptation from Store's facts to Queue's input and from
Queue's selected target to Study's question source. That code is in
[queue_source.rs](src/application/queue_source.rs). Study does not acquire an
import of Queue, Chess or Store. Request/response values travel both into and out
of the application; dependency ownership stays in the composition root.

## Replace one implementation at a time

| Port | When to implement it | Fixture and executable scenario |
| --- | --- | --- |
| [`Clock`](src/ports/clock.rs) | Supply real application time or a controlled timeline | [`FixedClock`](src/adapters/fixed_clock.rs); the port's test injects a clock and checks the issued timestamp. |
| [`AttemptIds`](src/ports/attempt_ids.rs) | Supply identities unique across durable restarts | [`CounterIds`](src/adapters/counter_ids.rs); the port's test reserves one ID and proves resume does not allocate another. |
| [`ChessGrader`](src/ports/chess_grader.rs) | Bind a replacement grader to the concrete chess associated types | `ExactSequenceGrader`; the alias's test starts a two-decision exercise through a trait object. |
| [`GameProvider`](src/ports/game_provider.rs) | Fetch games in a job outside ordinary study | [`FixtureGames`](src/adapters/fixture_games.rs) and [`UnavailableGames`](src/adapters/unavailable_games.rs); examples cover filtering, ingestion and independent failure. |
| [`SnapshotCodec`](src/ports/snapshot_codec.rs) | Change the portable representation | [`JsonSnapshotCodec`](src/adapters/json_snapshot_codec.rs); the port's example plugs in a tagged encoding without changing Tempo. |

Domain policy ports stay in their owning crates: Queue decides eligibility and
ordering, Study advances the attempt using its grader, Chess interprets content,
Learning interprets assessments, and Store accepts a complete transaction.

## Operation guarantees worth reading first

Receipt replay runs before current-content validation. A committed reply remains
recoverable after an intervening content edit. A new accepted answer saves its
progress, any new assessment and learning change, and the receipt in one
optimistic transaction. Local queue history changes only after that commit.
[receipts.rs](src/application/receipts.rs) and
[current_attempt.rs](src/application/current_attempt.rs) keep those checks separate.

Game fetching is an independent caller action. Fetched games enter `ingest_games`,
then the same `again` operation available to other external feedback adapters.
Each deviation commits independently: a batch can be partly processed if a later
comparison or commit fails, and stable identities make rerunning it safe.

Content import updates the current dataset and preserves learning unless the
definition actually changes. Portable restore requires an empty destination,
compatible settings/memory and a fresh dataset identity. It is not an undo or a
merge into a live dataset.

## Read and run one scenario

From the local workspace root, use the module name as the Cargo test filter.
Each line below is an independently useful command; the UI demo and boundary
tests require the build first.

```sh
# One concept's same-module example (open its source alongside the output).
cargo test -p tempo --lib commands::submit_command::tests
cargo test -p tempo --lib ports::game_provider::tests
cargo test -p tempo --lib views::study_view::tests

# All nearby examples, the compiling usage example, and retained contract journeys.
cargo test -p tempo --lib
cargo test -p tempo --doc
cargo test -p tempo --test contracts
cargo doc -p tempo --no-deps --open

# Real Rust/UI boundary and the transport-backed demonstration.
cargo build -p tempo
node --test tempo/tests/ui_boundary.mjs
node tempo/demo/run.mjs
```

`cargo run -p tempo -- stdio` accepts one JSON command per line; begin with
`{"command":"next"}`. [src/main.rs](src/main.rs) owns that adapter, and the UI
imports no backend or process code. The established `--test contracts` target is
unchanged and includes failed commits, stale content, grouped queue substitution,
import/restore, corrective tactics, and replay. The Node boundary suite includes
a response lost after a real Rust commit.

`tests/contracts.rs` is now a small index. To follow a longer journey, choose
[study_journeys.rs](tests/contracts/study_journeys.rs),
[submission_recovery.rs](tests/contracts/submission_recovery.rs),
[external_feedback.rs](tests/contracts/external_feedback.rs),
[content_updates.rs](tests/contracts/content_updates.rs),
[portable_restore.rs](tests/contracts/portable_restore.rs), or
[queue_substitution.rs](tests/contracts/queue_substitution.rs).
For example, `cargo test -p tempo --test contracts content_updates` runs only the
content-update journeys. Shared explicit clock/store setup lives in
[support.rs](tests/contracts/support.rs).

## Current scope

Implemented: typed composition, attempt/learning rules, content-change
coordination, in-memory transaction invariants, replay, stale checks and UI
serialization. Fixture implementations cover parsing/canonicalization/comparison
inputs, interval scheduling, ordering, counter IDs, fixed time and process-local
storage. The JSON codec proves interchange; enriched PGN is a replacement codec.
There is no production HTTP server, SQL store or chess engine here yet.

Replace counter IDs before durable storage. Store snapshots currently read the
complete dataset; a future bounded-read interface should follow a measured need.
Cross-repository ownership lives in `.planning/INTERFACES.md`.
