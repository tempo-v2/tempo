use anyhow::{Context, Result, bail, ensure};
use pgn_reader::shakmaty::{
    CastlingMode, Chess, Color, EnPassantMode, Move, Position, Role, fen::Fen, uci::UciMove,
};
use pgn_reader::{RawTag, Reader, SanPlus, Visitor};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Cursor, ops::ControlFlow};

pub const MAX_PLIES: usize = 1024;
const MODE: CastlingMode = CastlingMode::Standard;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub fen: String,
    pub moves: Vec<String>,
}

#[derive(Debug)]
pub struct Pgn {
    pub tags: BTreeMap<String, String>,
    pub main: Route,
    pub variations: Vec<Route>,
    pub finished: bool,
    pub terminal_outcome: Option<pgn_reader::Outcome>,
}

pub fn position(fen: &str) -> Result<Chess> {
    if fen.trim().is_empty() {
        return Ok(Chess::default());
    }
    Ok(fen.parse::<Fen>()?.into_position(MODE)?)
}

pub fn fen(board: &Chess) -> String {
    Fen::from_position(board, EnPassantMode::Legal).to_string()
}

pub fn id(parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in parts {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

pub fn position_id(board: &Chess) -> String {
    let canonical = fen(board);
    let (position, _) = canonical.rsplit_once(' ').expect("fullmove number");
    let (position, _) = position.rsplit_once(' ').expect("halfmove clock");
    id(&["standard-position-v1", position])
}

pub fn uci(m: Move) -> String {
    UciMove::from_move(m, MODE).to_string()
}

/// Both the issued answer set and the already validated submission use canonical UCI.
pub fn answer_matches(accepted: &[String], played: Option<&str>) -> bool {
    played.is_some_and(|played| accepted.iter().any(|answer| answer == played))
}

pub fn legal(board: &Chess) -> Vec<String> {
    board.legal_moves().into_iter().map(uci).collect()
}

pub fn parse_move(board: &Chess, uci: &str) -> Result<Move> {
    Ok(uci.parse::<UciMove>()?.to_move(board)?)
}

pub fn color_name(color: Color) -> &'static str {
    if color == Color::White {
        "white"
    } else {
        "black"
    }
}

pub fn board_at(route: &Route, ply: usize) -> Result<Chess> {
    ensure!(
        ply <= route.moves.len() && route.moves.len() <= MAX_PLIES,
        "invalid route length"
    );
    let mut board = position(&route.fen)?;
    for m in &route.moves[..ply] {
        let legal = parse_move(&board, m).with_context(|| format!("invalid move {m}"))?;
        board.play_unchecked(legal);
    }
    Ok(board)
}

#[derive(Clone)]
struct Line {
    board: Chess,
    before: Option<Chess>,
    moves: Vec<String>,
}

struct Movetext {
    tags: BTreeMap<String, String>,
    fen: String,
    current: Line,
    stack: Vec<Line>,
    variations: Vec<Route>,
    terminal_outcome: Option<pgn_reader::Outcome>,
    moves_read: usize,
}

struct Parser {
    branches: bool,
}

impl Visitor for Parser {
    type Tags = BTreeMap<String, String>;
    type Movetext = Movetext;
    type Output = Result<Pgn>;

    fn begin_tags(&mut self) -> ControlFlow<Self::Output, Self::Tags> {
        ControlFlow::Continue(BTreeMap::new())
    }

    fn tag(
        &mut self,
        tags: &mut Self::Tags,
        key: &[u8],
        value: RawTag<'_>,
    ) -> ControlFlow<Self::Output> {
        if let Ok(key) = std::str::from_utf8(key)
            && [
                "White", "Black", "Result", "FEN", "Variant", "UTCDate", "UTCTime", "Date",
                "Round", "Site", "Link", "Event",
            ]
            .contains(&key)
        {
            tags.insert(key.to_owned(), value.decode_utf8_lossy().into_owned());
        }
        ControlFlow::Continue(())
    }

    fn begin_movetext(&mut self, tags: Self::Tags) -> ControlFlow<Self::Output, Self::Movetext> {
        if tags.get("Variant").is_some_and(|v| {
            !v.eq_ignore_ascii_case("standard") && !v.eq_ignore_ascii_case("chess")
        }) {
            return ControlFlow::Break(Err(anyhow::anyhow!("unsupported variant")));
        }
        match position(tags.get("FEN").map_or("", String::as_str)) {
            Ok(board) => ControlFlow::Continue(Movetext {
                tags,
                fen: fen(&board),
                current: Line {
                    board,
                    before: None,
                    moves: vec![],
                },
                stack: vec![],
                variations: vec![],
                terminal_outcome: None,
                moves_read: 0,
            }),
            Err(error) => ControlFlow::Break(Err(error)),
        }
    }

    fn san(&mut self, text: &mut Movetext, san: SanPlus) -> ControlFlow<Self::Output> {
        text.moves_read += 1;
        if text.current.moves.len() >= MAX_PLIES || text.moves_read > 20_000 {
            return ControlFlow::Break(Err(anyhow::anyhow!("PGN move limit exceeded")));
        }
        match san.san.to_move(&text.current.board) {
            Ok(m) => {
                text.current.before = Some(text.current.board.clone());
                text.current.moves.push(uci(m));
                text.current.board.play_unchecked(m);
                ControlFlow::Continue(())
            }
            Err(error) => ControlFlow::Break(Err(anyhow::anyhow!(
                "illegal PGN move {}: {error}",
                san.san
            ))),
        }
    }

    fn begin_variation(
        &mut self,
        text: &mut Movetext,
    ) -> ControlFlow<Self::Output, pgn_reader::Skip> {
        if !self.branches {
            return ControlFlow::Continue(pgn_reader::Skip(true));
        }
        if text.stack.len() >= 32 {
            return ControlFlow::Break(Err(anyhow::anyhow!(
                "PGN variation nesting limit exceeded"
            )));
        }
        let Some(board) = text.current.before.clone() else {
            return ControlFlow::Break(Err(anyhow::anyhow!("variation has no preceding move")));
        };
        let mut moves = text.current.moves.clone();
        moves.pop();
        let original = std::mem::replace(
            &mut text.current,
            Line {
                board,
                before: None,
                moves,
            },
        );
        text.stack.push(original);
        ControlFlow::Continue(pgn_reader::Skip(false))
    }

    fn end_variation(&mut self, text: &mut Movetext) -> ControlFlow<Self::Output> {
        if let Some(parent) = text.stack.pop() {
            let branch = std::mem::replace(&mut text.current, parent);
            text.variations.push(Route {
                fen: text.fen.clone(),
                moves: branch.moves,
            });
        }
        ControlFlow::Continue(())
    }

    fn outcome(
        &mut self,
        text: &mut Movetext,
        outcome: pgn_reader::Outcome,
    ) -> ControlFlow<Self::Output> {
        if text.stack.is_empty() {
            text.terminal_outcome = Some(outcome);
        }
        ControlFlow::Continue(())
    }

    fn end_game(&mut self, text: Movetext) -> Self::Output {
        ensure!(!text.current.moves.is_empty(), "PGN has no moves");
        ensure!(text.stack.is_empty(), "unclosed PGN variation");
        let finished = text
            .terminal_outcome
            .is_some_and(|outcome| !matches!(outcome, pgn_reader::Outcome::Unknown))
            || text
                .tags
                .get("Result")
                .is_some_and(|r| matches!(r.as_str(), "1-0" | "0-1" | "1/2-1/2"));
        Ok(Pgn {
            tags: text.tags,
            main: Route {
                fen: text.fen,
                moves: text.current.moves,
            },
            variations: text.variations,
            finished,
            terminal_outcome: text.terminal_outcome,
        })
    }
}

pub fn read_pgn(
    text: &str,
    branches: bool,
    mut visit: impl FnMut(Result<Pgn>) -> Result<()>,
) -> Result<()> {
    if text.trim().is_empty() {
        bail!("enter a PGN");
    }
    let mut reader = Reader::new(Cursor::new(text.as_bytes()));
    let mut parser = Parser { branches };
    let first = reader.read_game(&mut parser)?.context("PGN has no games")?;
    visit(first)?;
    while let Some(game) = reader.read_game(&mut parser)? {
        visit(game)?;
    }
    Ok(())
}

/// Read one record at a committed PGN boundary, excluding reader lookahead from the next offset.
pub fn pgn_record<R: std::io::Read + std::io::Seek>(
    mut input: R,
    offset: u64,
) -> Result<Option<(Result<Pgn>, u64)>> {
    input.seek(std::io::SeekFrom::Start(offset))?;
    let mut reader = Reader::new(input);
    let mut parser = Parser { branches: false };
    let Some(game) = reader.read_game(&mut parser)? else {
        ensure!(offset != 0, "PGN has no games");
        return Ok(None);
    };
    // An early visitor result leaves the rest of the physical game pending.
    // Complete that skip before recording a restart checkpoint.
    reader.has_more()?;
    let buffered = reader.buffer().len() as u64;
    let next = reader
        .get_mut()
        .stream_position()?
        .checked_sub(buffered)
        .context("invalid PGN checkpoint")?;
    ensure!(next > offset, "PGN checkpoint did not advance");
    Ok(Some((game, next)))
}

pub fn parse_pgn(text: &str, branches: bool, limit: usize) -> Result<Vec<Result<Pgn>>> {
    let mut games = Vec::new();
    read_pgn(text, branches, |game| {
        ensure!(games.len() < limit, "too many games in this batch");
        games.push(game);
        Ok(())
    })?;
    Ok(games)
}

fn value(role: Role) -> u8 {
    match role {
        Role::Pawn => 1,
        Role::Knight | Role::Bishop => 3,
        Role::Rook => 5,
        Role::Queen => 9,
        Role::King => 0,
    }
}

pub fn missed_captures(board: &Chess, played: Move) -> Vec<String> {
    if played.is_capture() || played.promotion().is_some() {
        return vec![];
    }
    let mut actual = board.clone();
    actual.play_unchecked(played);
    if actual.is_check() {
        return vec![];
    }
    board
        .legal_moves()
        .into_iter()
        .filter_map(|m| {
            let victim = m.capture().filter(|&role| value(role) >= 3)?;
            let mut after = board.clone();
            after.play_unchecked(m);
            let gives_back = after.legal_moves().into_iter().any(|reply| {
                if reply
                    .capture()
                    .is_some_and(|role| reply.to() == m.to() || value(role) >= value(victim))
                {
                    return true;
                }
                let mut response = after.clone();
                response.play_unchecked(reply);
                response.is_checkmate()
            });
            (!gives_back).then(|| uci(m))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_capture_and_recapture_are_distinct() -> Result<()> {
        for (fen, expected) in [
            ("4k3/8/8/3n4/8/8/8/3RK3 w - - 0 1", vec!["d1d5"]),
            ("4k3/8/4p3/3n4/8/8/8/3RK3 w - - 0 1", vec![]),
        ] {
            let board = position(fen)?;
            assert_eq!(
                missed_captures(&board, parse_move(&board, "e1e2")?),
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn variation_keeps_the_correct_prefix() -> Result<()> {
        let game = parse_pgn("1. e4 c5 (1... e5 2. Nf3) 2. Nc3 *", true, 1)?.remove(0)?;
        assert_eq!(game.main.moves, ["e2e4", "c7c5", "b1c3"]);
        assert_eq!(game.variations[0].moves, ["e2e4", "e7e5", "g1f3"]);
        Ok(())
    }
}
