use crate::{
    game_pipeline,
    job_store::{self, JobReads},
    providers::{self, Provider},
    writer::Writer,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

pub const MAX_PGN: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
pub struct StartJob {
    pub provider: String,
    pub operation: String,
    pub username: Option<String>,
    pub pgn: Option<String>,
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
    pub cursor: Option<String>,
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

pub fn worker(path: &Path, writer: Writer) -> Result<()> {
    let db = JobReads::open(path)?;
    while writer.background(job_store::recover_running)? == 128 {}
    let client = Provider::client()?;
    loop {
        match writer.background(job_store::next) {
            Ok(Some(job)) => {
                if let Err(error) = run(path, &db, &writer, &client, &job) {
                    let message = error.to_string();
                    let id = job.id;
                    writer.background(move |db| job_store::fail(db, id, &message))?;
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_secs(1)),
            Err(error) => {
                eprintln!("Games worker could not claim work: {error}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn scan_game(
    db: &JobReads,
    writer: &Writer,
    job: i64,
    game: String,
    cursor: Option<String>,
) -> Result<()> {
    let Some(found) = db.findings(job, &game)? else {
        return yield_turn(writer, job, cursor, 0);
    };
    let save = |mismatches: Vec<_>, captures: Vec<_>| {
        let game = game.clone();
        writer.background(move |db| {
            game_pipeline::save_findings(db, job, &game, &mismatches, &captures)
        })
    };
    let mut mismatches = found.mismatches.into_iter();
    loop {
        let batch: Vec<_> = mismatches.by_ref().take(120).collect();
        if batch.is_empty() {
            break;
        }
        save(batch, Vec::new())?;
    }
    let mut captures = found.captures.into_iter();
    loop {
        let batch: Vec<_> = captures.by_ref().take(120).collect();
        if batch.is_empty() {
            break;
        }
        save(Vec::new(), batch)?;
    }
    writer.background(move |db| game_pipeline::finish_scan(db, job, &game, cursor.as_deref()))
}

fn run(path: &Path, db: &JobReads, writer: &Writer, client: &Client, job: &Job) -> Result<()> {
    let id = job.id;
    let mut upload_file = None;
    let checkpoint = match (job.provider.as_str(), job.operation.as_str()) {
        ("pgn", "upload") => {
            let upload = db.upload(id)?;
            let offset = job
                .cursor
                .as_deref()
                .map(str::parse::<u64>)
                .transpose()?
                .unwrap_or(0);
            let Some(upload) = upload else {
                let pgn = db.inline_upload(id)?.context("PGN is missing")?;
                let mut staged = crate::uploads::stage(path, &pgn)?;
                return writer.background(move |db| {
                    job_store::stage_inline_upload(db, id, staged.id())?;
                    staged.commit();
                    Ok(())
                });
            };
            let record = providers::upload_record(
                crate::uploads::open(path, &upload)?,
                offset,
                &job.username,
            )?;
            match record {
                Some((Some(game), next)) => {
                    return writer
                        .background(move |db| game_pipeline::archive_upload(db, &game, id, next));
                }
                Some((None, next)) => {
                    return yield_turn(writer, id, Some(next.to_string()), 1);
                }
                None => upload_file = Some(upload),
            }
            None
        }
        ("archive", "scan") => {
            let mut cursor = ArchiveCursor::load(db, job.cursor.as_deref())?;
            if let Some(game) = db.archive_next(&cursor.after, cursor.through)? {
                cursor.after.clone_from(&game);
                return scan_game(db, writer, id, game, Some(serde_json::to_string(&cursor)?));
            }
            None
        }
        ("archive", "scan_new") => {
            if let Some(game) = db.pending_game(id)? {
                return scan_game(db, writer, id, game, None);
            }
            None
        }
        ("archive", "publish") => {
            if let Some(game) = db.pending_game(id)? {
                let plans = db.publication_batch(&game)?;
                writer.background(move |db| game_pipeline::publish_batch(db, id, &game, &plans))?;
                return Ok(());
            }
            None
        }
        (name, "all" | "new") => {
            match provider_turn(
                db,
                writer,
                client,
                job,
                Provider::named(name).context("unknown provider")?,
            )? {
                ProviderTurn::Yielded => return Ok(()),
                ProviderTurn::Complete {
                    through,
                    rejected_floor,
                } => Some((through, rejected_floor)),
            }
        }
        _ => bail!("unknown provider"),
    };
    let provider = job.provider.clone();
    let username = job.username.to_ascii_lowercase();
    writer.background(move |db| job_store::complete(db, id, &provider, &username, checkpoint))?;
    if let Some(file) = upload_file {
        crate::uploads::remove(path, &file);
    }
    Ok(())
}

fn yield_turn(writer: &Writer, id: i64, cursor: Option<String>, rejected: usize) -> Result<()> {
    writer.background(move |db| {
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

enum ProviderTurn {
    Yielded,
    Complete { through: i64, rejected_floor: usize },
}

fn provider_turn(
    db: &JobReads,
    writer: &Writer,
    client: &Client,
    job: &Job,
    provider: Provider,
) -> Result<ProviderTurn> {
    let user = job.username.to_ascii_lowercase();
    let id = job.id;
    let mut cursor = match job.cursor.as_deref() {
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
                None => provider.first_date(client, &user)?,
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
        let windows = provider.windows(client, &user, cursor.from, cursor.through)?;
        cursor.freeze(provider, windows);
        yield_turn(writer, id, Some(serde_json::to_string(&cursor)?), 0)?;
        return Ok(ProviderTurn::Yielded);
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
        return Ok(ProviderTurn::Complete {
            through: cursor.through,
            rejected_floor: cursor.recounted_rejected.unwrap_or_default(),
        });
    };
    let rejected = Provider::supply(client, &job.username, window, |game| {
        writer.background(move |db| game_pipeline::archive_import(db, &game, id))
    })?;
    let rejected = cursor.advance(rejected);
    yield_turn(writer, id, Some(serde_json::to_string(&cursor)?), rejected)?;
    Ok(ProviderTurn::Yielded)
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
