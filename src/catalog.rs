use crate::{
    chess::{self, Route},
    scanner::ReferenceAnswer,
};
use anyhow::{Context, Result, ensure};
use pgn_reader::shakmaty::{Chess, Position};
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
    pub repertoire_example: String,
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
        "SELECT o.id,o.repertoire_id,r.example_line,o.name,o.color,o.route,o.start_at
         FROM authored_openings o CROSS JOIN repertoire_choices r
         ON r.repertoire_id=o.repertoire_id AND r.color=o.color
         WHERE o.id>?1 ORDER BY o.id LIMIT 100",
    )?;
    let rows = q.query_map([after], |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get::<_, String>(5)?,
            r.get(6)?,
        ))
    })?;
    rows.map(|r| {
        let (id, repertoire_id, repertoire_example, name, color, route, start_at) = r?;
        Ok(Opening {
            id,
            repertoire_id,
            repertoire_example,
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

pub fn repertoire(db: &Connection, id: &str, color: &str) -> Result<Option<Repertoire>> {
    Ok(db
        .query_row(
            "SELECT example_line FROM repertoire_choices WHERE repertoire_id=?1 AND color=?2",
            (id, color),
            |row| {
                Ok(Repertoire {
                    id: id.into(),
                    color: color.into(),
                    example_line: row.get(0)?,
                })
            },
        )
        .optional()?)
}

pub struct CurrentOpeningReply {
    pub id: String,
    pub version: String,
    pub quarantined: bool,
    pub answers: String,
}

/// Return the current reply, including an unresolved reply when no active one exists.
pub fn current_opening_reply(
    db: &Connection,
    repertoire: &str,
    position: &str,
) -> Result<Option<CurrentOpeningReply>> {
    let mut query = db.prepare_cached(
        "SELECT id,answer_version,quarantined,answers FROM cards
         WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 AND opening_retired=0
         ORDER BY quarantined LIMIT 1",
    )?;
    Ok(query
        .query_row((repertoire, position), |row| {
            Ok(CurrentOpeningReply {
                id: row.get(0)?,
                version: row.get(1)?,
                quarantined: row.get(2)?,
                answers: row.get(3)?,
            })
        })
        .optional()?)
}

/// The route recorded when this reply was created. Its current authorship
/// must be established separately.
pub fn recorded_opening_context(db: &Connection, id: &str) -> Result<Option<Route>> {
    let text: Option<String> = db.query_row(
        "SELECT context FROM cards WHERE id=?1 AND kind='opening'",
        [id],
        |row| row.get(0),
    )?;
    Ok(text.and_then(|source| serde_json::from_str(&source).ok()))
}

pub struct ReplyContexts {
    pub routes: std::collections::BTreeMap<String, Route>,
    pub limited: bool,
}

/// Current authored routes for the requested reply versions in this repertoire.
/// A missing route is conclusive only when the bounded search was exhausted.
pub fn reply_contexts(
    db: &Connection,
    repertoire: &str,
    color: &str,
    mut wanted: std::collections::BTreeMap<String, String>,
) -> Result<ReplyContexts> {
    const LINE_LIMIT: usize = 2000;
    let mut found = ReplyContexts {
        routes: std::collections::BTreeMap::new(),
        limited: false,
    };
    if wanted.is_empty() {
        return Ok(found);
    }
    let mut query = db.prepare(
        "SELECT route,start_at FROM authored_openings
         WHERE repertoire_id=?1 AND color=?2 ORDER BY id LIMIT ?3",
    )?;
    let mut rows = query.query((repertoire, color, LINE_LIMIT + 1))?;
    let mut moves_left = 100_000usize;
    let mut checked = 0;
    while let Some(row) = rows.next()? {
        if checked == LINE_LIMIT {
            found.limited = true;
            break;
        }
        checked += 1;
        let route: Route = serde_json::from_str(&row.get::<_, String>(0)?)?;
        let start_at: usize = row.get(1)?;
        let Some(remaining) = moves_left.checked_sub(route.moves.len()) else {
            found.limited = true;
            break;
        };
        moves_left = remaining;
        let mut board = chess::position(&route.fen)?;
        for (ply, played) in route.moves.iter().enumerate() {
            let movement = chess::parse_move(&board, played)?;
            if ply >= start_at && chess::color_name(board.turn()) == color {
                let position = chess::position_id(&board);
                if wanted.get(&position).is_some_and(|version| {
                    version == &chess::id(&["answer-v1", &chess::uci(movement)])
                }) {
                    wanted.remove(&position);
                    found.routes.insert(
                        position,
                        Route {
                            fen: route.fen.clone(),
                            moves: route.moves[..ply].to_vec(),
                        },
                    );
                    if wanted.is_empty() {
                        return Ok(found);
                    }
                }
            }
            board.play_unchecked(movement);
        }
    }
    Ok(found)
}

pub fn authored_reply_revision(c: &Connection, rep: &str) -> Result<i64> {
    Ok(c.query_row(
        "SELECT COALESCE((SELECT revision FROM coverage_revision WHERE repertoire=?1),0)",
        [rep],
        |r| r.get(0),
    )?)
}

#[derive(Debug)]
pub struct MissingRepertoire;

impl std::fmt::Display for MissingRepertoire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The selected repertoire is unavailable for this practice color.")
    }
}

impl std::error::Error for MissingRepertoire {}

pub fn existing_repertoire(db: &Connection, id: &str, color: &str) -> Result<()> {
    ensure!(
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM repertoire_choices WHERE repertoire_id=?1 AND color=?2)",
            params![id, color],
            |row| row.get::<_, bool>(0),
        )?,
        MissingRepertoire
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
            "SELECT a.repertoire_id,a.color FROM openings o
             CROSS JOIN authored_openings a ON a.id=coalesce(o.alias_of,o.id)
               AND a.repertoire_id=o.repertoire_id AND a.color=o.color
             WHERE o.id=?1 AND o.archived=0",
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
    line_key: String,
    start_at: usize,
    pub(crate) cards: Vec<OpeningCard>,
}

pub(crate) struct OpeningCard {
    ply: std::num::NonZeroU32,
    pub(crate) position: String,
    pub(crate) version: String,
    pub(crate) fen: String,
    pub(crate) answers: String,
    pub(crate) context: String,
}

fn canonical_opening_route(
    input: &Route,
    start_at: usize,
    mut visit: impl FnMut(&Chess, usize, &Route, &str) -> Result<()>,
) -> Result<Route> {
    ensure!(
        start_at < input.moves.len() && input.moves.len() <= chess::MAX_PLIES,
        "enter a continuation"
    );
    let mut board = chess::position(&input.fen)?;
    let mut route = Route {
        fen: chess::fen(&board),
        moves: Vec::with_capacity(input.moves.len()),
    };
    for (ply, played) in input.moves.iter().enumerate() {
        let movement = chess::parse_move(&board, played)?;
        let normalized = chess::uci(movement);
        visit(&board, ply, &route, &normalized)?;
        route.moves.push(normalized);
        board.play_unchecked(movement);
    }
    Ok(route)
}

fn opening_line_key(color: &str, route: &str, start_at: usize) -> String {
    chess::id(&["authored-opening-v2", color, route, &start_at.to_string()])
}

pub fn prepare_opening(input: &OpeningInput) -> Result<OpeningPlan> {
    let name = name(&input.name)?.to_owned();
    ensure!(
        matches!(input.color.as_str(), "white" | "black"),
        "select a practice color"
    );
    let mut cards = Vec::new();
    let route = canonical_opening_route(
        &input.route,
        input.start_at,
        |board, ply, prefix, normalized| {
            if ply >= input.start_at && chess::color_name(board.turn()) == input.color {
                cards.push(OpeningCard {
                    ply: board.fullmoves(),
                    position: chess::position_id(board),
                    version: chess::id(&["answer-v1", normalized]),
                    fen: chess::fen(board),
                    answers: serde_json::to_string(&[normalized])?,
                    context: serde_json::to_string(prefix)?,
                });
            }
            Ok(())
        },
    )?;
    ensure!(
        !cards.is_empty(),
        "the continuation needs a move for the selected color"
    );
    let moves = serde_json::to_string(&route.moves)?;
    let fen = route.fen.clone();
    let route = serde_json::to_string(&route)?;
    Ok(OpeningPlan {
        name,
        color: input.color.clone(),
        fen,
        moves,
        line_key: opening_line_key(&input.color, &route, input.start_at),
        route,
        start_at: input.start_at,
        cards,
    })
}

/// Populate derived identity and an alias for each non-authoritative retained row.
/// The caller holds the stopped database's exclusive lock and a migration transaction.
pub fn migrate_opening_identities(db: &Connection) -> Result<()> {
    db.execute_batch(include_str!("opening_identity_columns.sql"))?;
    let mut scan = db.prepare("SELECT id,color,route,start_at FROM openings ORDER BY rowid")?;
    let mut rows = scan.query([])?;
    let mut update =
        db.prepare("UPDATE openings SET line_key=?1,canonical_route=?2 WHERE id=?3")?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let color: String = row.get(1)?;
        let start_at: usize = row.get(3)?;
        let input: Route = serde_json::from_str(row.get_ref(2)?.as_str()?)
            .with_context(|| format!("Read retained opening {id}"))?;
        let canonical = canonical_opening_route(&input, start_at, |_, _, _, _| Ok(()))
            .with_context(|| format!("Canonicalize retained opening {id}"))?;
        let route = serde_json::to_string(&canonical)?;
        update.execute((opening_line_key(&color, &route, start_at), route, id))?;
    }
    let duplicate_roots: i64 = db.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM openings WHERE id=repertoire_id
          GROUP BY name,color,line_key HAVING count(*)>1)",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        duplicate_roots == 0,
        "Opening migration found {duplicate_roots} equivalent root groups across separate repertoires"
    );
    db.execute_batch(include_str!("opening_identity.sql"))?;
    Ok(())
}

// A batch can publish at most 40 learner responses of one prepared route.
pub enum OpeningBatch {
    More(usize),
    Finished(String),
}

pub fn apply_opening_batch(
    db: &Connection,
    plan: &OpeningPlan,
    repertoire: &str,
    offset: usize,
) -> Result<OpeningBatch> {
    ensure!(
        offset < plan.cards.len(),
        "Invalid opening batch checkpoint"
    );
    let end = (offset + 40).min(plan.cards.len());
    let next = (end < plan.cards.len()).then_some(end);
    let id = apply_opening_cards(
        db,
        plan,
        Some(repertoire),
        true,
        &plan.cards[offset..end],
        next.is_none(),
    )?;
    Ok(match next {
        Some(next) => OpeningBatch::More(next),
        None => OpeningBatch::Finished(id),
    })
}

pub fn apply_opening(
    db: &Connection,
    plan: &OpeningPlan,
    inherited_book: Option<&str>,
    online: bool,
) -> Result<String> {
    apply_opening_cards(db, plan, inherited_book, online, &plan.cards, true)
}

/// Schema 17's hash is retained for completed Gaps batch prefixes. PGN
/// parsing already emitted canonical UCI for every one of those input routes.
pub fn legacy_opening_id(plan: &OpeningPlan, inherited_book: Option<&str>) -> String {
    chess::id(&[
        "opening-v1",
        inherited_book.unwrap_or(""),
        &plan.name,
        &plan.color,
        &plan.fen,
        &plan.moves,
        &plan.start_at.to_string(),
    ])
}

fn opening_destination(
    db: &Connection,
    plan: &OpeningPlan,
    inherited_book: Option<&str>,
) -> Result<(String, String)> {
    let repertoire = if let Some(repertoire) = inherited_book {
        repertoire.to_owned()
    } else {
        // Explicit New accepts the proposed repertoire label. Equivalent
        // delivery reuses the same root across move spellings.
        let root: Option<String> = db
            .query_row(
                "SELECT id FROM openings
             WHERE id=repertoire_id AND name=?1 AND color=?2 AND line_key=?3",
                (&plan.name, &plan.color, &plan.line_key),
                |row| row.get(0),
            )
            .optional()?;
        let Some(root) = root else {
            let id = legacy_opening_id(plan, None);
            return Ok((id.clone(), id));
        };
        root
    };
    let id: Option<String> = db
        .query_row(
            "SELECT id FROM openings
         WHERE repertoire_id=?1 AND color=?2 AND line_key=?3 AND alias_of IS NULL",
            (&repertoire, &plan.color, &plan.line_key),
            |row| row.get(0),
        )
        .optional()?;
    let id =
        id.unwrap_or_else(|| chess::id(&["opening-in-repertoire-v2", &repertoire, &plan.line_key]));
    Ok((id, repertoire))
}

fn apply_opening_cards(
    db: &Connection,
    plan: &OpeningPlan,
    inherited_book: Option<&str>,
    online: bool,
    cards: &[OpeningCard],
    finish_line: bool,
) -> Result<String> {
    let (id, repertoire) = opening_destination(db, plan, inherited_book)?;
    let new_choice = inherited_book.is_none()
        && !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM repertoire_choices WHERE repertoire_id=?1 AND color=?2)",
            (&repertoire, &plan.color),
            |row| row.get::<_, bool>(0),
        )?;
    let mut pending = std::collections::HashMap::new();
    for card in cards {
        let card_id = chess::id(&[
            "opening-card-v1",
            &repertoire,
            &card.position,
            &card.version,
        ]);
        let prior = db.query_row(
            "SELECT id,quarantined FROM cards WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 AND opening_retired=0
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
            // Each new card writes its prepared entry and repertoire revision.
            ensure!(
                !online || pending.len() * 3 + 2 + usize::from(new_choice) <= 128,
                OverBudget
            );
        }
    }
    for (card_id, card) in pending.into_values() {
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers,repertoire_id,answer_version,context)
             VALUES (?1,'opening',?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET opening_retired=0 WHERE cards.opening_retired=1",
            params![card_id,plan.name,plan.color,card.fen,card.position,card.answers,repertoire,card.version,card.context],
        )?;
    }
    if finish_line {
        db.execute(
            "INSERT INTO openings(id,repertoire_id,name,color,route,start_at,line_key,canonical_route)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?5)
             ON CONFLICT(id) DO UPDATE SET archived=0 WHERE openings.archived=1",
            params![
                id,
                repertoire,
                plan.name,
                plan.color,
                plan.route,
                plan.start_at,
                plan.line_key,
            ],
        )?;
    }
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
        "SELECT id,name,route,game_id,game_ply FROM tactics
         WHERE id>?1 AND retired=0 ORDER BY id LIMIT 100",
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
        "INSERT OR IGNORE INTO tactics(id,name,route,game_id,game_ply) VALUES (?1,?2,?3,?4,?5)",
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

/// Apply an already qualified answer or retire the game occurrence. The caller
/// commits this with its qualification receipt under the same writer transaction.
pub fn qualify_game_tactic(
    db: &Connection,
    game: &str,
    ply: usize,
    fen: &str,
    played: &str,
    answer: Option<&str>,
) -> Result<&'static str> {
    let existing = db
        .query_row(
            "SELECT t.id,c.fen,c.played_move,c.answers FROM tactics t JOIN cards c ON c.id=t.id
         WHERE t.game_id=?1 AND t.game_ply=?2",
            params![game, ply],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((id, saved_fen, saved_played, saved_answers)) = existing else {
        if let Some(answer) = answer {
            let route = Route {
                fen: fen.to_owned(),
                moves: vec![answer.to_owned()],
            };
            let plan = prepare_tactic(TacticInput {
                name: &format!("Missed capture · move {}", ply / 2 + 1),
                route: &route,
                answers: &route.moves,
                source: Some((game, ply, played)),
            })?;
            apply_tactic(db, &plan)?;
            return Ok("published");
        }
        return Ok("absent");
    };
    ensure!(
        fen == saved_fen && played == saved_played,
        "Published tactic position changed"
    );
    let Some(answer) = answer else {
        db.execute("UPDATE tactics SET retired=1 WHERE id=?1", [&id])?;
        db.execute("DELETE FROM deck WHERE card_id=?1", [&id])?;
        db.execute("DELETE FROM repeat_obligation WHERE card_id=?1", [&id])?;
        return Ok("removed");
    };
    let original: Vec<String> = serde_json::from_str(&saved_answers)?;
    ensure!(
        original.iter().any(|move_| move_ == answer),
        "Published tactic candidates changed"
    );
    if original.len() == 1 {
        return Ok("kept");
    }
    db.execute(
        "UPDATE attempts SET issued_answers=?2
         WHERE id=(SELECT active FROM study) AND card_id=?1 AND result IS NULL AND issued_answers IS NULL",
        params![id,saved_answers],
    )?;
    let route = Route {
        fen: saved_fen,
        moves: vec![answer.to_owned()],
    };
    db.execute(
        "UPDATE tactics SET route=?2 WHERE id=?1",
        params![id, serde_json::to_string(&route)?],
    )?;
    db.execute(
        "UPDATE cards SET answers=?2 WHERE id=?1",
        params![id, serde_json::to_string(&route.moves)?],
    )?;
    Ok("narrowed")
}

pub fn visit_answers_at(
    db: &Connection,
    position: &str,
    color: &str,
    mut visit: impl FnMut(ReferenceAnswer) -> Result<()>,
) -> Result<()> {
    let mut q = db.prepare_cached("SELECT repertoire_id,answer_version,answers FROM active_cards WHERE position_id=?1 AND kind='opening' AND color=?2")?;
    let mut rows = q.query(params![position, color])?;
    while let Some(row) = rows.next()? {
        visit(ReferenceAnswer {
            repertoire_id: row.get(0)?,
            answer_version: row.get(1)?,
            moves: serde_json::from_str(row.get_ref(2)?.as_str()?)?,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_replay_uses_canonical_moves_and_preserves_the_saved_title() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let db = crate::db::Database::init(&directory.path().join("openings"))?;
        let opening = |name: &str, castle: &str, start_at| OpeningInput {
            name: name.into(),
            color: "white".into(),
            route: Route {
                fen: "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1".into(),
                moves: vec![castle.into(), "e8c8".into(), "f1c1".into()],
            },
            start_at,
        };
        let id = save_opening(&db, &opening("First", "e1g1", 0), None)?;
        let writes = db.total_changes();
        assert_eq!(save_opening(&db, &opening("First", "e1h1", 0), None)?, id);
        assert_eq!(
            save_opening(&db, &opening("Later title", "e1h1", 0), Some(&id))?,
            id
        );
        assert_eq!(db.total_changes(), writes);
        let saved = openings(&db, "")?;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].name, "First");
        assert_eq!(saved[0].route.moves[0], "e1g1");

        let other_line = save_opening(&db, &opening("From ply 2", "e1h1", 2), Some(&id))?;
        assert_ne!(other_line, id);
        let independent = save_opening(&db, &opening("A separate repertoire", "e1h1", 0), None)?;
        assert_ne!(independent, id);
        assert_eq!(branch_repertoire(&db, &other_line, "white")?, id);
        assert_eq!(branch_repertoire(&db, &independent, "white")?, independent);
        Ok(())
    }

    #[test]
    fn tactics_pages_skip_retired_ids_using_the_filtered_index() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let db = crate::db::Database::init(&directory.path().join("tactics"))?;
        let route = serde_json::to_string(&Route {
            fen: "".into(),
            moves: vec!["e2e4".into()],
        })?;
        db.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<10000)
             INSERT INTO tactics(id,name,route,retired) SELECT printf('a%05d',i),'Old',?1,1 FROM n",
            [&route],
        )?;
        db.execute(
            "INSERT INTO tactics(id,name,route) VALUES ('b','New',?1)",
            [&route],
        )?;
        db.progress_handler(256, Some(|| true));
        let page = tactics(&db, "")?;
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, "b");
        assert!(tactics(&db, "b")?.is_empty());
        Ok(())
    }
}
