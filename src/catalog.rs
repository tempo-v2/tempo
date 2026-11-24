use crate::{
    chess::{self, Route},
    scanner::ReferenceAnswer,
};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension, params};
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
    pub repertoire_id: String,
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
        "SELECT id,repertoire_id,name,color,route,start_at FROM openings WHERE id>?1 ORDER BY id LIMIT 100",
    )?;
    let rows = q.query_map([after], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get::<_, String>(4)?,
            r.get(5)?,
        ))
    })?;
    rows.map(|r| {
        let (id, repertoire_id, name, color, route, start_at) = r?;
        Ok(Opening {
            id,
            repertoire_id,
            name,
            color,
            route: serde_json::from_str(&route)?,
            start_at,
        })
    })
    .collect()
}

#[derive(Serialize)]
pub struct Repertoire {
    pub id: String,
    pub example_line: String,
    pub color: String,
}

pub fn repertoires(db: &Connection, after: &str, color: &str) -> Result<Vec<Repertoire>> {
    let mut q = db.prepare(
        "SELECT repertoire_id,example_line,color FROM repertoire_choices
        WHERE (repertoire_id,color) > (?1,?2) ORDER BY repertoire_id,color LIMIT 50",
    )?;
    Ok(q.query_map((after, color), |r| {
        Ok(Repertoire {
            id: r.get(0)?,
            example_line: r.get(1)?,
            color: r.get(2)?,
        })
    })?
    .collect::<rusqlite::Result<_>>()?)
}

pub fn existing_repertoire(db: &Connection, id: &str, color: &str) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM repertoire_choices WHERE repertoire_id=?1 AND color=?2)",
            params![id, color],
            |row| row.get::<_, bool>(0),
        )?,
        "The selected repertoire is unavailable for this practice color."
    );
    Ok(())
}

#[derive(Debug)]
pub struct OpeningConflict(String);

impl std::fmt::Display for OpeningConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OpeningConflict {}

pub fn branch_repertoire(db: &Connection, source: &str, color: &str) -> Result<String> {
    let (repertoire, saved_color): (String, String) = db
        .query_row(
            "SELECT repertoire_id,color FROM openings WHERE id=?1",
            [source],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .context("The opening you branched from is no longer available.")?;
    ensure!(
        color == saved_color,
        "Use the source opening's practice color when branching."
    );
    Ok(repertoire)
}

#[derive(Debug)]
pub struct OverBudget;
impl std::fmt::Display for OverBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This save exceeds the online row budget. Use tempo import-content.")
    }
}
impl std::error::Error for OverBudget {}

pub struct OpeningPlan {
    pub color: String,
    name: String,
    fen: String,
    moves: String,
    route: String,
    start_at: usize,
    cards: Vec<OpeningCard>,
}

struct OpeningCard {
    ply: std::num::NonZeroU32,
    position: String,
    version: String,
    fen: String,
    answers: String,
    context: String,
}

pub fn prepare_opening(input: &OpeningInput) -> Result<OpeningPlan> {
    let name = name(&input.name)?.to_owned();
    ensure!(
        matches!(input.color.as_str(), "white" | "black"),
        "select a practice color"
    );
    let mut board = chess::position(&input.route.fen)?;
    let route = Route {
        fen: chess::fen(&board),
        moves: input.route.moves.clone(),
    };
    ensure!(
        input.start_at < route.moves.len() && route.moves.len() <= chess::MAX_PLIES,
        "enter a continuation"
    );
    let mut cards = Vec::new();
    for (ply, played) in route.moves.iter().enumerate() {
        let movement = chess::parse_move(&board, played)?;
        if ply >= input.start_at && chess::color_name(board.turn()) == input.color {
            let normalized = chess::uci(movement);
            cards.push(OpeningCard {
                ply: board.fullmoves(),
                position: chess::position_id(&board),
                version: chess::id(&["answer-v1", &normalized]),
                fen: chess::fen(&board),
                answers: serde_json::to_string(&[normalized])?,
                context: serde_json::to_string(&Route {
                    fen: route.fen.clone(),
                    moves: route.moves[..ply].to_vec(),
                })?,
            });
        }
        board.play_unchecked(movement);
    }
    ensure!(
        !cards.is_empty(),
        "the continuation needs a move for the selected color"
    );
    Ok(OpeningPlan {
        name,
        color: input.color.clone(),
        fen: route.fen.clone(),
        moves: serde_json::to_string(&route.moves)?,
        route: serde_json::to_string(&route)?,
        start_at: input.start_at,
        cards,
    })
}

pub fn apply_opening(
    db: &Connection,
    plan: &OpeningPlan,
    inherited_book: Option<&str>,
    online: bool,
) -> Result<String> {
    let id = chess::id(&[
        "opening-v1",
        inherited_book.unwrap_or(""),
        &plan.name,
        &plan.color,
        &plan.fen,
        &plan.moves,
        &plan.start_at.to_string(),
    ]);
    let repertoire = inherited_book.unwrap_or(&id);
    let new_choice = inherited_book.is_none()
        && !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM repertoire_choices WHERE repertoire_id=?1 AND color=?2)",
            (repertoire, &plan.color),
            |row| row.get::<_, bool>(0),
        )?;
    let mut pending = std::collections::HashMap::new();
    for card in &plan.cards {
        let card_id = chess::id(&["opening-card-v1", repertoire, &card.position, &card.version]);
        let prior = db.query_row(
            "SELECT id,quarantined FROM cards WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2
             ORDER BY quarantined LIMIT 1",
            params![repertoire,card.position],
            |row| Ok((row.get::<_,String>(0)?,row.get::<_,bool>(1)?)),
        ).optional()?;
        if prior.as_ref().is_some_and(|(_, quarantined)| *quarantined) {
            return Err(OpeningConflict(format!(
                "Move {}: this repertoire has conflicting replies at this position awaiting resolution.", card.ply
            )).into());
        }
        let earlier = prior
            .as_ref()
            .map(|(id, _)| id)
            .or_else(|| pending.get(&card.position).map(|(id, _)| id));
        if let Some(earlier) = earlier {
            if earlier != &card_id {
                return Err(OpeningConflict(format!(
                    "Move {}: this repertoire already has a different reply at this position.",
                    card.ply
                ))
                .into());
            }
        } else {
            pending.insert(card.position.clone(), (card_id, card));
            // One card and one prepared entry per position, plus the line and new choice.
            ensure!(
                !online || pending.len() * 2 + 1 + usize::from(new_choice) <= 128,
                OverBudget
            );
        }
    }
    for (card_id, card) in pending.into_values() {
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,repertoire_id,answer_version,context)
             VALUES (?1,'opening',?2,?3,?4,?5,?6,?7,?8,?9)",
            params![card_id,plan.name,plan.color,card.fen,card.position,card.answers,repertoire,card.version,card.context],
        )?;
    }
    db.execute(
        "INSERT OR IGNORE INTO openings VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            id,
            repertoire,
            plan.name,
            plan.color,
            plan.route,
            plan.start_at
        ],
    )?;
    Ok(id)
}

pub fn save_opening(
    db: &Connection,
    input: &OpeningInput,
    inherited_book: Option<&str>,
) -> Result<String> {
    apply_opening(db, &prepare_opening(input)?, inherited_book, false)
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

pub struct TacticPlan {
    name: String,
    route: String,
    id: String,
    fen: String,
    color: String,
    position: String,
    answers: String,
    source: Option<(String, usize, String)>,
}

pub fn prepare_tactic(input: TacticInput<'_>) -> Result<TacticPlan> {
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
    Ok(TacticPlan {
        name: name.to_owned(),
        route: encoded,
        id,
        fen: chess::fen(&board),
        color: chess::color_name(board.turn()).to_owned(),
        position: position_id,
        answers,
        source: game
            .zip(ply)
            .zip(played)
            .map(|((game, ply), played)| (game.to_owned(), ply, played.to_owned())),
    })
}

pub fn apply_tactic(db: &Connection, plan: &TacticPlan) -> Result<String> {
    let (game, ply, played) = plan
        .source
        .as_ref()
        .map_or((None, None, None), |(game, ply, played)| {
            (Some(game), Some(ply), Some(played))
        });
    db.execute(
        "INSERT OR IGNORE INTO tactics VALUES (?1,?2,?3,?4,?5)",
        params![plan.id, plan.name, plan.route, game, ply],
    )?;
    db.execute(
        "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,played_move,source_game_id,source_ply)
         VALUES (?1,'tactic',?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(id) DO NOTHING",
        params![
            plan.id,
            plan.name,
            plan.color,
            plan.fen,
            plan.position,
            plan.answers,
            played,
            game,
            ply
        ],
    )?;
    Ok(plan.id.clone())
}

pub fn answers_at(db: &Connection, position: &str, color: &str) -> Result<Vec<ReferenceAnswer>> {
    let mut q = db.prepare_cached("SELECT repertoire_id,answer_version,answers FROM active_cards WHERE position_id=?1 AND kind='opening' AND color=?2")?;
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
