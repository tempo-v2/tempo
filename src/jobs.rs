use crate::{
    game_pipeline,
    job_store::{self, ARCHIVE_RESUME_PAGE, ArchiveGame, FailedTurn, JobReads},
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
    let cursor = if whole_archive {
        let cursor = ArchiveCheckpoint::decode(db.cursor(id)?.as_deref())?;
        // Freeze older plain-ID checkpoints once before reading any archive work.
        let cursor = match cursor {
            ArchiveCheckpoint::Unfrozen { after } => {
                let legacy = ArchiveCheckpoint::Legacy {
                    through: db.archive_end()?,
                    after,
                };
                let saved = if sealed.is_some() {
                    legacy
                } else {
                    legacy.into_ordinal(db)?
                };
                yield_turn(
                    writer,
                    deadline,
                    id,
                    Some(serde_json::to_string(&saved)?),
                    0,
                )?;
                return Ok(true);
            }
            legacy @ ArchiveCheckpoint::Legacy { .. } if sealed.is_none() => {
                let saved = legacy.into_ordinal(db)?;
                yield_turn(
                    writer,
                    deadline,
                    id,
                    Some(serde_json::to_string(&saved)?),
                    0,
                )?;
                return Ok(true);
            }
            cursor => cursor,
        };
        Some(cursor)
    } else {
        None
    };
    if let Some((game, file)) = sealed {
        let (findings, after, finished) =
            read_detector_batch(staging, &file, job.window_offset, deadline)?;
        let cursor = cursor
            .map(|cursor| {
                if !finished {
                    return Ok(cursor);
                }
                let ordinal = db.archive_ordinal(&game)?;
                cursor.after_game(ArchiveGame {
                    ordinal,
                    id: game.clone(),
                })
            })
            .transpose()?;
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
    let (game, next_cursor) = match cursor {
        Some(ArchiveCheckpoint::Ordinal { mut v2 }) => match v2.next(db)? {
            ArchiveStep::Game(game) => {
                v2.after_ordinal = game.ordinal;
                (Some(game.id), Some(ArchiveCheckpoint::Ordinal { v2 }))
            }
            ArchiveStep::SkipTo(ordinal) => {
                v2.after_ordinal = ordinal;
                let saved = serde_json::to_string(&ArchiveCheckpoint::Ordinal { v2 })?;
                yield_turn(writer, deadline, id, Some(saved), 0)?;
                return Ok(true);
            }
            ArchiveStep::Complete => (None, None),
        },
        Some(_) => bail!("archive checkpoint was not converted"),
        None => (db.pending_game(id)?, None),
    };
    let Some(game) = game else { return Ok(false) };
    let bytes = db.detector_plan(&game, detector.program(version)?, deadline)?;
    if bytes.is_empty() {
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
            cursor: next_cursor
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
    let mut findings = Vec::with_capacity(game_pipeline::MAX_FINDINGS_PER_APPLY);
    let mut after = offset;
    let mut line = Vec::new();
    for _ in 0..game_pipeline::MAX_FINDINGS_PER_APPLY {
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
            let cursor = ArchiveCheckpoint::decode(db.cursor(id)?.as_deref())?.into_ordinal(db)?;
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
#[serde(untagged, deny_unknown_fields)]
enum ArchiveCheckpoint {
    Ordinal {
        v2: ArchiveCursor,
    },
    Legacy {
        through: i64,
        after: String,
    },
    #[serde(skip)]
    Unfrozen {
        after: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveCursor {
    through: i64,
    after_ordinal: i64,
    legacy_after_id: Option<String>,
}

impl ArchiveCheckpoint {
    fn decode(saved: Option<&str>) -> Result<Self> {
        match saved {
            Some(json) if json.starts_with('{') => {
                let checkpoint = serde_json::from_str(json)?;
                match &checkpoint {
                    Self::Ordinal { v2 } => ensure!(
                        (0..=v2.through).contains(&v2.after_ordinal),
                        "invalid archive ordinal checkpoint"
                    ),
                    Self::Legacy { through, .. } => {
                        ensure!(*through >= 0, "invalid archive cutoff")
                    }
                    Self::Unfrozen { .. } => unreachable!(),
                }
                Ok(checkpoint)
            }
            after => Ok(Self::Unfrozen {
                after: after.unwrap_or_default().to_owned(),
            }),
        }
    }

    fn from_legacy(through: i64, after: String) -> Self {
        Self::Ordinal {
            v2: ArchiveCursor {
                through,
                after_ordinal: 0,
                legacy_after_id: (!after.is_empty()).then_some(after),
            },
        }
    }

    fn into_ordinal(self, db: &JobReads) -> Result<Self> {
        Ok(match self {
            Self::Ordinal { .. } => self,
            Self::Legacy { through, after } => Self::from_legacy(through, after),
            Self::Unfrozen { after } => Self::from_legacy(db.archive_end()?, after),
        })
    }

    fn after_game(self, game: ArchiveGame) -> Result<Self> {
        match self {
            Self::Ordinal { mut v2 } => {
                ensure!(
                    game.ordinal <= v2.through
                        && game.ordinal > v2.after_ordinal
                        && v2.includes_id(&game.id),
                    "sealed game is outside its archive checkpoint"
                );
                v2.after_ordinal = game.ordinal;
                Ok(Self::Ordinal { v2 })
            }
            Self::Legacy { through, after } => {
                ensure!(
                    game.ordinal <= through && game.id > after,
                    "sealed game is outside its legacy archive checkpoint"
                );
                // The old traversal completed every ID up to this game, regardless of ordinal.
                Ok(Self::from_legacy(through, game.id))
            }
            Self::Unfrozen { .. } => bail!("archive cutoff was not saved"),
        }
    }
}

enum ArchiveStep {
    Game(ArchiveGame),
    SkipTo(i64),
    Complete,
}

impl ArchiveCursor {
    fn includes_id(&self, id: &str) -> bool {
        self.legacy_after_id
            .as_deref()
            .is_none_or(|after| id > after)
    }

    fn next(&self, db: &JobReads) -> Result<ArchiveStep> {
        let limit = if self.legacy_after_id.is_some() {
            ARCHIVE_RESUME_PAGE
        } else {
            1
        };
        let page = db.archive_page(self.after_ordinal, self.through, limit)?;
        let last = page.last().map(|game| game.ordinal);
        Ok(
            match page.into_iter().find(|game| self.includes_id(&game.id)) {
                Some(game) => ArchiveStep::Game(game),
                None => last.map_or(ArchiveStep::Complete, ArchiveStep::SkipTo),
            },
        )
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
