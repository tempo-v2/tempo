use super::{Analysis, Error, Line, MODE, Score, Search};
use shakmaty::{Chess, Color, EnPassantMode, Position, fen::Fen, uci::UciMove};
use std::{collections::HashSet, future::Future, path::Path, process::Stdio, time::Instant};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
    time::timeout_at,
};

const MAX_LINE_BYTES: usize = 8 * 1024;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_VARIATION_PLIES: usize = 256;

pub(super) async fn analyze(
    executable: &Path,
    board: Chess,
    search: Search,
    deadline: Instant,
    cancel: impl Future<Output = ()>,
) -> Result<Analysis, Error> {
    if Instant::now() >= deadline {
        return Err(Error::Timeout);
    }
    let mut child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::EngineUnavailable)?;
    let result = tokio::select! {
        biased;
        () = cancel => Err(Error::Cancelled),
        result = timeout_at(deadline.into(), exchange(&mut child, board, search)) => {
            result.unwrap_or(Err(Error::Timeout))
        }
    };
    // Each request owns a fresh engine. Wait for OS acknowledgement even on timeout.
    let _ = child.start_kill();
    child.wait().await.map_err(|_| Error::Protocol)?;
    result
}

async fn exchange(child: &mut Child, board: Chess, search: Search) -> Result<Analysis, Error> {
    let mut input = child.stdin.take().ok_or(Error::Protocol)?;
    let mut output = Output {
        reader: BufReader::new(child.stdout.take().ok_or(Error::Protocol)?),
        remaining: MAX_OUTPUT_BYTES,
        line: Vec::new(),
    };
    input
        .write_all(b"uci\n")
        .await
        .map_err(|_| Error::Protocol)?;
    let mut engine = None;
    loop {
        let line = output.next().await?;
        if line == "uciok" {
            break;
        }
        if let Some(name) = line.strip_prefix("id name Stockfish ") {
            if name.is_empty() || name.len() > 128 {
                return Err(Error::Protocol);
            }
            engine = Some(format!("Stockfish {name}"));
        }
    }
    let engine = engine.ok_or(Error::Protocol)?;
    let configure = format!(
        "setoption name Threads value 1\nsetoption name Hash value 16\nsetoption name MultiPV value {}\nisready\n",
        search.lines
    );
    input
        .write_all(configure.as_bytes())
        .await
        .map_err(|_| Error::Protocol)?;
    while output.next().await? != "readyok" {}
    let fen = Fen::from_position(&board, EnPassantMode::Legal);
    let command = format!("position fen {fen}\n{}", search.command());
    input
        .write_all(command.as_bytes())
        .await
        .map_err(|_| Error::Protocol)?;

    let mut frame = Vec::with_capacity(search.lines);
    loop {
        let line = output.next().await?;
        if let Some(rest) = line.strip_prefix("bestmove ") {
            let best = rest
                .split_ascii_whitespace()
                .next()
                .ok_or(Error::Protocol)?;
            if frame.len() != search.lines {
                return Err(Error::Incomplete);
            }
            let lines = frame
                .into_iter()
                .collect::<Option<Vec<Line>>>()
                .ok_or(Error::Incomplete)?;
            let unique: HashSet<_> = lines.iter().map(|line| &line.first_move).collect();
            if unique.len() != lines.len()
                || lines.first().is_none_or(|line| line.first_move != best)
            {
                return Err(Error::Protocol);
            }
            return Ok(Analysis::Evaluated {
                engine,
                depth: search.depth,
                lines,
            });
        }
        if let Some((rank, line)) = parse_line(line, &board, &search)? {
            if rank == 1 {
                frame.clear();
            }
            if rank != frame.len() + 1 {
                return Err(Error::Protocol);
            }
            frame.push(line);
        }
    }
}

struct Output {
    reader: BufReader<ChildStdout>,
    remaining: usize,
    line: Vec<u8>,
}

impl Output {
    async fn next(&mut self) -> Result<&str, Error> {
        self.line.clear();
        let limit = MAX_LINE_BYTES.min(self.remaining);
        let count = (&mut self.reader)
            .take((limit + 1) as u64)
            .read_until(b'\n', &mut self.line)
            .await
            .map_err(|_| Error::Protocol)?;
        if count == 0 || count > limit || !self.line.ends_with(b"\n") {
            return Err(Error::Protocol);
        }
        self.remaining -= count;
        let line = std::str::from_utf8(&self.line).map_err(|_| Error::Protocol)?;
        Ok(line.trim_end_matches(['\r', '\n']))
    }
}

fn parse_line(
    line: &str,
    board: &Chess,
    search: &Search,
) -> Result<Option<(usize, Option<Line>)>, Error> {
    let Some(info) = line.strip_prefix("info depth ") else {
        return Ok(None);
    };
    let mut fields = info.split_ascii_whitespace().peekable();
    let depth = fields
        .next()
        .and_then(|field| field.parse::<u8>().ok())
        .ok_or(Error::Protocol)?;
    if depth != search.depth || fields.peek() == Some(&"currmove") {
        return Ok(None);
    }
    let mut rank = None;
    let mut score = None;
    let mut variation = None;
    let mut bounded = false;
    while let Some(field) = fields.next() {
        match field {
            "multipv" => {
                rank = fields
                    .next()
                    .and_then(|number| number.parse::<usize>().ok())
            }
            "score" => score = Some(parse_score(fields.next(), fields.next(), board.turn())?),
            "lowerbound" | "upperbound" => bounded = true,
            "pv" => {
                variation = Some(parse_variation(board, fields)?);
                break;
            }
            _ => {}
        }
    }
    let rank = rank
        .filter(|rank| (1..=search.lines).contains(rank))
        .ok_or(Error::Protocol)?;
    if bounded {
        return Ok(Some((rank, None)));
    }
    let score = score.ok_or(Error::Protocol)?;
    let (root, variation) = variation.ok_or(Error::Protocol)?;
    if !search.roots.contains(&root) {
        return Err(Error::Protocol);
    }
    Ok(Some((
        rank,
        Some(Line {
            first_move: root.to_string(),
            variation,
            score,
        }),
    )))
}

fn parse_score(kind: Option<&str>, value: Option<&str>, turn: Color) -> Result<Score, Error> {
    let value = value
        .and_then(|number| number.parse::<i32>().ok())
        .ok_or(Error::Protocol)?;
    match kind {
        Some("cp") => {
            let white = if turn.is_white() {
                value
            } else {
                value.checked_neg().ok_or(Error::Protocol)?
            };
            Ok(Score::Centipawns { white })
        }
        Some("mate") if value != 0 => Ok(Score::Mate {
            winner: if value > 0 {
                turn.into()
            } else {
                (!turn).into()
            },
            moves: value.unsigned_abs(),
        }),
        _ => Err(Error::Protocol),
    }
}

fn parse_variation<'a>(
    board: &Chess,
    fields: impl Iterator<Item = &'a str>,
) -> Result<(UciMove, Vec<String>), Error> {
    let mut board = board.clone();
    let mut roots = None;
    let mut moves = Vec::new();
    for field in fields {
        if moves.len() == MAX_VARIATION_PLIES {
            return Err(Error::Protocol);
        }
        let uci: UciMove = field.parse().map_err(|_| Error::Protocol)?;
        let play = uci.to_move(&board).map_err(|_| Error::Protocol)?;
        let canonical = UciMove::from_move(play, MODE);
        roots.get_or_insert(canonical);
        moves.push(canonical.to_string());
        board.play_unchecked(play);
    }
    Ok((roots.ok_or(Error::Protocol)?, moves))
}
