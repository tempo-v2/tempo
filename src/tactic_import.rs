//! Authored puzzle intake. Source links select import defaults and are catalog metadata.
use crate::{catalog, chess};
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    GenericPgn,
    ChessComPuzzle,
    LichessPgn,
}

impl Format {
    fn first_move_is_setup(self) -> bool {
        matches!(self, Self::ChessComPuzzle)
    }
}

#[derive(Deserialize, Serialize)]
pub struct OptionsInput {
    pub pgn: String,
    pub puzzle_url: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Input {
    #[serde(flatten)]
    pub source: OptionsInput,
    pub name: Option<String>,
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

pub fn lichess_id(text: &str) -> bool {
    text.len() == 5 && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn study_id(text: &str) -> bool {
    text.len() == 8 && text.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn source(text: &str) -> Option<Options> {
    let text = text.trim();
    if text.len() > 2048 {
        return None;
    }
    let mut url = Url::parse(text).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return None;
    }
    let mut path = url.path_segments()?;
    let format = match (url.host_str()?, path.next()?, path.next()?, path.next()) {
        ("www.chess.com", "puzzles", "problem", Some(id))
            if path.next().is_none()
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit()) =>
        {
            Format::ChessComPuzzle
        }
        ("lichess.org", "training", id, None) if lichess_id(id) => Format::LichessPgn,
        ("lichess.org", "study", study, Some(chapter))
            if path.next().is_none() && study_id(study) && study_id(chapter) =>
        {
            Format::LichessPgn
        }
        _ => return None,
    };
    url.set_query(None);
    url.set_fragment(None);
    Some(Options {
        format,
        source_url: Some(url.into()),
    })
}

fn resolve(tags: &BTreeMap<String, String>, field: Option<&str>) -> Options {
    [
        field,
        tags.get("Link").map(String::as_str),
        tags.get("Site").map(String::as_str),
        tags.get("ChapterURL").map(String::as_str),
    ]
    .into_iter()
    .flatten()
    .find_map(source)
    .unwrap_or(Options {
        format: Format::GenericPgn,
        source_url: None,
    })
}

/// Inspect the first record's tags without parsing moves or opening the catalog.
pub fn options(pgn: impl Read, puzzle_url: Option<&str>) -> Result<Options> {
    Ok(resolve(&chess::read_pgn_tags(pgn)?, puzzle_url))
}

pub fn ensure_pgn_size(pgn: &str) -> Result<()> {
    ensure!(pgn.len() <= crate::jobs::MAX_PGN, "PGN exceeds 64 MiB");
    Ok(())
}

/// The existing HTTP authoring and CLI operations accept solver-first PGNs.
pub fn prepare_authored(mut input: Input) -> Result<Prepared> {
    catalog::name(input.name.as_deref().context("enter a puzzle name")?)?;
    if input.format.is_none() {
        input.format = Some(Format::GenericPgn);
    }
    prepare(input)
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
    if !matches!(format, Format::LichessPgn) {
        answers.extend(
            puzzle
                .variations
                .iter()
                .filter_map(|branch| branch.moves.strip_prefix(setup_move.as_slice()))
                .filter_map(|moves| moves.first().cloned()),
        );
    }
    let default_name = match detected.source_url.as_deref() {
        Some(url) if matches!(detected.format, Format::ChessComPuzzle) => {
            format!(
                "Chess.com puzzle {}",
                url.rsplit('/').next().unwrap_or_default()
            )
        }
        Some(url) if url.starts_with("https://lichess.org/training/") => {
            format!(
                "Lichess puzzle {}",
                url.rsplit('/').next().unwrap_or_default()
            )
        }
        _ => puzzle
            .tags
            .get("ChapterName")
            .and_then(|name| catalog::name(name).ok())
            .unwrap_or("Captured puzzle")
            .to_owned(),
    };
    prepare_route(
        input.name.as_deref(),
        &default_name,
        &puzzle.main,
        &answers,
        detected.source_url,
    )
}

pub fn prepare_route(
    name: Option<&str>,
    default_name: &str,
    route: &chess::Route,
    answers: &[String],
    source_url: Option<String>,
) -> Result<Prepared> {
    let plan = catalog::prepare_tactic(catalog::TacticInput {
        name: name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(default_name),
        route,
        answers,
        source: None,
    })?;
    Ok(Prepared { plan, source_url })
}
