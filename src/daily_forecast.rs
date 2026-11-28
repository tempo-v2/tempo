//! Private forecast protocol and model worker. The worker has no Store capability.
use crate::daily_openings::{ForecastReply, ForecastRoute};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    signal::unix::{SignalKind, signal},
    sync::watch,
    time::{Instant, timeout_at},
};

const BYTES: usize = 32 * 1024 * 1024;
pub const BUDGET: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(2);
const FRAME_TIME: Duration = Duration::from_secs(15);
const VERSION: u8 = 1;
pub const PROFILE: &str = "maia3-5m-1500-authored-history-v2";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub profile: String,
    pub basis: String,
    pub routes: Vec<ForecastRoute>,
    pub replies: Vec<ForecastReply>,
    pub cards: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Score {
    Estimated { probability: f64 },
    Unscored,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub profile: String,
    pub basis: String,
    pub succeeded: bool,
    #[serde(deserialize_with = "distinct_scores")]
    pub scores: BTreeMap<String, Score>,
}

impl Response {
    fn failed(basis: String) -> Self {
        Self {
            profile: PROFILE.to_owned(),
            basis,
            succeeded: false,
            scores: BTreeMap::new(),
        }
    }

    pub fn verify(&self, basis: &str, cards: &[String]) -> Result<()> {
        ensure!(
            self.profile == PROFILE,
            "Incompatible daily forecast profile"
        );
        ensure!(self.basis == basis, "Unexpected daily forecast basis");
        ensure!(self.succeeded, "Daily model did not complete");
        ensure!(
            self.scores.len() == cards.len()
                && cards.iter().all(|card| self.scores.contains_key(card))
                && self.scores.values().all(|score| match score {
                    Score::Estimated { probability } => (0.0..=1.0).contains(probability),
                    Score::Unscored => true,
                }),
            "Invalid daily forecast scores"
        );
        Ok(())
    }
}

async fn evaluate(
    bytes: Vec<u8>,
    deadline: Instant,
    stop: &mut watch::Receiver<bool>,
) -> Result<Response> {
    let root = PathBuf::from(std::env::var_os("HOME").context("Missing home directory")?)
        .join("Library/Application Support/Tempo Lean/Maia3");
    let mut child = Command::new(root.join("venv/bin/python"))
        .arg("-c")
        .arg(include_str!("../tools/daily_forecast.py"))
        .arg(&root)
        .arg(include_str!("../tools/coverage.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Maia is unavailable for daily forecast")?;
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let exchange = async {
        let write = async move {
            input.write_all(&bytes).await?;
            Ok::<_, anyhow::Error>(())
        };
        let read = async move {
            let mut bytes = Vec::new();
            output
                .take(BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(bytes.len() <= BYTES, "Daily forecast exceeds 32 MiB");
            Ok::<_, anyhow::Error>(bytes)
        };
        let (_, bytes) = tokio::try_join!(write, read)?;
        let status = child.wait().await?;
        ensure!(status.code() != Some(75), "Maia is already running");
        ensure!(
            status.success(),
            "Maia could not complete the daily forecast"
        );
        Ok(serde_json::from_slice(&bytes)?)
    };
    let result = tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => Err(anyhow::anyhow!("Daily forecast stopped")),
        _ = tokio::time::sleep_until(deadline) => Err(anyhow::anyhow!("Daily forecast timed out")),
        result = exchange => result,
    };
    if result.is_err() {
        let _ = child.start_kill();
        child.wait().await?;
    }
    result
}

#[derive(Deserialize, Serialize)]
pub struct Claim {
    pub ticket: String,
    pub expires: i64,
    pub request: Request,
}

#[derive(Deserialize, Serialize)]
enum CommandBody {
    ClaimToday { profile: String },
    SubmitForecast { ticket: String, outcome: Response },
}

#[derive(Deserialize, Serialize)]
struct Envelope {
    version: u8,
    command: CommandBody,
}

#[derive(Deserialize, Serialize)]
enum Reply {
    Work(Claim),
    NoWork,
    Busy,
    Unsupported,
    Delivery(Delivery),
}

#[derive(Deserialize, Serialize, PartialEq, Debug)]
pub enum Delivery {
    Published,
    WithoutRank,
    Stale,
    Invalid,
    Busy,
}

fn distinct_scores<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<BTreeMap<String, Score>, D::Error> {
    struct Scores;
    impl<'de> serde::de::Visitor<'de> for Scores {
        type Value = BTreeMap<String, Score>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("one score per card")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut scores = BTreeMap::new();
            while let Some((id, score)) = map.next_entry()? {
                if scores.insert(id, score).is_some() {
                    return Err(serde::de::Error::custom("duplicate forecast card"));
                }
            }
            Ok(scores)
        }
    }
    d.deserialize_map(Scores)
}

pub fn check_request_size(request: &Request) -> Result<()> {
    // Reserve space for the fixed-size claim envelope.
    ensure!(
        serde_json::to_vec(request)?.len() <= BYTES - 256,
        "Forecast request exceeds its budget"
    );
    Ok(())
}

async fn read_frame(stream: &mut UnixStream, limit: Instant) -> Result<Vec<u8>> {
    timeout_at(limit, async {
        let len = stream.read_u32().await? as usize;
        ensure!(len <= BYTES, "Forecast frame exceeds its budget");
        let mut bytes = vec![0; len];
        stream.read_exact(&mut bytes).await?;
        Ok(bytes)
    })
    .await?
}

async fn write_frame(stream: &mut UnixStream, bytes: &[u8], limit: Instant) -> Result<()> {
    ensure!(bytes.len() <= BYTES, "Forecast frame exceeds its budget");
    timeout_at(limit, async {
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(bytes).await
    })
    .await??;
    Ok(())
}

fn socket(directory: &Path) -> PathBuf {
    directory.join("forecast.sock")
}

async fn exchange<T: DeserializeOwned>(
    directory: &Path,
    command: CommandBody,
    limit: Instant,
) -> Result<T> {
    let admission = limit.min(Instant::now() + FRAME_TIME);
    let socket = socket(directory);
    let mut stream = timeout_at(admission, UnixStream::connect(socket)).await??;
    let bytes = serde_json::to_vec(&Envelope {
        version: VERSION,
        command,
    })?;
    write_frame(&mut stream, &bytes, admission).await?;
    let response = read_frame(&mut stream, limit).await?;
    Ok(serde_json::from_slice(&response)?)
}

pub fn worker(directory: &Path) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let (shutdown, mut stop) = watch::channel(false);
            let mut worker_lock = None;
            tokio::spawn(async move {
                tokio::select! { _ = terminate.recv() => (), _ = interrupt.recv() => () }
                let _ = shutdown.send(true);
            });
            while !*stop.borrow() {
                if worker_lock.is_none() {
                    use fs2::FileExt;
                    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
                    match std::fs::symlink_metadata(directory) {
                        Ok(metadata) => {
                            ensure!(
                                metadata.is_dir() && metadata.mode() & 0o777 == 0o700,
                                "Daily forecast requires an owner-only directory"
                            );
                            let lock = std::fs::OpenOptions::new()
                                .create(true)
                                .truncate(false)
                                .write(true)
                                .mode(0o600)
                                .open(directory.join("worker.lock"))?;
                            lock.try_lock_exclusive()
                                .context("Daily forecast worker already running")?;
                            worker_lock = Some(lock);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            tokio::select! {
                                _ = stop.wait_for(|done| *done) => break,
                                _ = tokio::time::sleep(POLL) => continue,
                            }
                        }
                        Err(error) => return Err(error.into()),
                    }
                }

                if let Err(error) = worker_turn(directory, &mut stop).await {
                    // Socket absence is normal during managed startup and restart.
                    if !error.is::<std::io::Error>() {
                        eprintln!("Daily forecast: {error:#}");
                    }
                }
                tokio::select! {
                    _ = stop.wait_for(|done| *done) => break,
                    _ = tokio::time::sleep(POLL) => (),
                }
            }
            Ok(())
        })
}

async fn worker_turn(directory: &Path, stop: &mut watch::Receiver<bool>) -> Result<()> {
    let reply: Reply = exchange(
        directory,
        CommandBody::ClaimToday {
            profile: PROFILE.to_owned(),
        },
        Instant::now() + FRAME_TIME,
    )
    .await?;
    let Reply::Work(claim) = reply else {
        return Ok(());
    };
    let remaining = claim
        .expires
        .saturating_sub(chrono::Utc::now().timestamp())
        .max(0) as u64;
    let deadline = Instant::now() + Duration::from_secs(remaining);
    if Instant::now() >= deadline || *stop.borrow() {
        return Ok(());
    }
    let outcome = match evaluate(serde_json::to_vec(&claim.request)?, deadline, stop).await {
        Ok(result) => result,
        Err(error) => {
            eprintln!("Daily forecast model: {error:#}");
            Response::failed(claim.request.basis)
        }
    };
    let apply_limit = deadline + Duration::from_secs(60);
    while Instant::now() < apply_limit && !*stop.borrow() {
        let reply = exchange::<Reply>(
            directory,
            CommandBody::SubmitForecast {
                ticket: claim.ticket.clone(),
                outcome: outcome.clone(),
            },
            apply_limit,
        );
        let result = tokio::select! {
            _ = stop.wait_for(|done| *done) => break,
            result = reply => result,
        };
        match result {
            Ok(Reply::Busy | Reply::Delivery(Delivery::Busy)) | Err(_) => {
                tokio::select! {
                    _ = stop.wait_for(|done| *done) => break,
                    _ = tokio::time::sleep_until((Instant::now() + POLL).min(apply_limit)) => (),
                }
            }
            _ => break,
        }
    }
    Ok(())
}

mod server;
pub use server::start;
