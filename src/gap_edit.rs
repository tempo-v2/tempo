use crate::{
    ApiError, ApiResult, App, Database, catalog, chess, coverage, prepare, write_db, writer,
};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State};
use pgn_reader::shakmaty::Position;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug)]
pub struct Stale;
impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The repertoire changed. Rescan and preview again.")
    }
}
impl std::error::Error for Stale {}

#[derive(Serialize, Deserialize)]
pub struct Draft {
    report: String,
    gap: usize,
    pgn: String,
}

struct Edit {
    repertoire: String,
    revision: i64,
    command: String,
    plans: Vec<catalog::OpeningPlan>,
    requires_offline: bool,
    preview: Value,
}

fn build(c: &Connection, draft: Draft, online: bool) -> Result<Edit> {
    ensure!(
        draft.pgn.len() <= 128 * 1024,
        "Paste one PGN up to 128 KiB."
    );
    let command = chess::id(&["gap-save-v1", &serde_json::to_string(&draft)?]);
    let (scan, revision, gap) = coverage::selected(c, &draft.report, draft.gap)?;
    let target = chess::position_id(&chess::position(&gap.fen)?);
    let game = match chess::parse_gap_pgn(&draft.pgn, online) {
        Ok(game) => game,
        Err(error) if error.is::<chess::ExpandedPgn>() => {
            return Ok(Edit {
                repertoire: scan.repertoire,
                revision,
                command: command.clone(),
                plans: vec![],
                requires_offline: true,
                preview: json!({
                    "command":command, "responses":[], "requires_offline":true,
                    "validation_deferred":true
                }),
            });
        }
        Err(error) => return Err(error),
    };
    let mut plans = vec![];
    let mut included = HashSet::new();
    let mut responses = BTreeMap::new();
    let mut rows = vec![];
    let mut new_responses = 0;
    for route in std::iter::once(game.main).chain(game.variations) {
        let mut board = chess::position(&route.fen)?;
        let mut anchor = None;
        for (ply, m) in route.moves.iter().enumerate() {
            if chess::position_id(&board) == target {
                anchor = Some(ply);
                break;
            }
            let legal = chess::parse_move(&board, m)?;
            board.play_unchecked(legal);
        }
        let Some(start_at) = anchor else { continue };
        if !included.insert((route.fen.clone(), route.moves.clone())) {
            continue;
        }
        let plan = catalog::prepare_opening(&catalog::OpeningInput {
            name: "Gap continuation".into(),
            color: scan.color.clone(),
            route,
            start_at,
        })?;
        for (pos, version, answers) in plan.responses() {
            if let Some(prior) = responses.insert(pos.to_owned(), version.to_owned()) {
                ensure!(
                    prior == version,
                    "The PGN proposes competing replies at the same position."
                );
                continue;
            }
            let prior:Option<(String,bool)>=c.query_row("SELECT answer_version,quarantined FROM cards WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 ORDER BY quarantined LIMIT 1",(&scan.repertoire,pos),|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let status = match prior {
                None => {
                    new_responses += 1;
                    "new"
                }
                Some((v, false)) if v == version => "existing",
                _ => "conflict",
            };
            rows.push(json!({"move":serde_json::from_str::<Vec<String>>(answers)?[0],"status":status,"position":pos}));
        }
        plans.push(plan);
    }
    ensure!(
        !plans.is_empty(),
        "The PGN must reach this gap and supply a reply."
    );
    let requires_offline = 1 + plans.len() + 3 * new_responses > 128;
    Ok(Edit {
        repertoire: scan.repertoire,
        revision,
        command: command.clone(),
        plans,
        requires_offline,
        preview: json!({"command":command,"responses":rows,"requires_offline":requires_offline}),
    })
}

async fn prepare_edit(app: &App, draft: Draft) -> Result<Edit, ApiError> {
    let path = app.database.clone();
    prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || {
            let mut c = Database::read(&path)?;
            let tx = c.transaction()?;
            build(&tx, draft, true)
        },
    )
    .await
}
pub async fn preview(State(app): State<App>, Json(draft): Json<Draft>) -> ApiResult {
    let edit = prepare_edit(&app, draft).await?;
    let path = app.database.clone();
    let current = tokio::task::spawn_blocking(move || {
        coverage::revision(&Database::read(&path)?, &edit.repertoire)
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    if current != edit.revision {
        return Err(ApiError(anyhow::anyhow!(
            "Repertoire changed. Rescan before editing."
        )));
    }
    Ok(Json(edit.preview))
}

#[derive(Deserialize)]
pub struct Save {
    draft: Draft,
    command: String,
}

pub async fn save(State(app): State<App>, Json(input): Json<Save>) -> ApiResult {
    let identity = chess::id(&[
        "gap-save-v1",
        &serde_json::to_string(&input.draft).map_err(|e| ApiError(e.into()))?,
    ]);
    if input.command != identity {
        return Err(ApiError(anyhow::anyhow!("Preview this PGN before saving.")));
    }
    let path = app.database.clone();
    let key = identity.clone();
    let prior = tokio::task::spawn_blocking(move || receipt(&Database::read(&path)?, &key))
        .await
        .map_err(|e| ApiError(e.into()))?
        .map_err(ApiError)?;
    if let Some(prior) = prior {
        return Ok(Json(prior));
    }
    let edit = prepare_edit(&app, input.draft).await?;
    write_db(app, move |c| commit(c, edit, true)).await
}

pub fn import(c: &mut Connection, input: Save) -> Result<Value> {
    let identity = chess::id(&["gap-save-v1", &serde_json::to_string(&input.draft)?]);
    ensure!(
        input.command == identity,
        "Preview this PGN before importing."
    );
    if let Some(prior) = receipt(c, &identity)? {
        return Ok(prior);
    }
    let edit = build(c, input.draft, false)?;
    commit(c, edit, false)
}

#[derive(Debug)]
pub struct OfflineRequired;
impl std::fmt::Display for OfflineRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("This continuation is too large for an online save. Download it from Preview for offline import.")
    }
}
impl std::error::Error for OfflineRequired {}

fn commit(c: &mut Connection, edit: Edit, online: bool) -> Result<Value> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(prior) = receipt(&tx, &edit.command)? {
        return Ok(prior);
    }
    ensure!(
        coverage::revision(&tx, &edit.repertoire)? == edit.revision,
        Stale
    );
    ensure!(!online || !edit.requires_offline, OfflineRequired);
    let mut lines = vec![];
    for plan in &edit.plans {
        lines.push(catalog::apply_opening(
            &tx,
            plan,
            Some(&edit.repertoire),
            online,
        )?);
    }
    let reply = json!({"command":edit.command,"lines":lines});
    tx.execute(
        "INSERT INTO gap_save_receipt VALUES(?1,?2)",
        (&edit.command, reply.to_string()),
    )?;
    tx.commit()?;
    Ok(reply)
}

fn receipt(c: &Connection, id: &str) -> Result<Option<Value>> {
    let prior: Option<String> = c
        .query_row(
            "SELECT reply FROM gap_save_receipt WHERE command=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(prior.map(|s| serde_json::from_str(&s)).transpose()?)
}
