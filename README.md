# tempo

The application connects the domain crates and commits complete operations. Start
at [src/lib.rs](src/lib.rs) for the public outline, then open the file named for
the concept you need. A file contains its purpose, usage guidance, implementation,
and a `#[cfg(test)] mod tests` example where the concept has its own public behavior.
The longer cross-component journeys stay in [tests/contracts.rs](tests/contracts.rs).

## Run the local release

Use the [Mac development and service guide](OPENING.md) and the compatible
component pins in the Planning release ledger. Normal `tempo serve` composes
`DailyComponents` with SQLite, the Study clock and the built Tempo UI. The
browser consumes Study wire 5 and signed content wire 4.

For slow or timed-out requests, start with [Diagnosing Tempo waits](DIAGNOSTICS.md).
The application records bounded local JSONL timings, request IDs, active-operation
watchdog observations and resource samples. The UI exports its delivery timeline;
`tools/diagnose.py` optionally adds a bounded host-process snapshot without
contacting Tempo or reading its database. Logging does not determine whether a
command was accepted.

Normal serving registers the hanging-material detector when Stockfish and the
Python chess environment are available. Background analysis resumes retained
games and follows new imports. Games displays the saved comparisons; every
confirmed missed good capture creates or reuses a single-answer puzzle and a
durable request for the next Study slot, after any active attempt.

The runtime looks for Python at the database-adjacent `maia/venv/bin/python`
and Stockfish at the existing Homebrew or system locations. Override them for
an isolated instance with `TEMPO_TACTICAL_PYTHON` and `TEMPO_STOCKFISH`.
Qualification uses `chess==1.11.2` and Stockfish 17.1. Missing dependencies are
reported as unavailable; Study still runs. The engine adapter keeps one owned
process group, uses two million nodes per position, and requires complete exact
root scores at depth 12 or greater. The game host has a 120-second deadline.
Incomplete evidence is recorded as uncertain and does not create a puzzle.

The detector considers legal captures of opposing nonking pieces, including
pawns, that permit no immediate legal recapture of the capturing piece.
Stockfish compares every legal move; a 30-centipawn tolerance admits equivalent
choices. Inferior captures are retained for comparison. When several captures
qualify, the strongest is the puzzle's one answer, with a deterministic tie
break. The actual game history and the standalone puzzle are evaluated
separately before a miss is accepted.

The implementation reuses the existing detector host, exact puzzle compiler,
Study grader and scheduler. New captures bypass ordinary introduction limits.
A new game miss involving an already tested puzzle receives immediate
Practice without another scheduled grade that day. Replay or revised analysis
does not duplicate an occurrence's request. An explicit Skip defers that target
for the session; a distinct new miss restores its priority. Analysis and request
state survive database restart; portable content/learning export keeps its
existing scope and does not include game-analysis delivery state.

For a disposable external-detector consumer, build
`game_detection_contract_host` and pass the temporary database,
built UI path, loopback port and scenario. The example calls the same
[public host entry](src/local_runtime.rs) as production. The
[HTTP proof](tests/game_detection_http.mjs) checks the addressed game, distinct
outcomes and stable durable state. The companion browser proof lives in Tempo UI.

## Choose the entry point

| When you need to… | Reach for | Source and nearby example |
| --- | --- | --- |
| Assemble production Daily Study | `DailyComponents` and `Tempo::daily` | [daily.rs](src/application/daily.rs) uses Learning availability, DailySession and Study issuance. |
| Assemble Daily Study | `DailyComponents` and `Tempo::daily` | [components.rs](src/components.rs) declares the runtime ports. |
| Begin study, or continue the active attempt | `Tempo::next_exercise` | [next_exercise.rs](src/application/next_exercise.rs) demonstrates that another Next resumes without issuing a competing attempt. |
| Reload persisted progress | `Tempo::resume` | [resume.rs](src/application/resume.rs) demonstrates a read with no new assessment or write. |
| Answer the displayed learner decision | `SubmitCommand` → `Tempo::submit` | [submit_command.rs](src/commands/submit_command.rs) constructs a retryable action; [submit.rs](src/application/submit.rs) shows progress, learning, assessment and receipt saved together. |
| Stop an attempt | `AbandonCommand` → `Tempo::abandon` | [abandon_command.rs](src/commands/abandon_command.rs) supplies revision/identity without an answer; [abandon.rs](src/application/abandon.rs) clears the active pointer. |
| Update production repertoire or tactics content | Signed Preview and Save or the validated tactic transfer | [repertoires.rs](src/application/repertoires.rs) and [tactic_capture.rs](src/application/tactic_capture.rs) preserve content and Learning fences. |
| Read a detector's observations for a stored game | Games detection discovery, start and result routes | [game_detection.rs](src/game_detection.rs) owns bounded ephemeral delivery. Chess checks legal occurrences and reports. |
| Analyze retained games and prioritize missed captures | Normal runtime's `TacticalJobs`, then `POST /api/games/tactics` for a bounded read | [tactical_jobs.rs](src/tactical_jobs.rs) reuses the detector worker; [tactical_analysis.rs](src/application/tactical_analysis.rs) accepts observation, puzzle and request atomically. |
| Process fixture-profile games | `Tempo::ingest_games` | [ingest_games.rs](src/application/ingest_games.rs) demonstrates replay-aware ingestion. The count includes processed deviations that were already committed. |
| Transfer content and learning to an empty dataset | `Tempo::export`, `Tempo::restore` | [export.rs](src/application/export.rs) shows a read-only export; [restore.rs](src/application/restore.rs) restores under a new dataset identity. |
| Dispatch one decoded UI intent | `wire::StudyCommand` → `Tempo::handle` | [study_command.rs](src/commands/study_command.rs) decodes the versioned envelope; [handle.rs](src/application/handle.rs) returns typed replies even on application failure. |

The existing root imports remain available, for example `tempo::SubmitCommand`
and `tempo::DailyComponents`. Grouped modules add navigable paths without requiring
callers to adopt them. `tempo::wire::{StudyCommand, StudyView}` remains the public
transport boundary. Your `src/views/` files remain the homes of those view concepts.

## Values and their direction

| Value | Produced by | Consumed by | What to notice |
| --- | --- | --- | --- |
| `SubmitCommand` / `AbandonCommand` | UI controller or another caller | Application operation | Copy the latest **attempt revision**, allocate one submission ID per user action, reuse the whole command for retries. |
| [`GameRange`](src/game_range.rs) | Refresh caller | `GameJobs` | Inclusive observation-time endpoints; the provider validates their order. |
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

The production Games UI uses the [loopback Games routes](src/local_runtime/http.rs).
PGN imports call `Tempo::import_game_pgn`. Provider refresh uses `GameJobs` to fetch
and deliver games separately from Study. `Tempo::reprocess_games` compares stored
games again, and `Tempo::choose_game_correction` resolves an ambiguous target.
These paths use combined repertoire coverage and `DailyLearning::assess_game`,
which owns the model update, corrective obligation and human Test cadence.
Store commits the results and command receipt together.

Content import updates the current dataset and preserves learning unless the
definition actually changes. Portable restore requires an empty destination,
compatible settings/memory and a fresh dataset identity. It is not an undo or a
merge into a live dataset.

## Read and run one scenario

From the local workspace root, use the module name as the Cargo test filter.
The HTTP boundary tests require the Tempo binary to be built first.

```sh
# One concept's same-module example (open its source alongside the output).
cargo test -p tempo --lib commands::submit_command::tests
cargo test -p tempo --lib ports::attempt_ids::tests
cargo test -p tempo --lib views::study_view::tests

# All nearby examples, the compiling usage example, and retained contract journeys.
cargo test -p tempo --lib
cargo test -p tempo --doc
cargo test -p tempo --test contracts
cargo doc -p tempo --no-deps --open

# Real Rust/UI boundary over HTTP against disposable SQLite data.
cargo build -p tempo
node --test tempo/tests/ui_boundary.mjs
```

The Node boundary suite starts the managed HTTP server on a disposable database,
validates replies through the Study UI, and checks that retrying a lost reply
does not duplicate the accepted assessment or receipt.

Use [study_journeys.rs](tests/contracts/study_journeys.rs) for normal Study,
[submission_recovery.rs](tests/contracts/submission_recovery.rs) for exact retry,
or [opening_release.rs](tests/contracts/opening_release.rs) for signed repertoire
updates. For example, `cargo test -p tempo --test contracts submission_recovery`
runs only the retry scenarios.

## Current scope

The loopback host uses the real Chess rules, SQLite Store, UTC clock, and
restart-safe identities.
Store snapshots currently read the complete dataset. Cross-repository ownership
lives in `.planning/INTERFACES.md`.

For the local opening checkpoint and start instructions, see [Opening study](OPENING.md).
