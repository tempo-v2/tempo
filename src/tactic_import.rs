//! Authored puzzle intake. Source links select import defaults and are catalog metadata.
use crate::{catalog, chess};
use anyhow::{Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    GenericPgn,
    ChessComPuzzle,
}

impl Format {
    fn first_move_is_setup(self) -> bool {
        matches!(self, Self::ChessComPuzzle)
    }
}

#[derive(Deserialize)]
pub struct OptionsInput {
    pub pgn: String,
    pub puzzle_url: Option<String>,
}

#[derive(Deserialize)]
pub struct Input {
    #[serde(flatten)]
    pub source: OptionsInput,
    pub name: String,
    pub format: Option<Format>,
    pub first_move_is_setup: Option<bool>,
}

#[derive(Serialize)]
pub struct Options {
    pub format: Format,
    pub source_url: Option<String>,
}

pub struct Prepared {
    pub plan: catalog::TacticPlan,
    pub source_url: Option<String>,
}

fn chess_com_puzzle_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() > 2048 {
        return None;
    }
    let mut url = Url::parse(text).ok()?;
    let id = url.path().strip_prefix("/puzzles/problem/")?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.chess.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || id.is_empty()
        || !id.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn resolve(tags: &BTreeMap<String, String>, field: Option<&str>) -> Options {
    let source_url = [
        field,
        tags.get("Link").map(String::as_str),
        tags.get("Site").map(String::as_str),
    ]
    .into_iter()
    .flatten()
    .find_map(chess_com_puzzle_url);
    let format = if source_url.is_some() {
        Format::ChessComPuzzle
    } else {
        Format::GenericPgn
    };
    Options { format, source_url }
}

/// Inspect the first record's tags without parsing moves or opening the catalog.
pub fn options(pgn: impl Read, puzzle_url: Option<&str>) -> Result<Options> {
    Ok(resolve(&chess::read_pgn_tags(pgn)?, puzzle_url))
}

pub fn ensure_pgn_size(pgn: &str) -> Result<()> {
    ensure!(pgn.len() <= crate::jobs::MAX_PGN, "PGN exceeds 64 MiB");
    Ok(())
}

pub fn prepare(input: Input) -> Result<Prepared> {
    ensure_pgn_size(&input.source.pgn)?;
    let mut puzzle = chess::parse_pgn(&input.source.pgn, true, 1)?.remove(0)?;
    let detected = resolve(&puzzle.tags, input.source.puzzle_url.as_deref());
    let format = input.format.unwrap_or(detected.format);
    let setup = input
        .first_move_is_setup
        .unwrap_or_else(|| format.first_move_is_setup());
    let setup_move = if setup {
        ensure!(
            puzzle.main.moves.len() > 1,
            "a puzzle needs a solution move after setup"
        );
        puzzle.main.fen = chess::fen(&chess::board_at(&puzzle.main, 1)?);
        Some(puzzle.main.moves.remove(0))
    } else {
        None
    };

    let mut answers = vec![puzzle.main.moves[0].clone()];
    answers.extend(
        puzzle
            .variations
            .iter()
            .filter_map(|branch| branch.moves.strip_prefix(setup_move.as_slice()))
            .filter_map(|moves| moves.first().cloned()),
    );
    let plan = catalog::prepare_tactic(catalog::TacticInput {
        name: &input.name,
        route: &puzzle.main,
        answers: &answers,
        source: None,
    })?;
    Ok(Prepared {
        plan,
        source_url: detected.source_url,
    })
}
