//! Optional, version-bound evidence and Openings edits for an accepted Study decision.
use crate::{
    ApiError, ApiResult, App, Database, catalog, chess, gap_pgn, opening_engine,
    opening_replacement::{self, Replacement},
    optional_read::{self, Reader},
    prepare, repertoire_similarity as similarity, study_store,
    tactic_worker_protocol::{EngineMode, EngineReply, EngineRequest},
    write_db, writer,
};
use anyhow::{Result, ensure};
use axum::{Json, extract::State};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct Stale;
impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This Study answer no longer matches the active saved reply.")
    }
}
impl std::error::Error for Stale {}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    dataset: String,
    attempt: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Basis {
    address: Address,
    repertoire: String,
    position: String,
    version: String,
    reply_revision: i64,
    line_revision: i64,
}

fn current(db: &Connection, address: &Address) -> Result<(Basis, study_store::CompletedOpening)> {
    ensure!(crate::db::dataset(db)? == address.dataset, Stale);
    let issued = study_store::completed_opening(db, address.attempt)?.ok_or(Stale)?;
    let current = catalog::current_opening_reply(db, &issued.repertoire, &issued.position)?
        .filter(|reply| !reply.quarantined)
        .ok_or(Stale)?;
    let [movement]: [String; 1] = serde_json::from_str(&current.answers)?;
    ensure!(
        current.id == issued.card
            && current.version == issued.version
            && movement == issued.movement,
        Stale
    );
    let basis = Basis {
        address: address.clone(),
        repertoire: issued.repertoire.clone(),
        position: issued.position.clone(),
        version: issued.version.clone(),
        reply_revision: catalog::authored_reply_revision(db, &issued.repertoire)?,
        line_revision: opening_replacement::revision(db, &issued.repertoire)?,
    };
    Ok((basis, issued))
}

fn matching(db: &Connection, basis: &Basis) -> Result<study_store::CompletedOpening> {
    let (actual, issued) = current(db, &basis.address)?;
    ensure!(actual == *basis, Stale);
    Ok(issued)
}

pub async fn read(State(app): State<App>, Json(address): Json<Address>) -> ApiResult {
    optional_read::read_value(
        Reader::StudyMoveReview,
        app.database.clone(),
        move |db, _| {
            let tx = db.transaction()?;
            let (basis, issued) = current(&tx, &address)?;
            let repertoire =
                catalog::repertoire(&tx, &issued.repertoire, &issued.color)?.ok_or(Stale)?;
            let mut routes = catalog::reply_contexts(
                &tx,
                &issued.repertoire,
                &issued.color,
                BTreeMap::from([(issued.position.clone(), issued.version)]),
            )?;
            let proved_route = routes.routes.remove(&issued.position);
            let editing = if proved_route.is_some() {
                "available"
            } else {
                "route_unavailable"
            };
            let route = proved_route
                .map(|route| gap_pgn::line_text(&route, &issued.position, &issued.movement))
                .transpose()?;
            Ok(Json(json!({
                "basis": basis, "fen": issued.fen, "issued_move": issued.movement,
                "repertoire": repertoire.example_line, "route": route, "editing": editing,
            })))
        },
    )
    .await
    .map_err(ApiError)
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Source {
    Engine,
    Similarity,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    basis: Basis,
    source: Source,
}

pub async fn evidence(State(app): State<App>, Json(input): Json<Evidence>) -> ApiResult {
    let basis = input.basis.clone();
    let value = match input.source {
        Source::Engine => {
            let issued = optional_read::read_value(
                Reader::StudyMoveReview,
                app.database.clone(),
                move |db, _| {
                    let tx = db.transaction()?;
                    matching(&tx, &basis)
                },
            )
            .await
            .map_err(ApiError)?;
            let reply = opening_engine::evaluate(
                app.opening_engine.as_ref(),
                EngineRequest {
                    fen: issued.fen.clone(),
                    mode: EngineMode::ScoredRoots,
                },
            )
            .await;
            let board = chess::position(&issued.fen).map_err(ApiError)?;
            match reply {
                EngineReply::Evaluated {
                    engine,
                    depth: 10,
                    lines,
                } if !engine.is_empty() && engine.len() <= 128 => {
                    match opening_engine::rank_roots(&board, lines) {
                        Ok(lines) => {
                            json!({"status":"ready","engine":engine,"depth":10,"lines":lines})
                        }
                        Err(_) => json!({"status":"unavailable"}),
                    }
                }
                EngineReply::Busy => json!({"status":"busy"}),
                _ => json!({"status":"unavailable"}),
            }
        }
        Source::Similarity => optional_read::read_value(
            Reader::RepertoireSimilarity,
            app.database.clone(),
            move |db, stopped| {
                let tx = db.transaction()?;
                let issued = matching(&tx, &basis)?;
                let snapshot = similarity::snapshot(&tx, &issued.repertoire, &issued.color)?;
                drop(tx);
                let position = similarity::PositionIr::parse(&issued.fen)?;
                let candidates =
                    similarity::other_position_candidates(position, snapshot, stopped)?;
                Ok(json!({"status":"ready","candidates":candidates}))
            },
        )
        .await
        .map_err(ApiError)?,
    };
    let basis = input.basis.clone();
    let reader = match input.source {
        Source::Engine => Reader::StudyMoveReview,
        Source::Similarity => Reader::RepertoireSimilarity,
    };
    optional_read::read_value(reader, app.database.clone(), move |db, _| {
        let tx = db.transaction()?;
        matching(&tx, &basis)?;
        Ok(())
    })
    .await
    .map_err(ApiError)?;
    Ok(Json(
        json!({"basis":input.basis,"source":input.source,"evidence":value}),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Replace { movement: String },
    Remove,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    basis: Basis,
    action: Action,
}

impl Edit {
    fn command(&self) -> Result<String> {
        Ok(chess::id(&[
            "study-opening-edit-v1",
            &serde_json::to_string(self)?,
        ]))
    }
}

fn plan(db: &Connection, edit: &Edit) -> Result<Replacement> {
    let issued = matching(db, &edit.basis)?;
    let movement = match &edit.action {
        Action::Replace { movement } => {
            let board = chess::position(&issued.fen)?;
            let legal = chess::parse_move(&board, movement)?;
            ensure!(
                chess::uci(legal) == *movement && *movement != issued.movement,
                "Choose a different legal reply."
            );
            Some(movement.as_str())
        }
        Action::Remove => None,
    };
    opening_replacement::plan_study_reply(
        db,
        &issued.repertoire,
        &issued.color,
        &issued.position,
        &issued.movement,
        movement,
    )
}

pub async fn preview(State(app): State<App>, Json(edit): Json<Edit>) -> ApiResult {
    let path = app.database.clone();
    let basis = edit.basis.clone();
    let result = prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || {
            let mut db = Database::read(&path)?;
            let tx = db.transaction()?;
            let mut replacement = plan(&tx, &edit)?;
            // Only oversized, non-committable previews can omit affected lines.
            let affected = replacement.impact.len();
            if replacement.over_budget {
                replacement.impact.truncate(catalog::ONLINE_WRITE_LIMIT);
            }
            Ok(json!({
                "command":edit.command()?, "edit":edit, "impact":replacement.impact,
                "affected":affected, "over_budget":replacement.over_budget,
            }))
        },
    )
    .await?;
    optional_read::read_value(
        Reader::StudyMovePreview,
        app.database.clone(),
        move |db, _| {
            let tx = db.transaction()?;
            matching(&tx, &basis)?;
            Ok(())
        },
    )
    .await
    .map_err(ApiError)?;
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Confirm {
    edit: Edit,
    command: String,
}

pub async fn apply(State(app): State<App>, Json(input): Json<Confirm>) -> ApiResult {
    let command = input.edit.command().map_err(ApiError)?;
    if input.command != command {
        return Err(ApiError(anyhow::anyhow!(
            "Preview the change before confirming."
        )));
    }
    let path = app.database.clone();
    let edit = input.edit;
    let key = command.clone();
    let prepared = prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || {
            let mut db = Database::read(&path)?;
            let tx = db.transaction()?;
            if let Some(prior) = opening_replacement::receipt(&tx, &key)? {
                return Ok(Err(prior));
            }
            let replacement = plan(&tx, &edit)?;
            ensure!(!replacement.over_budget, opening_replacement::OverBudget);
            Ok(Ok((edit, replacement)))
        },
    )
    .await?;
    match prepared {
        Ok((edit, replacement)) => {
            write_db(app, move |db| commit(db, &edit, &command, &replacement)).await
        }
        Err(prior) => Ok(Json(prior)),
    }
}

fn commit(
    db: &mut Connection,
    edit: &Edit,
    command: &str,
    replacement: &Replacement,
) -> Result<Value> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(prior) = opening_replacement::receipt(&tx, command)? {
        return Ok(prior);
    }
    matching(&tx, &edit.basis)?;
    let before = tx.total_changes();
    opening_replacement::apply(&tx, &edit.basis.repertoire, replacement)?;
    let receipt = json!({"command":command,"edit":edit,"saved":true});
    opening_replacement::record_receipt(&tx, command, &receipt)?;
    ensure!(
        tx.total_changes() - before <= catalog::ONLINE_WRITE_LIMIT as u64,
        opening_replacement::OverBudget
    );
    tx.commit()?;
    Ok(receipt)
}

#[cfg(test)]
mod tests;
