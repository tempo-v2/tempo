use crate::{chess, games};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Datelike};
use reqwest::blocking::{Client, Response};
use serde::Deserialize;
use std::{
    io::Read,
    time::{Duration, Instant},
};
const MAX_DOWNLOAD: usize = 32 * 1024 * 1024;
fn request(
    client: &Client,
    url: &str,
    deadline: Instant,
) -> Result<reqwest::blocking::RequestBuilder> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "Provider turn timed out.");
    Ok(client.get(url).timeout(remaining))
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

    pub fn first_date(self, client: &Client, user: &str, deadline: Instant) -> Result<i64> {
        match self {
            Self::ChessCom => Ok(0),
            Self::Lichess => {
                let text = response_text(
                    request(
                        client,
                        &format!("https://lichess.org/api/user/{user}"),
                        deadline,
                    )?
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
        deadline: Instant,
    ) -> Result<Vec<String>> {
        let urls = if matches!(self, Self::ChessCom) {
            let root = format!("https://api.chess.com/pub/player/{user}/games/");
            let list =
                response_text(request(client, &format!("{root}archives"), deadline)?.send()?)?;
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
                ensure!(Instant::now() < deadline, "Provider turn timed out.");
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

    pub fn download(client: &Client, window: &str, deadline: Instant) -> Result<String> {
        response_text(
            request(client, window, deadline)?
                .header("Accept", "application/x-chess-pgn")
                .send()?,
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_body_reads_share_the_turn_deadline() -> Result<()> {
        use std::{io::Write, net::TcpListener};
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let endpoint = format!("http://{}/games.pgn", listener.local_addr()?);
        let server = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut request = [0; 4096];
            ensure!(stream.read(&mut request)? > 0, "provider request was empty");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\na")?;
            // The request has begun but its body outlives the turn.
            std::thread::sleep(Duration::from_millis(350));
            let _ = stream.write_all(b"bc");
            Ok(())
        });
        let client = Provider::client()?;
        let result = Provider::download(
            &client,
            &endpoint,
            Instant::now() + Duration::from_millis(100),
        );
        server.join().expect("fixture server panicked")?;
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn a_rejected_variant_skips_the_whole_physical_game() -> Result<()> {
        let pgn = concat!(
            "[Variant \"Atomic\"]\n[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1-0\"]\n\n1. e4 e5 1-0\n\n",
            "[White \"Learner\"]\n[Black \"Other\"]\n[Result \"1-0\"]\n\n1. d4 d5 1-0\n",
        );
        let (rejected, next) = upload_record(std::io::Cursor::new(pgn), 0, "Learner")?
            .context("unsupported variant")?;
        assert!(rejected.is_none());
        let (accepted, end) = upload_record(std::io::Cursor::new(pgn), next, "Learner")?
            .context("next physical game")?;
        assert!(accepted.is_some());
        assert!(upload_record(std::io::Cursor::new(pgn), end, "Learner")?.is_none());
        Ok(())
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
