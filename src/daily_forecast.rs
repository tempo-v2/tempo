//! The private immutable daily forecast exchange. The worker holds no Store capability.

use crate::daily_openings::{ForecastReply, ForecastRoute};
use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;
use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    signal::unix::{SignalKind, signal},
    sync::watch,
    time::Instant,
};

const BYTES: usize = 32 * 1024 * 1024;
const BUDGET: Duration = Duration::from_secs(300);
const POLL: Duration = Duration::from_secs(2);
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

#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Score {
    Estimated { probability: f64 },
    Unscored,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub profile: String,
    pub basis: String,
    pub succeeded: bool,
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

pub fn directory(database: &Path) -> PathBuf {
    database.with_extension("dq")
}

fn safe_directory(directory: &Path) -> Result<bool> {
    match fs::symlink_metadata(directory) {
        Ok(info) => {
            ensure!(
                info.is_dir() && info.permissions().mode() & 0o777 == 0o700,
                "Daily forecast directory must be owner-only"
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn check_day(day: &str) -> Result<()> {
    ensure!(
        day.len() == 10 && NaiveDate::parse_from_str(day, "%Y-%m-%d")?.to_string() == day,
        "Invalid daily forecast day"
    );
    Ok(())
}

fn is_private_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(info) => {
            ensure!(
                info.is_file() && info.permissions().mode() & 0o777 == 0o600,
                "Daily forecast file must be owner-only"
            );
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn open_private_file(path: &Path) -> Result<Option<File>> {
    if !is_private_file(path)? {
        return Ok(None);
    }
    Ok(Some(File::open(path)?))
}

fn decode<T: DeserializeOwned>(file: File) -> Result<(T, Vec<u8>)> {
    let mut bytes = Vec::new();
    file.take(BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= BYTES, "Daily forecast exceeds 32 MiB");
    Ok((serde_json::from_slice(&bytes)?, bytes))
}

struct BoundedFile<'a> {
    file: &'a mut File,
    remaining: usize,
}

impl Write for BoundedFile<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Daily forecast exceeds 32 MiB",
            ));
        }
        let count = self.file.write(bytes)?;
        self.remaining -= count;
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn publish(directory: &Path, path: &Path, message: &impl Serialize) -> Result<()> {
    publish_file(directory, path, message, false)
}

fn publish_file(
    directory: &Path,
    path: &Path,
    message: &impl Serialize,
    replace: bool,
) -> Result<()> {
    let mut file = tempfile::Builder::new()
        .prefix(".pending-")
        .tempfile_in(directory)?;
    {
        let mut output = io::BufWriter::new(BoundedFile {
            file: file.as_file_mut(),
            remaining: BYTES,
        });
        serde_json::to_writer(&mut output, message)?;
        output.flush()?;
    }
    file.as_file().sync_all()?;
    let persisted = if replace {
        file.persist(path)
    } else {
        file.persist_noclobber(path)
    };
    match persisted {
        Ok(_) => File::open(directory)?.sync_all()?,
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(is_private_file(path)?, "Daily forecast file disappeared");
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn active_day(directory: &Path) -> Result<Option<String>> {
    let Some(file) = open_private_file(&directory.join("active.json"))? else {
        return Ok(None);
    };
    let day: String = serde_json::from_reader(file.take(64))?;
    check_day(&day)?;
    Ok(Some(day))
}

pub fn request_exists(directory: &Path, day: &str) -> Result<bool> {
    check_day(day)?;
    Ok(safe_directory(directory)?
        && active_day(directory)?.as_deref() == Some(day)
        && is_private_file(&directory.join(format!("{day}.request.json")))?)
}

pub fn publish_request(directory: &Path, day: &str, request: &Request) -> Result<()> {
    if request_exists(directory, day)? {
        return Ok(());
    }
    if !safe_directory(directory)? {
        match fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {
                let parent = directory
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty());
                File::open(parent.unwrap_or_else(|| Path::new(".")))?.sync_all()?;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        ensure!(
            safe_directory(directory)?,
            "Daily forecast directory disappeared"
        );
    }
    let path = directory.join(format!("{day}.request.json"));
    if !is_private_file(&path)? {
        publish(directory, &path, request)?;
    }
    publish_file(directory, &directory.join("active.json"), &day, true)?;
    clean_previous(directory, day)
}

pub fn read_response(directory: &Path, day: &str) -> Result<Option<Response>> {
    check_day(day)?;
    if !safe_directory(directory)? {
        return Ok(None);
    }
    let response = open_private_file(&directory.join(format!("{day}.response.json")))?;
    response
        .map(|file| decode(file).map(|(message, _)| message))
        .transpose()
}

fn mark_started(directory: &Path, day: &str) -> Result<bool> {
    let path = directory.join(format!("{day}.started"));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(marker) => {
            marker.sync_all()?;
            File::open(directory)?.sync_all()?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            ensure!(is_private_file(&path)?, "Daily forecast marker disappeared");
            Ok(false)
        }
        Err(error) => Err(error.into()),
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

async fn complete(directory: &Path, day: &str, stop: &mut watch::Receiver<bool>) -> Result<()> {
    let response_path = directory.join(format!("{day}.response.json"));
    if is_private_file(&response_path)? {
        return Ok(());
    }
    let Some(file) = open_private_file(&directory.join(format!("{day}.request.json")))? else {
        return Ok(());
    };
    let remaining =
        BUDGET.saturating_sub(file.metadata()?.modified()?.elapsed().unwrap_or_default());
    let deadline = Instant::now() + remaining;
    let (request, bytes): (Request, _) = decode(file)?;
    let response = if mark_started(directory, day)?
        && request.profile == PROFILE
        && Instant::now() < deadline
        && !*stop.borrow()
    {
        match evaluate(bytes, deadline, stop).await.and_then(|response| {
            response.verify(&request.basis, &request.cards)?;
            ensure!(Instant::now() < deadline, "Daily forecast timed out");
            Ok(response)
        }) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("Daily forecast for {day}: {error}");
                Response::failed(request.basis)
            }
        }
    } else {
        Response::failed(request.basis)
    };
    publish(directory, &response_path, &response)
}

fn clean_previous(directory: &Path, day: &str) -> Result<()> {
    let earliest = NaiveDate::parse_from_str(day, "%Y-%m-%d")?
        .pred_opt()
        .context("Invalid first Study day")?
        .to_string();
    for entry in fs::read_dir(directory)?.take(32) {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let previous_day = [".request.json", ".response.json", ".started"]
            .iter()
            .any(|suffix| {
                name.strip_suffix(suffix)
                    .is_some_and(|date| date < earliest.as_str() && check_day(date).is_ok())
            });
        let abandoned_temp = name.starts_with(".pending-")
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified.elapsed().is_ok_and(|age| age > BUDGET));
        if (previous_day || abandoned_temp) && is_private_file(&entry.path())? {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// Run the independently supervised worker against file exchange only.
pub fn worker(directory: &Path) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut terminate = signal(SignalKind::terminate())?;
            let mut interrupt = signal(SignalKind::interrupt())?;
            let (shutdown, mut stop) = watch::channel(false);
            tokio::spawn(async move {
                tokio::select! {
                    _ = terminate.recv() => (),
                    _ = interrupt.recv() => (),
                }
                let _ = shutdown.send(true);
            });
            let mut worker_lock = None;
            loop {
                if safe_directory(directory)? {
                    if worker_lock.is_none() {
                        let path = directory.join("worker.lock");
                        is_private_file(&path)?;
                        let lock = OpenOptions::new()
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .mode(0o600)
                            .open(&path)?;
                        FileExt::try_lock_exclusive(&lock)
                            .context("Daily forecast worker already running")?;
                        worker_lock = Some(lock);
                    }
                    if let Some(day) = active_day(directory)? {
                        complete(directory, &day, &mut stop).await?;
                    }
                }
                tokio::select! {
                    _ = stop.wait_for(|stopped| *stopped) => return Ok(()),
                    _ = tokio::time::sleep(POLL) => (),
                }
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abandoned_claim_fails_once_without_replacing_the_request() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let directory = directory(&temporary.path().join("tempo.sqlite3"));
        let day = "2026-11-27";
        let mut request = Request {
            profile: PROFILE.to_owned(),
            basis: "original basis".into(),
            routes: Vec::new(),
            replies: Vec::new(),
            cards: vec!["a".into()],
        };
        assert!(!request_exists(&directory, day)?);
        publish_request(&directory, day, &request)?;
        assert!(request_exists(&directory, day)?);
        assert_eq!(active_day(&directory)?.as_deref(), Some(day));
        assert!(mark_started(&directory, day)?);
        request.basis = "later basis".into();
        publish_request(&directory, day, &request)?;

        let (_shutdown, mut stop) = watch::channel(false);
        complete(&directory, day, &mut stop).await?;
        complete(&directory, day, &mut stop).await?;
        let response = read_response(&directory, day)?.expect("abandoned request finished");
        assert_eq!(response.basis, "original basis");
        assert!(!response.succeeded);
        assert!(response.scores.is_empty());
        assert!(request_exists(&directory, "../bad-day").is_err());
        Ok(())
    }
}
