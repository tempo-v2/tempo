use crate::{
    chess::{self, Route},
    db,
    learning::{self, Rating},
    review,
    study::{AnswerAction, Conflict, RepeatDisposition},
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct Start {
    dataset: String,
    revision: String,
    puzzle: String,
    request: String,
    practice: bool,
}
#[derive(Deserialize, Serialize)]
pub struct Move {
    pub dataset: String,
    pub play: i64,
    pub step: i64,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub played: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct Outcome {
    rating: Rating,
    repeat: RepeatDisposition,
    due_day: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct View {
    pub dataset: String,
    pub play: i64,
    pub puzzle: String,
    pub step: i64,
    pub mode: String,
    pub fen: String,
    pub color: String,
    pub legal: Vec<String>,
    pub last_move: Option<String>,
    pub result: Option<Outcome>,
    pub solution: Option<Route>,
}

#[derive(Serialize)]
pub struct MoveReply {
    #[serde(flatten)]
    pub view: View,
    before_fen: String,
}

struct Play {
    puzzle: String,
    version: String,
    mode: String,
    review: Option<i64>,
    ply: usize,
    result: Option<Outcome>,
    route: Route,
    final_move: Option<String>,
}

fn load(db: &Connection, id: i64) -> Result<Play> {
    let row = db
        .query_row(
            "SELECT p.puzzle,p.version,p.mode,p.review,p.ply,p.result,v.fen,v.moves,p.final_move
        FROM lichess_plays p JOIN lichess_versions v USING(puzzle,version) WHERE p.id=?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get(6)?,
                    r.get::<_, String>(7)?,
                    r.get(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(Conflict::Attempt)?;
    Ok(Play {
        puzzle: row.0,
        version: row.1,
        mode: row.2,
        review: row.3,
        ply: row.4,
        result: row
            .5
            .map(|value| serde_json::from_str(&value))
            .transpose()?,
        route: Route {
            fen: row.6,
            moves: serde_json::from_str(&row.7)?,
        },
        final_move: row.8,
    })
}
fn view(db: &Connection, id: i64) -> Result<View> {
    let Play {
        puzzle,
        mode,
        ply,
        result,
        mut route,
        final_move,
        ..
    } = load(db, id)?;
    if let Some(last) = final_move {
        route.moves[ply - 1] = last;
    }
    let board = chess::board_at(&route, ply)?;
    let color = chess::color_name(chess::position(&route.fen)?.turn()).to_owned();
    let finished = result.is_some();
    Ok(View {
        dataset: db::dataset(db)?,
        play: id,
        puzzle,
        step: ply as i64,
        mode,
        fen: chess::fen(&board),
        color,
        legal: if finished {
            vec![]
        } else {
            chess::legal(&board)
        },
        last_move: ply.checked_sub(1).map(|last| route.moves[last].clone()),
        result,
        solution: finished.then_some(route),
    })
}

#[derive(Serialize)]
pub struct Pending {
    play: i64,
    puzzle: String,
}
pub fn pending(db: &Connection, after: i64) -> Result<Vec<Pending>> {
    let mut statement=db.prepare("SELECT id,puzzle FROM lichess_plays WHERE mode='first' AND result IS NULL AND id>?1 ORDER BY id LIMIT 50")?;
    Ok(statement
        .query_map([after], |r| {
            Ok(Pending {
                play: r.get(0)?,
                puzzle: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn resume(db: &Connection, id: i64) -> Result<View> {
    view(db, id)
}
pub fn resume_tactics(db: &Connection, id: i64) -> Result<View> {
    check_surface(db, id, Surface::Tactics)?;
    view(db, id)
}
#[derive(Clone, Copy)]
pub enum Surface {
    Tactics,
    Study,
}
fn check_surface(db: &Connection, id: i64, surface: Surface) -> Result<()> {
    let mode: String = db
        .query_row("SELECT mode FROM lichess_plays WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?
        .ok_or(Conflict::Attempt)?;
    ensure!(
        (mode == "study") == matches!(surface, Surface::Study),
        Conflict::Attempt
    );
    Ok(())
}

pub fn start(db: &mut Connection, input: Start) -> Result<View> {
    ensure!(
        input.request.len() == 36
            && input
                .request
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "Invalid play request"
    );
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_dataset(&tx, &input.dataset)?;
    let prior: Option<(String, bool, i64, Option<String>)> = tx
        .query_row(
            "SELECT puzzle,practice,play,revision FROM lichess_starts WHERE request=?1",
            [&input.request],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((puzzle, practice, id, revision)) = prior {
        ensure!(
            puzzle == input.puzzle
                && practice == input.practice
                && revision.is_none_or(|revision| revision == input.revision),
            Conflict::Submission
        );
        return view(&tx, id);
    }
    let version:String=tx.query_row("SELECT m.version FROM lichess_members m CROSS JOIN lichess_catalog c WHERE m.puzzle=?1 AND c.artifact=?2 LIMIT 1",
        params![input.puzzle,input.revision],|r|r.get(0)).optional()?.ok_or(Conflict::PackChanged)?;
    let existing = if input.practice {
        None
    } else {
        tx.query_row(
            "SELECT id FROM lichess_plays WHERE puzzle=?1 AND mode='first'",
            [&input.puzzle],
            |r| r.get(0),
        )
        .optional()?
    };
    let id = if let Some(id) = existing {
        id
    } else {
        let learned: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM lichess_cards WHERE puzzle=?1)",
            [&input.puzzle],
            |r| r.get(0),
        )?;
        ensure!(learned == input.practice, Conflict::Attempt);
        let mode = if input.practice { "practice" } else { "first" };
        tx.execute(
            "INSERT INTO lichess_plays(puzzle,version,mode) VALUES (?1,?2,?3)",
            params![input.puzzle, version, mode],
        )?;
        tx.last_insert_rowid()
    };
    tx.execute(
        "INSERT INTO lichess_starts(request,puzzle,practice,play,revision) VALUES (?1,?2,?3,?4,?5)",
        params![
            input.request,
            input.puzzle,
            input.practice,
            id,
            input.revision
        ],
    )?;
    let reply = view(&tx, id)?;
    tx.commit()?;
    Ok(reply)
}

fn check_dataset(db: &Connection, dataset: &str) -> Result<()> {
    ensure!(db::dataset(db)? == dataset, Conflict::Dataset);
    Ok(())
}

pub fn for_review(db: &Connection, review: i64) -> Result<Option<i64>> {
    Ok(db
        .query_row(
            "SELECT id FROM lichess_plays WHERE review=?1",
            [review],
            |r| r.get(0),
        )
        .optional()?)
}
pub fn issue_review(db: &Connection, card: &str, review: i64) -> Result<()> {
    db.execute("INSERT INTO lichess_plays(puzzle,version,mode,review) SELECT puzzle,version,'study',?2 FROM lichess_cards WHERE card=?1",
        params![card,review])?;
    Ok(())
}

fn admit(
    db: &Connection,
    play: i64,
    puzzle: &str,
    version: &str,
    route: &Route,
) -> Result<(String, i64)> {
    let board = chess::position(&route.fen)?;
    let card = chess::id(&["lichess-puzzle-v1", puzzle]);
    let title = format!("Lichess puzzle {puzzle}");
    db.execute(
        "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
        VALUES (?1,'tactic',?2,?3,?4,?5,?6)",
        params![
            card,
            title,
            chess::color_name(board.turn()),
            route.fen,
            chess::position_id(&board),
            serde_json::to_string(&route.moves[..1])?
        ],
    )?;
    db.execute(
        "INSERT INTO lichess_cards VALUES (?1,?2,?3)",
        params![puzzle, version, card],
    )?;
    db.execute("INSERT INTO attempts(card_id) VALUES (?1)", [&card])?;
    let review = db.last_insert_rowid();
    db.execute(
        "UPDATE lichess_plays SET review=?2 WHERE id=?1",
        params![play, review],
    )?;
    Ok((card, review))
}

pub fn play(db: &mut Connection, input: Move, surface: Surface) -> Result<MoveReply> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_dataset(&tx, &input.dataset)?;
    check_surface(&tx, input.play, surface)?;
    let action = input
        .played
        .map_or(AnswerAction::Reveal, AnswerAction::Play);
    let encoded = serde_json::to_string(&action)?;
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT action,reply FROM lichess_moves WHERE play=?1 AND step=?2",
            params![input.play, input.step],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((action, reply)) = saved {
        ensure!(action == encoded, Conflict::Submission);
        let route = load(&tx, input.play)?.route;
        let before = chess::board_at(&route, input.step.try_into()?)?;
        return Ok(MoveReply {
            view: serde_json::from_str(&reply)?,
            before_fen: chess::fen(&before),
        });
    }
    let current = load(&tx, input.play)?;
    ensure!(
        current.result.is_none() && current.ply as i64 == input.step,
        Conflict::Attempt
    );
    if current.mode == "study" {
        let active: Option<i64> = tx.query_row("SELECT active FROM study", [], |r| r.get(0))?;
        ensure!(current.review == active, Conflict::Attempt);
    }
    let before = chess::board_at(&current.route, current.ply)?;
    let before_fen = chess::fen(&before);
    let (ply, rating, final_move) =
        match chess::score_puzzle_move(&current.route, current.ply, action.played())? {
            chess::PuzzleMove::Illegal => return Err(Conflict::IllegalMove.into()),
            chess::PuzzleMove::Again => (current.ply, Some(Rating::Again), None),
            chess::PuzzleMove::Continue { ply } => (ply, None, None),
            chess::PuzzleMove::Solved { ply, alternate } => (ply, Some(Rating::Good), alternate),
        };
    let mut reviewed = None;
    let result = if let Some(rating) = rating {
        let mut outcome = Outcome {
            rating,
            repeat: RepeatDisposition::None,
            due_day: None,
        };
        if current.mode != "practice" {
            let (card, attempt) = if current.mode == "first" {
                admit(
                    &tx,
                    input.play,
                    &current.puzzle,
                    &current.version,
                    &current.route,
                )?
            } else {
                let id = current.review.context("Missing Study attempt")?;
                (
                    tx.query_row("SELECT card_id FROM attempts WHERE id=?1", [id], |r| {
                        r.get(0)
                    })?,
                    id,
                )
            };
            let now = Utc::now().timestamp();
            let first_due = if current.mode == "first" && matches!(rating, Rating::Good) {
                Some(learning::first_pack_due(now, learning::draw_pack_days())?)
            } else {
                None
            };
            let previous: Option<String> = if matches!(rating, Rating::Again) {
                tx.query_row("SELECT last_kind FROM study", [], |r| r.get(0))?
            } else {
                None
            };
            let (repeat, day) = review::record_rating(
                &tx,
                &card,
                attempt,
                previous.as_deref(),
                rating,
                now,
                first_due,
            )?;
            outcome.repeat = repeat;
            outcome.due_day = day;
            reviewed = Some(attempt);
        }
        Some(serde_json::to_string(&outcome)?)
    } else {
        None
    };
    tx.execute(
        "UPDATE lichess_plays SET ply=?2,result=?3,final_move=?4 WHERE id=?1",
        params![input.play, ply, result, final_move],
    )?;
    let reply = view(&tx, input.play)?;
    let saved = serde_json::to_string(&reply)?;
    if let Some(attempt) = reviewed {
        tx.execute(
            "UPDATE attempts SET result=?2 WHERE id=?1",
            params![attempt, saved],
        )?;
    }
    tx.execute(
        "INSERT INTO lichess_moves VALUES (?1,?2,?3,?4)",
        params![input.play, input.step, encoded, saved],
    )?;
    tx.commit()?;
    Ok(MoveReply {
        view: reply,
        before_fen,
    })
}
