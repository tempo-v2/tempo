//! Bounded, serial reads from the two public completed-game APIs.

use crate::{AppError, GameRange};
use chrono::{DateTime, Datelike, NaiveDate, NaiveTime};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    io::Read,
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime},
};

const MAX_RANGE_MS: u64 = 31 * 86_400_000;
const MAX_GAMES: usize = 500;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const MAX_ERRORS: usize = MAX_GAMES;
const CACHE_ENTRIES: usize = 4;
const CHESS_COM_API: &str = "https://api.chess.com/pub/player/";

#[derive(Clone, Copy, Debug, Deserialize, serde::Serialize, Eq, Hash, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Lichess,
    ChessCom,
}

impl Provider {
    fn label(self) -> &'static str {
        match self {
            Self::Lichess => "Lichess",
            Self::ChessCom => "Chess.com",
        }
    }
}

/// One completed orthodox game supplied by the chosen provider. Account IDs are
/// provider identities. The timestamps bound played moves, with unknown start
/// represented explicitly. The caller supplies the separate ingestion time.
pub(crate) struct Game {
    pub(crate) provider: &'static str,
    pub(crate) id: String,
    pub(crate) url: String,
    pub(crate) white: String,
    pub(crate) black: String,
    pub(crate) start_ms: Option<u64>,
    pub(crate) end_ms: u64,
    pub(crate) pgn: String,
}

#[derive(Default)]
pub(crate) struct FetchedGames {
    pub(crate) received: usize,
    pub(crate) games: Vec<Game>,
    pub(crate) errors: Vec<String>,
}

impl FetchedGames {
    fn record(&mut self, row: Result<Option<Game>, AppError>) {
        if !matches!(row, Ok(None)) {
            self.received += 1;
        }
        match row {
            Ok(Some(game)) => self.games.push(game),
            Ok(None) => {}
            Err(error) if self.errors.len() < MAX_ERRORS => self.errors.push(error.message),
            Err(_) if self.errors.len() == MAX_ERRORS => {
                self.errors
                    .push("Additional invalid games were omitted from this report.".into());
            }
            Err(_) => {}
        }
    }

    fn cap(&mut self) {
        if self.games.len() > MAX_GAMES {
            self.games.truncate(MAX_GAMES);
            self.errors
                .push("This refresh exceeded 500 games. Use a shorter date range.".into());
        }
    }
}

/// One worker owns this adapter, so requests and cooldown updates are serial.
/// Validators and their bodies are paired in a bounded, process-local cache.
#[derive(Default)]
pub(crate) struct PublicGames {
    cache: VecDeque<(String, Cached)>,
    retry_after: HashMap<Provider, Instant>,
}

impl PublicGames {
    /// Read an explicit account and half-open UTC date range. Lichess applies
    /// uses time of creation for export windows. Chess.com groups archives by end time.
    pub(crate) fn fetch(
        &mut self,
        provider: Provider,
        account: &str,
        range: &GameRange,
    ) -> Result<FetchedGames, AppError> {
        let account = account.trim();
        if !valid_account(account) {
            return Err(AppError::new(
                "invalid_account",
                "Enter a provider account name.",
            ));
        }
        if !matches!(
            range.to_ms.checked_sub(range.from_ms),
            Some(1..=MAX_RANGE_MS)
        ) || timestamp(range.from_ms).is_none()
            || timestamp(range.to_ms).is_none()
        {
            return Err(AppError::new(
                "invalid_range",
                "Choose a date range of at most 31 days.",
            ));
        }
        let account = account.to_ascii_lowercase();
        match provider {
            Provider::Lichess => self.lichess(&account, range),
            Provider::ChessCom => self.chess_com(&account, range),
        }
    }

    fn lichess(&mut self, account: &str, range: &GameRange) -> Result<FetchedGames, AppError> {
        let limit = MAX_GAMES + 1;
        let url = format!(
            "https://lichess.org/api/games/user/{account}?since={}&until={}&max={limit}&finished=true&ongoing=false&pgnInJson=true",
            range.from_ms.saturating_sub(1),
            range.to_ms
        );
        let body = self.read(Provider::Lichess, &url, "application/x-ndjson")?;
        Ok(decode_lichess(&body, account, range))
    }

    fn chess_com(&mut self, account: &str, range: &GameRange) -> Result<FetchedGames, AppError> {
        chess_com_archives(account, range, |url| {
            self.read(Provider::ChessCom, url, "application/json")
        })
    }

    fn read(&mut self, provider: Provider, url: &str, accept: &str) -> Result<Vec<u8>, AppError> {
        if self
            .retry_after
            .get(&provider)
            .is_some_and(|&until| until > Instant::now())
        {
            return Err(rate_limited(provider));
        }
        let index = self.cache.iter().position(|(key, _)| key == url);
        let cached = index.map(|index| &self.cache[index].1);
        let output = curl(provider, url, accept, cached)?;
        let response = Response::parse(&output)?;
        match response.status {
            200 => {
                let body = response.body.to_vec();
                if let Some(index) = index {
                    self.cache.remove(index);
                }
                let etag = response.header("etag").map(str::to_owned);
                let modified = response.header("last-modified").map(str::to_owned);
                if etag.is_some() || modified.is_some() {
                    self.cache.push_back((
                        url.into(),
                        Cached {
                            etag,
                            modified,
                            body: body.clone(),
                        },
                    ));
                    if self.cache.len() > CACHE_ENTRIES {
                        self.cache.pop_front();
                    }
                }
                Ok(body)
            }
            304 => {
                let index = index
                    .ok_or_else(|| provider_error("Provider returned an empty cached response."))?;
                let entry = self.cache.remove(index).expect("found cache entry");
                let body = entry.1.body.clone();
                self.cache.push_back(entry);
                Ok(body)
            }
            429 => {
                let delay = response
                    .header("retry-after")
                    .and_then(retry_delay)
                    .unwrap_or_default()
                    .max(Duration::from_secs(60));
                let now = Instant::now();
                let until = now
                    .checked_add(delay)
                    .unwrap_or_else(|| now + Duration::from_secs(60));
                self.retry_after.insert(provider, until);
                Err(rate_limited(provider))
            }
            status => Err(provider_error(format!(
                "{} returned HTTP {status}.",
                provider.label()
            ))),
        }
    }
}

fn chess_com_archives(
    account: &str,
    range: &GameRange,
    mut read: impl FnMut(&str) -> Result<Vec<u8>, AppError>,
) -> Result<FetchedGames, AppError> {
    let base = format!("{CHESS_COM_API}{account}/games");
    let body = read(&format!("{base}/archives"))?;
    let archives: Archives = serde_json::from_slice(&body)
        .map_err(|_| provider_error("Chess.com returned an invalid archive list."))?;

    let mut batch = FetchedGames::default();
    for archive in archive_urls(&base, range) {
        // Construct URLs locally so a changed or untrusted archive list
        // cannot broaden host, path, or requested month.
        if !archives.archives.contains(&archive) {
            continue;
        }
        let month = &archive[archive.len() - 7..];
        let result = read(&archive)
            .and_then(|body| decode_chess_com(&body, account, range, month, &mut batch));
        if let Err(error) = result {
            batch
                .errors
                .push(format!("Chess.com {month}: {}", error.message));
            if error.code == "provider_rate_limited" {
                break;
            }
        }
        if batch.games.len() > MAX_GAMES {
            break;
        }
    }
    batch.cap();
    Ok(batch)
}

fn valid_account(account: &str) -> bool {
    !account.is_empty()
        && account.len() <= 40
        && account
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

fn timestamp(ms: u64) -> Option<DateTime<chrono::Utc>> {
    DateTime::from_timestamp_millis(i64::try_from(ms).ok()?)
}

fn provider_error(message: impl Into<String>) -> AppError {
    AppError::new("provider_failed", message)
}

fn rate_limited(provider: Provider) -> AppError {
    AppError::new(
        "provider_rate_limited",
        format!(
            "{} is limiting requests. Try again later.",
            provider.label()
        ),
    )
}

fn retry_delay(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at: SystemTime = DateTime::parse_from_rfc2822(value).ok()?.into();
    Some(
        retry_at
            .duration_since(SystemTime::now())
            .unwrap_or_default(),
    )
}

struct Cached {
    etag: Option<String>,
    modified: Option<String>,
    body: Vec<u8>,
}

/// macOS ships a TLS client with system trust roots. Bound both the client
/// deadline and its stdout, including providers that stream without a length.
fn curl(
    provider: Provider,
    url: &str,
    accept: &str,
    cached: Option<&Cached>,
) -> Result<Vec<u8>, AppError> {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--disable",
        "--silent",
        "--include",
        "--suppress-connect-headers",
        "--proto",
        "=https",
        "--connect-timeout",
        "4",
        "--max-time",
        // Lichess streams at a documented 20 games/second for anonymous requests.
        if provider == Provider::Lichess {
            "30"
        } else {
            "12"
        },
        "--max-filesize",
        &MAX_RESPONSE.to_string(),
        "--user-agent",
        "Tempo/2 (https://github.com/tempo-v2/tempo)",
        "--header",
        &format!("Accept: {accept}"),
    ]);
    if let Some(cached) = cached {
        if let Some(etag) = &cached.etag {
            command.args(["--header", &format!("If-None-Match: {etag}")]);
        }
        if let Some(modified) = &cached.modified {
            command.args(["--header", &format!("If-Modified-Since: {modified}")]);
        }
    }
    let mut child = command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| provider_error("The system HTTPS client could not start."))?;
    let mut output = Vec::new();
    let read = child
        .stdout
        .take()
        .expect("piped stdout")
        .take((MAX_RESPONSE + 1) as u64)
        .read_to_end(&mut output);
    if read.is_err() || output.len() > MAX_RESPONSE {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|_| provider_error("The system HTTPS client did not complete."))?;
    if output.len() > MAX_RESPONSE || status.code() == Some(63) {
        return Err(provider_error(
            "Provider response was too large. Use a shorter date range.",
        ));
    }
    if read.is_err() || !status.success() {
        let reason = match status.code() {
            Some(6) => "The provider hostname could not be resolved.",
            Some(28) => "The provider request timed out.",
            _ => "The provider connection did not complete.",
        };
        return Err(provider_error(reason));
    }
    Ok(output)
}

struct Response<'a> {
    status: u16,
    headers: &'a str,
    body: &'a [u8],
}

impl<'a> Response<'a> {
    fn parse(mut bytes: &'a [u8]) -> Result<Self, AppError> {
        let invalid = || provider_error("Provider returned malformed HTTP headers.");
        loop {
            let end = bytes
                .windows(4)
                .position(|sep| sep == b"\r\n\r\n")
                .ok_or_else(invalid)?;
            let headers = std::str::from_utf8(&bytes[..end]).map_err(|_| invalid())?;
            let status = headers
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("HTTP/"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|status| status.parse::<u16>().ok())
                .ok_or_else(invalid)?;
            bytes = &bytes[end + 4..];
            if status >= 200 {
                return Ok(Self {
                    status,
                    headers,
                    body: bytes,
                });
            }
        }
    }

    fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(key, value)| {
                let value = value.trim();
                (key.eq_ignore_ascii_case(name)
                    && !value.is_empty()
                    && value.len() <= 1024
                    && value.bytes().all(|ch| ch == b' ' || ch.is_ascii_graphic()))
                .then_some(value)
            })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LichessGame {
    id: String,
    variant: String,
    status: String,
    created_at: u64,
    last_move_at: u64,
    players: LichessPlayers,
    pgn: String,
}

#[derive(Deserialize)]
struct LichessPlayers {
    white: LichessPlayer,
    black: LichessPlayer,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LichessPlayer {
    user: Option<LichessUser>,
    ai_level: Option<u8>,
}

#[derive(Deserialize)]
struct LichessUser {
    id: String,
}

impl LichessPlayer {
    fn identity(self) -> Option<String> {
        self.user
            .map(|user| user.id)
            .filter(|id| valid_account(id))
            .or_else(|| self.ai_level.map(|level| format!("ai:{level}")))
    }
}

fn decode_lichess(body: &[u8], account: &str, range: &GameRange) -> FetchedGames {
    let mut batch = FetchedGames::default();
    let rows = body
        .split(|&byte| byte == b'\n')
        .filter(|row| !row.iter().all(u8::is_ascii_whitespace));
    for (index, row) in rows.enumerate() {
        if index == MAX_GAMES {
            batch.errors.push(
                "This refresh reached the Lichess export limit. Use a shorter date range.".into(),
            );
            break;
        }
        let game = serde_json::from_slice::<Value>(row)
            .and_then(|row| {
                if outside_window(row["createdAt"].as_u64(), range) {
                    return Ok(None);
                }
                serde_json::from_value::<LichessGame>(row).map(Some)
            })
            .map_err(|_| provider_error("Incomplete or invalid game data."))
            .and_then(|game| game.map(|game| game.normalize(account)).transpose());
        batch.record(game.map_err(|error| {
            provider_error(format!("Lichess game {}: {}", index + 1, error.message))
        }));
    }
    batch
}

impl LichessGame {
    fn normalize(self, account: &str) -> Result<Game, AppError> {
        if self.variant != "standard" {
            return Err(provider_error("Unsupported chess variant."));
        }
        if !matches!(
            self.status.as_str(),
            "mate"
                | "resign"
                | "stalemate"
                | "timeout"
                | "draw"
                | "outoftime"
                | "cheat"
                | "unknownFinish"
                | "insufficientMaterialClaim"
                | "variantEnd"
        ) {
            return Err(provider_error("The game has no completed result."));
        }
        if self.id.len() != 8 || !self.id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(provider_error("Missing or invalid game identity."));
        }
        if self.created_at == 0
            || self.created_at > self.last_move_at
            || timestamp(self.last_move_at).is_none()
        {
            return Err(provider_error("Missing or inconsistent game times."));
        }
        let white = self.players.white.identity();
        let black = self.players.black.identity();
        let (white, black) = participants(white, black, account)?;
        Ok(Game {
            provider: "lichess",
            url: format!("https://lichess.org/game/{}", self.id),
            id: self.id,
            white,
            black,
            start_ms: Some(self.created_at),
            end_ms: self.last_move_at,
            pgn: self.pgn,
        })
    }
}

fn participants(
    white: Option<String>,
    black: Option<String>,
    account: &str,
) -> Result<(String, String), AppError> {
    let (Some(white), Some(black)) = (white, black) else {
        return Err(provider_error("Missing player identity."));
    };
    if white.eq_ignore_ascii_case(account) == black.eq_ignore_ascii_case(account) {
        return Err(provider_error(
            "The selected account does not identify one player's side.",
        ));
    }
    Ok((white, black))
}

#[derive(Deserialize)]
struct Archives {
    archives: Vec<String>,
}

fn archive_urls(base: &str, range: &GameRange) -> Vec<String> {
    let from = timestamp(range.from_ms).expect("validated range");
    let to = timestamp(range.to_ms - 1).expect("validated range");
    let start = from.year() * 12 + from.month0() as i32;
    let end = to.year() * 12 + to.month0() as i32;
    (start..=end)
        .map(|month| format!("{base}/{:04}/{:02}", month / 12, month % 12 + 1))
        .collect()
}

#[derive(Deserialize)]
struct ChessComArchive {
    games: Vec<Value>,
}

#[derive(Deserialize)]
struct ChessComGame {
    url: String,
    white: ChessComPlayer,
    black: ChessComPlayer,
    pgn: String,
    start_time: Option<u64>,
    end_time: u64,
    rules: String,
}

#[derive(Deserialize)]
struct ChessComPlayer {
    username: String,
}

fn outside_window(time_ms: Option<u64>, range: &GameRange) -> bool {
    time_ms
        .filter(|&time| time > 0 && timestamp(time).is_some())
        .is_some_and(|time| !(range.from_ms..range.to_ms).contains(&time))
}

fn decode_chess_com(
    body: &[u8],
    account: &str,
    range: &GameRange,
    month: &str,
    batch: &mut FetchedGames,
) -> Result<(), AppError> {
    let archive: ChessComArchive =
        serde_json::from_slice(body).map_err(|_| provider_error("Invalid game archive."))?;
    for (index, row) in archive.games.into_iter().enumerate() {
        if outside_window(
            row["end_time"]
                .as_u64()
                .and_then(|end| end.checked_mul(1000)),
            range,
        ) {
            continue;
        }
        let game = serde_json::from_value::<ChessComGame>(row)
            .map_err(|_| provider_error("Incomplete or invalid game data."))
            .and_then(|game| game.normalize(account, range));
        batch.record(game.map_err(|error| {
            provider_error(format!(
                "Chess.com {month} game {}: {}",
                index + 1,
                error.message
            ))
        }));
        if batch.games.len() > MAX_GAMES {
            break;
        }
    }
    Ok(())
}

impl ChessComGame {
    fn normalize(self, account: &str, range: &GameRange) -> Result<Option<Game>, AppError> {
        let end = self
            .end_time
            .checked_mul(1000)
            .filter(|&end| end > 0 && timestamp(end).is_some())
            .ok_or_else(|| provider_error("Missing or invalid game end time."))?;
        if !(range.from_ms..range.to_ms).contains(&end) {
            return Ok(None);
        }
        if self.rules != "chess" {
            return Err(provider_error("Unsupported chess variant."));
        }
        let (kind, number) = chess_com_identity(&self.url)
            .ok_or_else(|| provider_error("Missing or invalid game identity."))?;
        let white = valid_account(&self.white.username).then_some(self.white.username);
        let black = valid_account(&self.black.username).then_some(self.black.username);
        let (white, black) = participants(white, black, account)?;
        let start = match self.start_time {
            Some(start) => Some(
                start
                    .checked_mul(1000)
                    .filter(|&start| start > 0 && start <= end)
                    .ok_or_else(|| provider_error("Inconsistent game start and end times."))?,
            ),
            None => pgn_utc_start(&self.pgn).filter(|&start| start <= end),
        };
        Ok(Some(Game {
            provider: "chess_com",
            id: format!("{kind}/{number}"),
            url: format!("https://www.chess.com/game/{kind}/{number}"),
            white,
            black,
            start_ms: start,
            // Keep the entire provider-reported second in the uncertainty bound.
            end_ms: end + 999,
            pgn: self.pgn,
        }))
    }
}

fn chess_com_identity(url: &str) -> Option<(&'static str, &str)> {
    let path = url.strip_prefix("https://www.chess.com/")?;
    for (prefix, kind) in [
        ("game/live/", "live"),
        ("game/daily/", "daily"),
        ("live/game/", "live"),
        ("daily/game/", "daily"),
    ] {
        if let Some(id) = path.strip_prefix(prefix) {
            return (!id.is_empty() && id.len() <= 24 && id.bytes().all(|b| b.is_ascii_digit()))
                .then_some((kind, id));
        }
    }
    None
}

/// PGN UTCDate/UTCTime mark start. A missing, duplicated, malformed or
/// date-only tag leaves the lower bound unknown. Only provider PGN uses this.
fn pgn_utc_start(pgn: &str) -> Option<u64> {
    let mut date = None;
    let mut time = None;
    for tag in pgn.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if !tag.starts_with('[') {
            break;
        }
        if let Some(value) = tag
            .strip_prefix("[UTCDate \"")
            .and_then(|value| value.strip_suffix("\"]"))
        {
            if date.replace(value).is_some() {
                return None;
            }
        } else if let Some(value) = tag
            .strip_prefix("[UTCTime \"")
            .and_then(|value| value.strip_suffix("\"]"))
        {
            if time.replace(value).is_some() {
                return None;
            }
        }
    }
    let date = NaiveDate::parse_from_str(date?, "%Y.%m.%d").ok()?;
    let time = NaiveTime::parse_from_str(time?, "%H:%M:%S").ok()?;
    u64::try_from(date.and_time(time).and_utc().timestamp_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const JULY_17: u64 = 1784246400000;

    // The documented NDJSON shape, including the structured account IDs and
    // millisecond timestamps observed on the real LhL9TRJY export.
    fn lichess_row() -> Value {
        json!({
            "id": "LhL9TRJY",
            "variant": "standard",
            "status": "resign",
            "createdAt": JULY_17,
            "lastMoveAt": JULY_17 + 10_000,
            "players": {
                "white": {"user": {"id": "leelaalien", "name": "LeelaAlien"}},
                "black": {"user": {"id": "thibault"}}
            },
            "pgn": "[White \"LeelaAlien\"]\n[Black \"thibault\"]\n1. h3 e5 1-0"
        })
    }

    #[test]
    fn lichess_accepts_bounded_completed_games_and_keeps_per_record_diagnostics() {
        let range = GameRange {
            from_ms: JULY_17,
            to_ms: JULY_17 + 86_400_000,
        };
        let mut unfinished = lichess_row();
        unfinished["status"] = json!("started");
        let mut unsupported = lichess_row();
        unsupported["variant"] = json!("atomic");
        let mut outside = lichess_row();
        outside["createdAt"] = json!(range.to_ms);
        let body = format!(
            "{unfinished}\n{unsupported}\n{outside}\nnot JSON\n{}",
            lichess_row()
        );
        let batch = decode_lichess(body.as_bytes(), "Thibault", &range);
        assert_eq!(batch.received, 4);
        assert_eq!(batch.errors.len(), 3);
        assert_eq!(batch.games.len(), 1);
        let game = &batch.games[0];
        assert_eq!(game.provider, "lichess");
        assert_eq!(game.id, "LhL9TRJY");
        assert_eq!(game.url, "https://lichess.org/game/LhL9TRJY");
        assert_eq!(game.black, "thibault");
        assert_eq!(game.white, "leelaalien");
        assert_eq!(game.start_ms, Some(JULY_17));
        assert_eq!(game.end_ms, JULY_17 + 10_000);
        assert!(game.pgn.ends_with("1-0"));
        let mismatched = decode_lichess(body.as_bytes(), "another_account", &range);
        assert!(mismatched.games.is_empty());
        assert_eq!(mismatched.errors.len(), 4);
    }

    // The archived monthly shape and UTC tags from a public Erik game.
    #[test]
    fn chess_com_gates_on_end_time_and_retains_second_precision() {
        let row = json!({
            "url": "https://www.chess.com/game/daily/29099782",
            "white": {"username": "Mainline_Novelty"},
            "black": {"username": "erik"},
            "start_time": 1254438881_u64,
            "end_time": 1254670734_u64,
            "rules": "chess",
            "pgn": "[UTCDate \"2009.10.01\"]\n[UTCTime \"23:14:41\"]\n1. e4 d6 1-0"
        });
        let mut live_shape = row.clone();
        live_shape.as_object_mut().unwrap().remove("start_time");
        live_shape["url"] = json!("https://www.chess.com/live/game/29099782");
        let mut outside = row.clone();
        outside["end_time"] = json!(1254670735_u64);
        let mut invalid = row.clone();
        invalid["url"] = json!("https://example.com/game/daily/29099782");
        let body = json!({"games": [row, null, live_shape, outside, invalid]});
        let range = GameRange {
            from_ms: 1_254_670_734_000,
            to_ms: 1_254_670_735_000,
        };
        let mut batch = FetchedGames::default();
        decode_chess_com(
            body.to_string().as_bytes(),
            "Erik",
            &range,
            "2009/10",
            &mut batch,
        )
        .unwrap();
        assert_eq!(batch.received, 4);
        assert_eq!(batch.errors.len(), 2);
        assert_eq!(batch.games.len(), 2);
        assert_eq!(batch.games[0].start_ms, batch.games[1].start_ms);
        assert_eq!(batch.games[0].start_ms, Some(1_254_438_881_000));
        assert_eq!(batch.games[0].end_ms, 1_254_670_734_999);
        assert_eq!(batch.games[0].id, "daily/29099782");
        assert_eq!(batch.games[1].id, "live/29099782");
        assert_eq!(pgn_utc_start("[Date \"2009.10.01\"]\n1. e4 1-0"), None);
        assert_eq!(
            pgn_utc_start(
                "[UTCDate \"2009.10.01\"]\n[UTCTime \"23:14:41\"]\n[UTCTime \"23:14:41\"]"
            ),
            None
        );
    }

    #[test]
    fn chess_com_archive_and_limit_diagnostics_do_not_count_as_games() {
        let range = GameRange {
            from_ms: 1_254_672_000_000 - 86_400_000,
            to_ms: 1_254_672_000_000,
        };
        let month = format!("{CHESS_COM_API}erik/games/2009/10");
        let archives = json!({"archives": [&month]}).to_string().into_bytes();
        let failed = chess_com_archives("erik", &range, |url| {
            if url.ends_with("/archives") {
                return Ok(archives.clone());
            }
            assert_eq!(url, month);
            Err(provider_error("Provider request timed out."))
        })
        .unwrap();
        assert_eq!(failed.received, 0);
        assert!(failed.games.is_empty());
        assert_eq!(
            failed.errors,
            ["Chess.com 2009/10: Provider request timed out."]
        );

        let valid = json!({
            "url": "https://www.chess.com/game/daily/29099782",
            "white": {"username": "learner"},
            "black": {"username": "erik"},
            "end_time": 1254670734_u64,
            "rules": "chess",
            "pgn": "1. e4 d6 1-0"
        });
        let mut rows = vec![Value::Null];
        rows.extend(std::iter::repeat_n(valid, MAX_GAMES + 1));
        let archive = json!({"games":rows}).to_string().into_bytes();
        let partial = chess_com_archives("erik", &range, |url| {
            Ok(if url.ends_with("/archives") {
                archives.clone()
            } else {
                archive.clone()
            })
        })
        .unwrap();
        assert_eq!(partial.received, MAX_GAMES + 2);
        assert_eq!(partial.games.len(), MAX_GAMES);
        assert_eq!(partial.errors.len(), 2);
        assert!(partial.errors[1].contains("exceeded 500 games"));
    }

    #[test]
    fn restricted_archive_paths_and_http_headers() {
        let range = GameRange {
            from_ms: 1_733_011_200_000, // December 1, 2024 at UTC midnight
            to_ms: 1_735_776_000_000,   // January 2, 2025 at UTC midnight
        };
        assert_eq!(
            archive_urls("games", &range),
            ["games/2024/12", "games/2025/01"]
        );
        let reply = Response::parse(b"HTTP/2 103\r\nLink: hints\r\n\r\nHTTP/2 429\r\nRetry-After: 120\r\nETag: W/\"1\"\r\n\r\nbody").unwrap();
        assert_eq!(reply.status, 429);
        assert_eq!(reply.header("etag"), Some("W/\"1\""));
        assert_eq!(
            reply.header("retry-after").and_then(retry_delay),
            Some(Duration::from_secs(120))
        );
        assert_eq!(reply.body, b"body");
        assert!(!valid_account("player?max=10000"));
    }
}
