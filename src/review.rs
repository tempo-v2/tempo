// Coordinate the accepted answer across Study, Chess, Learning and Queue.
use crate::{
    chess, db,
    learning::{self, Rating},
    queue, queue_store,
    study::{self, AcceptedView, Answer, AnswerState},
};
use anyhow::Result;
use chrono::Utc;
use rusqlite::{Connection, TransactionBehavior::Immediate};

pub fn answer(db: &mut Connection, input: Answer) -> Result<AcceptedView> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let issued = match study::begin_answer(&tx, &input)? {
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
    let repeat_after =
        if matches!(rating, Rating::Again) && queue_store::can_repeat(&tx, &issued.card)? {
            Some(queue_store::place_repeat(
                &tx,
                &issued.card,
                now,
                queue::draw_repeat_offset(),
            )?)
        } else {
            None
        };
    let accepted = study::accept_answer(&tx, &input, issued, rating.label(), repeat_after)?;
    tx.commit()?;
    Ok(accepted)
}
