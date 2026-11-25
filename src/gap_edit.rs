use crate::{
    ApiError, ApiResult, App, Database, catalog, chess, coverage, gap_pgn, opening_replacement,
    prepare, write_db, writer,
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

#[derive(Clone, Serialize, Deserialize)]
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
    anchor: String,
    conflicts: BTreeMap<String, Conflict>,
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
        plans.push(plan);
    }
    ensure!(
        !plans.is_empty(),
        "The PGN must reach this gap and supply a reply."
    );
    let mut responses = BTreeMap::<String, Vec<&catalog::OpeningCard>>::new();
    for card in plans.iter().flat_map(|plan| &plan.cards) {
        let replies = responses.entry(card.position.clone()).or_default();
        if !replies.iter().any(|prior| prior.version == card.version) {
            replies.push(card);
        }
    }
    let mut rows = vec![];
    let mut conflicts = BTreeMap::new();
    let mut new_responses = 0;
    for (position, cards) in responses {
        let prior = saved_reply(c, &scan.repertoire, &position)?;
        let status = match &prior {
            None if cards.len() == 1 => {
                new_responses += 1;
                "new"
            }
            Some(prior)
                if !prior.quarantined && cards.len() == 1 && prior.version == cards[0].version =>
            {
                "existing"
            }
            _ => {
                conflicts.insert(
                    position.clone(),
                    describe_conflict(&position, &cards, prior.as_ref())?,
                );
                "conflict"
            }
        };
        rows.push(
            json!({"move":first_reply(&cards[0].answers)?,"status":status,"position":position}),
        );
    }
    let batched = 1 + plans.len() + 3 * new_responses > 128 || has_checkpoint(c, &command)?;
    let preview = json!({
        "command":command, "responses":rows, "batched":batched,
        "new_replies":new_responses, "conflicts":conflicts.values().collect::<Vec<_>>(),
    });
    Ok(Edit {
        repertoire: scan.repertoire,
        revision,
        command,
        plans,
        batched,
        has_conflicts: !conflicts.is_empty(),
        preview,
        anchor: target,
        conflicts,
    })
}

struct SavedReply {
    version: String,
    quarantined: bool,
    answers: String,
    context: Option<String>,
}

fn saved_reply(c: &Connection, repertoire: &str, position: &str) -> Result<Option<SavedReply>> {
    Ok(c.query_row(
        "SELECT answer_version,quarantined,answers,context FROM cards
         WHERE kind='opening' AND repertoire_id=?1 AND position_id=?2 AND opening_retired=0
         ORDER BY quarantined LIMIT 1",
        (repertoire, position),
        |row| {
            Ok(SavedReply {
                version: row.get(0)?,
                quarantined: row.get(1)?,
                answers: row.get(2)?,
                context: row.get(3)?,
            })
        },
    )
    .optional()?)
}

fn first_reply(answers: &str) -> Result<String> {
    use anyhow::Context;
    serde_json::from_str::<Vec<String>>(answers)?
        .into_iter()
        .next()
        .context("Missing opening reply")
}

#[derive(Serialize)]
struct Reply {
    #[serde(skip)]
    route: Option<chess::Route>,
    movement: String,
    reply: String,
    line: Option<String>,
}

#[derive(Serialize)]
struct Conflict {
    position: String,
    turn: &'static str,
    number: u32,
    saved: Option<Reply>,
    proposed: Vec<Reply>,
    transposition: bool,
    unresolved: bool,
}

fn describe_conflict(
    position: &str,
    cards: &[&catalog::OpeningCard],
    saved: Option<&SavedReply>,
) -> Result<Conflict> {
    let board = chess::position(&cards[0].fen)?;
    let mut proposed = Vec::new();
    for card in cards {
        let route = serde_json::from_str(&card.context)?;
        let movement = first_reply(&card.answers)?;
        proposed.push(Reply {
            line: Some(gap_pgn::line_text(&route, position, &movement)?),
            reply: gap_pgn::reply_text(&chess::position(&card.fen)?, &movement)?,
            movement,
            route: Some(route),
        });
    }
    let unresolved = saved.is_some_and(|saved| saved.quarantined);
    let saved = saved
        .filter(|saved| !saved.quarantined)
        .map(|saved| -> Result<_> {
            let movement = first_reply(&saved.answers)?;
            let route = saved
                .context
                .as_deref()
                .and_then(|text| serde_json::from_str::<chess::Route>(text).ok());
            let line = route
                .as_ref()
                .and_then(|route| gap_pgn::line_text(route, position, &movement).ok());
            Ok(Reply {
                reply: gap_pgn::reply_text(&board, &movement)?,
                movement,
                route: route.filter(|_| line.is_some()),
                line,
            })
        })
        .transpose()?;
    let transposition = saved
        .as_ref()
        .and_then(|reply| reply.route.as_ref())
        .is_some_and(|saved_route| {
            proposed
                .iter()
                .filter_map(|reply| reply.route.as_ref())
                .any(|route| {
                    route.moves != saved_route.moves
                        || chess::position(&route.fen)
                            .ok()
                            .map(|b| chess::position_id(&b))
                            != chess::position(&saved_route.fen)
                                .ok()
                                .map(|b| chess::position_id(&b))
                })
        });
    Ok(Conflict {
        position: position.into(),
        turn: chess::color_name(board.turn()),
        number: board.fullmoves().get(),
        saved,
        proposed,
        transposition,
        unresolved,
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
#[derive(Deserialize)]
pub struct Preview {
    #[serde(flatten)]
    draft: Draft,
    #[serde(default)]
    replacements: opening_replacement::Choices,
}

pub async fn preview(State(app): State<App>, Json(input): Json<Preview>) -> ApiResult {
    if input.replacements.is_empty() {
        let edit = prepare_edit(&app, input.draft).await?;
        check_fresh(&app, edit.repertoire, edit.revision).await?;
        return Ok(Json(edit.preview));
    }
    let path = app.database.clone();
    let (repertoire, revision, line_revision, view) = prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || {
            let mut c = Database::read(&path)?;
            let tx = c.transaction()?;
            let mut edit = build(&tx, input.draft.clone())?;
            let line_revision = opening_replacement::revision(&tx, &edit.repertoire)?;
            let staged = stage(&mut edit, &input.replacements);
            edit.preview["conflicts"] = json!(edit.conflicts.values().collect::<Vec<_>>());
            edit.preview["staged"] = json!(staged);
            if !edit.has_conflicts && staged.iter().all(|choice| choice.valid) {
                let replacement = plan_replacement(&tx, &input, &edit)?;
                let identity = replacement_id(
                    &input.draft,
                    &input.replacements,
                    edit.revision,
                    line_revision,
                )?;
                edit.preview["replacement"] = json!({
                    "command":identity,
                    "revision":edit.revision,
                    "line_revision":line_revision,
                    "impact":replacement.impact,
                    "over_budget":replacement.over_budget,
                });
            }
            Ok((edit.repertoire, edit.revision, line_revision, edit.preview))
        },
    )
    .await?;
    check_fresh(&app, repertoire.clone(), revision).await?;
    let path = app.database.clone();
    let current = tokio::task::spawn_blocking(move || {
        opening_replacement::revision(&Database::read(&path)?, &repertoire)
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    if current != line_revision {
        return Err(ApiError(Stale.into()));
    }
    Ok(Json(view))
}

#[derive(Serialize)]
struct Staged {
    position: String,
    old: Option<String>,
    new: String,
    valid: bool,
}

fn stage(edit: &mut Edit, choices: &opening_replacement::Choices) -> Vec<Staged> {
    let mut staged = Vec::new();
    for (position, movement) in choices {
        let mut item = Staged {
            position: position.clone(),
            old: None,
            new: movement.clone(),
            valid: false,
        };
        if let Some(conflict) = edit.conflicts.get(position)
            && let Some(saved) = &conflict.saved
            && let [reply] = conflict.proposed.as_slice()
            && reply.movement == *movement
            && saved.movement != *movement
        {
            item.old = Some(saved.reply.clone());
            item.new = reply.reply.clone();
            item.valid = true;
            edit.conflicts.remove(position);
        }
        staged.push(item);
    }
    edit.has_conflicts = !edit.conflicts.is_empty();
    staged
}

fn plan_replacement(
    c: &Connection,
    input: &Preview,
    edit: &Edit,
) -> Result<opening_replacement::Replacement> {
    let (scan, _, _) = coverage::selected(c, &input.draft.report, input.draft.gap)?;
    opening_replacement::plan(
        c,
        &edit.repertoire,
        &scan.color,
        &input.replacements,
        &edit.plans,
    )
}

fn replacement_id(
    draft: &Draft,
    choices: &opening_replacement::Choices,
    revision: i64,
    line_revision: i64,
) -> Result<String> {
    Ok(chess::id(&[
        "gap-replace-v1",
        &serde_json::to_string(draft)?,
        &serde_json::to_string(choices)?,
        &revision.to_string(),
        &line_revision.to_string(),
    ]))
}

#[derive(Deserialize)]
pub struct Replace {
    draft: Draft,
    replacements: opening_replacement::Choices,
    command: String,
    revision: i64,
    line_revision: i64,
}

pub async fn replace(State(app): State<App>, Json(input): Json<Replace>) -> ApiResult {
    let identity = replacement_id(
        &input.draft,
        &input.replacements,
        input.revision,
        input.line_revision,
    )
    .map_err(ApiError)?;
    if input.command != identity || input.replacements.is_empty() {
        return Err(ApiError(anyhow::anyhow!(
            "Preview and confirm the replacement before saving."
        )));
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
    let path = app.database.clone();
    let prepared = prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || {
            let mut c = Database::read(&path)?;
            let tx = c.transaction()?;
            if let Some(prior) = receipt(&tx, &identity)? {
                return Ok(Err(prior));
            }
            let preview = Preview {
                draft: input.draft,
                replacements: input.replacements,
            };
            let mut edit = build(&tx, preview.draft.clone())?;
            ensure!(
                edit.revision == input.revision
                    && opening_replacement::revision(&tx, &edit.repertoire)? == input.line_revision,
                Stale
            );
            let staged = stage(&mut edit, &preview.replacements);
            ensure!(
                !edit.has_conflicts && staged.iter().all(|choice| choice.valid),
                Conflicts
            );
            let replacement = plan_replacement(&tx, &preview, &edit)?;
            edit.command = identity;
            Ok(Ok((edit, replacement)))
        },
    )
    .await?;
    match prepared {
        Ok((edit, replacement)) => {
            write_db(app, move |c| commit_replacement(c, edit, replacement)).await
        }
        Err(prior) => Ok(Json(prior)),
    }
}

fn commit_replacement(
    c: &mut Connection,
    edit: Edit,
    replacement: opening_replacement::Replacement,
) -> Result<Value> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(prior) = receipt(&tx, &edit.command)? {
        return Ok(prior);
    }
    ensure!(
        coverage::revision(&tx, &edit.repertoire)? == edit.revision,
        Stale
    );
    opening_replacement::apply(&tx, &edit.repertoire, &replacement)?;
    let lines = edit
        .plans
        .iter()
        .map(|plan| catalog::apply_opening(&tx, plan, Some(&edit.repertoire), true))
        .collect::<Result<Vec<_>>>()?;
    let reply = json!({"command":edit.command,"lines":lines});
    tx.execute(
        "INSERT INTO gap_save_receipt VALUES(?1,?2)",
        (&edit.command, reply.to_string()),
    )?;
    tx.commit()?;
    Ok(reply)
}

async fn check_fresh(app: &App, repertoire: String, revision: i64) -> Result<(), ApiError> {
    let path = app.database.clone();
    let current = tokio::task::spawn_blocking(move || {
        coverage::revision(&Database::read(&path)?, &repertoire)
    })
    .await
    .map_err(|e| ApiError(e.into()))?
    .map_err(ApiError)?;
    if current != revision {
        return Err(ApiError(Stale.into()));
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct Correct {
    draft: Draft,
    position: String,
    movement: Option<String>,
}

pub async fn correct(State(app): State<App>, Json(input): Json<Correct>) -> ApiResult {
    use anyhow::Context;
    let path = app.database.clone();
    let (repertoire, revision, result) = prepare(
        &app.content_preparation,
        || ApiError(writer::Unavailable.into()),
        move || -> Result<_> {
            let mut c = Database::read(&path)?;
            let tx = c.transaction()?;
            let edit = build(&tx, input.draft.clone())?;
            let conflict = edit
                .conflicts
                .get(&input.position)
                .context("Preview this conflict again.")?;
            let saved = conflict
                .saved
                .as_ref()
                .context("This position has no single saved reply.")?;
            let reply = if let Some(movement) = input.movement {
                conflict
                    .proposed
                    .iter()
                    .find(|reply| reply.movement == movement)
                    .context("That reply is no longer in this draft.")?
            } else {
                saved
            };
            let correction = if conflict.proposed.len() == 1
                && conflict.proposed[0].movement == reply.movement
            {
                gap_pgn::Correction {
                    pgn: input.draft.pgn,
                    trims: vec![],
                }
            } else {
                gap_pgn::correct(
                    &input.draft.pgn,
                    &edit.anchor,
                    &input.position,
                    &reply.movement,
                )?
            };
            Ok((edit.repertoire, edit.revision, json!(correction)))
        },
    )
    .await?;
    check_fresh(&app, repertoire, revision).await?;
    Ok(Json(result))
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

    ensure!(!edit.has_conflicts, Conflicts);
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

    #[tokio::test]
    async fn preview_and_replace_transposed_reply_preserves_history_and_replays() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let database = directory.path().join("fixture.db");
        let mut c = Database::init(&database)?;
        let main = |pgn| Ok::<_, anyhow::Error>(chess::parse_gap_pgn(pgn)?.main);
        let original = main("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bb5 a6 5. Bxc6 *")?;
        let repertoire = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Old line".into(),
                color: "white".into(),
                route: original.clone(),
                start_at: 0,
            },
            None,
        )?;
        let sibling = main("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bb5 h6 5. a3 *")?;
        let sibling = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Retained suffix".into(),
                color: "white".into(),
                route: sibling,
                start_at: 0,
            },
            Some(&repertoire),
        )?;
        let old_position = chess::position_id(&chess::board_at(&original, 6)?);
        let old_id: String = c.query_row(
            "SELECT id FROM cards WHERE position_id=?1 AND repertoire_id=?2",
            (&old_position, &repertoire),
            |row| row.get(0),
        )?;
        c.execute("INSERT INTO learning VALUES (?1,1,1,1,2)", [&old_id])?;
        crate::study_store::issue(&c, Some((&old_id, "opening")))?;
        let dataset = crate::db::dataset(&c)?;
        let attempt: i64 = c.query_row("SELECT active FROM study", [], |row| row.get(0))?;
        let draft = Draft {
            report: "scan".into(),
            gap: 0,
            pgn: "1. Nf3 Nc6 2. e4 e5 3. Nc3 Nf6 4. d4 *".into(),
        };
        let route = main(&draft.pgn)?;
        let anchor = chess::board_at(&route, 2)?;
        let scan = json!({"repertoire":repertoire,"color":"white","route":{"fen":chess::fen(&chess::position("")?),"moves":[]},"through":6});
        let forecast = json!({"model":"maia3-5m-1500-history-v1","gaps":[{"fen":chess::fen(&anchor),"moves":["g1f3","b8c6"],"probability":1.0}],"reach":{},"paths":[],"ended":0.0,"conflict":0.0,"unresolved":0.0});
        c.execute(
            "INSERT INTO coverage_report VALUES (?1,?2,?3,?4,?5)",
            (
                &draft.report,
                &repertoire,
                coverage::revision(&c, &repertoire)?,
                scan.to_string(),
                forecast.to_string(),
            ),
        )?;
        coverage::backfill_views(&c)?;
        let candidate = build(&c, draft.clone())?;
        assert_eq!(candidate.conflicts.len(), 1);
        assert!(candidate.conflicts[&old_position].transposition);
        assert_eq!(
            candidate.conflicts[&old_position]
                .saved
                .as_ref()
                .unwrap()
                .reply,
            "4. Bb5"
        );
        let clean = build(
            &c,
            Draft {
                pgn: draft.pgn.replace("4. d4", "4. Bb5"),
                ..draft.clone()
            },
        )?;
        assert!(!clean.has_conflicts);
        assert_eq!(clean.preview["new_replies"], 1);

        let alternatives = build(
            &c,
            Draft {
                pgn: draft.pgn.replace("4. d4", "4. d4 (4. Bc4)"),
                ..draft.clone()
            },
        )?;
        assert_eq!(alternatives.conflicts[&old_position].proposed.len(), 2);

        let choices = opening_replacement::Choices::from([(old_position.clone(), "d2d4".into())]);
        let mut edition = build(&c, draft.clone())?;
        assert!(stage(&mut edition, &choices)[0].valid);
        let replacement =
            opening_replacement::plan(&c, &repertoire, "white", &choices, &edition.plans)?;
        assert_eq!(replacement.impact.len(), 2);
        assert!(!replacement.over_budget);
        let before = edition.revision;
        edition.command = replacement_id(&draft, &choices, before, replacement.line_revision)?;
        let confirmation = json!({"draft":draft,"replacements":choices,"command":edition.command,"revision":before,"line_revision":replacement.line_revision});

        let other = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Another already-authored route".into(),
                color: "white".into(),
                route: original.clone(),
                start_at: 0,
            },
            Some(&repertoire),
        )?;
        assert!(
            commit_replacement(&mut c, edition, replacement)
                .unwrap_err()
                .is::<Stale>()
        );
        assert_eq!(coverage::revision(&c, &repertoire)?, before);

        let app = std::sync::Arc::new(crate::Runtime::new(
            database.clone(),
            writer::Writer::start(c, None)?,
            crate::uploads::Staging::start(&database)?,
        ));
        let request = |value: Value| serde_json::from_value::<Replace>(value).unwrap();
        let old = replace(State(app.clone()), Json(request(confirmation.clone()))).await;
        assert!(old.err().unwrap().0.is::<Stale>());
        let view = preview(
            State(app.clone()),
            Json(Preview {
                draft: draft.clone(),
                replacements: choices.clone(),
            }),
        )
        .await
        .map_err(|e| e.0)?
        .0;
        assert_eq!(view["replacement"]["impact"].as_array().unwrap().len(), 3);
        let confirmed = json!({"draft":draft,"replacements":choices,"command":view["replacement"]["command"],"revision":view["replacement"]["revision"],"line_revision":view["replacement"]["line_revision"]});
        let first = replace(State(app.clone()), Json(request(confirmed.clone())))
            .await
            .map_err(|e| e.0)?
            .0;
        let retry = replace(State(app.clone()), Json(request(confirmed)))
            .await
            .map_err(|e| e.0)?
            .0;
        assert_eq!(first, retry);
        let mut c = Database::open(&database)?;
        for id in [&repertoire, &other, &sibling] {
            assert!(
                c.query_row("SELECT archived FROM openings WHERE id=?1", [id], |row| row
                    .get::<_, bool>(0))?
            );
        }
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM openings WHERE archived=0",
                [],
                |row| row.get::<_, usize>(0)
            )?,
            4
        );
        assert_eq!(
            saved_reply(&c, &repertoire, &old_position)?
                .unwrap()
                .answers,
            r#"["d2d4"]"#
        );
        assert!(c.query_row(
            "SELECT opening_retired FROM cards WHERE id=?1",
            [&old_id],
            |row| row.get::<_, bool>(0)
        )?);
        assert_eq!(
            c.query_row(
                "SELECT due_at FROM learning WHERE card_id=?1",
                [&old_id],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        let input = json!({"dataset":dataset,"attempt":attempt,"played":"f1b5"});
        let intent: crate::study::AnswerWire = serde_json::from_value(input)?;
        let study = crate::review::answer(&mut c, intent.into())?;
        assert_eq!(serde_json::to_value(study)?["rating"], "Good");
        Ok(())
    }

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
            anchor: String::new(),
            conflicts: BTreeMap::new(),
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
