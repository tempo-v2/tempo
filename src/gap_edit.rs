use crate::{
    ApiError, ApiResult, App, Database, catalog, chess, coverage, prepare, write_db, writer,
};
use anyhow::{Result, ensure};
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
    batched: bool,
    has_conflicts: bool,
    preview: Value,
}

fn build(c: &Connection, draft: Draft) -> Result<Edit> {
    ensure!(
        draft.pgn.len() <= 128 * 1024,
        "Paste one PGN up to 128 KiB."
    );
    let command = chess::id(&["gap-save-v1", &serde_json::to_string(&draft)?]);
    let (scan, revision, gap) = coverage::selected(c, &draft.report, draft.gap)?;
    let target = chess::position_id(&chess::position(&gap.fen)?);
    let game = chess::parse_gap_pgn(&draft.pgn).map_err(|error| {
        if error.is::<chess::ExpandedPgn>() {
            anyhow::anyhow!(
                "This PGN expands to too many variations. Split it into smaller continuations."
            )
        } else {
            error
        }
    })?;
    let mut plans = vec![];
    let mut included = HashSet::new();
    let mut responses = BTreeMap::new();
    let mut rows = vec![];
    let mut new_responses = 0;
    let mut has_conflicts = false;
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
                _ => {
                    has_conflicts = true;
                    "conflict"
                }
            };
            rows.push(json!({"move":serde_json::from_str::<Vec<String>>(answers)?[0],"status":status,"position":pos}));
        }
        plans.push(plan);
    }
    ensure!(
        !plans.is_empty(),
        "The PGN must reach this gap and supply a reply."
    );
    let batched = 1 + plans.len() + 3 * new_responses > 128 || has_checkpoint(c, &command)?;
    Ok(Edit {
        repertoire: scan.repertoire,
        revision,
        command: command.clone(),
        plans,
        batched,
        has_conflicts,
        preview: json!({"command":command,"responses":rows,"batched":batched}),
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
            build(&tx, draft)
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
    if !edit.batched {
        return write_db(app, move |c| commit(c, edit, true)).await;
    }
    static BATCH_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let permit = BATCH_SLOT
        .try_acquire()
        .map_err(|_| ApiError(writer::Unavailable.into()))?;
    let reply = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        apply_batches(&app.writer, edit)
    })
    .await
    .map_err(|error| ApiError(error.into()))?
    .map_err(ApiError)?;
    Ok(Json(reply))
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
    ensure!(
        !has_checkpoint(c, &identity)?,
        "Resume this continuation from Tempo's Gaps tab."
    );
    let edit = build(c, input.draft)?;
    commit(c, edit, false)
}

fn commit(c: &mut Connection, edit: Edit, online: bool) -> Result<Value> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(prior) = receipt(&tx, &edit.command)? {
        return Ok(prior);
    }
    ensure!(
        coverage::revision(&tx, &edit.repertoire)? == edit.revision,
        Stale
    );

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

#[derive(Debug)]
pub struct Interrupted(pub usize, pub i64);
impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The repertoire changed during Save. {} lines finished and {} replies were added. Rescan before continuing.",
            self.0, self.1
        )
    }
}
impl std::error::Error for Interrupted {}

#[derive(Debug)]
pub struct Conflicts;
impl std::fmt::Display for Conflicts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Resolve conflicting replies before saving this continuation.")
    }
}
impl std::error::Error for Conflicts {}

fn has_checkpoint(c: &Connection, command: &str) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM gap_batch WHERE command=?1)",
        [command],
        |r| r.get(0),
    )?)
}

struct Batch {
    revision: i64,
    line: usize,
    card: usize,
}

fn checkpoint(c: &Connection, edit: &Edit) -> Result<Option<Batch>> {
    Ok(c.query_row(
        "SELECT revision,line,card FROM gap_batch WHERE command=?1",
        [&edit.command],
        |row| {
            Ok(Batch {
                revision: row.get(0)?,
                line: row.get(1)?,
                card: row.get(2)?,
            })
        },
    )
    .optional()?)
}

fn apply_batches(writer: &writer::Writer, edit: Edit) -> Result<Value> {
    let lines: Vec<_> = edit
        .plans
        .iter()
        .map(|plan| catalog::opening_id(plan, Some(&edit.repertoire)))
        .collect();
    let reply = json!({"command":edit.command,"lines":lines});
    let completed = std::sync::Arc::new(reply.to_string());
    let edit = std::sync::Arc::new(edit);
    loop {
        let edit = edit.clone();
        let completed = completed.clone();
        if writer.background(move |c| batch_turn(c, &edit, &completed))? {
            return Ok(reply);
        }
    }
}

fn batch_turn(c: &mut Connection, edit: &Edit, completed: &str) -> Result<bool> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if receipt(&tx, &edit.command)?.is_some() {
        return Ok(true);
    }
    let saved = checkpoint(&tx, edit)?;
    let mut batch = saved.unwrap_or(Batch {
        revision: edit.revision,
        line: 0,
        card: 0,
    });
    if coverage::revision(&tx, &edit.repertoire)? != batch.revision {
        if batch.line != 0 || batch.card != 0 {
            anyhow::bail!(Interrupted(batch.line, batch.revision - edit.revision));
        }
        anyhow::bail!(Stale);
    }
    ensure!(!edit.has_conflicts, Conflicts);
    let plan = &edit.plans[batch.line];
    let next = catalog::apply_opening_batch(&tx, plan, &edit.repertoire, batch.card)?;
    if let Some(next) = next {
        batch.card = next;
    } else {
        batch.line += 1;
        batch.card = 0;
    }
    let finished = batch.line == edit.plans.len();
    if finished {
        tx.execute(
            "INSERT INTO gap_save_receipt VALUES(?1,?2)",
            (&edit.command, completed),
        )?;
        tx.execute("DELETE FROM gap_batch WHERE command=?1", [&edit.command])?;
    } else {
        let revision = coverage::revision(&tx, &edit.repertoire)?;
        tx.execute(
            "INSERT INTO gap_batch VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(command) DO UPDATE SET revision=excluded.revision,line=excluded.line,card=excluded.card",
            (&edit.command,revision,batch.line,batch.card,edit.plans.len()),
        )?;
    }
    tx.commit()?;
    Ok(finished)
}

pub async fn progress(
    State(app): State<App>,
    axum::extract::Path(command): axum::extract::Path<String>,
) -> crate::ViewResult {
    crate::optional_view(app, crate::optional_read::Reader::Coverage, move |c| {
        Ok(c.query_row(
            "SELECT line,total FROM gap_batch WHERE command=?1",
            [&command],
            |row| Ok(json!({"finished":row.get::<_,usize>(0)?,"total":row.get::<_,usize>(1)?})),
        )
        .optional()?)
    })
    .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_checkpoint_and_receipt_follow_committed_opening_rows() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("tempo-gap-{:x}", rand::random::<u128>()));
        std::fs::create_dir(&directory)?;
        let path = directory.join("fixture.db");
        let result = test_checkpoints(&path);
        std::fs::remove_dir_all(directory)?;
        result
    }

    fn test_checkpoints(path: &std::path::Path) -> Result<()> {
        let mut c = Database::init(path)?;
        let opening = |name: &str, moves: &[&str]| catalog::OpeningInput {
            name: name.into(),
            color: "white".into(),
            route: chess::Route {
                fen: String::new(),
                moves: moves.iter().map(|m| (*m).into()).collect(),
            },
            start_at: 0,
        };
        let repertoire = catalog::save_opening(&c, &opening("Original", &["e2e4"]), None)?;
        let first = catalog::prepare_opening(&opening("First", &["e2e4", "e7e5", "g1f3"]))?;
        let second = catalog::prepare_opening(&opening("Second", &["e2e4", "c7c5", "g1f3"]))?;
        let edit = Edit {
            revision: coverage::revision(&c, &repertoire)?,
            repertoire,
            command: "fixture".into(),
            plans: vec![first, second],
            batched: true,
            has_conflicts: false,
            preview: Value::Null,
        };
        let ids: Vec<_> = edit
            .plans
            .iter()
            .map(|plan| catalog::opening_id(plan, Some(&edit.repertoire)))
            .collect();
        let accepted = json!({"lines":ids,"command":edit.command});
        let serialized = accepted.to_string();
        assert!(!batch_turn(&mut c, &edit, &serialized)?);
        assert!(receipt(&c, &edit.command)?.is_none());
        assert_eq!(checkpoint(&c, &edit)?.unwrap().line, 1);
        c.execute_batch(
            "CREATE TRIGGER reject_gap_receipt BEFORE INSERT ON gap_save_receipt
            BEGIN SELECT RAISE(ABORT, 'interrupted'); END;",
        )?;
        assert!(batch_turn(&mut c, &edit, &serialized).is_err());
        assert_eq!(checkpoint(&c, &edit)?.unwrap().line, 1);
        assert_eq!(coverage::revision(&c, &edit.repertoire)?, edit.revision + 1);
        c.execute_batch("DROP TRIGGER reject_gap_receipt;")?;
        drop(c);
        let mut c = Database::open(path)?;
        assert!(batch_turn(&mut c, &edit, &serialized)?);
        assert_eq!(receipt(&c, &edit.command)?, Some(accepted));
        assert!(checkpoint(&c, &edit)?.is_none());
        assert!(batch_turn(&mut c, &edit, &serialized)?);
        assert_eq!(coverage::revision(&c, &edit.repertoire)?, edit.revision + 2);
        Ok(())
    }
}
