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

fn scan_game(db: &JobReads, writer: &Writer, job: i64, game: &str) -> Result<()> {
    let Some(found) = db.findings(job, game)? else {
        return Ok(());
    };
    let game = game.to_owned();
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
    writer.background(move |db| game_pipeline::finish_scan(db, job, &game))
}

fn run(path: &Path, db: &JobReads, writer: &Writer, client: &Client, job: &Job) -> Result<()> {
    let id = job.id;
    let mut upload_file = None;
    let checkpoint = match (job.provider.as_str(), job.operation.as_str()) {
        ("pgn", "upload") => {
            let (upload, legacy) = db.upload(id)?;
            let offset = job
                .cursor
                .as_deref()
                .map(str::parse::<u64>)
                .transpose()?
                .unwrap_or(0);
            let accept = |game, next: u64| match game {
                Some(game) => {
                    writer.background(move |db| game_pipeline::archive_upload(db, &game, id, next))
                }
                None => save_cursor(writer, id, next.to_string(), 1),
            };
            if let Some(file) = upload.as_deref() {
                providers::resume_upload(
                    crate::uploads::open(path, file)?,
                    offset,
                    &job.username,
                    accept,
                )?;
            } else {
                let pgn = legacy.context("PGN is missing")?;
                providers::resume_upload(std::io::Cursor::new(pgn), offset, &job.username, accept)?;
            }
            upload_file = upload;
            None
        }
        ("archive", "scan") => {
            let mut cursor = job.cursor.clone().unwrap_or_default();
            loop {
                let games = db.archive(&cursor)?;
                if games.is_empty() {
                    break;
                }
                for game in &games {
                    scan_game(db, writer, id, &game.id)?;
                }
                cursor = games.last().unwrap().id.clone();
                save_cursor(writer, id, cursor.clone(), 0)?;
            }
            None
        }
        ("archive", operation @ ("scan_new" | "publish")) => {
            loop {
                let games = db.pending_games(id)?;
                if games.is_empty() {
                    break;
                }
                for game in games {
                    if operation == "scan_new" {
                        scan_game(db, writer, id, &game)?;
                    } else {
                        let plans = db.publication_batch(&game)?;
                        let done = writer.background(move |db| {
                            game_pipeline::publish_batch(db, id, &game, &plans)
                        })?;
                        if !done {
                            break;
                        }
                    }
                }
            }
            None
        }
        (name, "all" | "new") => Some(fetch_provider(
            db,
            writer,
            client,
            job,
            Provider::named(name).context("unknown provider")?,
        )?),
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

fn save_cursor(writer: &Writer, id: i64, cursor: String, rejected: usize) -> Result<()> {
    writer.background(move |db| job_store::save_cursor(db, id, &cursor, rejected))
}

#[derive(Serialize, Deserialize)]
struct ProviderCursor {
    from: i64,
    through: i64,
    next: usize,
}

fn fetch_provider(
    db: &JobReads,
    writer: &Writer,
    client: &reqwest::blocking::Client,
    job: &Job,
    provider: Provider,
) -> Result<i64> {
    let user = job.username.to_ascii_lowercase();
    let mut cursor = match &job.cursor {
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
            let cursor = ProviderCursor {
                from,
                through,
                next: 0,
            };
            save_cursor(writer, job.id, serde_json::to_string(&cursor)?, 0)?;
            cursor
        }
    };
    let id = job.id;
    let windows = provider.windows(client, &user, cursor.from, cursor.through)?;
    for (index, window) in windows.iter().enumerate().skip(cursor.next) {
        let rejected = Provider::supply(client, &job.username, window, |game| {
            writer.background(move |db| game_pipeline::archive_import(db, &game, id))
        })?;
        cursor.next = index + 1;
        save_cursor(writer, id, serde_json::to_string(&cursor)?, rejected)?;
    }
    Ok(cursor.through)
}
