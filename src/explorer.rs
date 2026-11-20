//! Lichess position statistics for the rated Rapid and Masters populations.
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
pub(crate) const TOKEN_FILE: &str = "lichess-explorer.token";
const ENDPOINT: &str = "https://explorer.lichess.org/lichess";

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Ratings {
    Below1500,
    #[default]
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

#[derive(Deserialize, Serialize)]
struct Counts {
    white: u64,
    draws: u64,
    black: u64,
}

impl Counts {
    fn buckets(&self) -> [u64; 3] {
        [self.white, self.draws, self.black]
    }

    fn validate_total(&self) -> Result<(), &'static str> {
        const MAX_SAFE_COUNT: u64 = (1 << 53) - 1;
        self.buckets()
            .into_iter()
            .try_fold(0u64, u64::checked_add)
            .filter(|total| *total <= MAX_SAFE_COUNT)
            .map(|_| ())
            .ok_or("invalid_reply")
    }
}

#[derive(Deserialize, Serialize)]
struct MoveCounts {
    uci: String,
    #[serde(flatten)]
    counts: Counts,
}

#[derive(Deserialize, Serialize)]
pub(crate) struct Statistics {
    #[serde(flatten)]
    counts: Counts,
    moves: Vec<MoveCounts>,
}

pub struct ExplorerClient {
    pub(crate) token_path: PathBuf,
    connection: Mutex<()>,
    retry_after: Mutex<Option<Instant>>,
}
impl ExplorerClient {
    pub fn new(database: &Path) -> Self {
        Self {
            token_path: fs::canonicalize(database)
                .unwrap_or_else(|_| database.to_owned())
                .with_file_name(TOKEN_FILE),
            connection: Mutex::default(),
            retry_after: Mutex::default(),
        }
    }
    pub(crate) fn limited(&self) -> bool {
        self.retry_after
            .lock()
            .expect("Explorer retry deadline")
            .is_some_and(|deadline| Instant::now() < deadline)
    }
    pub(crate) fn position(
        &self,
        fen: &str,
        legal: &[String],
        ratings: Option<Ratings>,
    ) -> Result<Statistics, &'static str> {
        // Rated, Masters, and Fix Response share one outbound connection budget.
        let _connection = self.connection.lock().expect("Explorer connection");
        if self.limited() {
            return Err("rate_limited");
        }
        let result = lookup(&self.token_path, fen, legal, ratings);
        if matches!(result, Err("rate_limited")) {
            *self.retry_after.lock().expect("Explorer retry deadline") =
                Some(Instant::now() + Duration::from_secs(60));
        }
        result
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

fn lookup(
    path: &Path,
    fen: &str,
    legal: &[String],
    ratings: Option<Ratings>,
) -> Result<Statistics, &'static str> {
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
        if ratings.is_some() {
            ENDPOINT
        } else {
            "https://explorer.lichess.org/masters"
        },
        "--data-urlencode",
        "moves=12",
        "--data-urlencode",
        "topGames=0",
        "--data-urlencode",
        &format!("fen={fen}"),
    ]);
    if ratings.is_some() {
        command.args([
            "--data-urlencode",
            "variant=standard",
            "--data-urlencode",
            "speeds=rapid",
            "--data-urlencode",
            "recentGames=0",
        ]);
    }
    if let Some(bucket) = ratings.and_then(|rating| rating.bucket()) {
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
    parse_reply(&output.stdout, legal)
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
    stats.counts.validate_total()?;
    let population = stats.counts.buckets();
    let mut returned = [0u64; 3];
    for item in &stats.moves {
        item.counts.validate_total()?;
        for (index, count) in item.counts.buckets().into_iter().enumerate() {
            returned[index] = returned[index]
                .checked_add(count)
                .filter(|sum| *sum <= population[index])
                .ok_or("invalid_reply")?;
        }
    }
    stats.moves.truncate(12);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_rejects_inconsistent_populations_before_truncating() {
        use serde_json::json;
        let legal: Vec<_> = (0..13).map(|index| format!("move-{index}")).collect();
        let mut rows: Vec<_> = legal
            .iter()
            .map(|uci| json!({"uci":uci,"white":1,"draws":0,"black":0}))
            .collect();
        let parse = |white: u64, draws: u64, black: u64, rows| {
            let body = json!({"white":white,"draws":draws,"black":black,"moves":rows});
            parse_reply(format!("{body}\n200").as_bytes(), &legal)
        };
        const SAFE: u64 = (1 << 53) - 1;
        assert_eq!(parse(12, 0, 0, rows.clone()).err(), Some("invalid_reply"));
        assert_eq!(parse(0, 0, 0, rows.clone()).err(), Some("invalid_reply"));
        assert_eq!(parse(SAFE, 1, 0, vec![]).err(), Some("invalid_reply"));
        assert_eq!(parse(u64::MAX, 1, 0, vec![]).err(), Some("invalid_reply"));
        rows[0]["white"] = json!(SAFE);
        rows[0]["draws"] = json!(1);
        assert_eq!(parse(SAFE, 0, 0, rows).err(), Some("invalid_reply"));
        assert!(parse(0, 0, 0, vec![]).unwrap().moves.is_empty());
        let valid = parse(
            100,
            0,
            0,
            vec![json!({"uci":legal[0],"white":5,"draws":0,"black":0})],
        )
        .unwrap();
        assert_eq!(valid.moves[0].counts.white, 5);
        assert_eq!(valid.counts.white, 100);
    }

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
