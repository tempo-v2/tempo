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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<DraftAnchor>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DraftAnchor {
    RecordedLeadIn,
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
    recorded_moves: Option<Vec<String>>,
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
    let prefix = draft
        .anchor
        .map(|DraftAnchor::RecordedLeadIn| coverage::recorded_lead_in(&scan, &gap));
    if let Some(prefix) = &prefix {
        let board = chess::board_at(prefix, prefix.moves.len())?;
        ensure!(
            chess::position_id(&board) == target,
            "The recorded lead-in does not reach this gap."
        );
    }
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
        let anchor = match &prefix {
            Some(prefix) => {
                let ply = prefix.moves.len();
                if route.moves.len() <= ply
                    || !same_route(
                        prefix,
                        &chess::Route {
                            fen: route.fen.clone(),
                            moves: route.moves[..ply].to_vec(),
                        },
                    )?
                {
                    continue;
                }
                Some(ply)
            }
            None => first_anchor(&route, &target)?,
        };
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
        if prefix.is_some() {
            "Continue the recorded lead-in before adding the reply."
        } else {
            "The PGN must reach this gap and supply a reply."
        }
    );
    let mut responses = BTreeMap::<String, Vec<&catalog::OpeningCard>>::new();
    for card in plans.iter().flat_map(|plan| &plan.cards) {
        responses
            .entry(card.position.clone())
            .or_default()
            .push(card);
    }
    let mut rows = vec![];
    let mut pending_conflicts = Vec::new();
    let mut wanted_contexts = BTreeMap::new();
    let mut new_responses = 0;
    for (position, cards) in responses {
        let prior = catalog::current_opening_reply(c, &scan.repertoire, &position)?;
        let unanimous = cards.iter().all(|card| card.version == cards[0].version);
        let status = match &prior {
            None if unanimous => {
                new_responses += 1;
                "new"
            }
            Some(prior) if !prior.quarantined && unanimous && prior.version == cards[0].version => {
                "existing"
            }
            _ => {
                if let Some(saved) = &prior
                    && !saved.quarantined
                {
                    wanted_contexts.insert(position.clone(), saved.version.clone());
                }
                pending_conflicts.push((position.clone(), prior, cards.clone()));
                "conflict"
            }
        };
        rows.push(
            json!({"move":first_reply(&cards[0].answers)?,"status":status,"position":position}),
        );
    }
    let mut contexts = catalog::reply_contexts(c, &scan.repertoire, &scan.color, wanted_contexts)?;
    let conflicts = pending_conflicts
        .into_iter()
        .map(|(position, saved, cards)| {
            let current_route = contexts.routes.remove(&position);
            let missing_current = current_route.is_none();
            let saved_route = match (&saved, current_route) {
                (Some(reply), None) if !reply.quarantined => {
                    let movement = first_reply(&reply.answers)?;
                    catalog::recorded_opening_context(c, &reply.id)?
                        .filter(|route| gap_pgn::line_text(route, &position, &movement).is_ok())
                }
                (_, current) => current,
            };
            let recorded = missing_current && saved_route.is_some();
            let mut conflict = describe_conflict(&position, &cards, saved.as_ref(), saved_route)?;
            conflict.saved_route_limited =
                missing_current && contexts.limited && conflict.saved.is_some();
            conflict.saved_route_recorded = recorded;
            Ok((position, conflict))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let batched = 1
        + catalog::OPENING_LINE_WRITES * plans.len()
        + catalog::OPENING_REPLY_WRITES * new_responses
        > catalog::ONLINE_WRITE_LIMIT
        || has_checkpoint(c, &command)?;
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
        recorded_moves: prefix.map(|route| route.moves),
        conflicts,
    })
}

fn first_anchor(route: &chess::Route, target: &str) -> Result<Option<usize>> {
    let mut board = chess::position(&route.fen)?;
    for (ply, movement) in route.moves.iter().enumerate() {
        if chess::position_id(&board) == target {
            return Ok(Some(ply));
        }
        let legal = chess::parse_move(&board, movement)?;
        board.play_unchecked(legal);
    }
    Ok(None)
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
    movement: String,
    reply: String,
    line: Option<String>,
    alternate_line: Option<String>,
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
    saved_route_limited: bool,
    saved_route_recorded: bool,
}

fn same_route(a: &chess::Route, b: &chess::Route) -> Result<bool> {
    let mut board = chess::position(&a.fen)?;
    if a.moves.len() != b.moves.len()
        || chess::position_id(&board) != chess::position_id(&chess::position(&b.fen)?)
    {
        return Ok(false);
    }
    for (left, right) in a.moves.iter().zip(&b.moves) {
        let movement = chess::parse_move(&board, left)?;
        if chess::parse_move(&board, right).ok() != Some(movement) {
            return Ok(false);
        }
        board.play_unchecked(movement);
    }
    Ok(true)
}

fn describe_conflict(
    position: &str,
    cards: &[&catalog::OpeningCard],
    saved: Option<&catalog::CurrentOpeningReply>,
    saved_route: Option<chess::Route>,
) -> Result<Conflict> {
    let board = chess::position(&cards[0].fen)?;
    let mut transposition = false;
    let mut proposals = BTreeMap::<&str, (Reply, bool)>::new();
    for card in cards {
        let route: chess::Route = serde_json::from_str(&card.context)?;
        let different = saved_route
            .as_ref()
            .map(|saved| same_route(saved, &route))
            .transpose()?
            .is_some_and(|same| !same);
        transposition |= different;
        if let Some((reply, first_is_different)) = proposals.get_mut(card.version.as_str()) {
            if different && !*first_is_different && reply.alternate_line.is_none() {
                reply.alternate_line = Some(gap_pgn::line_text(&route, position, &reply.movement)?);
            }
            continue;
        }
        let movement = first_reply(&card.answers)?;
        let reply = Reply {
            line: Some(gap_pgn::line_text(&route, position, &movement)?),
            reply: gap_pgn::reply_text(&chess::position(&card.fen)?, &movement)?,
            movement,
            alternate_line: None,
        };
        proposals.insert(&card.version, (reply, different));
    }
    let unresolved = saved.is_some_and(|saved| saved.quarantined);
    let saved = saved
        .filter(|saved| !saved.quarantined)
        .map(|saved| -> Result<Reply> {
            let movement = first_reply(&saved.answers)?;
            let position_board = saved_route
                .as_ref()
                .map(|route| chess::board_at(route, route.moves.len()))
                .transpose()?;
            Ok(Reply {
                reply: gap_pgn::reply_text(position_board.as_ref().unwrap_or(&board), &movement)?,
                line: saved_route
                    .as_ref()
                    .map(|route| gap_pgn::line_text(route, position, &movement))
                    .transpose()?,
                movement,
                alternate_line: None,
            })
        })
        .transpose()?;
    Ok(Conflict {
        position: position.into(),
        turn: chess::color_name(board.turn()),
        number: board.fullmoves().get(),
        saved,
        proposed: proposals.into_values().map(|(reply, _)| reply).collect(),
        transposition,
        unresolved,
        saved_route_limited: false,
        saved_route_recorded: false,
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
    let before = tx.total_changes();
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
    ensure!(
        tx.total_changes() - before <= catalog::ONLINE_WRITE_LIMIT as u64,
        opening_replacement::OverBudget
    );
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
                    edit.recorded_moves.as_deref(),
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
        apply_batches(&app.writer, &app.database, edit)
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
    let before = tx.total_changes();
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
    if online {
        catalog::check_opening_write_budget(&tx, before)?;
    }
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
    resolved_lines: Vec<String>,
}

// Read the predecessor's completed line IDs once, before writer admission.
// These addressed records survive archive and aliasing.
fn predecessor_prefix(db: &Connection, edit: &Edit) -> Result<Vec<String>> {
    let line: Option<usize> = db
        .query_row(
            "SELECT line FROM gap_batch WHERE command=?1 AND resolved_lines IS NULL",
            [&edit.command],
            |row| row.get(0),
        )
        .optional()?;
    let Some(line) = line else {
        return Ok(Vec::new());
    };
    ensure!(
        line < edit.plans.len(),
        "Invalid predecessor Gaps line checkpoint"
    );
    let mut ids = Vec::with_capacity(line);
    let mut present = db.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM openings WHERE id=?1 AND repertoire_id=?2 AND color=?3)",
    )?;
    for plan in &edit.plans[..line] {
        let id = catalog::legacy_opening_id(plan, Some(&edit.repertoire));
        ensure!(
            present.query_row((&id, &edit.repertoire, &plan.color), |row| row
                .get::<_, bool>(0))?,
            "A completed Gaps line is missing from the predecessor checkpoint"
        );
        ids.push(id);
    }
    Ok(ids)
}

fn checkpoint(c: &Connection, edit: &Edit, predecessor_prefix: &[String]) -> Result<Option<Batch>> {
    let saved = c
        .query_row(
            "SELECT revision,line,card,total,resolved_lines FROM gap_batch WHERE command=?1",
            [&edit.command],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, usize>(1)?,
                    row.get::<_, usize>(2)?,
                    row.get::<_, usize>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((revision, line, card, total, resolved)) = saved else {
        return Ok(None);
    };
    ensure!(
        total == edit.plans.len() && line < total,
        "Invalid Gaps line checkpoint"
    );
    let resolved_lines = match resolved {
        Some(encoded) => serde_json::from_str(&encoded)?,
        None => predecessor_prefix.to_vec(),
    };
    ensure!(
        resolved_lines.len() == line,
        "Invalid Gaps resolved line checkpoint"
    );
    Ok(Some(Batch {
        revision,
        line,
        card,
        resolved_lines,
    }))
}

fn apply_batches(writer: &writer::Writer, path: &std::path::Path, edit: Edit) -> Result<Value> {
    let prefix = std::sync::Arc::new(predecessor_prefix(&Database::read(path)?, &edit)?);
    let edit = std::sync::Arc::new(edit);
    loop {
        let edit = edit.clone();
        let prefix = prefix.clone();
        if let Some(reply) = writer.background(move |c| batch_turn(c, &edit, &prefix))? {
            return Ok(reply);
        }
    }
}

fn batch_turn(
    c: &mut Connection,
    edit: &Edit,
    predecessor_prefix: &[String],
) -> Result<Option<Value>> {
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(prior) = receipt(&tx, &edit.command)? {
        return Ok(Some(prior));
    }
    let saved = checkpoint(&tx, edit, predecessor_prefix)?;
    let mut batch = saved.unwrap_or(Batch {
        revision: edit.revision,
        line: 0,
        card: 0,
        resolved_lines: Vec::new(),
    });
    if coverage::revision(&tx, &edit.repertoire)? != batch.revision {
        if batch.line != 0 || batch.card != 0 {
            anyhow::bail!(Interrupted(batch.line, batch.revision - edit.revision));
        }
        anyhow::bail!(Stale);
    }
    ensure!(!edit.has_conflicts, Conflicts);
    let before = tx.total_changes();
    let plan = &edit.plans[batch.line];
    match catalog::apply_opening_batch(&tx, plan, &edit.repertoire, batch.card)? {
        catalog::OpeningBatch::More(next) => batch.card = next,
        catalog::OpeningBatch::Finished(id) => {
            batch.resolved_lines.push(id);
            batch.line += 1;
            batch.card = 0;
        }
    }
    let result = if batch.line == edit.plans.len() {
        let reply = json!({"command": edit.command, "lines": batch.resolved_lines});
        tx.execute(
            "INSERT INTO gap_save_receipt VALUES(?1,?2)",
            (&edit.command, reply.to_string()),
        )?;
        tx.execute("DELETE FROM gap_batch WHERE command=?1", [&edit.command])?;
        Some(reply)
    } else {
        let revision = coverage::revision(&tx, &edit.repertoire)?;
        tx.execute(
            "INSERT INTO gap_batch(command,revision,line,card,total,resolved_lines) VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(command) DO UPDATE SET revision=excluded.revision,line=excluded.line,
                card=excluded.card,resolved_lines=excluded.resolved_lines",
            (&edit.command,revision,batch.line,batch.card,edit.plans.len(),serde_json::to_string(&batch.resolved_lines)?),
        )?;
        None
    };
    catalog::check_opening_write_budget(&tx, before)?;
    tx.commit()?;
    Ok(result)
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
    fn repeated_pasted_reply_keeps_the_transposed_route_visible() -> Result<()> {
        let authored = chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. Bb5 *")?.main;
        let canonical = chess::position_id(&chess::board_at(&authored, 6)?);
        let same = chess::parse_gap_pgn("1. e4 e5 2. Nf3 Nc6 3. Nc3 Nf6 4. d4 *")?.main;
        let other = chess::parse_gap_pgn("1. Nf3 Nc6 2. e4 e5 3. Nc3 Nf6 4. d4 *")?.main;
        let plan = |route| {
            catalog::prepare_opening(&catalog::OpeningInput {
                name: "Fixture".into(),
                color: "white".into(),
                route,
                start_at: 6,
            })
        };
        let saved = plan(authored.clone())?;
        let same = plan(same)?;
        let other = plan(other)?;
        let conflict = describe_conflict(
            &canonical,
            &[&same.cards[0], &other.cards[0]],
            Some(&catalog::CurrentOpeningReply {
                id: String::new(),
                version: saved.cards[0].version.clone(),
                quarantined: false,
                answers: saved.cards[0].answers.clone(),
            }),
            Some(chess::Route {
                fen: authored.fen,
                moves: authored.moves[..6].to_vec(),
            }),
        )?;
        assert_eq!(conflict.proposed.len(), 1);
        assert!(conflict.transposition);
        assert!(
            conflict.proposed[0]
                .line
                .as_deref()
                .unwrap()
                .starts_with("1. e4")
        );
        assert!(
            conflict.proposed[0]
                .alternate_line
                .as_deref()
                .unwrap()
                .starts_with("1. Nf3")
        );
        Ok(())
    }

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
        let independent = chess::Route {
            fen: chess::fen(&chess::board_at(&original, 8)?),
            moves: vec!["b5c6".into()],
        };
        catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Same reply from a separate setup".into(),
                color: "white".into(),
                route: independent.clone(),
                start_at: 0,
            },
            Some(&repertoire),
        )?;
        let context_only = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Historical lead-in only".into(),
                color: "white".into(),
                route: original.clone(),
                start_at: 8,
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
            anchor: None,
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
        let unrelated = serde_json::to_string(&main("1. e4 *")?)?;
        c.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<2000)
             INSERT INTO openings(id,repertoire_id,name,color,route,start_at)
             SELECT printf('!fixture-%04d',i),?1,'Other line','white',?2,0 FROM n",
            (&repertoire, unrelated),
        )?;
        let limited = build(&c, draft.clone())?;
        let from_record = &limited.conflicts[&old_position];
        assert!(from_record.saved_route_limited);
        assert!(from_record.saved_route_recorded);
        assert!(from_record.transposition);
        assert_eq!(
            from_record.saved.as_ref().unwrap().line,
            candidate.conflicts[&old_position]
                .saved
                .as_ref()
                .unwrap()
                .line
        );
        c.execute("DELETE FROM openings WHERE id GLOB '!fixture-*'", [])?;

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

        let line_revision = opening_replacement::revision(&c, &repertoire)?;
        let equivalent = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Same route under a different title".into(),
                color: "white".into(),
                route: original.clone(),
                start_at: 0,
            },
            Some(&repertoire),
        )?;
        assert_eq!(equivalent, repertoire);
        assert_eq!(
            opening_replacement::revision(&c, &repertoire)?,
            line_revision
        );
        let other = catalog::save_opening(
            &c,
            &catalog::OpeningInput {
                name: "Continue from a different authoring ply".into(),
                color: "white".into(),
                route: original.clone(),
                start_at: 2,
            },
            Some(&repertoire),
        )?;
        assert_ne!(other, repertoire);
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
            6
        );
        assert_eq!(
            catalog::current_opening_reply(&c, &repertoire, &old_position)?
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
        let surviving_position = chess::position_id(&chess::position(&independent.fen)?);
        let surviving =
            catalog::current_opening_reply(&c, &repertoire, &surviving_position)?.unwrap();
        assert_eq!(surviving.answers, r#"["b5c6"]"#);
        let routes = catalog::reply_contexts(
            &c,
            &repertoire,
            "white",
            BTreeMap::from([(surviving_position.clone(), surviving.version)]),
        )?;
        let surviving_route = &routes.routes[&surviving_position];
        assert_eq!(
            chess::position_id(&chess::board_at(
                surviving_route,
                surviving_route.moves.len()
            )?),
            surviving_position
        );
        assert!(!c.query_row(
            "SELECT archived FROM openings WHERE id=?1",
            [&context_only],
            |row| row.get::<_, bool>(0)
        )?);
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

    #[test]
    fn large_gap_batch_keeps_projection_writes_inside_each_turn() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut db = Database::init(&temp.path().join("batch.sqlite3"))?;
        let opening = |fen: &str, moves: &str| catalog::OpeningInput {
            name: "Fixture".into(),
            color: "white".into(),
            route: chess::Route {
                fen: fen.into(),
                moves: moves.split_whitespace().map(str::to_owned).collect(),
            },
            start_at: 0,
        };
        let repertoire = catalog::save_opening(
            &db,
            &opening(
                "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2",
                "g1f3",
            ),
            None,
        )?;
        let plan = catalog::prepare_opening(&opening(
            "",
            "g1f3 d7d5 f3g5 b8d7 a2a3 f7f5 a1a2 g8f6 h2h4 e7e6 h1h3 e6e5 a3a4 h7h5 g5e4
             e8e7 g2g3 f6g4 e4d6 f5f4 d2d4 a7a5 a2a1 e5e4 b2b4 h8h7 d6e8 b7b6 c1a3 g7g5
             c2c3 a8a7 e2e3 c8b7 f1a6 h7h6 e1d2 g4f6 a3c1 d7e5 d2e1 e5c6 b1a3 e7e6 c1b2
             f6d7 d1d3 f8g7 a1c1 c6b8 c1d1 d7e5 e1d2 e5f3 d2c2 b6b5 h3h1 e6f5 h1h2 g7f6
             a3b1 f6h8 b2a1 d8f6 b1d2 f6g6 e8f6 c7c5 d1c1 a7a8 h2g2 f3g1 f6h7 b8d7 d3e2
             g6b6 c1b1 c5c4 e2d3 g5g4 c2d1 f4f3 b1c1 d7f6 d2b3 b6d6 g2h2 h8g7 h7f8 g7h8
             f8e6 h6h7",
        ))?;
        let baseline = db.total_changes();
        assert!(
            catalog::apply_opening(&db, &plan, Some(&repertoire), true)
                .unwrap_err()
                .is::<catalog::OverBudget>()
        );
        assert_eq!(db.total_changes(), baseline);
        let edit = Edit {
            revision: coverage::revision(&db, &repertoire)?,
            repertoire,
            command: "large-batch".into(),
            plans: vec![plan],
            batched: true,
            has_conflicts: false,
            preview: Value::Null,
            anchor: String::new(),
            recorded_moves: None,
            conflicts: BTreeMap::new(),
        };
        assert!(batch_turn(&mut db, &edit, &[])?.is_none());
        assert_eq!(checkpoint(&db, &edit, &[])?.unwrap().card, 24);
        let first = db.total_changes() - baseline;
        let before = db.total_changes();
        let receipt = batch_turn(&mut db, &edit, &[])?.unwrap();
        let final_turn = db.total_changes() - before;
        assert_eq!((first, final_turn), (121, 115));
        assert_eq!(batch_turn(&mut db, &edit, &[])?, Some(receipt));
        Ok(())
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
            recorded_moves: None,
            conflicts: BTreeMap::new(),
        };
        assert!(batch_turn(&mut c, &edit, &[])?.is_none());
        assert!(receipt(&c, &edit.command)?.is_none());
        let completed = checkpoint(&c, &edit, &[])?.unwrap();
        assert_eq!(completed.line, 1);
        let predecessor_id = catalog::legacy_opening_id(&edit.plans[0], Some(&edit.repertoire));
        assert_ne!(completed.resolved_lines[0], predecessor_id);
        // Represent the predecessor's retained physical line and cursor, which
        // lacked a list of accepted line IDs.
        c.execute(
            "UPDATE openings SET id=?1 WHERE id=?2",
            (&predecessor_id, &completed.resolved_lines[0]),
        )?;
        c.execute(
            "UPDATE gap_batch SET resolved_lines=NULL WHERE command=?1",
            [&edit.command],
        )?;
        let prefix = predecessor_prefix(&c, &edit)?;
        assert_eq!(prefix.as_slice(), std::slice::from_ref(&predecessor_id));
        c.execute_batch(
            "CREATE TRIGGER reject_gap_receipt BEFORE INSERT ON gap_save_receipt
            BEGIN SELECT RAISE(ABORT, 'interrupted'); END;",
        )?;
        assert!(batch_turn(&mut c, &edit, &prefix).is_err());
        assert_eq!(checkpoint(&c, &edit, &prefix)?.unwrap().line, 1);
        assert_eq!(coverage::revision(&c, &edit.repertoire)?, edit.revision + 1);
        c.execute_batch("DROP TRIGGER reject_gap_receipt;")?;
        drop(c);
        let mut c = Database::open(path)?;
        let accepted = batch_turn(&mut c, &edit, &prefix)?.unwrap();
        assert_eq!(accepted["lines"][0], predecessor_id);
        let persisted_suffix: String =
            c.query_row("SELECT id FROM openings WHERE name='Second'", [], |r| {
                r.get(0)
            })?;
        assert_eq!(accepted["lines"][1], persisted_suffix);
        assert_eq!(receipt(&c, &edit.command)?, Some(accepted.clone()));
        assert!(checkpoint(&c, &edit, &[])?.is_none());
        assert_eq!(batch_turn(&mut c, &edit, &[])?, Some(accepted));
        assert_eq!(coverage::revision(&c, &edit.repertoire)?, edit.revision + 2);
        Ok(())
    }
}
