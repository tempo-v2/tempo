use crate::{chess, study::*};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

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

fn state(db: &Connection) -> Result<(Option<i64>, Option<String>)> {
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
        db.execute(
            "UPDATE study SET active=?1,last_kind=?2",
            params![attempt, kind],
        )?;
    } else {
        db.execute("UPDATE study SET active=NULL", [])?;
    }
    Ok(())
}

pub fn begin_answer(db: &Connection, input: &Answer) -> Result<AnswerState> {
    check_dataset(Some(&input.dataset), &crate::db::dataset(db)?)?;
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
        verify_replay(submission.as_deref(), &payload)?;
        return Ok(AnswerState::Replayed(accepted_view(
            serde_json::from_str(&saved)?,
            &fen,
            input.played.as_deref(),
        )));
    }
    let (active, previous_kind) = state(db)?;
    ensure!(active == Some(input.attempt), "the active card changed");
    let answers: Vec<String> = serde_json::from_str(&answers)?;
    Ok(AnswerState::Pending(IssuedAnswer {
        card,
        fen,
        answers,
        played_in_game,
        previous_kind,
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
