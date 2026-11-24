use crate::chess::{self, Route};
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct OpeningInput {
    pub name: String,
    pub color: String,
    pub route: Route,
    #[serde(default)]
    pub start_at: usize,
}

#[derive(Serialize)]
pub struct Opening {
    pub id: String,
    pub name: String,
    pub color: String,
    pub route: Route,
    pub start_at: usize,
}

#[derive(Serialize)]
pub struct Tactic {
    pub id: String,
    pub name: String,
    pub route: Route,
    pub source_game: Option<String>,
    pub source_ply: Option<usize>,
}

pub fn name(value: &str) -> Result<&str> {
    let value = value.trim();
    ensure!(
        !value.is_empty() && value.chars().count() <= 160,
        "enter a name of 1–160 characters"
    );
    Ok(value)
}

pub fn openings(db: &Connection, after: &str) -> Result<Vec<Opening>> {
    let mut q = db.prepare(
        "SELECT id,name,color,route,start_at FROM openings WHERE id>?1 ORDER BY id LIMIT 100",
    )?;
    let rows = q.query_map([after], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get::<_, String>(3)?,
            r.get(4)?,
        ))
    })?;
    rows.map(|r| {
        let (id, name, color, route, start_at) = r?;
        Ok(Opening {
            id,
            name,
            color,
            route: serde_json::from_str(&route)?,
            start_at,
        })
    })
    .collect()
}

pub fn save_opening(
    db: &Connection,
    input: &OpeningInput,
    inherited_book: Option<&str>,
) -> Result<String> {
    let name = name(&input.name)?;
    ensure!(
        matches!(input.color.as_str(), "white" | "black"),
        "select a practice color"
    );
    let encoded = serde_json::to_string(&input.route.moves)?;
    let mut board = chess::position(&input.route.fen)?;
    let route = Route {
        fen: chess::fen(&board),
        moves: input.route.moves.clone(),
    };
    ensure!(
        input.start_at < route.moves.len() && route.moves.len() <= chess::MAX_PLIES,
        "enter a continuation"
    );
    let id = chess::id(&[
        "opening-v1",
        inherited_book.unwrap_or(""),
        name,
        &input.color,
        &route.fen,
        &encoded,
        &input.start_at.to_string(),
    ]);
    let repertoire = inherited_book.unwrap_or(&id);
    let mut cards = 0;
    for (ply, played) in route.moves.iter().enumerate() {
        let m = chess::parse_move(&board, played)?;
        if ply >= input.start_at && chess::color_name(board.turn()) == input.color {
            let normalized = chess::uci(m);
            let version = chess::id(&["answer-v1", &normalized]);
            let position_id = chess::position_id(&board);
            let card_id = chess::id(&["opening-card-v1", repertoire, &position_id, &version]);
            let context = Route {
                fen: route.fen.clone(),
                moves: route.moves[..ply].to_vec(),
            };
            db.execute(
                "INSERT OR IGNORE INTO cards VALUES (?1,'opening',?2,?3,?4,?5,?6,?7,?8,NULL,?9,NULL,NULL)",
                params![
                    card_id,
                    name,
                    input.color,
                    chess::fen(&board),
                    position_id,
                    serde_json::to_string(&[normalized])?,
                    repertoire,
                    version,
                    serde_json::to_string(&context)?
                ],
            )?;
            cards += 1;
        }
        board.play_unchecked(m);
    }
    ensure!(
        cards > 0,
        "the continuation needs a move for the selected color"
    );
    db.execute(
        "INSERT OR IGNORE INTO openings VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            id,
            repertoire,
            name,
            input.color,
            serde_json::to_string(&route)?,
            input.start_at
        ],
    )?;
    Ok(id)
}

pub fn tactics(db: &Connection, after: &str) -> Result<Vec<Tactic>> {
    let mut q = db.prepare(
        "SELECT id,name,route,game_id,game_ply FROM tactics WHERE id>?1 ORDER BY id LIMIT 100",
    )?;
    let rows = q.query_map([after], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get::<_, String>(2)?,
            r.get(3)?,
            r.get(4)?,
        ))
    })?;
    rows.map(|r| {
        let (id, name, route, source_game, source_ply) = r?;
        Ok(Tactic {
            id,
            name,
            route: serde_json::from_str(&route)?,
            source_game,
            source_ply,
        })
    })
    .collect()
}

pub struct TacticInput<'a> {
    pub name: &'a str,
    pub route: &'a Route,
    pub answers: &'a [String],
    pub source: Option<(&'a str, usize, &'a str)>,
}

pub fn publish_tactic(db: &Connection, input: TacticInput<'_>) -> Result<String> {
    let name = name(input.name)?;
    ensure!(
        !input.answers.is_empty() && !input.route.moves.is_empty(),
        "a puzzle needs an accepted move"
    );
    let board = chess::board_at(input.route, 0)?;
    chess::board_at(input.route, input.route.moves.len())?;
    let mut answers = input
        .answers
        .iter()
        .map(|m| Ok(chess::uci(chess::parse_move(&board, m)?)))
        .collect::<Result<Vec<_>>>()?;
    answers.sort();
    answers.dedup();
    ensure!(
        answers.contains(&chess::uci(chess::parse_move(
            &board,
            &input.route.moves[0]
        )?)),
        "the solution must begin with an accepted move"
    );
    let answers = serde_json::to_string(&answers)?;
    let position_id = chess::position_id(&board);
    let encoded = serde_json::to_string(input.route)?;
    let id = match input.source {
        Some((game, ply, _)) => chess::id(&["game-puzzle-v1", game, &ply.to_string()]),
        None => chess::id(&[
            "pgn-puzzle-v1",
            &position_id,
            &answers,
            &serde_json::to_string(&input.route.moves)?,
        ]),
    };
    let (game, ply, played) = match input.source {
        Some((game, ply, played)) => (Some(game), Some(ply), Some(played)),
        None => (None, None, None),
    };
    db.execute(
        "INSERT OR IGNORE INTO tactics VALUES (?1,?2,?3,?4,?5)",
        params![id, name, encoded, game, ply],
    )?;
    db.execute(
        "INSERT OR IGNORE INTO cards VALUES (?1,'tactic',?2,?3,?4,?5,?6,NULL,NULL,?7,NULL,?8,?9)",
        params![
            id,
            name,
            chess::color_name(board.turn()),
            chess::fen(&board),
            position_id,
            answers,
            played,
            game,
            ply
        ],
    )?;
    Ok(id)
}

pub struct ReferenceAnswer {
    pub repertoire_id: String,
    pub answer_version: String,
    pub moves: Vec<String>,
}

pub fn answers_at(db: &Connection, position: &str, color: &str) -> Result<Vec<ReferenceAnswer>> {
    let mut q = db.prepare_cached("SELECT repertoire_id,answer_version,answers FROM cards WHERE position_id=?1 AND kind='opening' AND color=?2")?;
    let rows = q.query_map(params![position, color], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get::<_, String>(2)?))
    })?;
    rows.map(|r| {
        let (repertoire_id, answer_version, encoded) = r?;
        Ok(ReferenceAnswer {
            repertoire_id,
            answer_version,
            moves: serde_json::from_str(&encoded)?,
        })
    })
    .collect()
}
