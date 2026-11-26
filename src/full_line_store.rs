//! Addressed Store operations over the retained schema-18 full-line tables.
use crate::{
    chess::{self, Route},
    db,
    full_line::{InitialCard, Move, MoveReply, Outcome, PublishedExercise, Step, Surface, View},
    study::Conflict,
};
use anyhow::{Result, ensure};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

pub struct Play {
    pub puzzle: String,
    pub version: String,
    pub mode: String,
    pub review: Option<i64>,
    pub ply: usize,
    pub result: Option<Outcome>,
    pub route: Route,
    final_move: Option<String>,
}

pub fn load(db: &Connection, id: i64) -> Result<Play> {
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
pub fn check_surface(db: &Connection, id: i64, surface: Surface) -> Result<()> {
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

pub fn check_dataset(db: &Connection, dataset: &str) -> Result<()> {
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

pub fn replay_start(
    db: &Transaction<'_>,
    request: &str,
    puzzle_key: &str,
    practice_requested: bool,
    catalog_revision: &str,
) -> Result<Option<View>> {
    let prior: Option<(String, bool, i64, Option<String>)> = db
        .query_row(
            "SELECT puzzle,practice,play,revision FROM lichess_starts WHERE request=?1",
            [request],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((puzzle, practice, id, revision)) = prior {
        ensure!(
            puzzle == puzzle_key
                && practice == practice_requested
                && revision.is_none_or(|revision| revision == catalog_revision),
            Conflict::Submission
        );
        return view(db, id).map(Some);
    }
    Ok(None)
}

pub fn start_published(
    db: &Transaction<'_>,
    request: &str,
    puzzle: &str,
    version: &str,
    practice: bool,
    revision: &str,
) -> Result<View> {
    let existing = if practice {
        None
    } else {
        db.query_row(
            "SELECT id FROM lichess_plays WHERE puzzle=?1 AND mode='first'",
            [puzzle],
            |r| r.get(0),
        )
        .optional()?
    };
    let id = if let Some(id) = existing {
        id
    } else {
        let learned: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM lichess_cards WHERE puzzle=?1)",
            [puzzle],
            |r| r.get(0),
        )?;
        ensure!(learned == practice, Conflict::Attempt);
        let mode = if practice { "practice" } else { "first" };
        db.execute(
            "INSERT INTO lichess_plays(puzzle,version,mode) VALUES (?1,?2,?3)",
            params![puzzle, version, mode],
        )?;
        db.last_insert_rowid()
    };
    db.execute(
        "INSERT INTO lichess_starts(request,puzzle,practice,play,revision) VALUES (?1,?2,?3,?4,?5)",
        params![request, puzzle, practice, id, revision],
    )?;
    view(db, id)
}

/// Publish once inside the catalog import transaction. Issued rows retain this version.
pub fn publish(db: &Transaction<'_>, exercise: &PublishedExercise) -> Result<()> {
    let moves = serde_json::to_string(&exercise.route().moves)?;
    let inserted = db.execute(
        "INSERT INTO lichess_versions(puzzle,version,fen,moves) VALUES (?1,?2,?3,?4)
         ON CONFLICT(puzzle,version) DO NOTHING",
        params![
            exercise.key(),
            exercise.version(),
            exercise.route().fen,
            moves
        ],
    )?;
    if inserted == 0 {
        let same: bool = db.query_row(
            "SELECT fen=?3 AND moves=?4 FROM lichess_versions WHERE puzzle=?1 AND version=?2",
            params![
                exercise.key(),
                exercise.version(),
                exercise.route().fen,
                moves
            ],
            |r| r.get(0),
        )?;
        ensure!(same, "Conflicting published full-line version");
    }
    Ok(())
}

pub fn bind_card(db: &Connection, puzzle: &str, version: &str, card: &str) -> Result<()> {
    db.execute(
        "INSERT INTO lichess_cards VALUES (?1,?2,?3)",
        params![puzzle, version, card],
    )?;
    Ok(())
}

pub fn admit(
    db: &Connection,
    play: i64,
    issued: &Play,
    initial: InitialCard,
) -> Result<(String, i64)> {
    let route = &issued.route;
    let board = chess::position(&route.fen)?;
    let InitialCard { id: card, title } = initial;
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
    bind_card(db, &issued.puzzle, &issued.version, &card)?;
    db.execute("INSERT INTO attempts(card_id) VALUES (?1)", [&card])?;
    let review = db.last_insert_rowid();
    db.execute(
        "UPDATE lichess_plays SET review=?2 WHERE id=?1",
        params![play, review],
    )?;
    Ok((card, review))
}

pub fn replay_move(
    db: &Connection,
    play: i64,
    step: i64,
    action: &str,
) -> Result<Option<MoveReply>> {
    let saved: Option<(String, String)> = db
        .query_row(
            "SELECT action,reply FROM lichess_moves WHERE play=?1 AND step=?2",
            params![play, step],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((prior, reply)) = saved {
        ensure!(prior == action, Conflict::Submission);
        let route = load(db, play)?.route;
        let before = chess::board_at(&route, step.try_into()?)?;
        return Ok(Some(MoveReply {
            view: serde_json::from_str(&reply)?,
            before_fen: chess::fen(&before),
        }));
    }
    Ok(None)
}

pub fn reviewed_card(db: &Connection, attempt: i64) -> Result<String> {
    Ok(
        db.query_row("SELECT card_id FROM attempts WHERE id=?1", [attempt], |r| {
            r.get(0)
        })?,
    )
}

pub fn commit_move(
    db: &Transaction<'_>,
    input: &Move,
    action: &str,
    step: Step,
    result: Option<Outcome>,
    reviewed: Option<i64>,
) -> Result<View> {
    let result = result
        .map(|value| serde_json::to_string(&value))
        .transpose()?;
    db.execute(
        "UPDATE lichess_plays SET ply=?2,result=?3,final_move=?4 WHERE id=?1",
        params![input.play, step.ply, result, step.final_move],
    )?;
    let reply = view(db, input.play)?;
    let saved = serde_json::to_string(&reply)?;
    if let Some(attempt) = reviewed {
        db.execute(
            "UPDATE attempts SET result=?2 WHERE id=?1",
            params![attempt, saved],
        )?;
    }
    db.execute(
        "INSERT INTO lichess_moves VALUES (?1,?2,?3,?4)",
        params![input.play, input.step, action, saved],
    )?;
    Ok(reply)
}
