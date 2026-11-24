// Coordinate the accepted answer across Study, Chess, Learning and Queue.
use crate::{
    chess, db,
    learning::{self, Rating},
    queue, queue_store,
    study::{self, AcceptedView, Answer, AnswerState, Next, NextState, Review},
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
                issued.previous_kind.as_deref(),
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

pub fn next(db: &mut Connection, input: Next) -> Result<Option<Review>> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let NextState::Select { previous_kind } = study::begin_next(&tx, &input)? else {
        return study::current(&tx);
    };
    let selected = match queue_store::committed_head(&tx)? {
        Some(card) => Some(card),
        None => queue_store::select(&tx, previous_kind.as_deref(), Utc::now().timestamp())?,
    };
    study::issue(
        &tx,
        selected
            .as_ref()
            .map(|(id, kind)| (id.as_str(), kind.as_str())),
    )?;
    if let Some((id, _)) = selected {
        queue_store::consume(&tx, &id)?;
    }
    let view = study::current(&tx)?;
    tx.commit()?;
    Ok(view)
}
