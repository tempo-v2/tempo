//! Optional Lichess statistics for the validated learner position in a repertoire gap.
use crate::{AppError, Tempo, game_evidence::check_id, game_model::GapRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempo_chess::GapDraft;

const TOKEN_FILE: &str = "lichess-explorer.token";
const ENDPOINT: &str = "https://explorer.lichess.org/lichess";

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Ratings {
    Below1500,
    Around1500,
    Above1500,
    All,
}

impl Ratings {
    fn bucket(&self) -> Option<&'static str> {
        match self {
            Self::Below1500 => Some("1200"),
            Self::Around1500 => Some("1400"),
            Self::Above1500 => Some("1600"),
            Self::All => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapExplorerRequest {
    request_id: String,
    gap: GapRequest,
    ratings: Ratings,
}

#[derive(Deserialize, Serialize)]
struct Counts {
    white: u64,
    draws: u64,
    black: u64,
}

#[derive(Deserialize, Serialize)]
struct MoveCounts {
    uci: String,
    #[serde(flatten)]
    counts: Counts,
}

#[derive(Deserialize, Serialize)]
struct Statistics {
    #[serde(flatten)]
    counts: Counts,
    moves: Vec<MoveCounts>,
}

pub struct GapExplorer {
    token_path: PathBuf,
    reply: Arc<Mutex<Value>>,
    retry_after: Arc<Mutex<Option<Instant>>>,
}

impl GapExplorer {
    pub fn new(database: &Path) -> Self {
        Self {
            token_path: fs::canonicalize(database)
                .unwrap_or_else(|_| database.to_owned())
                .with_file_name(TOKEN_FILE),
            reply: Arc::new(Mutex::new(json!({"status": "idle"}))),
            retry_after: Arc::new(Mutex::new(None)),
        }
    }

    pub fn status(&self) -> Value {
        self.reply.lock().expect("Explorer reply").clone()
    }

    pub fn start(&self, app: &Tempo, request: GapExplorerRequest) -> Result<Value, AppError> {
        check_id(&request.request_id)?;
        if self.status()["status"] == "running" {
            return Err(AppError::new(
                "explorer_busy",
                "A Lichess lookup is still running.",
            ));
        }
        if self
            .retry_after
            .lock()
            .expect("Explorer retry deadline")
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return Err(AppError::new(
                "rate_limited",
                "Lichess is limiting requests. Try again in a minute.",
            ));
        }
        let (draft, revision) = app.gap_position(request.gap)?;
        let queued = json!({
            "status": "running", "request_id": request.request_id, "content_revision": revision,
        });
        let mut result = queued.clone();
        *self.reply.lock().expect("Explorer reply") = queued.clone();
        let reply = Arc::clone(&self.reply);
        let token_path = self.token_path.clone();
        let retry_after = Arc::clone(&self.retry_after);
        if std::thread::Builder::new()
            .name("tempo-gap-explorer".into())
            .spawn(move || {
                match lookup(&token_path, &draft, request.ratings) {
                    Ok(stats) => {
                        result["status"] = json!("complete");
                        result["statistics"] = json!(stats);
                    }
                    Err(code) => {
                        result["status"] = json!("unavailable");
                        result["code"] = json!(code);
                        match code {
                            "rate_limited" => {
                                *retry_after.lock().expect("Explorer retry deadline") =
                                    Some(Instant::now() + Duration::from_secs(60));
                            }
                            "token_missing" | "token_invalid" | "token_rejected" => {
                                result["token_path"] = json!(token_path.to_string_lossy());
                            }
                            _ => {}
                        }
                    }
                }
                *reply.lock().expect("Explorer reply") = result;
            })
            .is_err()
        {
            *self.reply.lock().expect("Explorer reply") = json!({"status": "idle"});
            return Err(AppError::new(
                "explorer_unavailable",
                "The Lichess lookup could not start.",
            ));
        }
        Ok(queued)
    }
}

fn token(path: &Path) -> Result<String, &'static str> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err("token_missing"),
        _ => return Err("token_invalid"),
    }
    let mut text = String::new();
    File::open(path)
        .and_then(|file| file.take(513).read_to_string(&mut text))
        .map_err(|_| "token_invalid")?;
    let value = text.trim();
    if value.is_empty()
        || text.len() > 512
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    {
        return Err("token_invalid");
    }
    Ok(value.to_owned())
}

fn lookup(path: &Path, draft: &GapDraft, ratings: Ratings) -> Result<Statistics, &'static str> {
    let token = token(path)?;
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--disable",
        "--silent",
        "--connect-timeout",
        "3",
        "--max-time",
        "8",
        "--max-filesize",
        "131072",
        "--proto",
        "=https",
        "--config",
        "-",
        "--write-out",
        "\n%{http_code}",
        "--get",
        ENDPOINT,
        "--data-urlencode",
        "variant=standard",
        "--data-urlencode",
        "speeds=rapid",
        "--data-urlencode",
        "moves=12",
        "--data-urlencode",
        "topGames=0",
        "--data-urlencode",
        "recentGames=0",
        "--data-urlencode",
        &format!("fen={}", draft.fen),
    ]);
    if let Some(bucket) = ratings.bucket() {
        command.args(["--data-urlencode", &format!("ratings={bucket}")]);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "offline")?;
    // Pass the credential through stdin so it never appears in a process argument.
    let wrote = writeln!(
        child.stdin.take().expect("piped curl stdin"),
        "header = \"Authorization: Bearer {token}\""
    );
    let output = child.wait_with_output().map_err(|_| "offline")?;
    wrote.map_err(|_| "offline")?;
    if !output.status.success() {
        return Err(if output.status.code() == Some(28) {
            "timeout"
        } else {
            "offline"
        });
    }
    parse_reply(&output.stdout, &draft.legal_moves)
}

fn parse_reply(output: &[u8], legal: &[String]) -> Result<Statistics, &'static str> {
    let split = output
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or("invalid_reply")?;
    match &output[split + 1..] {
        b"200" => {}
        b"401" | b"403" => return Err("token_rejected"),
        b"429" => return Err("rate_limited"),
        _ => return Err("offline"),
    }
    let mut stats: Statistics =
        serde_json::from_slice(&output[..split]).map_err(|_| "invalid_reply")?;
    let mut seen = BTreeSet::new();
    stats
        .moves
        .retain(|item| legal.contains(&item.uci) && seen.insert(item.uci.clone()));
    stats.moves.truncate(12);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_explorer_counts_and_only_returns_legal_distinct_candidates() {
        let stats = parse_reply(
            br#"{"white":55,"draws":5,"black":40,"moves":[
                {"uci":"e4d5","white":30,"draws":3,"black":20},
                {"uci":"e4d5","white":30,"draws":3,"black":20},
                {"uci":"e4e6","white":4,"draws":0,"black":8}]}
200"#,
            &["e4d5".into()],
        )
        .unwrap();
        assert_eq!(stats.counts.white, 55);
        assert_eq!(stats.moves.len(), 1);
        assert_eq!(stats.moves[0].counts.draws, 3);
        assert_eq!(parse_reply(b"\n429", &[]).err(), Some("rate_limited"));
        assert_eq!(parse_reply(b"\n401", &[]).err(), Some("token_rejected"));
        assert_eq!(parse_reply(b"{}\n200", &[]).err(), Some("invalid_reply"));
    }
}
