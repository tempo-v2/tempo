# Tempo Stockfish analysis

The Rust library exposes `Stockfish::analyze(Request)` for one standard-chess
position. The JSON CLI is the first consumer. Provision Stockfish on the host
and pass the absolute executable path. This capability was qualified with
Stockfish 19.

```sh
printf '%s\n' '{"fen":"rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"}' |
  cargo run --locked -p tempo-stockfish-analysis -- /opt/homebrew/bin/stockfish
```

Supply one JSON request on stdin and close stdin. The CLI reads at most 16 KiB,
prints one JSON reply, and exits nonzero on failure. The optional `options`
object supports `depth` (1–18, default 10) and `coverage`:

| Coverage | Result |
| --- | --- |
| `{"kind":"all"}` (default) | Each legal move once |
| `{"kind":"top","count":3}` | Up to three engine-ranked legal moves, with a requested maximum of ten |
| `{"kind":"moves","moves":["e2e4","f2f3"]}` | Exactly the supplied distinct legal UCI moves, ranked by the engine |

A successful evaluation includes the engine version, the common completed
depth, and the ranked lines. Each line has a `first_move`, a UCI `variation`
starting with that move, and a score such as
`{"kind":"centipawns","white":-80}` or
`{"kind":"mate","winner":"black","moves":3}`. Positive centipawns favor White,
negative centipawns favor Black. Lines are ordered best to worst for the side to move. The
scores are search estimates, and a restricted search can produce different
estimates from a full search. For a blunder comparison, use scores from the
same request.

Checkmate and stalemate return a terminal outcome without starting Stockfish.
FEN counters are retained within the Stockfish 19 limits of 32,767 halfmoves
and 100,000 fullmoves. Prior repetition history is outside this interface.
All-moves coverage can take longer than a targeted search. If the
requested search cannot complete in five seconds, the caller receives a typed
timeout and can request a lower depth. A partial or bounded-only frame never
becomes a successful comparison.

Share one `Stockfish` instance or its clones for a composition-wide slot.
The library uses Tokio and runs one engine per admitted request. Cancelling via
`analyze_until` waits for cleanup. Dropping the future also requests
cancellation, and the service remains busy until the child is reaped, even
when the caller’s runtime shuts down.
The result is transient and contains engine evidence for the caller to
interpret. There is no dependency from the Lean serving binary to this crate.

```sh
cargo test --locked -p tempo-stockfish-analysis
TEMPO_TEST_STOCKFISH=/opt/homebrew/bin/stockfish \
  cargo test --locked -p tempo-stockfish-analysis --test contract native_cli -- --ignored
```

## Generated tactic publication worker

`tempo-tactic-worker` is a separate process that evaluates pending generated
tactics over Tempo's owner-only Unix socket. The Tempo server validates and
applies the evidence through its normal SQLite writer. The worker has no
database access and the server has no engine dependency.

```sh
cargo build --locked --release --workspace
target/release/tempo-tactic-worker status /path/to/tempo.sqlite3
target/release/tempo-tactic-worker drain /path/to/tempo.sqlite3 /opt/homebrew/bin/stockfish
target/release/tempo-tactic-worker watch /path/to/tempo.sqlite3 /opt/homebrew/bin/stockfish
```

Run the server before starting the worker. `drain` processes currently ready
work and prints the resulting counts. Run it against a disposable served
backup to preview catalog impact. `watch` waits for new work and for deferred
retries, making it suitable for a separately managed LaunchAgent. One worker
per database takes one engine request at a time. If the companion is stopped,
Games import and Study continue and new capture candidates remain pending.

Tempo compares every legal root at depth 10. The best move must be a detector
candidate and have no equivalent alternative within 30 centipawns, inclusive.
Moves preserving the same forced mate count as equivalent across mate distances.
Existing capture cards remain available until checked. Rejected cards leave the
active catalog and queue while their saved reviews remain available for replay.
