//! Bounded, read-only position analysis using a local Stockfish executable.
mod uci;

use serde::{Deserialize, Serialize};
use shakmaty::{CastlingMode, Chess, Color, EnPassantMode, Position, fen::Fen, uci::UciMove};
use std::{
    collections::HashSet,
    future::{Future, pending},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tokio::{
    runtime,
    sync::{Semaphore, oneshot},
};

const DEADLINE: Duration = Duration::from_secs(5);
const MAX_FEN_BYTES: usize = 256;
// Stockfish supports up to 256 MultiPV lines, enough for every standard-chess root.
const MAX_ROOTS: usize = 256;
const MODE: CastlingMode = CastlingMode::Standard;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub fen: String,
    #[serde(default)]
    pub options: Options,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    pub coverage: Coverage,
    pub depth: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            coverage: Coverage::All {},
            depth: 10,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Coverage {
    All {},
    Top { count: u8 },
    Moves { moves: Vec<String> },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Analysis {
    Evaluated {
        engine: String,
        depth: u8,
        lines: Vec<Line>,
    },
    Terminal {
        outcome: Terminal,
    },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Terminal {
    Checkmate { winner: Side },
    Stalemate,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    White,
    Black,
}

impl From<Color> for Side {
    fn from(color: Color) -> Self {
        match color {
            Color::White => Self::White,
            Color::Black => Self::Black,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Line {
    pub first_move: String,
    pub variation: Vec<String>,
    pub score: Score,
}

/// Engine estimates are relative to White. Mate distance is in moves.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Score {
    Centipawns { white: i32 },
    Mate { winner: Side, moves: u32 },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Error {
    InvalidRequest { reason: &'static str },
    Busy,
    EngineUnavailable,
    Timeout,
    Cancelled,
    Protocol,
    Incomplete,
}

#[derive(Clone)]
pub struct Stockfish {
    executable: Arc<PathBuf>,
    slot: Arc<Semaphore>,
}

impl Stockfish {
    /// Use one instance (or its clones) per host composition. The executable is host setup.
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable: Arc::new(executable),
            slot: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn analyze(&self, request: Request) -> Result<Analysis, Error> {
        self.analyze_until(request, pending()).await
    }

    /// A completed cancellation waits for subprocess cleanup. Dropping the caller's
    /// future also cancels the work, and the worker retains the slot through cleanup.
    pub async fn analyze_until(
        &self,
        request: Request,
        cancel: impl Future<Output = ()>,
    ) -> Result<Analysis, Error> {
        let deadline = Instant::now() + DEADLINE;
        let (board, search) = Search::validate(request)?;
        if search.lines == 0 {
            return Ok(Analysis::Terminal {
                outcome: if board.is_check() {
                    Terminal::Checkmate {
                        winner: (!board.turn()).into(),
                    }
                } else {
                    Terminal::Stalemate
                },
            });
        }
        let slot = self
            .slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Busy)?;
        let executable = self.executable.clone();
        let (mut reply, mut result) = oneshot::channel();
        let (stop, mut cancelled) = oneshot::channel();
        // The engine and permit must outlive the caller's executor if it shuts down.
        thread::Builder::new()
            .name("tempo-stockfish".to_owned())
            .spawn(move || {
                let analysis = runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| Error::EngineUnavailable)
                    .and_then(|worker| {
                        worker.block_on(uci::analyze(&executable, board, search, deadline, async {
                            tokio::select! {
                                _ = &mut cancelled => {}
                                _ = reply.closed() => {}
                            }
                        }))
                    });
                drop(slot);
                let _ = reply.send(analysis);
            })
            .map_err(|_| Error::EngineUnavailable)?;
        tokio::pin!(cancel);
        tokio::select! {
            biased;
            analysis = &mut result => analysis.map_err(|_| Error::Protocol)?,
            () = &mut cancel => {
                let _ = stop.send(());
                result.await.map_err(|_| Error::Protocol)?
            }
        }
    }
}

struct Search {
    depth: u8,
    lines: usize,
    roots: HashSet<UciMove>,
    restricted: bool,
}

impl Search {
    fn validate(Request { fen, options }: Request) -> Result<(Chess, Search), Error> {
        let invalid = |reason| Error::InvalidRequest { reason };
        let fields = fen.split_ascii_whitespace();
        if fen.len() > MAX_FEN_BYTES
            || !fen
                .bytes()
                .all(|byte| (byte.is_ascii_graphic() && byte != b'_') || byte == b' ')
            || fields.clone().count() != 6
        {
            return Err(invalid("expected a complete standard-chess FEN"));
        }
        let mut counters = fields.clone().rev();
        let fullmove = counters.next().unwrap_or_default();
        let halfmove = counters.next().unwrap_or_default();
        if !fullmove.bytes().all(|byte| byte.is_ascii_digit())
            || !fullmove.bytes().any(|byte| byte != b'0')
            || !halfmove.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid("invalid FEN move counters"));
        }
        let fen: Fen = fen
            .parse()
            .map_err(|_| invalid("invalid standard-chess FEN"))?;
        let board: Chess = fen
            .into_position(MODE)
            .map_err(|_| invalid("invalid standard-chess position"))?;
        // Shakmaty also accepts variant encodings and normalizes some malformed
        // fields. The standard placement and castling fields must round-trip.
        let standard_fen = Fen::from_position(&board, EnPassantMode::Legal).to_string();
        let standard_fields = standard_fen.split_ascii_whitespace();
        if fields
            .step_by(2)
            .take(2)
            .ne(standard_fields.step_by(2).take(2))
        {
            return Err(invalid(
                "expected standard piece placement and castling fields",
            ));
        }
        // These are the Stockfish 19 limits for the FEN counters.
        if board.halfmoves() > 32_767 || board.fullmoves().get() > 100_000 {
            return Err(invalid("FEN counters exceed Stockfish limits"));
        }
        if !(1..=18).contains(&options.depth) {
            return Err(invalid("depth must be between 1 and 18"));
        }
        let legal: HashSet<_> = board
            .legal_moves()
            .into_iter()
            .map(|play| UciMove::from_move(play, MODE))
            .collect();
        let (roots, lines, restricted) = match options.coverage {
            Coverage::All {} => {
                let count = legal.len();
                (legal, count, false)
            }
            Coverage::Top { count } if (1..=10).contains(&count) => {
                let count = usize::from(count).min(legal.len());
                (legal, count, false)
            }
            Coverage::Top { .. } => return Err(invalid("top count must be between 1 and 10")),
            Coverage::Moves { moves } => {
                if moves.is_empty() || moves.len() > MAX_ROOTS {
                    return Err(invalid("expected a nonempty set of legal moves"));
                }
                let mut roots = HashSet::with_capacity(moves.len());
                for play in moves {
                    let play = play
                        .parse::<UciMove>()
                        .map_err(|_| invalid("invalid UCI move"))?;
                    if !legal.contains(&play) || !roots.insert(play) {
                        return Err(invalid("candidate moves must be legal and distinct"));
                    }
                }
                let count = roots.len();
                (roots, count, true)
            }
        };
        Ok((
            board,
            Search {
                depth: options.depth,
                lines,
                roots,
                restricted,
            },
        ))
    }

    fn command(&self) -> String {
        let mut command = format!("go depth {}", self.depth);
        if self.restricted {
            let mut roots: Vec<_> = self.roots.iter().map(ToString::to_string).collect();
            roots.sort_unstable();
            command.push_str(" searchmoves ");
            command.push_str(&roots.join(" "));
        }
        command.push('\n');
        command
    }
}
