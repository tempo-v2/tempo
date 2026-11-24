use crate::{chess, games};
use anyhow::{Result, ensure};
use chrono::{DateTime, Local, Utc};
use fsrs::{FSRS, MemoryState};
use rand::Rng;
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
            "SELECT a.id,c.kind,c.title,c.color,c.fen,a.result,a.submission
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
                ))
            },
        )
        .optional()?;
    row.map(|(attempt, kind, title, color, fen, result, submission)| {
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
        Ok(Review {
            attempt,
            dataset: crate::db::dataset(db)?,
            kind,
            title,
            color,
            fen,
            legal,
            result,
        })
    })
    .transpose()
}

fn select(db: &Connection, previous: Option<&str>, now: i64) -> Result<Option<(String, String)>> {
    let kinds = if previous == Some("opening") {
        ["tactic", "opening"]
    } else {
        ["opening", "tactic"]
    };
    let due = "SELECT l.card_id FROM learning l JOIN cards c ON c.id=l.card_id
        WHERE l.due_at<=?1 AND c.kind=?2 AND NOT EXISTS (SELECT 1 FROM deck d WHERE d.card_id=l.card_id)
        ORDER BY l.due_at,l.card_id LIMIT 1";
    let unseen = "SELECT c.id FROM cards c WHERE c.kind=?1 AND NOT EXISTS (SELECT 1 FROM learning l WHERE l.card_id=c.id)
        AND NOT EXISTS (SELECT 1 FROM deck d WHERE d.card_id=c.id) ORDER BY c.id LIMIT 1";
    for kind in kinds {
        if let Some(card) = db
            .query_row(due, params![now, kind], |r| r.get(0))
            .optional()?
        {
            return Ok(Some((card, kind.to_owned())));
        }
    }
    for kind in kinds {
        if let Some(card) = db.query_row(unseen, [kind], |r| r.get(0)).optional()? {
            return Ok(Some((card, kind.to_owned())));
        }
    }
    Ok(None)
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
    let queued = tx.query_row("SELECT d.card_id,c.kind FROM deck d JOIN cards c ON c.id=d.card_id ORDER BY d.slot LIMIT 1",
        [],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
    let next = if queued.is_some() {
        queued
    } else {
        select(&tx, last.as_deref(), Utc::now().timestamp())?
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

fn place_repeat(db: &Connection, card: &str, now: i64, draw: u8) -> Result<u8> {
    let mut q = db.prepare(
        "SELECT d.card_id,c.kind FROM deck d JOIN cards c ON c.id=d.card_id ORDER BY d.slot",
    )?;
    let mut queued = q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut previous = if let Some((_, kind)) = queued.last() {
        Some(kind.clone())
    } else {
        db.query_row("SELECT last_kind FROM study", [], |r| r.get(0))?
    };
    while queued.len() < usize::from(draw) {
        let Some((id, kind)) = select(db, previous.as_deref(), now)? else {
            break;
        };
        db.execute(
            "INSERT INTO deck SELECT coalesce(max(slot),0)+1,?1 FROM deck",
            [&id],
        )?;
        previous = Some(kind.clone());
        queued.push((id, kind));
    }
    let offset = repeat_after(draw, queued.len() as u8);
    db.execute("DELETE FROM deck", [])?;
    let mut ids = queued.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
    ids.insert(usize::from(offset), card.to_string());
    for (slot, id) in ids.into_iter().enumerate() {
        db.execute("INSERT INTO deck VALUES (?1,?2)", params![slot, id])?;
    }
    Ok(offset)
}

fn repeat_after(draw: u8, other_cards: u8) -> u8 {
    draw.min(other_cards)
}

pub fn answer(db: &mut Connection, input: Answer) -> Result<AcceptedView> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if crate::db::dataset(&tx)? != input.dataset {
        return Err(Conflict::Dataset.into());
    }
    let (card, submission, accepted, fen, answers, played_in_game): (
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    ) = tx.query_row(
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
        return Ok(accepted_view(
            serde_json::from_str(&saved)?,
            &fen,
            input.played.as_deref(),
        ));
    }
    let active: Option<i64> = tx.query_row("SELECT active FROM study", [], |r| r.get(0))?;
    ensure!(active == Some(input.attempt), "the active card changed");
    let answers: Vec<String> = serde_json::from_str(&answers)?;
    let good = if let Some(played) = input.played.as_deref() {
        let board = chess::position(&fen)?;
        let played = chess::uci(chess::parse_move(&board, played)?);
        answers.contains(&played)
    } else {
        false
    };
    let now = Utc::now().timestamp();
    let prior = tx
        .query_row(
            "SELECT stability,difficulty,last_at FROM learning WHERE card_id=?1",
            [&card],
            |r| {
                Ok((
                    MemoryState {
                        stability: r.get(0)?,
                        difficulty: r.get(1)?,
                    },
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    let elapsed = prior.map_or(0, |(_, last)| {
        let time = |t| {
            DateTime::from_timestamp(t, 0)
                .unwrap()
                .with_timezone(&Local)
                .date_naive()
        };
        (time(now) - time(last)).num_days().max(0) as u32
    });
    let states = FSRS::default().next_states(prior.map(|(memory, _)| memory), 0.9, elapsed)?;
    let item = if good { states.good } else { states.again };
    let due = now + i64::from(item.interval.round().max(1.0) as u32) * 86_400;
    tx.execute("INSERT INTO learning VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(card_id) DO UPDATE SET stability=excluded.stability,difficulty=excluded.difficulty,last_at=excluded.last_at,due_at=excluded.due_at",
        params![card,item.memory.stability,item.memory.difficulty,now,due])?;
    let repeat_after = if good {
        None
    } else {
        Some(place_repeat(
            &tx,
            &card,
            now,
            rand::rng().random_range(5..=20),
        )?)
    };
    let result = Accepted {
        rating: if good { "Good" } else { "Again" }.into(),
        repeat_after,
        answers,
        played_in_game,
    };
    tx.execute(
        "UPDATE attempts SET submission=?1,result=?2 WHERE id=?3",
        params![payload, serde_json::to_string(&result)?, input.attempt],
    )?;
    tx.commit()?;
    Ok(accepted_view(result, &fen, input.played.as_deref()))
}

type ContextRow = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<usize>,
);

pub fn context(db: &Connection, attempt: i64) -> Result<CardContext> {
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
    let (position, repertoire, version, route, mut game_id, mut ply) = row;
    let mut played = None;
    if let (Some(repertoire), Some(version)) = (repertoire, version)
        && let Some((game, index, m)) =
            games::mismatch_context(db, &position, &repertoire, &version)?
    {
        game_id = Some(game);
        ply = Some(index);
        played = Some(m);
    }
    Ok(CardContext {
        game_id,
        ply,
        played,
        lead_in: route.as_deref().map(serde_json::from_str).transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spacing_clamps_to_the_remaining_deck() {
        assert_eq!(repeat_after(5, 20), 5);
        assert_eq!(repeat_after(20, 20), 20);
        assert_eq!(repeat_after(20, 3), 3);
        assert_eq!(repeat_after(5, 0), 0);
    }
}
