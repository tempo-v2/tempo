use crate::{
    game_pipeline,
    job_store::{self, FailedTurn, JobReads},
    providers::{self, Provider},
    scanner::{Detector, Observation},
    uploads::Staging,
    writer::Writer,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::{Duration, Instant},
};

pub const MAX_PGN: usize = 64 * 1024 * 1024;
const GAME_TURN: Duration = Duration::from_secs(60);
const CLAIM_WAIT: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartJob {
    pub provider: String,
    pub operation: String,
}

pub enum NewJob {
    Command(StartJob),
    Upload { username: String, file: String },
}

#[derive(Serialize)]
pub struct JobView {
    pub id: i64,
    pub provider: String,
    pub operation: String,
    pub status: String,
    pub added: i64,
    pub existing: i64,
    pub rejected: i64,
    pub found: i64,
    pub error: Option<String>,
}

pub struct Job {
    pub id: i64,
    pub provider: String,
    pub username: String,
    pub operation: String,
    pub window_offset: u64,
    pub window_rejected: usize,
    pub replay_upload: bool,
}

pub fn valid_user(user: &str) -> Result<&str> {
    let user = user.trim();
    ensure!(
        !user.is_empty()
            && user.len() <= 80
            && user
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c)),
        "enter a valid player name"
    );
    Ok(user)
}

pub fn worker(path: &Path, writer: Writer, staging: Staging) -> Result<()> {
    let db = JobReads::open(path)?;
    loop {
        match writer.background_until(Instant::now() + CLAIM_WAIT, job_store::recover_running) {
            Ok(128) => {}
            Ok(_) => break,
            Err(error) => {
                eprintln!("Games worker could not recover jobs: {error}");
                std::thread::sleep(CLAIM_WAIT);
            }
        }
    }
    let client = Provider::client()?;
    let mut failed: Option<FailedTurn> = None;
    loop {
        let prior = failed.clone();
        let admission_deadline = Instant::now() + CLAIM_WAIT;
        match writer.background_until(admission_deadline, move |db| {
            job_store::next(db, prior.as_ref())
        }) {
            Ok(Some(job)) => {
                let deadline = Instant::now() + GAME_TURN;
                let result = db.within_turn(deadline, || {
                    run(&db, &writer, &client, &staging, &job, deadline)
                });
                // Temporary staging pressure leaves the existing job at its
                // committed checkpoint. Rotate so a cleanup never owns the runner.
                let result = match result {
                    Err(error) if error.is::<crate::uploads::Unavailable>() => {
                        let turned =
                            yield_turn(&writer, Instant::now() + CLAIM_WAIT, job.id, None, 0);
                        std::thread::sleep(Duration::from_millis(100));
                        turned
                    }
                    result => result,
                };
                failed = result.err().map(|error| FailedTurn {
                    job_id: job.id,
                    message: error.to_string(),
                });
            }
            Ok(None) => {
                failed = None;
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(error) => {
                eprintln!("Games worker could not claim work: {error}");
                std::thread::sleep(CLAIM_WAIT);
            }
        }
    }
}

/// Each worker turn either seals one immutable game analysis or applies one
/// batch from the exact sealed file. Repertoire edits cannot change a retry.
fn detector_turn(
    db: &JobReads,
    writer: &Writer,
    staging: &Staging,
    job: &Job,
    detector: Detector,
    whole_archive: bool,
    deadline: Instant,
) -> Result<bool> {
    let id = job.id;
    let (version, sealed) = db.detector_checkpoint(id)?;
    let mut cursor = if whole_archive {
        Some(ArchiveCursor::load(db, db.cursor(id)?.as_deref())?)
    } else {
        None
    };
    if let Some((game, file)) = sealed {
        let (findings, after, finished) =
            read_detector_batch(staging, &file, job.window_offset, deadline)?;
        if let Some(cursor) = cursor.as_mut() {
            cursor.after.clone_from(&game);
        }
        let batch = game_pipeline::DetectorBatch {
            job: id,
            detector,
            version,
            game,
            file: Some(file),
            before: job.window_offset,
            after,
            findings,
            finished,
            cursor: cursor
                .map(|cursor| serde_json::to_string(&cursor))
                .transpose()?,
        };
        writer.background_until(deadline, move |db| {
            game_pipeline::apply_detector(db, &batch)
        })?;
        if finished {
            staging.released();
        }
        return Ok(true);
    }
    let game = match cursor.as_ref() {
        Some(cursor) => db.archive_next(&cursor.after, cursor.through)?,
        None => db.pending_game(id)?,
    };
    let Some(game) = game else { return Ok(false) };
    let bytes = db.detector_plan(&game, detector.program(version)?, deadline)?;
    if bytes.is_empty() {
        if let Some(cursor) = cursor.as_mut() {
            cursor.after.clone_from(&game);
        }
        let batch = game_pipeline::DetectorBatch {
            job: id,
            detector,
            version,
            game,
            file: None,
            before: job.window_offset,
            after: 0,
            findings: Vec::new(),
            finished: true,
            cursor: cursor
                .map(|cursor| serde_json::to_string(&cursor))
                .transpose()?,
        };
        writer.background_until(deadline, move |db| {
            game_pipeline::apply_detector(db, &batch)
        })?;
        return Ok(true);
    }
    let staged = staging.worker(&bytes)?;
    writer.background_until(deadline, move |db| {
        game_pipeline::seal_detector(db, id, &game, staged.id())
    })?;
    Ok(true)
}

fn read_detector_batch(
    staging: &Staging,
    file: &str,
    offset: u64,
    deadline: Instant,
) -> Result<(Vec<Observation>, u64, bool)> {
    let mut input = BufReader::new(staging.open(file, deadline)?);
    input.seek(SeekFrom::Start(offset))?;
    let mut findings = Vec::with_capacity(120);
    let mut after = offset;
    let mut line = Vec::new();
    for _ in 0..120 {
        line.clear();
        let read = (&mut input).take(64 * 1024).read_until(b'\n', &mut line)?;
        if read == 0 {
            return Ok((findings, after, true));
        }
        ensure!(line.last() == Some(&b'\n'), "invalid detector record");
        findings.push(serde_json::from_slice(&line)?);
        after += read as u64;
    }
    Ok((findings, after, input.fill_buf()?.is_empty()))
}

fn run(
    db: &JobReads,
    writer: &Writer,
    client: &Client,
    staging: &Staging,
    job: &Job,
    deadline: Instant,
) -> Result<()> {
    let id = job.id;
    let mut upload_file = None;
    match (job.provider.as_str(), job.operation.as_str()) {
        ("pgn", "upload") => {
            let upload = db.upload(id)?;
            let offset = if job.replay_upload {
                job.window_offset
            } else {
                db.cursor(id)?
                    .as_deref()
                    .map(str::parse::<u64>)
                    .transpose()?
                    .unwrap_or(0)
            };
            let Some(upload) = upload else {
                let pgn = db.inline_upload(id)?.context("PGN is missing")?;
                ensure!(Instant::now() < deadline, "Games turn timed out.");
                let staged = staging.manual(&pgn)?;
                return writer.background_until(deadline, move |db| {
                    job_store::stage_inline_upload(db, id, staged.id())?;
                    Ok(())
                });
            };
            let record =
                providers::upload_record(staging.open(&upload, deadline)?, offset, &job.username)?;
            let replay = job.replay_upload;
            match record {
                Some((Some(game), next)) => {
                    return writer.background_until(deadline, move |db| {
                        if replay {
                            game_pipeline::archive_staged_record(db, &game, id, next)
                        } else {
                            game_pipeline::archive_upload(db, &game, id, next)
                        }
                    });
                }
                Some((None, next)) => {
                    if replay {
                        return writer.background_until(deadline, move |db| {
                            job_store::reject_staged_record(db, id, next)
                        });
                    }
                    return yield_turn(writer, deadline, id, Some(next.to_string()), 1);
                }
                None => upload_file = Some(upload),
            }
        }
        ("archive", "scan") => {
            let cursor = ArchiveCursor::load(db, db.cursor(id)?.as_deref())?;
            let saved = serde_json::to_string(&cursor)?;
            return writer.background_until(deadline, move |db| {
                game_pipeline::start_scan(db, id, &saved)
            });
        }
        ("archive", operation) if Detector::from_job(operation).is_some() => {
            let (detector, whole_archive) = Detector::from_job(operation).unwrap();
            if detector_turn(db, writer, staging, job, detector, whole_archive, deadline)? {
                return Ok(());
            }
        }
        ("archive", "publish") => {
            if let Some(game) = db.pending_game(id)? {
                let plans = db.publication_batch(&game)?;
                writer.background_until(deadline, move |db| {
                    game_pipeline::publish_batch(db, id, &game, &plans)
                })?;
                return Ok(());
            }
        }
        (name, "all" | "new") => {
            return provider_turn(
                db,
                writer,
                client,
                staging,
                job,
                Provider::named(name).context("unknown provider")?,
                deadline,
            );
        }
        _ => bail!("unknown provider"),
    }
    let provider = job.provider.clone();
    let username = job.username.to_ascii_lowercase();
    writer.background_until(deadline, move |db| {
        job_store::complete(db, id, &provider, &username, None)
    })?;
    if upload_file.is_some() {
        staging.released();
    }
    Ok(())
}

fn yield_turn(
    writer: &Writer,
    deadline: Instant,
    id: i64,
    cursor: Option<String>,
    rejected: usize,
) -> Result<()> {
    writer.background_until(deadline, move |db| {
        let tx = db.transaction()?;
        job_store::yield_turn(&tx, id, cursor.as_deref(), rejected)?;
        tx.commit()?;
        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
struct ArchiveCursor {
    through: i64,
    after: String,
}

impl ArchiveCursor {
    fn load(db: &JobReads, saved: Option<&str>) -> Result<Self> {
        match saved {
            Some(json) if json.starts_with('{') => Ok(serde_json::from_str(json)?),
            after => Ok(Self {
                through: db.archive_end()?,
                after: after.unwrap_or_default().to_owned(),
            }),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ProviderCursor {
    from: i64,
    through: i64,
    next: usize,
    windows: Option<Vec<String>>,
    recounted_rejected: Option<usize>,
}

impl ProviderCursor {
    fn freeze(&mut self, provider: Provider, windows: Vec<String>) {
        // Schema 3 recorded an ordinal against a mutable Chess.com month list.
        // Reconcile every current month because inserting one before that ordinal can skip a month.
        if matches!(provider, Provider::ChessCom) && self.next != 0 {
            self.next = 0;
            self.recounted_rejected = Some(0);
        }
        self.windows = Some(windows);
    }

    fn advance(&mut self, rejected: usize) -> usize {
        self.next += 1;
        if let Some(total) = self.recounted_rejected.as_mut() {
            *total += rejected;
            0
        } else {
            rejected
        }
    }
}

fn provider_turn(
    db: &JobReads,
    writer: &Writer,
    client: &Client,
    staging: &Staging,
    job: &Job,
    provider: Provider,
    deadline: Instant,
) -> Result<()> {
    let user = job.username.to_ascii_lowercase();
    let id = job.id;
    let upload = db.upload(id)?;
    if let Some(file) = &upload {
        let input = staging.open(file, deadline)?;
        match providers::upload_record(input, job.window_offset, &job.username)? {
            Some((Some(game), offset)) => {
                writer.background_until(deadline, move |db| {
                    game_pipeline::archive_staged_record(db, &game, id, offset)
                })?;
                return Ok(());
            }
            Some((None, offset)) => {
                writer.background_until(deadline, move |db| {
                    job_store::reject_staged_record(db, id, offset)
                })?;
                return Ok(());
            }
            None => {}
        }
    }
    let mut cursor = match db.cursor(id)?.as_deref() {
        Some(json) => serde_json::from_str::<ProviderCursor>(json)?,
        None => {
            let previous = if job.operation == "new" {
                db.checkpoint(&job.provider, &user)?
            } else {
                None
            };
            let through = Utc::now().timestamp_millis();
            let from = match previous {
                Some(value) => value.saturating_sub(86_400_000),
                None => provider.first_date(client, &user, deadline)?,
            };
            ProviderCursor {
                from,
                through,
                next: 0,
                windows: None,
                recounted_rejected: None,
            }
        }
    };
    if cursor.windows.is_none() {
        let windows = provider.windows(client, &user, cursor.from, cursor.through, deadline)?;
        cursor.freeze(provider, windows);
        let saved = serde_json::to_string(&cursor)?;
        writer.background_until(deadline, move |db| {
            job_store::checkpoint_provider_window(db, id, &saved, None, 0)
        })?;
        return Ok(());
    }
    let windows = cursor
        .windows
        .as_ref()
        .context("provider inventory is missing")?;
    ensure!(
        cursor.next <= windows.len(),
        "saved provider checkpoint exceeds the window inventory"
    );
    let Some(window) = windows.get(cursor.next) else {
        let provider = job.provider.clone();
        let checkpoint = (
            cursor.through,
            cursor.recounted_rejected.unwrap_or_default(),
        );
        return writer.background_until(deadline, move |db| {
            job_store::complete(db, id, &provider, &user, Some(checkpoint))
        });
    };
    if upload.is_none() {
        let pgn = Provider::download(client, window, deadline)?;
        if !pgn.trim().is_empty() {
            ensure!(Instant::now() < deadline, "Provider turn timed out.");
            let staged = staging.worker(pgn.as_bytes())?;
            let saved = serde_json::to_string(&cursor)?;
            writer.background_until(deadline, move |db| {
                job_store::checkpoint_provider_window(db, id, &saved, Some(staged.id()), 0)?;
                Ok(())
            })?;
            return Ok(());
        }
    }
    let rejected = cursor.advance(job.window_rejected);
    let saved = serde_json::to_string(&cursor)?;
    writer.background_until(deadline, move |db| {
        job_store::checkpoint_provider_window(db, id, &saved, None, rejected)
    })?;
    if upload.is_some() {
        staging.released();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predecessor_chesscom_cursor_reconciles_changed_window_inventory() -> Result<()> {
        let mut saved: ProviderCursor =
            serde_json::from_str(r#"{"from":0,"through":100,"next":1}"#)?;
        saved.freeze(
            Provider::ChessCom,
            vec!["inserted".into(), "original".into(), "last".into()],
        );
        assert_eq!(saved.next, 0);
        assert_eq!(saved.advance(2), 0);
        let mut restarted: ProviderCursor = serde_json::from_str(&serde_json::to_string(&saved)?)?;
        assert_eq!(
            restarted.windows.as_ref().unwrap()[restarted.next],
            "original"
        );
        assert_eq!(restarted.advance(3), 0);
        assert_eq!(restarted.recounted_rejected, Some(5));
        let mut lichess: ProviderCursor =
            serde_json::from_str(r#"{"from":0,"through":100,"next":1}"#)?;
        lichess.freeze(Provider::Lichess, vec!["original".into(), "last".into()]);
        assert_eq!(lichess.next, 1);
        assert_eq!(lichess.advance(2), 2);
        Ok(())
    }
}
