use crate::{chess, learning::Rating, study::*};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

pub fn current(db: &Connection) -> Result<Option<Review>> {
    let row = db
        .query_row(
            "SELECT a.id,c.kind,c.title,c.color,c.fen,a.result,a.submission,c.context,c.played_move
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
                    r.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(attempt, kind, title, color, fen, result, submission, context, played_in_game)| {
            let legal = chess::legal(&chess::position(&fen)?);
            let activity = if let Some(play) = crate::full_line_store::for_review(db, attempt)? {
                Activity::FullLine {
                    result: (),
                    puzzle: crate::full_line_store::resume(db, play)?,
                }
            } else {
                let result = result
                    .as_deref()
                    .map(|saved| {
                        serde_json::from_str::<AnswerReceipt>(saved)?.view(
                            &fen,
                            submission.as_deref(),
                            played_in_game,
                        )
                    })
                    .transpose()?;
                Activity::SingleAnswer { result }
            };
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
                activity,
            })
        },
    )
    .transpose()
}

pub(crate) fn state(db: &Connection) -> Result<(Option<i64>, Option<String>)> {
    Ok(db.query_row("SELECT active,last_kind FROM study", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })?)
}

pub fn unanswered_card(db: &Connection) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT a.card_id FROM study s JOIN attempts a ON a.id=s.active WHERE a.result IS NULL",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn begin_next(db: &Connection, input: &Next) -> Result<NextState> {
    let dataset = crate::db::dataset(db)?;
    if input.after.is_some() {
        check_dataset(input.dataset.as_deref(), &dataset)?;
    }
    let (active, previous_kind) = state(db)?;
    let accepted = if let Some(active) = active {
        db.query_row(
            "SELECT result IS NOT NULL FROM attempts WHERE id=?1",
            [active],
            |r| r.get(0),
        )?
    } else {
        false
    };
    if !should_select(active, accepted, input.after) {
        return Ok(NextState::Current);
    }
    Ok(NextState::Select { previous_kind })
}

pub fn issue(db: &Connection, card: Option<(&str, &str)>) -> Result<()> {
    if let Some((id, kind)) = card {
        db.execute("INSERT INTO attempts(card_id) VALUES (?1)", [id])?;
        let attempt = db.last_insert_rowid();
        crate::full_line_store::issue_review(db, id, attempt)?;
        db.execute(
            "UPDATE study SET active=?1,last_kind=?2",
            params![attempt, kind],
        )?;
    } else {
        db.execute("UPDATE study SET active=NULL", [])?;
    }
    Ok(())
}

pub fn begin_answer(db: &Connection, input: &AnswerIntentV1) -> Result<AnswerState> {
    check_dataset(Some(&input.dataset), &crate::db::dataset(db)?)?;
    ensure!(
        crate::full_line_store::for_review(db, input.attempt)?.is_none(),
        Conflict::FullSolution
    );
    type IssuedRow = (
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
    );
    let (card, submission, accepted, fen, answers, played_in_game): IssuedRow = db
        .query_row(
            "SELECT a.card_id,a.submission,a.result,c.fen,coalesce(a.issued_answers,c.answers),c.played_move
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
        )
        .optional()?
        .ok_or(Conflict::Attempt)?;
    if let Some(saved) = accepted {
        let receipt: AnswerReceipt = serde_json::from_str(&saved)?;
        receipt.verify(&fen, submission.as_deref(), &input.action)?;
        return Ok(AnswerState::Replayed(receipt.view(
            &fen,
            submission.as_deref(),
            played_in_game,
        )?));
    }
    let (active, previous_kind) = state(db)?;
    ensure!(active == Some(input.attempt), Conflict::Attempt);
    let action = input.action.canonical(&fen)?;
    let answers: Vec<String> = serde_json::from_str(&answers)?;
    Ok(AnswerState::Pending(IssuedAnswer {
        card,
        fen,
        answers,
        played_in_game,
        previous_kind,
        action,
    }))
}

pub fn accept_answer(
    db: &Connection,
    input: &AnswerIntentV1,
    issued: IssuedAnswer,
    rating: Rating,
    repeat: RepeatDisposition,
) -> Result<AcceptedView> {
    let receipt = AnswerReceipt::Versioned {
        v1: AnswerResultV1 {
            action: issued.action,
            rating,
            repeat,
            answers: issued.answers,
        },
    };
    db.execute(
        "UPDATE attempts SET result=?1 WHERE id=?2",
        params![serde_json::to_string(&receipt)?, input.attempt],
    )?;
    receipt.view(&issued.fen, None, issued.played_in_game)
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
