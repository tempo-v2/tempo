use crate::{chess, queue_store};
use anyhow::{Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum Conflict {
    Dataset,
    Submission,
}

impl Conflict {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Dataset => "stale_dataset",
            Self::Submission => "answer_conflict",
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dataset => "The Study dataset changed.",
            Self::Submission => "This card already has a different accepted answer.",
        })
    }
}
impl std::error::Error for Conflict {}

#[derive(Deserialize)]
pub struct Next {
    pub after: Option<i64>,
    pub dataset: Option<String>,
}

#[derive(Deserialize)]
pub struct Answer {
    pub attempt: i64,
    pub dataset: String,
    pub played: Option<String>,
}

#[derive(Serialize)]
pub struct Review {
    pub attempt: i64,
    pub dataset: String,
    pub kind: String,
    pub title: String,
    pub color: String,
    pub fen: String,
    pub legal: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead_in: Option<chess::Route>,
    pub result: Option<AcceptedView>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub rating: String,
    pub repeat_after: Option<u8>,
    pub answers: Vec<String>,
    pub played_in_game: Option<String>,
}

// The receipt stays in its original format. Project the displayed move
// from the accepted submission, including for receipts from older releases.
#[derive(Serialize)]
pub struct AcceptedView {
    #[serde(flatten)]
    result: Accepted,
    display_move: Option<String>,
}

fn accepted_view(result: Accepted, fen: &str, played: Option<&str>) -> AcceptedView {
    let played = played
        .filter(|_| result.rating == "Good")
        .and_then(|played| {
            chess::position(fen)
                .ok()
                .and_then(|board| chess::parse_move(&board, played).ok())
        })
        .map(chess::uci)
        .filter(|played| result.answers.contains(played));
    let display_move = played.or_else(|| result.answers.first().cloned());
    AcceptedView {
        result,
        display_move,
    }
}

#[derive(Serialize)]
pub struct CardContext {
    pub game_id: Option<String>,
    pub ply: Option<usize>,
    pub played: Option<String>,
    pub lead_in: Option<chess::Route>,
}

pub fn current(db: &Connection) -> Result<Option<Review>> {
    let row = db
        .query_row(
            "SELECT a.id,c.kind,c.title,c.color,c.fen,a.result,a.submission,c.context
        FROM study s JOIN attempts a ON a.id=s.active JOIN cards c ON c.id=a.card_id",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(attempt, kind, title, color, fen, result, submission, context)| {
            let legal = chess::legal(&chess::position(&fen)?);
            let result = result
                .as_deref()
                .map(|saved| -> Result<_> {
                    let played = submission
                        .as_deref()
                        .map(serde_json::from_str::<Option<String>>)
                        .transpose()?
                        .flatten();
                    Ok(accepted_view(
                        serde_json::from_str(saved)?,
                        &fen,
                        played.as_deref(),
                    ))
                })
                .transpose()?;
            // Opening context contains only authored moves before this issued decision.
            let lead_in = if kind == "opening" {
                context.and_then(|route| serde_json::from_str(&route).ok())
            } else {
                None
            };
            Ok(Review {
                attempt,
                dataset: crate::db::dataset(db)?,
                kind,
                title,
                color,
                fen,
                legal,
                lead_in,
                result,
            })
        },
    )
    .transpose()
}

pub fn next(db: &mut Connection, input: Next) -> Result<Option<Review>> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current_dataset = crate::db::dataset(&tx)?;
    if input.after.is_some() && input.dataset.as_deref() != Some(&current_dataset) {
        return Err(Conflict::Dataset.into());
    }
    let (active, last): (Option<i64>, Option<String>) =
        tx.query_row("SELECT active,last_kind FROM study", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if let Some(active) = active {
        let accepted: bool = tx.query_row(
            "SELECT result IS NOT NULL FROM attempts WHERE id=?1",
            [active],
            |r| r.get(0),
        )?;
        if !accepted || input.after != Some(active) {
            return current(&tx);
        }
    } else if input.after.is_some() {
        return current(&tx);
    }
    let queued = queue_store::committed_head(&tx)?;
    let next = if queued.is_some() {
        queued
    } else {
        queue_store::select(&tx, last.as_deref(), Utc::now().timestamp())?
    };
    if let Some((id, kind)) = next {
        tx.execute("INSERT INTO attempts(card_id) VALUES (?1)", [&id])?;
        let attempt = tx.last_insert_rowid();
        tx.execute("DELETE FROM deck WHERE card_id=?1", [&id])?;
        tx.execute(
            "UPDATE study SET active=?1,last_kind=?2",
            params![attempt, kind],
        )?;
    } else {
        tx.execute("UPDATE study SET active=NULL", [])?;
    }
    let view = current(&tx)?;
    tx.commit()?;
    Ok(view)
}

pub struct IssuedAnswer {
    pub card: String,
    pub fen: String,
    pub answers: Vec<String>,
    pub played_in_game: Option<String>,
    payload: String,
}

pub enum AnswerState {
    Replayed(AcceptedView),
    Pending(IssuedAnswer),
}

pub fn begin_answer(db: &Connection, input: &Answer) -> Result<AnswerState> {
    if crate::db::dataset(db)? != input.dataset {
        return Err(Conflict::Dataset.into());
    }
    let (card, submission, accepted, fen, answers, played_in_game): (
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    ) = db.query_row(
        "SELECT a.card_id,a.submission,a.result,c.fen,c.answers,c.played_move
            FROM attempts a JOIN cards c ON c.id=a.card_id WHERE a.id=?1",
        [input.attempt],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    let payload = serde_json::to_string(&input.played)?;
    if let Some(saved) = accepted {
        if submission.as_deref() != Some(&payload) {
            return Err(Conflict::Submission.into());
        }
        return Ok(AnswerState::Replayed(accepted_view(
            serde_json::from_str(&saved)?,
            &fen,
            input.played.as_deref(),
        )));
    }
    let active: Option<i64> = db.query_row("SELECT active FROM study", [], |r| r.get(0))?;
    ensure!(active == Some(input.attempt), "the active card changed");
    let answers: Vec<String> = serde_json::from_str(&answers)?;
    Ok(AnswerState::Pending(IssuedAnswer {
        card,
        fen,
        answers,
        played_in_game,
        payload,
    }))
}

pub fn accept_answer(
    db: &Connection,
    input: &Answer,
    issued: IssuedAnswer,
    rating: &str,
    repeat_after: Option<u8>,
) -> Result<AcceptedView> {
    let result = Accepted {
        rating: rating.into(),
        repeat_after,
        answers: issued.answers,
        played_in_game: issued.played_in_game,
    };
    db.execute(
        "UPDATE attempts SET submission=?1,result=?2 WHERE id=?3",
        params![
            issued.payload,
            serde_json::to_string(&result)?,
            input.attempt
        ],
    )?;
    Ok(accepted_view(result, &issued.fen, input.played.as_deref()))
}

pub struct GameAnchor {
    pub game_id: String,
    pub ply: usize,
    pub fen: String,
}

/// Read only the source identity of the issued card. Its answer remains private.
pub fn game_anchor(db: &Connection, attempt: i64) -> Result<Option<GameAnchor>> {
    Ok(db
        .query_row(
            "SELECT c.source_game_id,c.source_ply,c.fen
         FROM attempts a JOIN cards c ON c.id=a.card_id
         WHERE a.id=?1 AND c.source_game_id IS NOT NULL AND c.source_ply>0",
            [attempt],
            |r| {
                Ok(GameAnchor {
                    game_id: r.get(0)?,
                    ply: r.get(1)?,
                    fen: r.get(2)?,
                })
            },
        )
        .optional()?)
}

type ContextRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<usize>,
);

pub struct ContextSource {
    pub position_id: String,
    pub repertoire_id: Option<String>,
    pub answer_version: Option<String>,
    pub context: CardContext,
}

pub fn context_source(db: &Connection, attempt: i64) -> Result<ContextSource> {
    let row: ContextRow = db.query_row(
        "SELECT c.position_id,c.repertoire_id,c.answer_version,c.context,c.source_game_id,c.source_ply
            FROM attempts a JOIN cards c ON c.id=a.card_id
            WHERE a.id=?1 AND a.result IS NOT NULL",
        [attempt],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    let (position_id, repertoire_id, answer_version, route, game_id, ply) = row;
    Ok(ContextSource {
        position_id,
        repertoire_id,
        answer_version,
        context: CardContext {
            game_id,
            ply,
            played: None,
            lead_in: route.as_deref().map(serde_json::from_str).transpose()?,
        },
    })
}
