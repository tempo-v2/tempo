use crate::{
    db::Database,
    game_pipeline, games,
    providers::{self, Provider},
    writer::Writer,
};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::blocking::Client;
use rusqlite::{Connection, OptionalExtension, params};
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

struct Job {
    id: i64,
    provider: String,
    username: String,
    operation: String,
    cursor: Option<String>,
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

pub fn settings(db: &Connection) -> Result<std::collections::BTreeMap<String, String>> {
    let mut q = db.prepare("SELECT provider,username FROM settings")?;
    let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn save_settings(
    db: &mut Connection,
    names: std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let tx = db.transaction()?;
    for (provider, name) in names {
        ensure!(Provider::named(&provider).is_some(), "unknown provider");
        if name.trim().is_empty() {
            tx.execute("DELETE FROM settings WHERE provider=?1", [&provider])?;
        } else {
            tx.execute("INSERT INTO settings VALUES (?1,?2) ON CONFLICT(provider) DO UPDATE SET username=excluded.username",
                params![provider,valid_user(&name)?])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn views(db: &Connection) -> Result<Vec<JobView>> {
    let mut q = db.prepare(
        "SELECT id,provider,operation,status,added,existing,rejected,found,error FROM jobs
        WHERE id IN (SELECT id FROM jobs ORDER BY id DESC LIMIT 12)
           OR id IN (SELECT id FROM jobs WHERE status='failed' ORDER BY id DESC LIMIT 12)
        ORDER BY id DESC",
    )?;
    let rows = q.query_map([], |r| {
        Ok(JobView {
            id: r.get(0)?,
            provider: r.get(1)?,
            operation: r.get(2)?,
            status: r.get(3)?,
            added: r.get(4)?,
            existing: r.get(5)?,
            rejected: r.get(6)?,
            found: r.get(7)?,
            error: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn start(db: &mut Connection, input: StartJob, upload: Option<&str>) -> Result<i64> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let n: usize = tx.query_row(
        "SELECT count(*) FROM jobs WHERE status IN ('queued','running') AND parent_id IS NULL AND operation!='publish'",
        [],
        |r| r.get(0),
    )?;
    ensure!(n < 4, "the import queue is full");
    let username = match (input.provider.as_str(), input.operation.as_str()) {
        ("pgn", "upload") => {
            ensure!(upload.is_some(), "enter PGN up to 64 MiB");
            valid_user(input.username.as_deref().unwrap_or(""))?.to_owned()
        }
        ("archive", "scan") => String::new(),
        (name, "all" | "new") if Provider::named(name).is_some() => tx
            .query_row(
                "SELECT username FROM settings WHERE provider=?1",
                [&input.provider],
                |r| r.get(0),
            )
            .optional()?
            .context("save the username in Settings first")?,
        _ => bail!("unknown import operation"),
    };
    let existing = if input.provider != "pgn" {
        tx.query_row("SELECT id FROM jobs WHERE provider=?1 AND operation=?2 AND username=?3 AND status IN ('queued','running') LIMIT 1",
            params![input.provider,input.operation,username],|r|r.get::<_,i64>(0)).optional()?
    } else {
        None
    };
    if let Some(job) = existing {
        return Ok(job);
    }
    tx.execute("INSERT INTO jobs(provider,username,operation,upload_file,status) VALUES (?1,?2,?3,?4,'queued')",
        params![input.provider,username,input.operation,upload])?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

pub fn retry(db: &mut Connection, id: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let user_batch = tx
        .query_row(
            "SELECT parent_id IS NULL AND operation!='publish' FROM jobs WHERE id=?1",
            [id],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false);
    if user_batch {
        let n: usize = tx.query_row(
            "SELECT count(*) FROM jobs WHERE status IN ('queued','running') AND parent_id IS NULL AND operation!='publish'",
            [], |r| r.get(0),
        )?;
        ensure!(n < 4, "the import queue is full");
    }
    ensure!(
        tx.execute(
            "UPDATE jobs SET status='queued',error=NULL WHERE id=?1 AND status='failed'",
            [id]
        )? == 1,
        "the job is not waiting for retry"
    );
    tx.commit()?;
    Ok(())
}

fn next(db: &mut Connection) -> Result<Option<Job>> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let job = tx.query_row("SELECT id,provider,username,operation,cursor FROM jobs WHERE status='queued' ORDER BY id LIMIT 1",[],
        |r| Ok(Job { id:r.get(0)?,provider:r.get(1)?,username:r.get(2)?,operation:r.get(3)?,cursor:r.get(4)? })).optional()?;
    if let Some(job) = &job {
        tx.execute("UPDATE jobs SET status='running' WHERE id=?1", [job.id])?;
    }
    tx.commit()?;
    Ok(job)
}

pub fn worker(path: &Path, writer: Writer) -> Result<()> {
    let db = Database::read(path)?;
    while writer.background(|db| {
        Ok(db.execute(
            "UPDATE jobs SET status='queued' WHERE id IN
            (SELECT id FROM jobs WHERE status='running' ORDER BY id LIMIT 128)",
            [],
        )?)
    })? == 128
    {}
    let client = Provider::client()?;
    loop {
        match writer.background(next) {
            Ok(Some(job)) => {
                if let Err(error) = run(path, &db, &writer, &client, &job) {
                    let message = error.to_string();
                    let id = job.id;
                    writer.background(move |db| {
                        db.execute(
                            "UPDATE jobs SET status='failed',error=?2 WHERE id=?1",
                            params![id, message],
                        )?;
                        Ok(())
                    })?;
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

fn scan_game(db: &Connection, writer: &Writer, job: i64, game: &str) -> Result<()> {
    let Some(found) = game_pipeline::scan_findings(db, job, game)? else {
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

fn run(path: &Path, db: &Connection, writer: &Writer, client: &Client, job: &Job) -> Result<()> {
    let id = job.id;
    let mut upload_file = None;
    let checkpoint = match (job.provider.as_str(), job.operation.as_str()) {
        ("pgn", "upload") => {
            let (upload, legacy): (Option<String>, Option<String>) = db.query_row(
                "SELECT upload_file,payload FROM jobs WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
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
                let games = games::archive(db, &cursor, 25)?;
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
                let games = game_pipeline::pending_games(db, id)?;
                if games.is_empty() {
                    break;
                }
                for game in games {
                    if operation == "scan_new" {
                        scan_game(db, writer, id, &game)?;
                    } else {
                        let plans = game_pipeline::publication_batch(db, &game)?;
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
    writer.background(move |db| {
        let tx = db.transaction()?;
        if let Some(through) = checkpoint {
            tx.execute("INSERT INTO provider_checkpoint VALUES (?1,?2,?3) ON CONFLICT(provider,username) DO UPDATE SET through_at=max(through_at,excluded.through_at)",
                params![provider, username, through])?;
        }
        tx.execute("UPDATE jobs SET status='done',payload=NULL,upload_file=NULL,error=NULL WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    })?;
    if let Some(file) = upload_file {
        crate::uploads::remove(path, &file);
    }
    Ok(())
}

fn save_cursor(writer: &Writer, id: i64, cursor: String, rejected: usize) -> Result<()> {
    writer.background(move |db| {
        db.execute(
            "UPDATE jobs SET cursor=?2,rejected=rejected+?3 WHERE id=?1",
            params![id, cursor, rejected],
        )?;
        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
struct ProviderCursor {
    from: i64,
    through: i64,
    next: usize,
}

fn fetch_provider(
    db: &Connection,
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
                db.query_row(
                    "SELECT through_at FROM provider_checkpoint WHERE provider=?1 AND username=?2",
                    params![job.provider, user],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_job_remains_visible_alongside_prior_failures() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        for _ in 0..12 {
            db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('pgn','Learner','upload','failed')", [])?;
        }
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('archive','','scan','queued')", [])?;
        let jobs = views(&db)?;
        assert_eq!(jobs[0].status, "queued");
        assert_eq!(jobs.iter().filter(|job| job.status == "failed").count(), 12);
        Ok(())
    }
}
