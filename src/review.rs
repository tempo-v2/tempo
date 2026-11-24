// Coordinate the accepted answer across Study, Chess, Learning and Queue.
use crate::{
    chess, db,
    learning::{self, Rating},
    queue, queue_store,
    study::{AcceptedView, Answer, AnswerState, Next, NextState, Review},
    study_store,
};
use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior::Immediate};

pub fn answer(db: &mut Connection, input: Answer) -> Result<AcceptedView> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let issued = match study_store::begin_answer(&tx, &input)? {
        AnswerState::Replayed(view) => return Ok(view),
        AnswerState::Pending(issued) => issued,
    };
    let rating = if chess::answer_matches(&issued.fen, &issued.answers, input.played.as_deref())? {
        Rating::Good
    } else {
        Rating::Again
    };
    let now = Utc::now().timestamp();
    let prior = db::prior_review(&tx, &issued.card)?;
    let schedule = learning::schedule(rating, prior, now)?;
    db::save_review(&tx, &issued.card, now, schedule)?;
    let mut repeat_after = None;
    if matches!(rating, Rating::Again) && queue_store::can_repeat(&tx, &issued.card)? {
        let draw = queue::draw_repeat_offset();
        if queue_store::prepared(&tx)? {
            repeat_after = Some(queue_store::place_repeat(
                &tx,
                &issued.card,
                issued.previous_kind.as_deref(),
                now,
                draw,
            )?);
        } else {
            queue_store::defer_repeat(
                &tx,
                input.attempt,
                &issued.card,
                issued.previous_kind.as_deref(),
                now,
                draw,
            )?;
        }
    }
    let accepted = study_store::accept_answer(&tx, &input, issued, rating.label(), repeat_after)?;
    tx.commit()?;
    Ok(accepted)
}

#[derive(Debug)]
pub struct Preparing;
impl std::fmt::Display for Preparing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Preparing the Study queue. Retry Next.")
    }
}
impl std::error::Error for Preparing {}

pub fn next(db: &mut Connection, input: Next) -> Result<Option<Review>> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let NextState::Select { previous_kind } = study_store::begin_next(&tx, &input)? else {
        return study_store::current(&tx);
    };
    anyhow::ensure!(queue_store::place_deferred(&tx)?, Preparing);
    let selected = match queue_store::committed_head(&tx)? {
        Some(card) => Some(card),
        None => {
            anyhow::ensure!(queue_store::prepared(&tx)?, Preparing);
            queue_store::select(&tx, previous_kind.as_deref(), Utc::now().timestamp())?
        }
    };
    study_store::issue(
        &tx,
        selected
            .as_ref()
            .map(|(id, kind)| (id.as_str(), kind.as_str())),
    )?;
    if let Some((id, _)) = selected {
        queue_store::consume(&tx, &id)?;
    }
    let view = study_store::current(&tx)?;
    tx.commit()?;
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_again_survives_an_unprepared_queue_and_places_once() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute("INSERT INTO metadata VALUES ('fixture')", [])?;
        db.execute(
            "INSERT INTO cards(id,kind,title,color,fen,position_id,answers)
             VALUES ('a','tactic','First','white',?1,'a','[\"e2e4\"]')",
            ["rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"],
        )?;
        db.execute("INSERT INTO deck VALUES (1,'a')", [])?;
        let review = next(
            &mut db,
            Next {
                after: None,
                dataset: None,
            },
        )?
        .unwrap();
        let answer_command = || Answer {
            attempt: review.attempt,
            dataset: "fixture".into(),
            played: None,
        };
        let first = serde_json::to_value(answer(&mut db, answer_command())?)?;
        assert_eq!(first["rating"], "Again");
        let draw: u8 = db.query_row("SELECT draw FROM repeat_obligation", [], |row| row.get(0))?;
        let continuation = || Next {
            after: Some(review.attempt),
            dataset: Some("fixture".into()),
        };
        assert!(matches!(next(&mut db, continuation()), Err(error) if error.is::<Preparing>()));
        assert_eq!(
            serde_json::to_value(answer(&mut db, answer_command())?)?,
            first
        );
        assert_eq!(
            db.query_row("SELECT draw FROM repeat_obligation", [], |row| row
                .get::<_, u8>(0))?,
            draw
        );
        assert!(queue_store::seed_batch(&mut db)?);
        let next_attempt = next(&mut db, continuation())?.unwrap().attempt;
        assert_ne!(next_attempt, review.attempt);
        assert_eq!(
            next(&mut db, continuation())?.unwrap().attempt,
            next_attempt
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM repeat_obligation", [], |row| row
                .get::<_, u32>(0))?,
            0
        );
        assert_eq!(
            serde_json::to_value(answer(&mut db, answer_command())?)?,
            first
        );
        Ok(())
    }
}
