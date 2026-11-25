use super::{CURRENT_MODEL, Forecast, Projection, Scan, revision};
use crate::{ApiError, ApiResult, App, Database, catalog, chess, db};
use anyhow::{Context, Result, ensure};
use axum::{Json, extract::State, http::StatusCode};
use rusqlite::{Connection, TransactionBehavior};
use serde::Serialize;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{Semaphore, watch},
};

const BYTES: usize = 32 * 1024 * 1024;
const BUDGET: Duration = Duration::from_secs(300);
static SLOT: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanFailure {
    CoverageBusy,
    CoverageTimeout,
    CoverageTooLarge,
    CoverageCancelled,
}

impl ScanFailure {
    pub fn status(self) -> StatusCode {
        match self {
            Self::CoverageBusy | Self::CoverageCancelled => StatusCode::SERVICE_UNAVAILABLE,
            Self::CoverageTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::CoverageTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        }
    }
}
impl std::fmt::Display for ScanFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::CoverageBusy => "A coverage scan is already running. Retry after it finishes.",
            Self::CoverageTimeout => "The coverage scan timed out. Retry with a shorter horizon.",
            Self::CoverageTooLarge => "Coverage model input or output exceeds 32 MiB.",
            Self::CoverageCancelled => "Coverage scan cancelled.",
        })
    }
}
impl std::error::Error for ScanFailure {}

#[derive(Clone)]
struct Lifetime {
    deadline: Instant,
    caller: watch::Receiver<()>,
    shutdown: watch::Receiver<bool>,
}
impl Lifetime {
    fn check(&self) -> Result<()> {
        ensure!(Instant::now() < self.deadline, ScanFailure::CoverageTimeout);
        ensure!(
            self.caller.has_changed().is_ok() && !*self.shutdown.borrow(),
            ScanFailure::CoverageCancelled
        );
        Ok(())
    }

    async fn stopped(mut self) -> ScanFailure {
        tokio::select! {
            _ = tokio::time::sleep_until(self.deadline.into()) => ScanFailure::CoverageTimeout,
            _ = self.caller.changed() => ScanFailure::CoverageCancelled,
            _ = self.shutdown.wait_for(|stop| *stop) => ScanFailure::CoverageCancelled,
        }
    }
}

struct Snapshot {
    id: String,
    revision: i64,
    input: Option<Vec<u8>>,
}

// Read one coherent identity. Only a cache miss needs the addressed cards.
fn snapshot(path: &Path, scan: &Scan, life: &Lifetime) -> Result<Snapshot> {
    life.check()?;
    let mut db = Database::read_with_busy_timeout(path, Duration::ZERO)?;
    let guard = life.clone();
    db.progress_handler(1000, Some(move || guard.check().is_err()));
    let tx = db.transaction()?;
    catalog::existing_repertoire(&tx, &scan.repertoire, &scan.color)?;
    let revision = revision(&tx, &scan.repertoire)?;
    let id = chess::id(&[
        CURRENT_MODEL,
        &db::dataset(&tx)?,
        &revision.to_string(),
        &serde_json::to_string(scan)?,
    ]);
    let cached: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM coverage_report WHERE id=?1)",
        [&id],
        |r| r.get(0),
    )?;
    if cached {
        return Ok(Snapshot {
            id,
            revision,
            input: None,
        });
    }
    let mut input = serde_json::to_vec(scan)?;
    input.pop(); // Extend the serialized scan object with its card array.
    input.extend_from_slice(br#","cards":["#);
    let mut query=tx.prepare("SELECT id,fen,answers,quarantined FROM cards WHERE kind='opening' AND opening_retired=0 AND repertoire_id=?1 AND color=?2")?;
    let mut rows = query.query((&scan.repertoire, &scan.color))?;
    while let Some(row) = rows.next()? {
        life.check()?;
        let card = json!({"id":row.get::<_,String>(0)?,"fen":row.get::<_,String>(1)?,
            "answers":serde_json::from_str::<Vec<String>>(row.get_ref(2)?.as_str()?)?,"quarantined":row.get::<_,bool>(3)?});
        let bytes = serde_json::to_vec(&card)?;
        ensure!(
            input.len() + bytes.len() + 3 <= BYTES,
            ScanFailure::CoverageTooLarge
        );
        input.extend_from_slice(&bytes);
        input.push(b',');
    }
    if input.last() == Some(&b',') {
        input.pop();
    }
    input.extend_from_slice(b"]}");
    Ok(Snapshot {
        id,
        revision,
        input: Some(input),
    })
}

async fn evaluate(mut command: Command, input: Vec<u8>, life: &Lifetime) -> Result<Vec<u8>> {
    life.check()?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("Maia is unavailable. Check the Tempo host model setup.")?;
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let exchange = async {
        let write = async move {
            stdin.write_all(&input).await?;
            Ok::<_, anyhow::Error>(())
        };
        let read = async move {
            let mut bytes = Vec::new();
            stdout
                .take(BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            ensure!(bytes.len() <= BYTES, ScanFailure::CoverageTooLarge);
            Ok::<_, anyhow::Error>(bytes)
        };
        let (_, bytes) = tokio::try_join!(write, read)?;
        ensure!(
            child.wait().await?.success(),
            "Maia could not finish this scan. Check the host model setup and retry."
        );
        Ok(bytes)
    };
    let result = tokio::select! {
        biased;
        reason=life.clone().stopped()=>Err(reason.into()),
        result=exchange=>result,
    };
    if result.is_err() {
        let _ = child.start_kill();
        child.wait().await?;
    }
    result
}

fn run(app: App, mut scan: Scan, life: Lifetime) -> Result<String> {
    ensure!(
        scan.through > 0 && scan.through <= 100 && scan.route.moves.len() <= 200,
        "Choose a horizon from 1 to 100 and a starting line of at most 200 plies."
    );
    scan.route.fen = chess::fen(&chess::position(&scan.route.fen)?);
    chess::board_at(&scan.route, scan.route.moves.len())?;
    let snapshot = snapshot(&app.database, &scan, &life);
    life.check()?;
    let Snapshot {
        id,
        revision: expected,
        input,
    } = snapshot?;
    let Some(input) = input else { return Ok(id) };
    let root = PathBuf::from(std::env::var_os("HOME").context("Missing home directory")?)
        .join("Library/Application Support/Tempo Lean/Maia3");
    let mut command = Command::new(root.join("venv/bin/python"));
    command
        .arg("-c")
        .arg(include_str!("../../tools/coverage.py"))
        .arg(&root);
    let runtime = tokio::runtime::Handle::current();
    let bytes = runtime.block_on(evaluate(command, input, &life))?;
    life.check()?;
    let forecast: Forecast = serde_json::from_slice(&bytes)?;
    ensure!(forecast.model.is_current(), "Unexpected forecast model");
    let projection = Projection::prepare(&forecast)?;
    let report = serde_json::to_string(&forecast)?;
    life.check()?;
    let reply = app.writer.submit(false, move |c: &mut Connection| {
        life.check()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure!(
            revision(&tx, &scan.repertoire)? == expected,
            crate::gap_edit::Stale
        );
        let before = tx.total_changes();
        if tx.execute(
            "INSERT OR IGNORE INTO coverage_report VALUES(?1,?2,?3,?4,?5)",
            (
                &id,
                &scan.repertoire,
                expected,
                serde_json::to_string(&scan)?,
                report,
            ),
        )? != 0
        {
            projection.insert(&tx, &id)?;
        }
        ensure!(
            tx.total_changes() - before <= 128,
            "Coverage publication exceeds row budget"
        );
        life.check()?;
        tx.commit()?;
        Ok(id)
    })?;
    runtime.block_on(reply.wait())
}

pub async fn start(State(app): State<App>, Json(scan): Json<Scan>) -> ApiResult {
    let permit = SLOT
        .try_acquire()
        .map_err(|_| ApiError(ScanFailure::CoverageBusy.into()))?;
    let (owner, caller) = watch::channel(());
    let life = Lifetime {
        deadline: Instant::now() + BUDGET,
        caller,
        shutdown: app.shutdown.subscribe(),
    };
    let deadline = life.deadline;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        run(app, scan, life)
    });
    let result = tokio::time::timeout_at(deadline.into(), worker)
        .await
        .map_err(|_| ApiError(ScanFailure::CoverageTimeout.into()))?
        .map_err(|e| ApiError(e.into()))?;
    drop(owner);
    Ok(Json(json!({"id":result.map_err(ApiError)?})))
}
