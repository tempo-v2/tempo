//! Games' addressed feedback read and command, and the worker's sealed report.
use crate::{
    catalog::{self, REPERTOIRE_PAGE, Repertoire},
    chess,
    game_pipeline::{MAX_APPLY_ROWS, MAX_FINDINGS_PER_APPLY, Mismatch, write_mismatch},
    games, job_store,
    scanner::{self, REPERTOIRE_VERSION, RepertoireDecision, RepertoireOutcome},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior::Immediate, params,
};
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub const MAX_REPORT_BYTES: usize = 256 * 1024;
const MAX_DECISIONS: usize = chess::MAX_PLIES / 2;

#[derive(Debug)]
pub struct QueueFull;
impl std::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The game comparison queue is full. Try again shortly.")
    }
}
impl std::error::Error for QueueFull {}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Pair {
    pub game: String,
    pub repertoire: String,
}

#[derive(Default, Deserialize)]
pub struct ChoicesQuery {
    #[serde(default)]
    pub after: String,
    pub selected: Option<String>,
}

#[derive(Serialize)]
pub struct Choices {
    game: String,
    learner_color: String,
    choices: Vec<Repertoire>,
    selected: Option<Repertoire>,
    more: bool,
}

pub fn choices(db: &mut Connection, game: String, input: ChoicesQuery) -> Result<Choices> {
    ensure!(
        input.after.len() <= 128 && input.selected.as_ref().is_none_or(|id| id.len() <= 128),
        "invalid repertoire cursor"
    );
    let snapshot = db.transaction()?;
    let learner_color = games::learner_color(&snapshot, &game)?;
    let mut choices = catalog::repertoires_for_color(&snapshot, &learner_color, &input.after)?;
    let more = choices.len() > REPERTOIRE_PAGE;
    choices.truncate(REPERTOIRE_PAGE);
    let selected = input
        .selected
        .map(|id| catalog::repertoire(&snapshot, &id, &learner_color))
        .transpose()?
        .flatten();
    Ok(Choices {
        game,
        learner_color,
        choices,
        selected,
        more,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    NotCompared,
    Queued,
    Running,
    Failed,
    Complete,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct CompletedBasis {
    generation: i64,
    repertoire_revision: i64,
    comparator_version: i64,
}

#[derive(Serialize)]
pub struct Status {
    #[serde(flatten)]
    pair: Pair,
    learner_color: String,
    revision: i64,
    generation: Option<i64>,
    state: State,
    error: Option<String>,
    completed: Option<CompletedBasis>,
    current: bool,
    legacy_only: bool,
}

struct Work {
    job: i64,
    generation: i64,
    state: State,
    error: Option<String>,
    completed: Option<CompletedBasis>,
}

impl Work {
    fn active(&self) -> bool {
        matches!(self.state, State::Queued | State::Running)
    }

    fn current(&self, revision: i64) -> bool {
        self.completed.as_ref().is_some_and(|basis| {
            basis.repertoire_revision == revision && basis.comparator_version == REPERTOIRE_VERSION
        })
    }
}

fn work(db: &Connection, pair: &Pair) -> Result<Option<Work>> {
    let row = db
        .query_row(
            "SELECT f.job_id,f.generation,j.status,substr(j.error,1,512),
                f.completed_generation,f.repertoire_revision,f.comparator_version
         FROM repertoire_feedback f CROSS JOIN jobs j ON j.id=f.job_id
         WHERE f.game_id=?1 AND f.repertoire_id=?2",
            (&pair.game, &pair.repertoire),
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, String>(2)?,
                    row.get(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(job, generation, state, error, completed, repertoire_revision, comparator_version)| {
            let state = match state.as_str() {
                "queued" => State::Queued,
                "running" => State::Running,
                "failed" => State::Failed,
                "done" => State::Complete,
                _ => anyhow::bail!("unknown comparison job status"),
            };
            let completed = match (completed, repertoire_revision, comparator_version) {
                (Some(generation), Some(repertoire_revision), Some(comparator_version)) => {
                    Some(CompletedBasis {
                        generation,
                        repertoire_revision,
                        comparator_version,
                    })
                }
                (None, None, None) => None,
                _ => anyhow::bail!("incomplete comparison basis"),
            };
            Ok(Work {
                job,
                generation,
                state,
                error,
                completed,
            })
        },
    )
    .transpose()
}

fn status(
    db: &Connection,
    pair: Pair,
    color: String,
    revision: i64,
    work: Option<Work>,
) -> Result<Status> {
    let current = work.as_ref().is_some_and(|work| work.current(revision));
    let (generation, state, error, completed) = work
        .map_or((None, State::NotCompared, None, None), |w| {
            (Some(w.generation), w.state, w.error, w.completed)
        });
    let legacy_only = completed.is_none()
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM mismatches
         WHERE game_id=?1 AND repertoire_id=?2 AND detector_version=1)",
            (&pair.game, &pair.repertoire),
            |row| row.get(0),
        )?;
    Ok(Status {
        pair,
        learner_color: color,
        revision,
        generation,
        state,
        error,
        completed,
        current,
        legacy_only,
    })
}

fn scope(db: &Connection, pair: &Pair) -> Result<(String, i64)> {
    let color = games::learner_color(db, &pair.game)?;
    catalog::existing_repertoire(db, &pair.repertoire, &color)?;
    Ok((
        color,
        catalog::authored_reply_revision(db, &pair.repertoire)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub revision: i64,
    pub generation: Option<i64>,
}

pub fn request(db: &mut Connection, pair: Pair, request: Request) -> Result<Status> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let (color, revision) = scope(&tx, &pair)?;
    let mut saved = work(&tx, &pair)?;
    if request.revision == revision
        && request.generation == saved.as_ref().map(|work| work.generation)
        && !saved
            .as_ref()
            .is_some_and(|work| work.active() || work.current(revision))
    {
        enqueue(&tx, &pair, saved.as_ref())?.context(QueueFull)?;
        saved = work(&tx, &pair)?;
    }
    let view = status(&tx, pair, color, revision, saved)?;
    tx.commit()?;
    Ok(view)
}

fn enqueue(tx: &Transaction<'_>, pair: &Pair, saved: Option<&Work>) -> Result<Option<i64>> {
    let full: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs INDEXED BY jobs_active_feedback
         WHERE operation='compare_game' AND status IN ('queued','running') LIMIT 1 OFFSET 3)",
        [],
        |row| row.get(0),
    )?;
    if full {
        return Ok(None);
    }
    let generation = match saved {
        Some(saved) => {
            ensure!(tx.execute(
                "UPDATE jobs SET status='queued',error=NULL WHERE id=?1 AND status IN ('done','failed')",
                [saved.job],
            )? == 1, "comparison job changed");
            tx.query_row(
                "UPDATE repertoire_feedback SET generation=generation+1
                 WHERE job_id=?1 RETURNING generation",
                [saved.job],
                |row| row.get(0),
            )?
        }
        None => {
            tx.execute(
                "INSERT INTO jobs(provider,username,operation,status,detector_version)
                 VALUES ('archive','','compare_game','queued',?1)",
                [REPERTOIRE_VERSION],
            )?;
            tx.execute(
                "INSERT INTO repertoire_feedback(game_id,repertoire_id,job_id,generation)
                 VALUES (?1,?2,?3,1)",
                params![pair.game, pair.repertoire, tx.last_insert_rowid()],
            )?;
            1
        }
    };
    Ok(Some(generation))
}

/// A feeder can join exact durable work or cheaply reuse a current result.
pub enum Admission {
    Current,
    Wait(i64),
    Full,
}

pub fn feed(tx: &Transaction<'_>, pair: &Pair) -> Result<Admission> {
    let (_, revision) = scope(tx, pair)?;
    let saved = work(tx, pair)?;
    if let Some(saved) = &saved {
        if saved.current(revision) {
            return Ok(Admission::Current);
        }
        if saved.active() {
            return Ok(Admission::Wait(saved.generation));
        }
    }
    Ok(match enqueue(tx, pair, saved.as_ref())? {
        Some(generation) => Admission::Wait(generation),
        None => Admission::Full,
    })
}

pub fn feed_settled(db: &Connection, pair: &Pair, generation: i64) -> Result<bool> {
    let saved = work(db, pair)?.context("missing accepted comparison")?;
    if saved
        .completed
        .as_ref()
        .is_some_and(|basis| basis.generation >= generation)
    {
        return Ok(true);
    }
    ensure!(
        saved.state != State::Failed,
        "Repertoire comparison failed: {}",
        saved.error.as_deref().unwrap_or("retry the comparison.")
    );
    ensure!(saved.active(), "comparison completed without a report");
    Ok(false)
}

#[derive(Default, Deserialize)]
pub struct ReadQuery {
    pub known_result: Option<i64>,
}

#[derive(Serialize)]
pub struct View {
    #[serde(flatten)]
    status: Status,
    report: Option<Report>,
}

pub fn read(db: &mut Connection, pair: Pair, input: ReadQuery) -> Result<View> {
    let snapshot = db.transaction()?;
    let (color, revision) = scope(&snapshot, &pair)?;
    let saved = work(&snapshot, &pair)?;
    let status = status(&snapshot, pair, color, revision, saved)?;
    let report = status
        .completed
        .as_ref()
        .filter(|basis| input.known_result != Some(basis.generation))
        .map(|_| -> Result<Report> {
            let bytes: Vec<u8> = snapshot.query_row(
                "SELECT report FROM repertoire_feedback WHERE game_id=?1 AND repertoire_id=?2",
                (&status.pair.game, &status.pair.repertoire),
                |row| row.get(0),
            )?;
            Ok(serde_json::from_slice(&bytes)?)
        })
        .transpose()?;
    Ok(View { status, report })
}

#[derive(Serialize, Deserialize)]
pub struct Report {
    #[serde(flatten)]
    pair: Pair,
    learner_color: String,
    repertoire_revision: i64,
    comparator_version: i64,
    followed: usize,
    differed: usize,
    unassessed: usize,
    decisions: Vec<RepertoireDecision>,
}

pub fn prepare(db: &Connection, pair: Pair, deadline: Instant) -> Result<Vec<u8>> {
    let snapshot = db.unchecked_transaction()?;
    let (learner_color, repertoire_revision) = scope(&snapshot, &pair)?;
    let game = games::read(&snapshot, &pair.game)?;
    let mut report = Report {
        pair,
        learner_color,
        repertoire_revision,
        comparator_version: REPERTOIRE_VERSION,
        followed: 0,
        differed: 0,
        unassessed: 0,
        decisions: Vec::new(),
    };
    for decision in scanner::decisions(&game.mainline, game.learner)? {
        ensure!(Instant::now() < deadline, "Games turn timed out.");
        let reply = catalog::comparison_reply_at(
            &snapshot,
            &decision.position,
            &report.pair.repertoire,
            &report.learner_color,
        )?;
        let comparison = scanner::compare(&decision, reply);
        match comparison.outcome {
            RepertoireOutcome::Followed { .. } => report.followed += 1,
            RepertoireOutcome::Differed { .. } => report.differed += 1,
            RepertoireOutcome::Unassessed { .. } => report.unassessed += 1,
        }
        report.decisions.push(comparison);
    }
    snapshot.commit()?;
    let bytes = serde_json::to_vec(&report)?;
    ensure!(
        report.decisions.len() <= MAX_DECISIONS && bytes.len() <= MAX_REPORT_BYTES,
        "repertoire comparison exceeds its report limit"
    );
    Ok(bytes)
}

pub struct Claim {
    pub pair: Pair,
    pub generation: i64,
}

pub fn claim(db: &Connection, job: i64) -> Result<Claim> {
    Ok(db.query_row(
        "SELECT game_id,repertoire_id,generation FROM repertoire_feedback WHERE job_id=?1",
        [job],
        |row| {
            Ok(Claim {
                pair: Pair {
                    game: row.get(0)?,
                    repertoire: row.get(1)?,
                },
                generation: row.get(2)?,
            })
        },
    )?)
}

pub struct Batch {
    claim: Claim,
    job: i64,
    file: String,
    before: usize,
    through: usize,
    bytes: Vec<u8>,
    report: Report,
}

impl Batch {
    pub fn decode(
        job: i64,
        claim: Claim,
        file: String,
        before: usize,
        bytes: Vec<u8>,
    ) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_REPORT_BYTES,
            "invalid comparison file size"
        );
        let report: Report = serde_json::from_slice(&bytes).context("invalid saved comparison")?;
        ensure!(
            report.comparator_version == REPERTOIRE_VERSION
                && report.pair.game == claim.pair.game
                && report.pair.repertoire == claim.pair.repertoire
                && report.decisions.len() <= MAX_DECISIONS
                && before <= report.decisions.len()
                && report
                    .decisions
                    .iter()
                    .all(|row| row.ply < chess::MAX_PLIES)
                && report
                    .decisions
                    .windows(2)
                    .all(|pair| pair[0].ply < pair[1].ply),
            "invalid saved comparison basis"
        );
        let (mut followed, mut differed, mut unassessed) = (0, 0, 0);
        for decision in &report.decisions {
            match decision.outcome {
                RepertoireOutcome::Followed { .. } => followed += 1,
                RepertoireOutcome::Differed { .. } => differed += 1,
                RepertoireOutcome::Unassessed { .. } => unassessed += 1,
            }
        }
        ensure!(
            (followed, differed, unassessed)
                == (report.followed, report.differed, report.unassessed),
            "invalid saved comparison counts"
        );
        let through = (before + MAX_FINDINGS_PER_APPLY).min(report.decisions.len());
        Ok(Batch {
            job,
            claim,
            file,
            before,
            through,
            bytes,
            report,
        })
    }

    pub fn finished(&self) -> bool {
        self.through == self.report.decisions.len()
    }
}

pub fn apply(db: &mut Connection, batch: &Batch) -> Result<()> {
    let tx = db.transaction_with_behavior(Immediate)?;
    let before = tx.total_changes();
    let finished = batch.finished();
    ensure!(
        tx.execute(
            "UPDATE jobs SET window_offset=CASE WHEN ?5 THEN 0 ELSE ?6 END,
                upload_file=CASE WHEN ?5 THEN NULL ELSE upload_file END,
                detector_game=CASE WHEN ?5 THEN NULL ELSE detector_game END,
                status=CASE WHEN ?5 THEN 'done' ELSE status END
         WHERE id=?1 AND detector_game=?2 AND upload_file=?3 AND window_offset=?4
           AND operation='compare_game' AND status='running' AND detector_version=?7
           AND EXISTS(SELECT 1 FROM repertoire_feedback WHERE job_id=?1 AND generation=?8)",
            params![
                batch.job,
                batch.claim.pair.game,
                batch.file,
                batch.before,
                finished,
                batch.through,
                REPERTOIRE_VERSION,
                batch.claim.generation
            ],
        )? == 1,
        "comparison checkpoint changed"
    );
    for decision in &batch.report.decisions[batch.before..batch.through] {
        if let RepertoireOutcome::Differed { answer_version, .. } = &decision.outcome {
            write_mismatch(
                &tx,
                Mismatch {
                    game: &batch.claim.pair.game,
                    ply: decision.ply,
                    position: &decision.position,
                    repertoire: &batch.claim.pair.repertoire,
                    answer_version,
                    played: &decision.played.uci,
                    detector_version: REPERTOIRE_VERSION,
                },
            )?;
        }
    }
    if finished {
        let changed = tx.execute(
            "UPDATE repertoire_feedback SET completed_generation=?2,repertoire_revision=?3,
                    comparator_version=?4,report=?5
             WHERE job_id=?1 AND generation=?2",
            params![
                batch.job,
                batch.claim.generation,
                batch.report.repertoire_revision,
                REPERTOIRE_VERSION,
                batch.bytes
            ],
        )?;
        ensure!(changed == 1, "comparison work changed");
    } else {
        job_store::yield_turn(&tx, batch.job, None, 0)?;
    }
    ensure!(
        tx.total_changes() - before <= MAX_APPLY_ROWS,
        "comparison apply exceeds row budget"
    );
    tx.commit()?;
    Ok(())
}

/// A damaged or lost sealed file can be recomputed on explicit retry. The
/// existing complete result and immutable legacy observations stay available.
pub fn clear_damaged_checkpoint(db: &Connection, job: i64, file: &str) -> Result<()> {
    ensure!(
        db.execute(
            "UPDATE jobs SET upload_file=NULL,detector_game=NULL,window_offset=0
         WHERE id=?1 AND upload_file=?2 AND status='running' AND operation='compare_game'",
            (job, file),
        )? == 1,
        "comparison checkpoint changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests;

/// Completed comparison identity and the exact learner scope for external assessment.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MissBasis {
    pub report_generation: i64,
    pub comparator_version: i64,
    pub repertoire_revision: i64,
}

#[derive(Serialize)]
pub struct MissDecision {
    pub position: String,
    pub expected_uci: String,
    pub answer_version: String,
    pub occurrences: Vec<MissOccurrence>,
}

#[derive(Serialize)]
pub struct MissOccurrence {
    pub ply: usize,
    pub played_uci: String,
}

#[derive(Debug)]
pub enum MissError {
    InvalidScope,
    StaleReport,
    InvalidDecision,
}

impl std::fmt::Display for MissError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidScope => "The game or selected same-color repertoire is unavailable.",
            Self::StaleReport => {
                "Prepare with the current completed comparison for this game and repertoire."
            }
            Self::InvalidDecision => {
                "Select a differing learner decision and all its occurrences at that position."
            }
        })
    }
}
impl std::error::Error for MissError {}

pub fn check_miss_basis(db: &Connection, pair: &Pair, expected: MissBasis) -> Result<String> {
    let color = games::archived_learner_color(db, &pair.game)?.ok_or(MissError::InvalidScope)?;
    ensure!(
        catalog::repertoire(db, &pair.repertoire, &color)?.is_some(),
        MissError::InvalidScope
    );
    let revision = catalog::authored_reply_revision(db, &pair.repertoire)?;
    let current = db
        .query_row(
            "SELECT completed_generation,repertoire_revision,comparator_version
         FROM repertoire_feedback WHERE game_id=?1 AND repertoire_id=?2
         AND completed_generation IS NOT NULL",
            (&pair.game, &pair.repertoire),
            |r| {
                Ok(MissBasis {
                    report_generation: r.get(0)?,
                    repertoire_revision: r.get(1)?,
                    comparator_version: r.get(2)?,
                })
            },
        )
        .optional()?;
    ensure!(
        current == Some(expected)
            && expected.repertoire_revision == revision
            && expected.comparator_version == REPERTOIRE_VERSION,
        MissError::StaleReport
    );
    Ok(color)
}

pub fn miss_decision(
    db: &Connection,
    pair: &Pair,
    basis: MissBasis,
    position: &str,
    plies: &[usize],
) -> Result<(String, MissDecision)> {
    let color = check_miss_basis(db, pair, basis)?;
    let bytes: Vec<u8> = db
        .query_row(
            "SELECT report FROM repertoire_feedback WHERE game_id=?1 AND repertoire_id=?2
         AND length(CAST(report AS BLOB))<=?3",
            params![pair.game, pair.repertoire, MAX_REPORT_BYTES],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(MissError::StaleReport)?;
    let report: Report = serde_json::from_slice(&bytes)?;
    ensure!(
        report.pair.game == pair.game
            && report.pair.repertoire == pair.repertoire
            && report.learner_color == color
            && report.repertoire_revision == basis.repertoire_revision
            && report.comparator_version == basis.comparator_version
            && report.decisions.len() <= MAX_DECISIONS,
        MissError::StaleReport
    );
    let mut reply: Option<(String, String)> = None;
    let mut occurrences = Vec::new();
    for decision in report
        .decisions
        .into_iter()
        .filter(|d| d.position == position)
    {
        if let RepertoireOutcome::Differed {
            expected,
            answer_version,
        } = decision.outcome
        {
            let current = (expected.uci, answer_version);
            ensure!(
                reply.as_ref().is_none_or(|original| original == &current),
                MissError::InvalidDecision
            );
            reply = Some(current);
            occurrences.push(MissOccurrence {
                ply: decision.ply,
                played_uci: decision.played.uci,
            });
        }
    }
    let (expected_uci, answer_version) = reply.ok_or(MissError::InvalidDecision)?;
    occurrences.sort_by_key(|entry| entry.ply);
    ensure!(
        occurrences
            .iter()
            .map(|entry| entry.ply)
            .eq(plies.iter().copied()),
        MissError::InvalidDecision
    );
    Ok((
        color,
        MissDecision {
            position: position.to_owned(),
            expected_uci,
            answer_version,
            occurrences,
        },
    ))
}
