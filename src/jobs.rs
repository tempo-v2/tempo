use crate::{chess, db::Database, game_pipeline, games};
use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Utc};
use reqwest::blocking::{Client, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path, time::Duration};

pub const MAX_PGN: usize = 64 * 1024 * 1024;
const MAX_DOWNLOAD: usize = 32 * 1024 * 1024;

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
    payload: Option<String>,
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
        ensure!(
            matches!(provider.as_str(), "chesscom" | "lichess"),
            "unknown provider"
        );
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
    let mut q = db.prepare("SELECT id,provider,operation,status,added,existing,rejected,found,error FROM jobs ORDER BY (status='failed') DESC,id DESC LIMIT 12")?;
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

pub fn start(db: &mut Connection, input: StartJob) -> Result<i64> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let n: usize = tx.query_row(
        "SELECT count(*) FROM jobs WHERE status IN ('queued','running') AND parent_id IS NULL AND operation!='publish'",
        [],
        |r| r.get(0),
    )?;
    ensure!(n < 4, "the import queue is full");
    let username = match (input.provider.as_str(), input.operation.as_str()) {
        ("pgn", "upload") => {
            let pgn = input.pgn.as_deref().unwrap_or("");
            ensure!(
                !pgn.trim().is_empty() && pgn.len() <= MAX_PGN,
                "enter PGN up to 64 MiB"
            );
            valid_user(input.username.as_deref().unwrap_or(""))?.to_owned()
        }
        ("archive", "scan") => String::new(),
        ("chesscom" | "lichess", "all" | "new") => tx
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
    tx.execute("INSERT INTO jobs(provider,username,operation,payload,status) VALUES (?1,?2,?3,?4,'queued')",
        params![input.provider,username,input.operation,input.pgn])?;
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
    let job = tx.query_row("SELECT id,provider,username,operation,payload,cursor FROM jobs WHERE status='queued' ORDER BY id LIMIT 1",[],
        |r| Ok(Job { id:r.get(0)?,provider:r.get(1)?,username:r.get(2)?,operation:r.get(3)?,payload:r.get(4)?,cursor:r.get(5)? })).optional()?;
    if let Some(job) = &job {
        tx.execute("UPDATE jobs SET status='running' WHERE id=?1", [job.id])?;
    }
    tx.commit()?;
    Ok(job)
}

pub fn worker(path: &Path) -> Result<()> {
    let mut db = Database::open(path)?;
    db.execute("UPDATE jobs SET status='queued' WHERE status='running'", [])?;
    let client = Client::builder()
        .user_agent("Tempo/0.2 chess game archive importer")
        .connect_timeout(Duration::from_secs(12))
        .timeout(Duration::from_secs(60))
        .build()?;
    loop {
        match next(&mut db) {
            Ok(Some(job)) => {
                if let Err(error) = run(&mut db, &client, &job) {
                    db.execute(
                        "UPDATE jobs SET status='failed',error=?2 WHERE id=?1",
                        params![job.id, error.to_string()],
                    )?;
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

fn run(db: &mut Connection, client: &Client, job: &Job) -> Result<()> {
    let mut rejected = 0;
    let checkpoint = match (job.provider.as_str(), job.operation.as_str()) {
        ("pgn", "upload") => {
            rejected = ingest(db, job, job.payload.as_deref().context("PGN is missing")?)?;
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
                    game_pipeline::scan_game(db, job.id, &game.id)?;
                }
                cursor = games.last().unwrap().id.clone();
                save_cursor(db, job.id, &cursor)?;
            }
            None
        }
        ("archive", operation @ ("scan_new" | "publish")) => {
            let process = if operation == "scan_new" {
                game_pipeline::scan_game
            } else {
                game_pipeline::publish_game
            };
            loop {
                let games = game_pipeline::pending_games(db, job.id)?;
                if games.is_empty() {
                    break;
                }
                for game in games {
                    process(db, job.id, &game)?;
                }
            }
            None
        }
        ("chesscom" | "lichess", "all" | "new") => Some(fetch_provider(db, client, job)?),
        _ => bail!("unknown provider"),
    };
    let tx = db.transaction()?;
    if let Some(through) = checkpoint {
        tx.execute("INSERT INTO provider_checkpoint VALUES (?1,?2,?3) ON CONFLICT(provider,username) DO UPDATE SET through_at=max(through_at,excluded.through_at)",
            params![job.provider,job.username.to_ascii_lowercase(),through])?;
    }
    tx.execute(
        "UPDATE jobs SET status='done',payload=NULL,error=NULL,rejected=rejected+?2 WHERE id=?1",
        params![job.id, rejected],
    )?;
    tx.commit()?;
    Ok(())
}

fn save_cursor(db: &Connection, id: i64, cursor: &str) -> Result<()> {
    db.execute("UPDATE jobs SET cursor=?2 WHERE id=?1", params![id, cursor])?;
    Ok(())
}

fn ingest(db: &mut Connection, job: &Job, pgn: &str) -> Result<usize> {
    if pgn.trim().is_empty() {
        return Ok(0);
    }
    let mut rejected = 0;
    chess::read_pgn(pgn, false, |pgn| {
        match pgn.and_then(|pgn| games::archive_game(pgn, &job.username)) {
            Ok(game) => game_pipeline::archive_import(db, &game, job.id)?,
            Err(_) => rejected += 1,
        }
        Ok(())
    })?;
    Ok(rejected)
}

fn response_text(response: Response) -> Result<String> {
    let mut response = response.error_for_status()?.take((MAX_DOWNLOAD + 1) as u64);
    let mut text = String::new();
    response.read_to_string(&mut text)?;
    ensure!(
        text.len() <= MAX_DOWNLOAD,
        "the provider response exceeded 32 MiB"
    );
    Ok(text)
}

#[derive(Deserialize)]
struct ChessComArchives {
    archives: Vec<String>,
}

#[derive(Deserialize)]
struct LichessProfile {
    #[serde(rename = "createdAt")]
    created_at: i64,
}

#[derive(Serialize, Deserialize)]
struct ProviderCursor {
    from: i64,
    through: i64,
    next: usize,
}

fn month_start(t: i64) -> Result<chrono::NaiveDate> {
    let date = DateTime::from_timestamp_millis(t)
        .context("invalid provider date")?
        .date_naive();
    date.with_day(1).context("invalid calendar month")
}

fn epoch(date: chrono::NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis()
}

fn fetch_provider(db: &mut Connection, client: &Client, job: &Job) -> Result<i64> {
    let user = job.username.to_ascii_lowercase();
    let mut cursor = match &job.cursor {
        Some(json) => serde_json::from_str::<ProviderCursor>(json)?,
        None => {
            let previous = if job.operation == "new" {
                db.query_row(
                    "SELECT through_at FROM provider_checkpoint WHERE provider=?1 AND username=?2",
                    params![job.provider, user],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            } else {
                None
            };
            let now = Utc::now().timestamp_millis();
            let from = match (previous, job.provider.as_str()) {
                (Some(previous), _) => previous.saturating_sub(86_400_000),
                (None, "lichess") => {
                    let text = response_text(
                        client
                            .get(format!("https://lichess.org/api/user/{user}"))
                            .send()?,
                    )?;
                    serde_json::from_str::<LichessProfile>(&text)?.created_at
                }
                _ => 0,
            };
            let cursor = ProviderCursor {
                from,
                through: now,
                next: 0,
            };
            save_cursor(db, job.id, &serde_json::to_string(&cursor)?)?;
            cursor
        }
    };

    let urls = if job.provider == "chesscom" {
        let root = format!("https://api.chess.com/pub/player/{user}/games/");
        let list = response_text(client.get(format!("{root}archives")).send()?)?;
        let mut months = serde_json::from_str::<ChessComArchives>(&list)?.archives;
        months.sort();
        months
            .into_iter()
            .filter_map(|url| {
                let suffix = url.rsplit('/').take(2).collect::<Vec<_>>();
                let [month, year] = suffix.as_slice() else {
                    return None;
                };
                let date =
                    chrono::NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)?;
                let next = date.checked_add_months(chrono::Months::new(1))?;
                (epoch(next) > cursor.from && epoch(date) <= cursor.through)
                    .then(|| format!("{root}{year}/{month}/pgn"))
            })
            .collect::<Vec<_>>()
    } else {
        let mut month = month_start(cursor.from)?;
        let mut urls = vec![];
        while epoch(month) <= cursor.through {
            let next = month
                .checked_add_months(chrono::Months::new(1))
                .context("invalid next month")?;
            let from = epoch(month).max(cursor.from);
            let through = (epoch(next) - 1).min(cursor.through);
            urls.push(format!("https://lichess.org/api/games/user/{user}?since={from}&until={through}&variant=standard&clocks=false&evals=false&opening=false"));
            month = next;
        }
        urls
    };
    for (index, url) in urls.iter().enumerate().skip(cursor.next) {
        let pgn = response_text(
            client
                .get(url)
                .header("Accept", "application/x-chess-pgn")
                .send()?,
        )?;
        let rejected = ingest(db, job, &pgn)?;
        cursor.next = index + 1;
        db.execute(
            "UPDATE jobs SET cursor=?2,rejected=rejected+?3 WHERE id=?1",
            params![job.id, serde_json::to_string(&cursor)?, rejected],
        )?;
    }
    Ok(cursor.through)
}
