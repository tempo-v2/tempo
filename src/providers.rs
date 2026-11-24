use crate::{chess, games};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use std::{io::Read, time::Duration};
const MAX_DOWNLOAD: usize = 32 * 1024 * 1024;
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

#[derive(Clone, Copy)]
pub enum Provider {
    ChessCom,
    Lichess,
}

impl Provider {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "chesscom" => Some(Self::ChessCom),
            "lichess" => Some(Self::Lichess),
            _ => None,
        }
    }

    pub fn client() -> Result<Client> {
        Ok(Client::builder()
            .user_agent("Tempo/0.2 chess game archive importer")
            .connect_timeout(Duration::from_secs(12))
            .timeout(Duration::from_secs(60))
            .build()?)
    }

    pub fn first_date(self, client: &Client, user: &str) -> Result<i64> {
        match self {
            Self::ChessCom => Ok(0),
            Self::Lichess => {
                let text = response_text(
                    client
                        .get(format!("https://lichess.org/api/user/{user}"))
                        .send()?,
                )?;
                Ok(serde_json::from_str::<LichessProfile>(&text)?.created_at)
            }
        }
    }

    pub fn windows(
        self,
        client: &Client,
        user: &str,
        from: i64,
        through: i64,
    ) -> Result<Vec<String>> {
        let urls = if matches!(self, Self::ChessCom) {
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
                    let date = chrono::NaiveDate::from_ymd_opt(
                        year.parse().ok()?,
                        month.parse().ok()?,
                        1,
                    )?;
                    let next = date.checked_add_months(chrono::Months::new(1))?;
                    (epoch(next) > from && epoch(date) <= through)
                        .then(|| format!("{root}{year}/{month}/pgn"))
                })
                .collect::<Vec<_>>()
        } else {
            let mut month = month_start(from)?;
            let mut urls = vec![];
            while epoch(month) <= through {
                let next = month
                    .checked_add_months(chrono::Months::new(1))
                    .context("invalid next month")?;
                let from = epoch(month).max(from);
                let window_end = (epoch(next) - 1).min(through);
                urls.push(format!("https://lichess.org/api/games/user/{user}?since={from}&until={window_end}&variant=standard&clocks=false&evals=false&opening=false"));
                month = next;
            }
            urls
        };
        Ok(urls)
    }

    pub fn supply(
        client: &Client,
        user: &str,
        window: &str,
        accept: impl FnMut(games::Game) -> Result<()>,
    ) -> Result<usize> {
        let pgn = response_text(
            client
                .get(window)
                .header("Accept", "application/x-chess-pgn")
                .send()?,
        )?;
        supply_pgn(&pgn, user, accept)
    }
}

pub fn upload_record<R: std::io::Read + std::io::Seek>(
    input: R,
    offset: u64,
    user: &str,
) -> Result<Option<(Option<games::Game>, u64)>> {
    Ok(chess::pgn_record(input, offset)?.map(|(parsed, next)| {
        (
            parsed.and_then(|game| games::archive_game(game, user)).ok(),
            next,
        )
    }))
}

pub fn supply_pgn(
    pgn: &str,
    user: &str,
    mut accept: impl FnMut(games::Game) -> Result<()>,
) -> Result<usize> {
    if pgn.trim().is_empty() {
        return Ok(0);
    }
    let mut rejected = 0;
    chess::read_pgn(pgn, false, |parsed| {
        match parsed.and_then(|game| games::archive_game(game, user)) {
            Ok(game) => accept(game)?,
            Err(_) => rejected += 1,
        }
        Ok(())
    })?;
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_http_feed_uses_the_archive_intake() -> Result<()> {
        use crate::{game_pipeline, queue_store, review, study::Next, writer::Writer};
        use rusqlite::Connection;
        use std::{io::Write, net::TcpListener};

        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let endpoint = format!("http://{}/games.pgn", listener.local_addr()?);
        let serve = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0; 4096];
            ensure!(stream.read(&mut request)? > 0, "provider request was empty");
            let pgn = "[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{pgn}",
                pgn.len()
            )?;
            Ok(())
        });

        let mut db = Connection::open_in_memory()?;
        for schema in [
            include_str!("schema.sql"),
            include_str!("opening_policy.sql"),
            include_str!("opening_order.sql"),
            include_str!("game_pipeline.sql"),
            include_str!("prepared_queue.sql"),
        ] {
            db.execute_batch(schema)?;
        }
        db.execute("INSERT INTO metadata VALUES ('fixture')", [])?;
        db.execute("INSERT INTO jobs(provider,username,operation,status) VALUES ('fixture','Learner','all','running')", [])?;
        queue_store::seed_batch(&mut db)?;
        let writer = Writer::start(db, None)?;
        let client = Provider::client()?;
        assert_eq!(
            Provider::supply(&client, "Learner", &endpoint, |game| writer
                .background(move |db| game_pipeline::archive_import(db, &game, 1)))?,
            0
        );
        serve.join().expect("fixture server panicked")?;
        writer.background(|db| {
            assert_eq!(
                db.query_row("SELECT count(*) FROM games", [], |row| row
                    .get::<_, usize>(0))?,
                1
            );
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM jobs WHERE operation='scan_new'",
                    [],
                    |row| row.get::<_, usize>(0)
                )?,
                1
            );
            assert!(
                review::next(
                    db,
                    Next {
                        after: None,
                        dataset: None
                    }
                )?
                .is_none()
            );
            Ok(())
        })
    }

    #[test]
    fn upload_checkpoint_restarts_after_the_committed_game() -> Result<()> {
        let pgn = concat!(
            "[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n",
            "[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1-0\"]\n\n1. d4 d5 1-0\n",
        );
        let (first, cursor) =
            upload_record(std::io::Cursor::new(pgn), 0, "Learner")?.context("first record")?;
        let (second, end) = upload_record(std::io::Cursor::new(pgn), cursor, "Learner")?
            .context("second record")?;
        assert_ne!(first.unwrap().id, second.unwrap().id);
        assert!(upload_record(std::io::Cursor::new(pgn), end, "Learner")?.is_none());
        Ok(())
    }
}
