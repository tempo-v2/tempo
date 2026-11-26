//! Lichess pack Start and first-card identity. Issued plays use full-line Study.
use crate::{
    chess,
    full_line::{InitialCard, View},
    full_line_store, lichess_catalog,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Start {
    dataset: String,
    revision: String,
    puzzle: String,
    request: String,
    practice: bool,
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
    full_line_store::check_dataset(&tx, &input.dataset)?;
    if let Some(view) = full_line_store::replay_start(
        &tx,
        &input.request,
        &input.puzzle,
        input.practice,
        &input.revision,
    )? {
        return Ok(view);
    }
    let version = lichess_catalog::member_version(&tx, &input.puzzle, &input.revision)?;
    let view = full_line_store::start_published(
        &tx,
        &input.request,
        &input.puzzle,
        &version,
        input.practice,
        &input.revision,
    )?;
    tx.commit()?;
    Ok(view)
}

pub fn first_card(puzzle: &str) -> InitialCard {
    InitialCard {
        id: chess::id(&["lichess-puzzle-v1", puzzle]),
        title: format!("Lichess puzzle {puzzle}"),
    }
}
